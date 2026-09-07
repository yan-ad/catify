use super::selected_app_environment;
use crate::{authenticated_session, output::Output, select_text_choice};
use cfy_app::{AppManagementClient, BusinessPlatformClient, exchange_app_management_token};
use cfy_core::{Error, Result};
use cfy_functions::{
    FunctionInfo, FunctionSpec, FunctionsApiClient, FunctionsError, RunOptions,
    build as build_function, function_info, function_specs, list_replay_runs, replay_process_spec,
    resolve_runner, run as run_function, select_replay_run, typegen as typegen_function,
};
use cfy_process::Supervisor;
use clap::{Args, Subcommand};
use notify::{RecursiveMode, Watcher};
use std::{
    env,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;

fn function_replay_event_is_relevant(
    event: &notify::Event,
    function: &FunctionSpec,
    replay: &cfy_functions::ReplayRun,
) -> bool {
    let wasm_parent = function.wasm_path.parent();
    event.paths.iter().any(|path| {
        path == &replay.path
            || (path.starts_with(&function.directory)
                && wasm_parent.is_none_or(|output| !path.starts_with(output)))
    })
}

async fn execute_function_replay(
    function: &FunctionSpec,
    runner: &Path,
    replay: &cfy_functions::ReplayRun,
    json: bool,
) -> Result<()> {
    let process = replay_process_spec(function, runner, replay, json).map_err(functions_error)?;
    let result = Supervisor::new(Duration::from_secs(2))
        .spawn(process)?
        .wait_with_signal_forwarding()
        .await?;
    if result.status.success() {
        Ok(())
    } else {
        Err(Error::process("function replay failed"))
    }
}

async fn watch_function_replay(
    function: &FunctionSpec,
    runner: &Path,
    replay: &cfy_functions::ReplayRun,
    json: bool,
) -> Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })
    .map_err(|error| Error::process(format!("could not start Function watcher: {error}")))?;
    for path in cfy_functions::replay_watch_paths(function, replay) {
        let (watch_path, mode) = if path.is_dir() {
            (path, RecursiveMode::Recursive)
        } else {
            (
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf(),
                RecursiveMode::NonRecursive,
            )
        };
        watcher.watch(&watch_path, mode).map_err(|error| {
            Error::process(format!("could not watch {}: {error}", watch_path.display()))
        })?;
    }
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|error| Error::process(error.to_string()))?;
                return Ok(());
            }
            event = rx.recv() => {
                let Some(event) = event else { return Ok(()); };
                let event = event
                    .map_err(|error| Error::process(format!("Function watcher failed: {error}")))?;
                if !function_replay_event_is_relevant(&event, function, replay) {
                    continue;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                while rx.try_recv().is_ok() {}
                build_function(function, &Supervisor::new(Duration::from_secs(2)))
                    .await
                    .map_err(functions_error)?;
                execute_function_replay(function, runner, replay, json).await?;
            }
        }
    }
}

fn functions_error(error: FunctionsError) -> Error {
    Error::api(error.to_string())
}

fn function_cache_root() -> PathBuf {
    env::var_os("CFY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache").join("catify"))
        })
        .unwrap_or_else(|| PathBuf::from(".catify-cache"))
}

fn selected_function_context(
    context: AppFunctionContext,
    non_interactive: bool,
) -> Result<(cfy_config::project::ProjectEnvironment, FunctionSpec)> {
    let requested_path = context.path.clone().or_else(|| env::current_dir().ok());
    let selected = selected_app_environment(
        context.path,
        context.config,
        context.client_id,
        context.reset,
    )?;
    let graph =
        cfy_config::graph::AppConfigGraph::load_selected(&selected.project, &selected.config_path)?;
    let functions = function_specs(&graph).map_err(functions_error)?;
    if functions.is_empty() {
        return Err(functions_error(FunctionsError::NoFunctions));
    }
    if let Some(requested) = requested_path {
        let canonical = requested.canonicalize().unwrap_or(requested);
        if let Some(found) = functions.iter().find(|function| {
            let directory = function
                .directory
                .canonicalize()
                .unwrap_or_else(|_| function.directory.clone());
            canonical == directory || canonical.starts_with(&directory)
        }) {
            return Ok((selected, found.clone()));
        }
    }
    if functions.len() == 1 {
        return Ok((selected, functions[0].clone()));
    }
    if non_interactive || !io::stdin().is_terminal() {
        return Err(Error::invalid_input(
            "more than one Shopify Function is available; pass --path to select one",
        ));
    }
    let choices = functions
        .iter()
        .map(|function| {
            format!(
                "{} ({})",
                function
                    .name
                    .as_deref()
                    .or(function.handle.as_deref())
                    .unwrap_or("Function"),
                function.directory.display()
            )
        })
        .collect::<Vec<_>>();
    let index = select_text_choice("Which Function do you want to use?", &choices)?;
    Ok((selected, functions[index].clone()))
}

async fn functions_api_client(
    selected: &cfy_config::project::ProjectEnvironment,
    identity: Option<String>,
) -> Result<FunctionsApiClient> {
    let identity = identity.unwrap_or_else(|| "default".into());
    let session = authenticated_session(&identity).await?;
    let client_id = selected
        .document
        .get("client_id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| Error::config("selected app configuration has no client_id"))?;
    let organizations = BusinessPlatformClient::from_session(&session)
        .await?
        .list_organizations()
        .await?;
    let app_management = AppManagementClient::from_session(&session).await?;
    let mut app = None;
    for organization in organizations {
        if app_management
            .list_apps(&organization.id)
            .await?
            .iter()
            .any(|candidate| candidate.client_id == client_id)
        {
            app = Some(
                app_management
                    .app_by_client_id_in_organization(client_id, &organization.id)
                    .await?,
            );
            break;
        }
    }
    let app = app.ok_or_else(|| {
        Error::api(format!(
            "could not find Shopify app `{client_id}` in any accessible organization"
        ))
    })?;
    let token = exchange_app_management_token(&session).await?;
    FunctionsApiClient::new(token.expose(), &app.organization_id, &app.id).map_err(functions_error)
}

pub(super) async fn app_function_command(
    command: AppFunctionCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    let supervisor = || Supervisor::new(Duration::from_secs(2));
    match command {
        AppFunctionCommand::Build { context } => {
            let (_, function) = selected_function_context(context, non_interactive)?;
            build_function(&function, &supervisor())
                .await
                .map_err(functions_error)?;
            output
                .success(
                    "Function compiled to WebAssembly",
                    &serde_json::json!({"wasm_path": function.wasm_path}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppFunctionCommand::Info { context, json } => {
            let (_, function) = selected_function_context(context, non_interactive)?;
            let runner = resolve_runner(&function_cache_root()).map_err(functions_error)?;
            let info: FunctionInfo = function_info(&function, runner);
            output
                .with_json(json)
                .success(&format_function_info(&info), &info)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppFunctionCommand::Schema { context, stdout } => {
            let identity = context.auth_alias.clone();
            let (selected, function) = selected_function_context(context, non_interactive)?;
            let client = functions_api_client(&selected, identity).await?;
            let schema =
                tokio::task::spawn_blocking(move || client.write_schema(&function, stdout))
                    .await
                    .map_err(|error| Error::process(error.to_string()))?
                    .map_err(functions_error)?;
            if stdout {
                output
                    .success(&schema, &serde_json::json!({"schema": schema}))
                    .map_err(|error| Error::process(error.to_string()))?;
            } else {
                output
                    .success(
                        "Function schema written to schema.graphql",
                        &serde_json::json!({"written": true}),
                    )
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            Ok(0)
        }
        AppFunctionCommand::Typegen { context } => {
            let (_, function) = selected_function_context(context, non_interactive)?;
            typegen_function(&function, &supervisor())
                .await
                .map_err(functions_error)?;
            Ok(0)
        }
        AppFunctionCommand::Run {
            context,
            json,
            input,
            export_name,
            profile,
        } => {
            let (_, function) = selected_function_context(context, non_interactive)?;
            build_function(&function, &supervisor())
                .await
                .map_err(functions_error)?;
            let runner = resolve_runner(&function_cache_root()).map_err(functions_error)?;
            run_function(
                &function,
                &runner,
                &RunOptions {
                    input,
                    export: export_name,
                    json,
                    profile,
                    schema_path: None,
                    query_path: None,
                },
                &supervisor(),
            )
            .await
            .map_err(functions_error)?;
            Ok(0)
        }
        AppFunctionCommand::Replay {
            context,
            json,
            log,
            watch,
            no_watch,
        } => {
            let (selected, function) = selected_function_context(context, non_interactive)?;
            let handle = function.handle.as_deref().ok_or_else(|| {
                Error::config("the selected Function has no handle for replay log matching")
            })?;
            let log_directory = selected.project.root().join(".shopify").join("logs");
            let available = list_replay_runs(&log_directory, handle).map_err(functions_error)?;
            if non_interactive && log.is_none() {
                return Err(Error::invalid_input(
                    "--log is required for function replay in non-interactive mode",
                ));
            }
            let selected_log = if let Some(identifier) = log {
                select_replay_run(&log_directory, handle, Some(&identifier))
                    .map_err(functions_error)?
            } else if available.len() == 1 {
                available.into_iter().next().expect("one replay log")
            } else if non_interactive || !io::stdin().is_terminal() {
                return Err(Error::invalid_input(
                    "--log is required when more than one replay log is available non-interactively",
                ));
            } else {
                let choices = available
                    .iter()
                    .map(|run| run.data.identifier.clone())
                    .collect::<Vec<_>>();
                let index =
                    select_text_choice("Which function run do you want to replay?", &choices)?;
                available
                    .into_iter()
                    .nth(index)
                    .expect("selected replay log")
            };
            build_function(&function, &supervisor())
                .await
                .map_err(functions_error)?;
            let runner = resolve_runner(&function_cache_root()).map_err(functions_error)?;
            execute_function_replay(&function, &runner, &selected_log, json).await?;
            if !no_watch || watch {
                watch_function_replay(&function, &runner, &selected_log, json).await?;
            }
            Ok(0)
        }
    }
}

fn format_function_info(info: &FunctionInfo) -> String {
    let targets = info
        .targeting
        .keys()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Handle: {}\nName: {}\nAPI version: {}\nTargets: {}\nSchema: {}\nWASM: {}\nRunner: {}",
        info.handle.as_deref().unwrap_or("-"),
        info.name.as_deref().unwrap_or("-"),
        info.api_version.as_deref().unwrap_or("-"),
        if targets.is_empty() { "-" } else { &targets },
        info.schema_path
            .as_ref()
            .map_or_else(|| "-".into(), |path| path.display().to_string()),
        info.wasm_path.display(),
        info.function_runner_path.display()
    )
}

#[derive(Clone, Debug, Args)]
pub struct AppFunctionContext {
    #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
    config: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
    auth_alias: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID", conflicts_with = "config")]
    client_id: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_PATH")]
    path: Option<PathBuf>,
    #[arg(long, env = "SHOPIFY_FLAG_RESET")]
    reset: bool,
}

#[derive(Debug, Subcommand)]
pub enum AppFunctionCommand {
    /// Compile a function to WebAssembly.
    #[command(disable_version_flag = true)]
    Build {
        #[command(flatten)]
        context: AppFunctionContext,
    },
    /// Print basic information about a function.
    #[command(disable_version_flag = true)]
    Info {
        #[command(flatten)]
        context: AppFunctionContext,
        #[arg(short = 'j', long, env = "SHOPIFY_FLAG_JSON")]
        json: bool,
    },
    /// Replay a function run from an app log.
    #[command(disable_version_flag = true)]
    Replay {
        #[command(flatten)]
        context: AppFunctionContext,
        #[arg(short = 'j', long, env = "SHOPIFY_FLAG_JSON")]
        json: bool,
        #[arg(short = 'l', long, env = "SHOPIFY_FLAG_LOG")]
        log: Option<String>,
        #[arg(
            short = 'w',
            long,
            env = "SHOPIFY_FLAG_WATCH",
            conflicts_with = "no_watch"
        )]
        watch: bool,
        #[arg(
            long = "no-watch",
            env = "SHOPIFY_FLAG_NO_WATCH",
            conflicts_with = "watch"
        )]
        no_watch: bool,
    },
    /// Run a function locally for testing.
    #[command(disable_version_flag = true)]
    Run {
        #[command(flatten)]
        context: AppFunctionContext,
        #[arg(short = 'j', long, env = "SHOPIFY_FLAG_JSON")]
        json: bool,
        #[arg(short = 'i', long, env = "SHOPIFY_FLAG_INPUT")]
        input: Option<PathBuf>,
        #[arg(short = 'e', long = "export", env = "SHOPIFY_FLAG_EXPORT")]
        export_name: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PROFILE")]
        profile: bool,
    },
    /// Fetch the latest GraphQL schema for a function.
    #[command(disable_version_flag = true)]
    Schema {
        #[command(flatten)]
        context: AppFunctionContext,
        #[arg(long, env = "SHOPIFY_FLAG_STDOUT")]
        stdout: bool,
    },
    /// Generate GraphQL types for a function.
    #[command(disable_version_flag = true)]
    Typegen {
        #[command(flatten)]
        context: AppFunctionContext,
    },
}
