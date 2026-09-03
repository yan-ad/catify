pub mod output;
mod theme_check;
use crate::output::Output;
use cfy_api::theme::{Theme, ThemeAsset, ThemeChange, ThemeClient, diff_assets};
use cfy_app::{
    AppManagementClient, BusinessPlatformClient, LinkOptions, RemoteAppSummary, RemoteOrganization,
    exchange_app_management_token,
    extension_generate::{GenerateExtensionOptions, generate_extension},
    extension_import::{
        ExistingDirectoryPolicy, ImportExtensionsOptions, ImportSelection, import_extensions,
    },
    webhook::{
        WebhookClient, WebhookDeliveryMethod, deliver_local_webhook, resolve_delivery_method,
    },
    write_linked_config,
};
use cfy_auth::{
    CredentialStore, NativeCredentialStore, Secret, Session,
    flow::{LoginMode, headless_from_env},
    identity::{HttpIdentityTransport, IdentityClient, IdentityConfig},
};
use cfy_build::{BuildInput, BuildMode, BuildOptions, BuildPipeline};
use cfy_bulk::{
    AppCredentials as BulkAppCredentials, BulkClient, BulkOperationId, BulkOperationStatus,
    GraphiqlServer, StoreDomain as BulkStoreDomain, exchange_client_credentials,
    resolve_api_version,
};
use cfy_config::project::{
    Environment, ProjectKind, ProjectOverrides, discover, resolve_environment,
};
use cfy_config::theme::{
    StagedFile, commit_staged_files_cancellable, read_theme_files, safe_relative_path,
};
use cfy_config::theme_dev::{FileEvent, SyncAction, coalesce};
use cfy_config::{
    AutoCorrect, AutoUpgrade, UserSettings,
    active_config::ActiveConfigState,
    app_env::{from_project as app_environment, merge_dotenv, redacted as redact_app_environment},
    clear_cache_root, write_atomic,
};
use cfy_core::{Cancellation, Error, ErrorKind, Result};
use cfy_deploy::{
    AppManagementBackend as DeployBackend, DeployOptions, DeploySelection, VersionMetadata,
    deploy as deploy_app,
};
use cfy_dev::{ComponentSpec, DevOptions, DevSession};
use cfy_docs::{Cache as DocsCache, DocsClient, HttpDocsTransport};
use cfy_extension_adapter::{Adapter, AdapterCommand, Parallelism};
use cfy_hydrogen::run as run_hydrogen;
use cfy_plugins::{
    InstallOptions as PluginInstallOptions, LinkOptions as PluginLinkOptions,
    MutationResult as PluginMutationResult, PackageManagerConfig, PluginKind, PluginService,
    ResetOptions as PluginResetOptions, UpdateOptions as PluginUpdateOptions,
};
use cfy_process::{OutputMode, ProcessSpec, Supervisor};
use cfy_store::{
    AdminStoreBackend, OrganizationStoreClient, StoreBackend, StoreCommand as StoreOperation,
    StoreManagementBackend, StoreTarget, browser_url,
    store_auth::{StoreAuthBootstrap, StoreAuthCallback, StoreAuthRegistry, exchange_code},
};
use cfy_theme_init::{ThemeInitRequest, initialize as initialize_theme};
use cfy_tunnel::{CloudflaredAdapter, TunnelConfig, TunnelProvider, TunnelSession};
use cfy_upgrade::{
    ExecutionPolicy, detect as detect_upgrade, execute as execute_upgrade, plan as plan_upgrade,
};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use notify::{
    EventKind, RecursiveMode, Watcher,
    event::{ModifyKind, RenameMode},
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use std::{
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use zip::write::SimpleFileOptions;

const SHOPIFY_API_VERSION: &str = "2026-07";

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum WebhookDeliveryMethodArg {
    Http,
    GooglePubSub,
    EventBridge,
}

impl From<WebhookDeliveryMethodArg> for WebhookDeliveryMethod {
    fn from(value: WebhookDeliveryMethodArg) -> Self {
        match value {
            WebhookDeliveryMethodArg::Http => Self::Http,
            WebhookDeliveryMethodArg::GooglePubSub => Self::GooglePubSub,
            WebhookDeliveryMethodArg::EventBridge => Self::EventBridge,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AppWebhookCommand {
    /// Trigger delivery of a sample webhook topic payload.
    #[command(disable_version_flag = true)]
    Trigger {
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
        #[arg(long, env = "SHOPIFY_FLAG_TOPIC")]
        topic: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_API_VERSION")]
        api_version: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_DELIVERY_METHOD")]
        delivery_method: Option<WebhookDeliveryMethodArg>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_SECRET")]
        client_secret: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_ADDRESS")]
        address: Option<String>,
    },
}

async fn store_access_token(store: &str) -> Result<String> {
    if let Ok(token) = env::var("SHOPIFY_CLI_ADMIN_AUTH_TOKEN") {
        return Ok(token);
    }
    if let Ok(token) = env::var("SHOPIFY_CLI_TOKEN") {
        return Ok(token);
    }
    let token = StoreAuthRegistry::default()
        .access_token(store)
        .await?
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Api,
                format!(
                    "no store-auth session for `{store}`; run `cfy store auth --store {store} --scopes <comma-separated-scopes>`"
                ),
            )
        })?;
    Ok(token.expose().to_owned())
}

async fn authenticated_session(identity: &str) -> Result<Session> {
    let store = Arc::new(NativeCredentialStore::default());
    let identity_client = Arc::new(IdentityClient::new(
        HttpIdentityTransport::new()?,
        IdentityConfig::from_env(|key| env::var(key).ok())?,
    ));
    let sessions = cfy_auth::SessionManager::new(store, identity_client);
    sessions.session(identity).await?.ok_or_else(|| {
        Error::new(
            ErrorKind::Api,
            format!("no authenticated session for `{identity}`; run `cfy auth login --identity {identity}` first"),
        )
    })
}

fn required_interactive_value(
    value: Option<String>,
    label: &str,
    non_interactive: bool,
) -> Result<String> {
    if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
        return Ok(value);
    }
    if non_interactive || !io::stdin().is_terminal() {
        return Err(Error::invalid_input(format!(
            "{label} is required in non-interactive mode"
        )));
    }
    eprint!("{label}: ");
    io::stderr().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).map_err(|error| {
        Error::with_source(ErrorKind::Process, format!("could not read {label}"), error)
    })?;
    let value = input.trim();
    if value.is_empty() {
        return Err(Error::invalid_input(format!("{label} cannot be empty")));
    }
    Ok(value.to_owned())
}

async fn build_app_graph(
    graph: &cfy_config::graph::AppConfigGraph,
) -> Result<cfy_build::BuildReport> {
    if graph
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == cfy_config::graph::DiagnosticSeverity::Error)
    {
        return Err(Error::config(
            "app configuration contains errors; run `cfy app config validate` for details",
        ));
    }
    let inputs = graph
        .apps
        .first()
        .map(|app| {
            app.extensions
                .iter()
                .map(|extension| BuildInput {
                    output_dir: graph.root.join(".catify/build").join(
                        extension
                            .handle
                            .as_deref()
                            .or(extension.name.as_deref())
                            .unwrap_or("extension"),
                    ),
                    memory_mb: 256,
                    configuration: serde_json::to_value(&extension.raw)
                        .unwrap_or(serde_json::Value::Null),
                    extension: extension.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let supervisor = Supervisor::default();
    let adapter = if inputs.is_empty() {
        None
    } else {
        let command = env::var_os("CFY_EXTENSION_ADAPTER").ok_or_else(|| {
            Error::config(
                "this app contains extensions; set CFY_EXTENSION_ADAPTER to a compatible build adapter executable",
            )
        })?;
        Some(
            Adapter::discover(
                &supervisor,
                AdapterCommand::new(PathBuf::from(command)),
                None,
            )
            .await?,
        )
    };
    BuildPipeline::new(adapter.as_ref(), &supervisor)
        .run(
            graph,
            inputs,
            BuildOptions {
                mode: BuildMode::Incremental,
                parallelism: Parallelism {
                    max_jobs: std::thread::available_parallelism()
                        .map(usize::from)
                        .unwrap_or(1),
                    max_memory_mb: 1024,
                },
            },
        )
        .await
}

fn create_deploy_bundle(
    graph: &cfy_config::graph::AppConfigGraph,
    build: &cfy_build::BuildReport,
) -> Result<cfy_build::BuildReport> {
    let directory = graph.root.join(".catify");
    std::fs::create_dir_all(&directory).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not create deploy directory",
            error,
        )
    })?;
    let path = directory.join("deploy-bundle.tar.br");
    let file = std::fs::File::create(&path).map_err(|error| {
        Error::with_source(ErrorKind::Config, "could not create deploy bundle", error)
    })?;
    let encoder = brotli::CompressorWriter::new(file, 4096, 6, 22);
    let mut archive = tar::Builder::new(encoder);
    let app = graph
        .apps
        .first()
        .ok_or_else(|| Error::config("selected app graph has no app node"))?;
    let manifest = serde_json::json!({
        "name": app.config.name,
        "handle": app.config.raw.get("handle").and_then(toml::Value::as_str),
        "modules": app.extensions.iter().map(|extension| serde_json::json!({
            "type": extension.extension_type,
            "handle": extension.handle,
            "uid": extension.uid,
            "config": extension.raw,
        })).collect::<Vec<_>>(),
    });
    let manifest = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| Error::config(format!("could not encode deploy manifest: {error}")))?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "manifest.json", manifest.as_slice())
        .map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not archive deploy manifest",
                error,
            )
        })?;
    for artifact in &build.artifacts {
        let file_name = artifact.path.file_name().ok_or_else(|| {
            Error::config(format!(
                "invalid build artifact path {}",
                artifact.path.display()
            ))
        })?;
        let archive_path = PathBuf::from("artifacts")
            .join(&artifact.extension)
            .join(file_name);
        archive
            .append_path_with_name(&artifact.path, archive_path)
            .map_err(|error| {
                Error::with_source(ErrorKind::Config, "could not archive build artifact", error)
            })?;
    }
    archive.finish().map_err(|error| {
        Error::with_source(ErrorKind::Config, "could not finish deploy bundle", error)
    })?;
    Ok(cfy_build::BuildReport {
        mode: build.mode.clone(),
        skipped: build.skipped.clone(),
        artifacts: vec![cfy_build::Artifact {
            extension: "complete-source".into(),
            path,
        }],
        diagnostics: build.diagnostics.clone(),
    })
}

#[derive(Debug, Args)]
pub struct AppBulkContext {
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
    #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
    store: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum AppBulkCommand {
    /// Execute a bulk operation.
    #[command(disable_version_flag = true)]
    Execute {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(
            short = 'q',
            long,
            env = "SHOPIFY_FLAG_QUERY",
            conflicts_with = "query_file"
        )]
        query: Option<String>,
        #[arg(
            long,
            env = "SHOPIFY_FLAG_QUERY_FILE",
            required_unless_present = "query"
        )]
        query_file: Option<PathBuf>,
        #[arg(short = 'v', long, env = "SHOPIFY_FLAG_VARIABLES", action = ArgAction::Append, conflicts_with = "variable_file")]
        variables: Vec<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VARIABLE_FILE")]
        variable_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_OUTPUT_FILE", requires = "watch")]
        output_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_WATCH")]
        watch: bool,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
    },
    /// Check bulk operation status.
    Status {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(long, env = "SHOPIFY_FLAG_ID")]
        id: Option<String>,
    },
    /// Cancel a bulk operation.
    Cancel {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(long, env = "SHOPIFY_FLAG_ID", required = true)]
        id: String,
    },
}
fn select_theme_for_open<'a>(
    themes: &'a [Theme],
    requested: Option<&str>,
    development: bool,
    live: bool,
    non_interactive: bool,
) -> Result<&'a Theme> {
    if development {
        return themes
            .iter()
            .find(|theme| theme.role == "development")
            .ok_or_else(|| Error::invalid_input("no development theme was found"));
    }

    if live {
        return themes
            .iter()
            .find(|theme| theme.role == "main")
            .ok_or_else(|| Error::invalid_input("no live theme was found"));
    }
    if let Some(requested) = requested {
        return themes
            .iter()
            .find(|theme| theme.id.to_string() == requested || theme.name == requested)
            .ok_or_else(|| Error::invalid_input(format!("theme `{requested}` was not found")));
    }
    if non_interactive {
        return Err(Error::invalid_input(
            "theme open requires --development, --live, or --theme in non-interactive mode",
        ));
    }
    let choices = themes
        .iter()
        .map(|theme| format!("{} ({}, {})", theme.name, theme.id, theme.role))
        .collect::<Vec<_>>();
    let index = select_text_choice("Which theme would you like to open?", &choices)?;
    themes
        .get(index)
        .ok_or_else(|| Error::process("theme selection returned an invalid index"))
}

#[derive(Debug, Subcommand)]
pub enum PluginsCommand {
    /// Add one or more plugins.
    Add {
        #[arg(required = true, num_args = 1..)]
        plugin: Vec<String>,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 's', long, conflicts_with_all = ["plugin_verbose", "verbose"])]
        silent: bool,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Install one or more plugins.
    Install {
        #[arg(required = true, num_args = 1..)]
        plugin: Vec<String>,
        #[arg(short = 'f', long)]
        force: bool,
        #[arg(short = 's', long, conflicts_with_all = ["plugin_verbose", "verbose"])]
        silent: bool,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Inspect one or more plugins.
    Inspect {
        #[arg(default_value = ".", num_args = 0..)]
        plugin: Vec<String>,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Link a plugin directory.
    Link {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, conflicts_with = "no_install")]
        install: bool,
        #[arg(long)]
        no_install: bool,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Remove effective plugin registrations.
    Remove {
        #[arg(num_args = 0..)]
        plugin: Vec<String>,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Reset plugin registry state.
    Reset {
        #[arg(long)]
        hard: bool,
        #[arg(long)]
        reinstall: bool,
    },
    /// Uninstall installed plugins.
    Uninstall {
        #[arg(num_args = 0..)]
        plugin: Vec<String>,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Unlink linked plugins.
    Unlink {
        #[arg(num_args = 0..)]
        plugin: Vec<String>,
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
    /// Update installed plugins.
    Update {
        #[arg(short = 'v')]
        plugin_verbose: bool,
    },
}

fn plugin_registry_root() -> PathBuf {
    if let Some(root) = env::var_os("CFY_PLUGIN_ROOT") {
        return PathBuf::from(root);
    }
    if let Some(root) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(root).join("catify/plugins");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/catify/plugins");
    }
    PathBuf::from(".catify/plugins")
}

fn plugin_service() -> PluginService {
    PluginService::new(
        plugin_registry_root(),
        PackageManagerConfig {
            executable: env::var_os("CFY_PACKAGE_MANAGER")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("npm")),
        },
        Supervisor::default(),
    )
}

fn plugin_process_exit(result: &PluginMutationResult) -> Option<u8> {
    result.process.as_ref().and_then(|process| {
        (process.exit_code != Some(0)).then(|| {
            process
                .exit_code
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1)
        })
    })
}

fn plugin_names_for_kind(service: &PluginService, kind: Option<PluginKind>) -> Result<Vec<String>> {
    Ok(service
        .registry()
        .all()?
        .into_iter()
        .filter(|record| kind.is_none_or(|kind| record.kind == kind))
        .map(|record| record.name)
        .collect())
}

fn selected_plugin_names(
    service: &PluginService,
    names: Vec<String>,
    kind: Option<PluginKind>,
    non_interactive: bool,
) -> Result<Vec<String>> {
    if !names.is_empty() {
        return Ok(names);
    }
    let choices = plugin_names_for_kind(service, kind)?;
    if choices.is_empty() {
        return Ok(Vec::new());
    }
    if non_interactive || !io::stdin().is_terminal() {
        return Err(Error::invalid_input(
            "plugin names are required in non-interactive mode",
        ));
    }
    let index = select_text_choice("Which plugin do you want to remove?", &choices)?;
    Ok(vec![choices[index].clone()])
}

async fn plugins_command(
    command: PluginsCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    let service = plugin_service();
    match command {
        PluginsCommand::Add {
            plugin,
            force,
            silent,
            plugin_verbose,
        } => {
            let mut results = Vec::with_capacity(plugin.len());
            for source in plugin {
                let result = service
                    .add_with_options(
                        &source,
                        PluginInstallOptions {
                            force,
                            silent,
                            verbose: plugin_verbose,
                        },
                    )
                    .await?;
                let exit = plugin_process_exit(&result);
                results.push(result);
                if let Some(code) = exit {
                    output
                        .success("Plugin installation failed", &results)
                        .map_err(|error| Error::process(error.to_string()))?;
                    return Ok(code);
                }
            }
            output
                .success("Plugins installed", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Install {
            plugin,
            force,
            silent,
            plugin_verbose,
        } => {
            let mut results = Vec::with_capacity(plugin.len());
            for source in plugin {
                let result = service
                    .install_with_options(
                        &source,
                        PluginInstallOptions {
                            force,
                            silent,
                            verbose: plugin_verbose,
                        },
                    )
                    .await?;
                let exit = plugin_process_exit(&result);
                results.push(result);
                if let Some(code) = exit {
                    output
                        .success("Plugin installation failed", &results)
                        .map_err(|error| Error::process(error.to_string()))?;
                    return Ok(code);
                }
            }
            output
                .success("Plugins installed", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Inspect {
            plugin,
            plugin_verbose,
        } => {
            let records = service.inspect(&plugin)?;
            let human = records
                .iter()
                .map(|record| {
                    if plugin_verbose {
                        format!(
                            "{}\t{:?}\t{}\t{}",
                            record.name,
                            record.kind,
                            record.version.as_deref().unwrap_or("unknown"),
                            record.path.display()
                        )
                    } else {
                        format!(
                            "{}\t{}",
                            record.name,
                            record.version.as_deref().unwrap_or("unknown")
                        )
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            output
                .success(&human, &records)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Link {
            path,
            install,
            no_install,
            plugin_verbose,
        } => {
            let result = service
                .link(
                    path,
                    PluginLinkOptions {
                        install_dependencies: install || !no_install,
                        verbose: plugin_verbose,
                    },
                )
                .await?;
            let exit = plugin_process_exit(&result);
            output
                .success("Plugin linked", &result)
                .map_err(|error| Error::process(error.to_string()))?;
            if let Some(code) = exit {
                return Ok(code);
            }
        }
        PluginsCommand::Remove {
            plugin,
            plugin_verbose: _,
        } => {
            let names = selected_plugin_names(&service, plugin, None, non_interactive)?;
            let results = names
                .iter()
                .map(|name| service.remove(name))
                .collect::<Result<Vec<_>>>()?;
            output
                .success("Plugins removed", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Reset { hard, reinstall } => {
            let result = service
                .reset(PluginResetOptions { hard, reinstall })
                .await?;
            let exit = result.reinstalled.iter().find_map(plugin_process_exit);
            output
                .success("Plugin registry reset", &result)
                .map_err(|error| Error::process(error.to_string()))?;
            if let Some(code) = exit {
                return Ok(code);
            }
        }
        PluginsCommand::Uninstall {
            plugin,
            plugin_verbose: _,
        } => {
            let names = selected_plugin_names(
                &service,
                plugin,
                Some(PluginKind::Installed),
                non_interactive,
            )?;
            let results = names
                .iter()
                .map(|name| service.uninstall(name))
                .collect::<Result<Vec<_>>>()?;
            output
                .success("Plugins uninstalled", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Unlink {
            plugin,
            plugin_verbose: _,
        } => {
            let names =
                selected_plugin_names(&service, plugin, Some(PluginKind::Linked), non_interactive)?;
            let results = names
                .iter()
                .map(|name| service.unlink(name))
                .collect::<Result<Vec<_>>>()?;
            output
                .success("Plugins unlinked", &results)
                .map_err(|error| Error::process(error.to_string()))?;
        }
        PluginsCommand::Update { plugin_verbose } => {
            let results = service
                .update_with_options(PluginUpdateOptions {
                    verbose: plugin_verbose,
                })
                .await?;
            let exit = results.iter().find_map(plugin_process_exit);
            output
                .success("Plugins updated", &results)
                .map_err(|error| Error::process(error.to_string()))?;
            if let Some(code) = exit {
                return Ok(code);
            }
        }
    }
    Ok(0)
}
fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_char) in right.chars().enumerate() {
            current.push(std::cmp::min(
                std::cmp::min(current[right_index] + 1, previous[right_index + 1] + 1),
                previous[right_index] + usize::from(left_char != right_char),
            ));
        }
        previous = current;
    }
    previous[right.chars().count()]
}

fn corrected_command_args(arguments: &[std::ffi::OsString]) -> Option<Vec<std::ffi::OsString>> {
    let mut corrected = arguments.to_vec();
    let mut command = Cli::command();
    let mut changed = false;

    for argument in corrected.iter_mut().skip(1) {
        let token = argument.to_str()?;
        if token.starts_with('-') || command.get_subcommands().next().is_none() {
            break;
        }

        if let Some(exact) = command.find_subcommand(token).cloned() {
            command = exact;
            continue;
        }

        let mut candidates = command
            .get_subcommands()
            .filter_map(|candidate| {
                let distance = edit_distance(token, candidate.get_name());
                (distance <= 2).then_some((distance, candidate.get_name().to_owned()))
            })
            .collect::<Vec<_>>();
        candidates.sort();
        if candidates.len() != 1 {
            break;
        }
        *argument = candidates[0].1.clone().into();
        let exact = command.find_subcommand(&candidates[0].1)?.clone();
        command = exact;
        changed = true;
    }

    changed.then_some(corrected)
}

/// Parse process arguments, automatically applying unambiguous command corrections when enabled.
#[must_use]
pub fn parse_cli() -> Cli {
    let arguments = env::args_os().collect::<Vec<_>>();
    match Cli::try_parse_from(&arguments) {
        Ok(cli) => cli,
        Err(original) => {
            let settings = UserSettings::resolve(Some(&config_path()), None);
            if matches!(settings.autocorrect, AutoCorrect::On)
                && let Some(corrected) = corrected_command_args(&arguments)
                && let Ok(cli) = Cli::try_parse_from(&corrected)
            {
                eprintln!(
                    "Autocorrected command to `{}`.",
                    corrected
                        .iter()
                        .skip(1)
                        .filter_map(|value| value.to_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                return cli;
            }
            original.exit()
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ThemeMetafieldsCommand {
    /// Download metafield definitions into the theme project.
    #[command(disable_version_flag = true)]
    Pull {
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_CLI_THEME_TOKEN")]
        password: Option<String>,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: Option<String>,
        #[arg(short = 'e', long, env = "SHOPIFY_FLAG_ENVIRONMENT", action = ArgAction::Append)]
        environment: Vec<String>,
        #[arg(short = 'f', long, env = "SHOPIFY_FLAG_FORCE", hide = true)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum StoreAuthCommand {
    /// List stores authenticated directly with store auth.
    #[command(disable_version_flag = true)]
    List,
}

#[derive(Debug, Subcommand)]
pub enum StoreCreateCommand {
    /// Create a preview Shopify store.
    Preview {
        #[arg(long, env = "SHOPIFY_FLAG_PREVIEW_STORE_NAME")]
        name: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_STORE_COUNTRY")]
        country: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum StoreBulkCommand {
    /// Execute a bulk operation.
    #[command(disable_version_flag = true)]
    Execute {
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: String,
        #[arg(
            short = 'q',
            long,
            env = "SHOPIFY_FLAG_QUERY",
            conflicts_with = "query_file"
        )]
        query: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_QUERY_FILE")]
        query_file: Option<PathBuf>,
        #[arg(short = 'v', long, env = "SHOPIFY_FLAG_VARIABLES", action = ArgAction::Append, conflicts_with = "variable_file")]
        variables: Vec<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VARIABLE_FILE")]
        variable_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_OUTPUT_FILE")]
        output_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_WATCH")]
        watch: bool,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_MUTATIONS")]
        allow_mutations: bool,
    },
    /// Show bulk operation status.
    Status {
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: String,
        #[arg(long, env = "SHOPIFY_FLAG_ID")]
        id: Option<String>,
    },
    /// Cancel a bulk operation.
    Cancel {
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: String,
        #[arg(long, env = "SHOPIFY_FLAG_ID")]
        id: String,
    },
}

async fn app_dev(args: AppDevArgs, output: &Output) -> Result<u8> {
    let AppDevArgs {
        config,
        auth_alias: _,
        client_id,
        path,
        reset,
        store: _,
        skip_dependencies_installation,
        no_update: _,
        subscription_product_url,
        checkout_cart_url,
        install_mkcert,
        use_localhost,
        tunnel_url,
        localhost_port,
        theme,
        theme_app_extension_port,
        store_password,
        notify,
    } = args;
    if subscription_product_url.is_some()
        || checkout_cart_url.is_some()
        || install_mkcert
        || theme.is_some()
        || theme_app_extension_port.is_some()
        || store_password.is_some()
        || notify.is_some()
    {
        return Err(Error::api(
            "this app dev invocation requires preview features that are not wired to the native runtime yet; remove preview/theme/notify flags or track issue #29",
        ));
    }
    if skip_dependencies_installation {
        output
            .lifecycle(
                "warning: --skip-dependencies-installation is deprecated; Catify never installs dependencies during app dev",
            )
            .map_err(|error| Error::process(error.to_string()))?;
    }

    let selected = selected_app_environment(path, config, client_id, reset)?;
    let graph =
        cfy_config::graph::AppConfigGraph::load_selected(&selected.project, &selected.config_path)?;
    let app = graph
        .apps
        .first()
        .ok_or_else(|| Error::config("selected app configuration was not loaded"))?;
    let specs = app
        .webs
        .iter()
        .filter_map(web_dev_component)
        .collect::<Vec<_>>();
    if specs.is_empty() {
        return Err(Error::config(
            "no [commands].dev entries were found in shopify.web.toml files",
        ));
    }

    let supervisor = Supervisor::default();
    let cancellation = Cancellation::default();
    let mut tunnel = None;
    let public_url = if use_localhost {
        None
    } else if let Some(url) = tunnel_url {
        let url = url::Url::parse(&url)
            .map_err(|error| Error::invalid_input(format!("invalid --tunnel-url: {error}")))?;
        if url.scheme() != "https" {
            return Err(Error::invalid_input("--tunnel-url must use HTTPS"));
        }
        Some(url)
    } else {
        let mut session = TunnelSession::new(
            supervisor.clone(),
            CloudflaredAdapter,
            TunnelConfig {
                local_host: "127.0.0.1".into(),
                local_port: localhost_port.unwrap_or(3000),
                public_url: None,
                provider: TunnelProvider::Cloudflared {
                    executable: env::var("CFY_CLOUDFLARED_BIN")
                        .unwrap_or_else(|_| "cloudflared".into()),
                },
                max_reconnects: 2,
                readiness_timeout_ms: 30_000,
            },
        )?;
        let url = session.start(&cancellation).await?;
        tunnel = Some(session);
        Some(url)
    };
    let signal = cancellation.clone();
    let signal_supervisor = supervisor.clone();
    let _signal_task = AbortOnDrop(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
            let _ = signal_supervisor.shutdown().await;
        }
    }));
    let mut session = DevSession::new(supervisor, &specs, DevOptions::default())?;
    session.start(&specs, &cancellation).await?;
    output
        .lifecycle(&match public_url {
            Some(ref url) => format!(
                "Running {} app component(s); public URL: {url}",
                specs.len()
            ),
            None => format!("Running {} app component(s) on localhost", specs.len()),
        })
        .map_err(|error| Error::process(error.to_string()))?;
    let result = match session.wait(&cancellation).await {
        Ok(()) => Ok(0),
        Err(cfy_dev::DevError::Cancelled) if cancellation.is_cancelled() => Ok(0),
        Err(error) => Err(error.into()),
    };
    if let Some(mut tunnel) = tunnel {
        tunnel.stop().await?;
    }
    result
}

fn web_dev_component(web: &cfy_config::graph::WebConfig) -> Option<ComponentSpec> {
    let command = web.raw.get("commands")?.as_table()?.get("dev")?.as_str()?;
    #[cfg(windows)]
    let process = ProcessSpec::new("cmd")
        .args(["/C", command])
        .current_dir(&web.directory)
        .output(OutputMode::Inherit);
    #[cfg(not(windows))]
    let process = ProcessSpec::new("sh")
        .args(["-c", command])
        .current_dir(&web.directory)
        .output(OutputMode::Inherit);
    Some(ComponentSpec {
        name: web
            .name
            .clone()
            .unwrap_or_else(|| web.directory.display().to_string()),
        process,
        max_restarts: 1,
        restart_backoff_ms: 250,
    })
}

#[derive(Debug, Args)]
pub struct AppDevArgs {
    #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
    config: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
    auth_alias: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
    client_id: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_PATH")]
    path: Option<PathBuf>,
    #[arg(long, env = "SHOPIFY_FLAG_RESET")]
    reset: bool,
    #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
    store: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_SKIP_DEPENDENCIES_INSTALLATION")]
    skip_dependencies_installation: bool,
    #[arg(long, env = "SHOPIFY_FLAG_NO_UPDATE")]
    no_update: bool,
    #[arg(long, env = "SHOPIFY_FLAG_SUBSCRIPTION_PRODUCT_URL")]
    subscription_product_url: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_CHECKOUT_CART_URL")]
    checkout_cart_url: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_INSTALL_MKCERT")]
    install_mkcert: bool,
    #[arg(long, env = "SHOPIFY_FLAG_USE_LOCALHOST")]
    use_localhost: bool,
    #[arg(long, env = "SHOPIFY_FLAG_TUNNEL_URL")]
    tunnel_url: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_LOCALHOST_PORT", value_parser = clap::value_parser!(u16).range(1..))]
    localhost_port: Option<u16>,
    #[arg(short = 't', long, env = "SHOPIFY_FLAG_THEME")]
    theme: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_THEME_APP_EXTENSION_PORT", value_parser = clap::value_parser!(u16).range(1..))]
    theme_app_extension_port: Option<u16>,
    #[arg(long, env = "SHOPIFY_FLAG_STORE_PASSWORD")]
    store_password: Option<String>,
    #[arg(long, env = "SHOPIFY_FLAG_NOTIFY")]
    notify: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum AppDevCommand {
    /// Clean local development state for the selected app.
    Clean {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: Option<String>,
    },
}

fn select_text_choice(title: &str, choices: &[String]) -> Result<usize> {
    enable_raw_mode().map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not enable selector", error)
    })?;
    let _guard = AuthTerminalGuard;
    execute!(io::stderr(), cursor::Hide).map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not initialize selector", error)
    })?;
    let backend = CrosstermBackend::new(io::stderr());
    let height = u16::try_from(choices.len().saturating_add(4).min(15)).unwrap_or(15);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .map_err(|error| Error::with_source(ErrorKind::Process, "could not create selector", error))?;
    let mut selected = 0usize;
    loop {
        terminal
            .draw(|frame| {
                let mut lines = vec![Line::styled(
                    format!("? {title}"),
                    Style::default().add_modifier(Modifier::BOLD),
                )];
                for (index, choice) in choices.iter().enumerate() {
                    let active = index == selected;
                    lines.push(Line::styled(
                        format!("{}  {choice}", if active { ">" } else { " " }),
                        if active {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Press ↑↓ arrows to select, enter to confirm.",
                    Style::default().fg(Color::DarkGray),
                ));
                frame.render_widget(Paragraph::new(lines).block(Block::new()), frame.area());
            })
            .ok();
        if let Event::Key(key) = event::read().map_err(|error| {
            Error::with_source(ErrorKind::Process, "could not read selection", error)
        })? && key.kind == KeyEventKind::Press
            && let Some((next, confirmed)) =
                update_list_selection(selected, choices.len(), key.code)?
        {
            selected = next;
            if confirmed {
                terminal.clear().ok();
                return Ok(selected);
            }
        }
    }
}

fn select_organization(organizations: &[RemoteOrganization]) -> Result<RemoteOrganization> {
    if organizations.is_empty() {
        return Err(Error::new(
            ErrorKind::Api,
            "no Shopify organization with app access is available; verify Manage apps permission or log in with a different account",
        ));
    }
    if organizations.len() == 1 {
        return Ok(organizations[0].clone());
    }
    let duplicate_names = {
        let unique = organizations
            .iter()
            .map(|organization| organization.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        unique.len() != organizations.len()
    };
    let choices = organizations
        .iter()
        .map(|organization| {
            if duplicate_names {
                format!("{} ({})", organization.name, organization.id)
            } else {
                organization.name.clone()
            }
        })
        .collect::<Vec<_>>();
    let index = select_text_choice("Which organization do you want to use?", &choices)?;
    Ok(organizations[index].clone())
}

fn select_app_config_path(
    project: &cfy_config::project::Project,
    requested: Option<&str>,
) -> Result<PathBuf> {
    if let Some(requested) = requested {
        let normalized = normalized_app_config_name(requested);
        return project
            .config_files()
            .iter()
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name == normalized.as_str())
            })
            .cloned()
            .ok_or_else(|| {
                Error::invalid_input(format!("could not find configuration file {normalized}"))
            });
    }
    if let Some(default) = project.config_files().iter().find(|path| {
        path.file_name()
            .is_some_and(|name| name == "shopify.app.toml")
    }) {
        return Ok(default.clone());
    }
    match project.config_files() {
        [only] => Ok(only.clone()),
        choices => Err(Error::invalid_input(format!(
            "multiple app configurations are available; pass --config ({})",
            choices
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn app_config_name(path: &Path) -> String {
    let file = path.file_name().unwrap_or_default().to_string_lossy();
    if file == "shopify.app.toml" {
        "default".to_owned()
    } else {
        file.strip_prefix("shopify.app.")
            .and_then(|value| value.strip_suffix(".toml"))
            .unwrap_or(&file)
            .to_owned()
    }
}

fn app_config_validate(
    config: Option<String>,
    client_id: Option<String>,
    path: Option<PathBuf>,
    reset: bool,
    output: &Output,
) -> Result<u8> {
    let start = path.unwrap_or(env::current_dir().map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not determine app directory",
            error,
        )
    })?);
    let project = discover(&start, Some(ProjectKind::App))?;
    let state_path = app_state_path();
    let mut state = ActiveConfigState::load(&state_path)?;
    if reset {
        state.clear(project.root());
        state.write(&state_path)?;
    }
    let requested = if let Some(config) = config {
        Some(config)
    } else if let Some(client_id) = client_id {
        let choices = load_local_app_configs(&project)?;
        Some(
            choices
                .iter()
                .find(|choice| choice.client_id == client_id)
                .ok_or_else(|| {
                    Error::invalid_input(
                        "the specified client ID could not be found in any app TOML file",
                    )
                })?
                .file_name
                .clone(),
        )
    } else if !reset {
        state.selected(project.root()).map(ToOwned::to_owned)
    } else {
        None
    };
    let selected_path = select_app_config_path(&project, requested.as_deref())?;
    let selected_name = app_config_name(&selected_path);
    let graph = cfy_config::AppConfigGraph::load_selected(&project, &selected_path)?;
    let errors = graph
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == cfy_config::DiagnosticSeverity::Error)
        .count();
    let warnings = graph.diagnostics.len() - errors;
    let diagnostics = graph
        .diagnostics
        .iter()
        .map(|diagnostic| {
            serde_json::json!({
                "severity": match diagnostic.severity {
                    cfy_config::DiagnosticSeverity::Warning => "warning",
                    cfy_config::DiagnosticSeverity::Error => "error",
                },
                "message": diagnostic.message,
                "file": diagnostic.location.file,
                "line": diagnostic.location.line,
                "column": diagnostic.location.column,
            })
        })
        .collect::<Vec<_>>();
    let report = serde_json::json!({
        "valid": errors == 0,
        "config": selected_name,
        "config_path": selected_path,
        "extensions": graph.apps[0].extensions.len(),
        "webs": graph.apps[0].webs.len(),
        "errors": errors,
        "warnings": warnings,
        "diagnostics": diagnostics,
    });
    let mut human = if errors == 0 {
        format!(
            "App configuration is valid ({} extension(s), {} web component(s), {} warning(s))",
            graph.apps[0].extensions.len(),
            graph.apps[0].webs.len(),
            warnings
        )
    } else {
        format!(
            "App configuration validation failed with {errors} error(s) and {warnings} warning(s)"
        )
    };
    for diagnostic in &graph.diagnostics {
        human.push_str(&format!(
            "\n{}:{}:{} [{}] {}",
            diagnostic.location.file.display(),
            diagnostic.location.line,
            diagnostic.location.column,
            match diagnostic.severity {
                cfy_config::DiagnosticSeverity::Warning => "warning",
                cfy_config::DiagnosticSeverity::Error => "error",
            },
            diagnostic.message
        ));
    }
    if errors == 0 {
        output
            .success(&human, &report)
            .map_err(|error| Error::process(error.to_string()))?;
        Ok(0)
    } else {
        output
            .success(&human, &report)
            .map_err(|error| Error::process(error.to_string()))?;
        Ok(1)
    }
}

fn app_state_path() -> PathBuf {
    env::var_os("CFY_APP_STATE_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .map(|path| PathBuf::from(path).join("catify/app-state.json"))
        })
        .or_else(|| {
            env::var_os("HOME")
                .map(|path| PathBuf::from(path).join(".config/catify/app-state.json"))
        })
        .unwrap_or_else(|| PathBuf::from(".catify/app-state.json"))
}

fn app_config_use(
    config: Option<String>,
    client_id: Option<String>,
    path: Option<PathBuf>,
    reset: bool,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    let start = path.unwrap_or(env::current_dir().map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not determine app directory",
            error,
        )
    })?);
    let project = discover(&start, Some(ProjectKind::App))?;
    let state_path = app_state_path();
    let mut state = ActiveConfigState::load(&state_path)?;

    if reset {
        state.clear(project.root());
        state.write(&state_path)?;
        output
            .success(
                "Cleared current configuration",
                &serde_json::json!({"project": project.root(), "state_path": state_path}),
            )
            .map_err(|error| Error::process(error.to_string()))?;
        return Ok(0);
    }

    let choices = load_local_app_configs(&project)?;
    let selected = if let Some(config) = config {
        find_local_app_config(&choices, &config).ok_or_else(|| {
            Error::invalid_input(format!(
                "could not find configuration file {}; available configurations: {}",
                normalized_app_config_name(&config),
                choices
                    .iter()
                    .map(|choice| choice.file_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?
    } else if let Some(client_id) = client_id {
        choices
            .iter()
            .find(|choice| choice.client_id == client_id)
            .ok_or_else(|| {
                Error::invalid_input(
                    "the specified client ID could not be found in any app TOML file",
                )
            })?
    } else {
        if non_interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(Error::invalid_input(
                "app config use requires CONFIG or --client-id outside an interactive terminal",
            ));
        }
        select_local_app_config(&choices)?
    };

    state.set(project.root(), selected.file_name.clone());
    state.write(&state_path)?;
    output
        .success(
            &format!("Using configuration file {}", selected.file_name),
            &serde_json::json!({
                "project": project.root(),
                "config": selected.file_name,
                "path": selected.path,
                "state_path": state_path,
            }),
        )
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

#[derive(Debug, Clone)]
struct LocalAppConfig {
    path: PathBuf,
    file_name: String,
    client_id: String,
}

fn load_local_app_configs(project: &cfy_config::project::Project) -> Result<Vec<LocalAppConfig>> {
    project
        .config_files()
        .iter()
        .map(|path| {
            let contents = std::fs::read_to_string(path).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("could not read {}", path.display()),
                    error,
                )
            })?;
            let document = toml::from_str::<toml::Value>(&contents).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("could not parse {}", path.display()),
                    error,
                )
            })?;
            let client_id = document
                .get("client_id")
                .and_then(toml::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    Error::invalid_input(format!(
                        "configuration file {} needs a client_id",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ))
                })?;
            Ok(LocalAppConfig {
                path: path.clone(),
                file_name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                client_id: client_id.to_owned(),
            })
        })
        .collect()
}

fn normalized_app_config_name(config: &str) -> String {
    if config == "shopify.app.toml" || config.ends_with(".toml") {
        config.to_owned()
    } else {
        format!("shopify.app.{config}.toml")
    }
}

fn find_local_app_config<'a>(
    choices: &'a [LocalAppConfig],
    config: &str,
) -> Option<&'a LocalAppConfig> {
    let normalized = normalized_app_config_name(config);
    choices.iter().find(|choice| choice.file_name == normalized)
}

fn select_local_app_config(choices: &[LocalAppConfig]) -> Result<&LocalAppConfig> {
    if choices.is_empty() {
        return Err(Error::invalid_input(
            "no app configuration files were found",
        ));
    }
    enable_raw_mode().map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not enable configuration selector",
            error,
        )
    })?;
    let _guard = AuthTerminalGuard;
    execute!(io::stderr(), cursor::Hide).map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not initialize configuration selector",
            error,
        )
    })?;
    let backend = CrosstermBackend::new(io::stderr());
    let height = u16::try_from(choices.len().saturating_add(4).min(15)).unwrap_or(15);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not initialize configuration selector",
            error,
        )
    })?;
    let mut selected = 0;
    loop {
        terminal
            .draw(|frame| {
                let area = frame.area();
                let mut lines = vec![Line::styled(
                    "? Which app configuration would you like to use?",
                    Style::default().add_modifier(Modifier::BOLD),
                )];
                for (index, choice) in choices.iter().enumerate() {
                    let active = index == selected;
                    lines.push(Line::styled(
                        format!("{}  {}", if active { ">" } else { " " }, choice.file_name),
                        if active {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Press ↑↓ arrows to select, enter to confirm.",
                    Style::default().fg(Color::DarkGray),
                ));
                frame.render_widget(Paragraph::new(lines).block(Block::new()), area);
            })
            .ok();
        if let Event::Key(key) = event::read().map_err(|error| {
            Error::with_source(
                ErrorKind::Process,
                "could not read configuration selection",
                error,
            )
        })? && key.kind == KeyEventKind::Press
            && let Some((next, confirmed)) =
                update_list_selection(selected, choices.len(), key.code)?
        {
            selected = next;
            if confirmed {
                terminal.clear().ok();
                return Ok(&choices[selected]);
            }
        }
    }
}
fn update_list_selection(
    selected: usize,
    total: usize,
    code: KeyCode,
) -> Result<Option<(usize, bool)>> {
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            Ok(Some((selected.checked_sub(1).unwrap_or(total - 1), false)))
        }
        KeyCode::Down | KeyCode::Char('j') => Ok(Some(((selected + 1) % total, false))),
        KeyCode::Enter => Ok(Some((selected, true))),
        KeyCode::Esc | KeyCode::Char('q') => Err(Error::invalid_input("app selection cancelled")),
        _ => Ok(None),
    }
}

fn select_remote_app(apps: &[RemoteAppSummary]) -> Result<RemoteAppSummary> {
    if apps.is_empty() {
        return Err(Error::new(
            ErrorKind::Api,
            "no Shopify apps are available for this account; create an app first or pass --delegate",
        ));
    }
    enable_raw_mode().map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not enable app selector", error)
    })?;
    let _guard = AuthTerminalGuard;
    execute!(io::stderr(), cursor::Hide).map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not initialize app selector",
            error,
        )
    })?;
    let backend = CrosstermBackend::new(io::stderr());
    let height = u16::try_from(apps.len().min(8) + 4).unwrap_or(12);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not create app selector", error)
    })?;
    let mut selected = 0usize;
    loop {
        terminal
            .draw(|frame| {
                let area = frame.area();
                let mut lines = vec![
                    Line::styled(
                        "Which app would you like to link?",
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                ];
                for (index, app) in apps.iter().enumerate().take(8) {
                    let active = index == selected;
                    lines.push(Line::from(vec![
                        Span::styled(
                            if active { "> " } else { "  " },
                            if active {
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::styled(
                            format!("{}  {}", app.name, app.client_id),
                            if active {
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                    ]));
                }
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "Press ↑↓ arrows to select, enter to confirm.",
                    Style::default().fg(Color::DarkGray),
                ));
                frame.render_widget(Paragraph::new(lines).block(Block::new()), area);
            })
            .ok();
        if let Event::Key(key) = event::read().map_err(|error| {
            Error::with_source(ErrorKind::Process, "could not read app selection", error)
        })? && key.kind == KeyEventKind::Press
            && let Some((next, confirmed)) = update_list_selection(selected, apps.len(), key.code)?
        {
            selected = next;
            if confirmed {
                terminal.clear().ok();
                return Ok(apps[selected].clone());
            }
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AppConfigCommand {
    /// Fetch app configuration from the Developer Dashboard.
    Link {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_APP_CONFIG_FILE_NAME")]
        file_name: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_FORCE")]
        force: bool,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        /// Delegate to the official Shopify CLI instead of using the native backend.
        #[arg(long)]
        delegate: bool,
    },
    /// Refresh an already-linked app configuration.
    Pull {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
    },
    /// Activate an app configuration.
    Use {
        /// Configuration name or filename to activate.
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
    },
    /// Validate app configuration and extensions.
    Validate {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
    },
}

async fn app_config_command(
    command: AppConfigCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    match command {
        AppConfigCommand::Link {
            config,
            auth_alias,
            client_id,
            file_name,
            force,
            path,
            reset,
            delegate,
        } => {
            if delegate {
                let mut args = vec!["config".to_owned(), "link".to_owned()];
                push_option(&mut args, "--config", config);
                push_option(&mut args, "--auth-alias", auth_alias);
                push_option(&mut args, "--client-id", client_id);
                push_option(&mut args, "--file-name", file_name);
                if force {
                    args.push("--force".to_owned());
                }
                if let Some(path) = path {
                    args.push("--path".to_owned());
                    args.push(path.to_string_lossy().into_owned());
                }
                if reset {
                    args.push("--reset".to_owned());
                }
                return delegate_shopify_command("app", &args);
            }

            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let store = Arc::new(NativeCredentialStore::default());
            let identity_client = Arc::new(IdentityClient::new(
                HttpIdentityTransport::new()?,
                IdentityConfig::from_env(|key| env::var(key).ok())?,
            ));
            let sessions = cfy_auth::SessionManager::new(Arc::clone(&store), identity_client);
            let session = sessions.session(&identity).await?.ok_or_else(|| {
                Error::new(
                    ErrorKind::Api,
                    format!("no authenticated session for `{identity}`; run `cfy auth login --identity {identity}` first"),
                )
            })?;
            let backend = AppManagementClient::from_session(&session).await?;
            let organizations = BusinessPlatformClient::from_session(&session)
                .await?
                .list_organizations()
                .await?;
            let organization = if organizations.len() == 1 {
                organizations[0].clone()
            } else {
                if non_interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                    return Err(Error::invalid_input(
                        "app config link requires an interactive terminal when multiple organizations are available",
                    ));
                }
                select_organization(&organizations)?
            };
            let selected_client_id = if let Some(client_id) = client_id {
                client_id
            } else {
                if non_interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                    return Err(Error::invalid_input(
                        "app config link requires --client-id outside an interactive terminal",
                    ));
                }
                let apps = backend.list_apps(&organization.id).await?;
                select_remote_app(&apps)?.client_id
            };
            let app = backend
                .app_by_client_id_in_organization(&organization.id, &selected_client_id)
                .await?;
            let directory = path.unwrap_or(env::current_dir().map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    "could not determine app directory",
                    error,
                )
            })?);
            let requested_file_name = file_name.or_else(|| {
                config.map(|name| {
                    if name == "shopify.app.toml" || name.ends_with(".toml") {
                        name
                    } else {
                        format!("shopify.app.{name}.toml")
                    }
                })
            });
            let report = write_linked_config(
                &LinkOptions {
                    directory,
                    client_id: Some(selected_client_id),
                    file_name: requested_file_name,
                    force: force || reset,
                },
                &app,
            )?;
            output
                .success(
                    &format!("Linked {} to {}", report.app_name, report.path.display()),
                    &report,
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppConfigCommand::Pull {
            config,
            auth_alias,
            client_id,
            path,
            reset,
        } => {
            let start = path.unwrap_or(env::current_dir().map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    "could not determine app directory",
                    error,
                )
            })?);
            let project = discover(&start, Some(ProjectKind::App))?;
            let state_path = app_state_path();
            let mut state = ActiveConfigState::load(&state_path)?;
            if reset {
                state.clear(project.root());
                state.write(&state_path)?;
            }
            let choices = load_local_app_configs(&project)?;
            let selected = if let Some(config) = config {
                find_local_app_config(&choices, &config).ok_or_else(|| {
                    Error::invalid_input(format!(
                        "could not find configuration file {}",
                        normalized_app_config_name(&config)
                    ))
                })?
            } else if let Some(client_id) = client_id {
                choices
                    .iter()
                    .find(|choice| choice.client_id == client_id)
                    .ok_or_else(|| {
                        Error::invalid_input(
                            "the specified client ID could not be found in any app TOML file",
                        )
                    })?
            } else if !reset {
                state
                    .selected(project.root())
                    .and_then(|name| find_local_app_config(&choices, name))
                    .or_else(|| {
                        choices
                            .iter()
                            .find(|choice| choice.file_name == "shopify.app.toml")
                    })
                    .or_else(|| (choices.len() == 1).then(|| &choices[0]))
                    .ok_or_else(|| {
                        Error::invalid_input(
                            "multiple app configurations are available; pass --config",
                        )
                    })?
            } else {
                choices
                    .iter()
                    .find(|choice| choice.file_name == "shopify.app.toml")
                    .or_else(|| (choices.len() == 1).then(|| &choices[0]))
                    .ok_or_else(|| {
                        Error::invalid_input(
                            "multiple app configurations are available; pass --config",
                        )
                    })?
            };

            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let store = Arc::new(NativeCredentialStore::default());
            let identity_client = Arc::new(IdentityClient::new(
                HttpIdentityTransport::new()?,
                IdentityConfig::from_env(|key| env::var(key).ok())?,
            ));
            let sessions = cfy_auth::SessionManager::new(Arc::clone(&store), identity_client);
            let session = sessions.session(&identity).await?.ok_or_else(|| {
                Error::new(
                    ErrorKind::Api,
                    format!(
                        "no authenticated session for `{identity}`; run `cfy auth login --identity {identity}` first"
                    ),
                )
            })?;
            let backend = AppManagementClient::from_session(&session).await?;
            let app = backend.app_by_client_id(&selected.client_id).await?;
            let report = write_linked_config(
                &LinkOptions {
                    directory: selected
                        .path
                        .parent()
                        .unwrap_or(project.root())
                        .to_path_buf(),
                    client_id: Some(selected.client_id.clone()),
                    file_name: Some(selected.file_name.clone()),
                    force: true,
                },
                &app,
            )?;
            output
                .success(
                    &format!("Pulled configuration into {}", report.path.display()),
                    &report,
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppConfigCommand::Use {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
        } => app_config_use(config, client_id, path, reset, non_interactive, output),
        AppConfigCommand::Validate {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
        } => app_config_validate(config, client_id, path, reset, output),
    }
}

fn push_option(args: &mut Vec<String>, flag: &str, value: Option<String>) {
    if let Some(value) = value {
        args.push(flag.to_owned());
        args.push(value);
    }
}

fn delegate_shopify_command(command: &str, args: &[String]) -> Result<u8> {
    let executable = env::var("CFY_SHOPIFY_BIN").unwrap_or_else(|_| "shopify".to_owned());
    let status = std::process::Command::new(&executable)
        .arg(command)
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|error| {
            Error::with_source(
                ErrorKind::Process,
                format!("could not start `{executable} {command}`"),
                error,
            )
        })?;
    Ok(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1))
}

struct AuthTerminalGuard;

impl Drop for AuthTerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), cursor::Show, Clear(ClearType::CurrentLine));
    }
}

fn update_auth_selection(selected: usize, code: KeyCode) -> Result<Option<(usize, bool)>> {
    match code {
        KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k') => {
            Ok(Some((1 - selected, false)))
        }
        KeyCode::Enter => Ok(Some((selected, true))),
        KeyCode::Esc | KeyCode::Char('q') => {
            Err(Error::invalid_input("account selection cancelled"))
        }
        _ => Ok(None),
    }
}

fn select_auth_account(session: &Session) -> Result<bool> {
    enable_raw_mode().map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not enable account selector",
            error,
        )
    })?;
    let _guard = AuthTerminalGuard;
    execute!(io::stderr(), cursor::Hide).map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not initialize account selector",
            error,
        )
    })?;
    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(8),
        },
    )
    .map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not create account selector",
            error,
        )
    })?;
    let account = session.display_name.as_deref().unwrap_or(&session.identity);
    let mut selected = 0usize;

    loop {
        terminal
            .draw(|frame| {
                let area = frame.area();
                let width = area.width.min(72);
                let height = area.height.min(8);
                let area = Rect::new(area.x, area.y, width, height);
                let marker = |index| {
                    if selected == index {
                        Span::styled(
                            "> ",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        Span::raw("  ")
                    }
                };
                let selected_style = |index| {
                    if selected == index {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    }
                };
                let lines = vec![
                    Line::styled(
                        "Which account would you like to use?",
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    Line::from(vec![marker(0), Span::styled(account, selected_style(0))]),
                    Line::from(vec![
                        marker(1),
                        Span::styled("Log in with a different account", selected_style(1)),
                    ]),
                    Line::raw(""),
                    Line::styled(
                        "Press ↑↓ arrows to select, enter to confirm.",
                        Style::default().fg(Color::DarkGray),
                    ),
                ];
                frame.render_widget(Paragraph::new(lines).block(Block::new()), area);
            })
            .map_err(|error| {
                Error::with_source(
                    ErrorKind::Process,
                    "could not render account selector",
                    error,
                )
            })?;

        if let Event::Key(key) = event::read().map_err(|error| {
            Error::with_source(
                ErrorKind::Process,
                "could not read account selection",
                error,
            )
        })? && key.kind == KeyEventKind::Press
            && let Some((next, confirmed)) = update_auth_selection(selected, key.code)?
        {
            selected = next;
            if confirmed {
                terminal.clear().ok();
                return Ok(selected == 0);
            }
        }
    }
}

fn reusable_session(session: &Session, now_unix: u64) -> bool {
    session.is_valid_at(now_unix, 60)
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn delegate_shopify_login(output: &Output) -> Result<u8> {
    let executable = env::var("CFY_SHOPIFY_BIN").unwrap_or_else(|_| "shopify".to_owned());
    output
        .lifecycle("Delegating authentication to the official Shopify CLI...")
        .map_err(|error| {
            Error::with_source(ErrorKind::Process, "could not write login status", error)
        })?;
    // Interactive terminal applications must remain in the terminal's foreground
    // process group. The regular supervisor creates an isolated process group so it
    // can terminate full process trees, which prevents Shopify CLI's TUI from
    // receiving raw arrow-key input correctly.
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&executable)
            .args(["auth", "login"])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
    })
    .await
    .map_err(|error| Error::process(format!("Shopify login task failed: {error}")))?
    .map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not start the official Shopify CLI login",
            error,
        )
    })?;
    let code = status.code().unwrap_or(1);
    if code == 0 {
        output
            .lifecycle(
                "Shopify CLI authentication succeeded. The session remains managed by Shopify CLI.",
            )
            .map_err(|error| {
                Error::with_source(ErrorKind::Process, "could not write login status", error)
            })?;
    }
    Ok(u8::try_from(code).unwrap_or(1))
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).status();

    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).status();

    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .status();

    result.is_ok_and(|status| status.success())
}

fn collect_files(root: &Path, current: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(current).map_err(|error| Error::api(error.to_string()))? {
        let entry = entry.map_err(|error| Error::api(error.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() && path != root.join("theme.zip") {
            files.push(path);
        }
    }
    Ok(())
}

fn store_token() -> Result<String> {
    env::var("SHOPIFY_CLI_TOKEN")
        .or_else(|_| env::var("SHOPIFY_CLI_THEME_TOKEN"))
        .map_err(|_| {
            Error::new(
                ErrorKind::Api,
                "store authentication is required; set SHOPIFY_CLI_TOKEN or complete cfy auth login",
            )
        })
}

fn command_column_title(column: &CommandColumn) -> &'static str {
    match column {
        CommandColumn::Id => "Id",
        CommandColumn::Plugin => "Plugin",
        CommandColumn::Summary => "Summary",
        CommandColumn::Type => "Type",
    }
}

fn command_column_value(command: &CommandRecord, column: CommandColumn) -> String {
    match column {
        CommandColumn::Id => command.name.clone(),
        CommandColumn::Plugin => command.plugin_name.clone().unwrap_or_default(),
        CommandColumn::Summary => command.summary.clone(),
        CommandColumn::Type => command.plugin_type.clone().unwrap_or_default(),
    }
}

async fn theme_parity_command(
    command: ThemeCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    match command {
        ThemeCommand::Init {
            name,
            path,
            clone_url,
            latest,
        } => {
            let name = name.ok_or_else(|| {
                Error::invalid_input(
                    "theme name is required in non-interactive mode; pass `cfy theme init <name>`",
                )
            })?;
            let mut request = ThemeInitRequest::new(path, name);
            request.clone_url = clone_url;
            request.latest = latest;
            request.interactive = !non_interactive;
            let report = initialize_theme(request).await.map_err(|error| {
                Error::with_source(ErrorKind::Process, error.to_string(), error)
            })?;
            output
                .success(
                    "Theme initialized",
                    &serde_json::json!({
                        "destination": report.destination,
                        "repository": report.repository,
                        "branch": report.branch,
                        "tag": report.checked_out_tag,
                        "shallow": report.shallow,
                    }),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::Package {
            source,
            output: archive,
        } => {
            let archive = archive.unwrap_or_else(|| {
                source
                    .file_name()
                    .map(|name| PathBuf::from(format!("{}.zip", name.to_string_lossy())))
                    .unwrap_or_else(|| PathBuf::from("theme.zip"))
            });
            let file = std::fs::File::create(&archive).map_err(|error| {
                Error::api(format!("could not create {}: {error}", archive.display()))
            })?;
            let mut zip = zip::ZipWriter::new(file);
            let options =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            let mut paths = Vec::new();
            collect_files(&source, &source, &mut paths)?;
            paths.sort();
            let file_count = paths.len();
            for path in &paths {
                let relative = path
                    .strip_prefix(&source)
                    .map_err(|error| Error::api(error.to_string()))?;
                let name = relative.to_string_lossy().replace('\\', "/");
                zip.start_file(name, options)
                    .map_err(|error| Error::api(error.to_string()))?;
                let bytes = std::fs::read(path).map_err(|error| Error::api(error.to_string()))?;
                std::io::Write::write_all(&mut zip, &bytes)
                    .map_err(|error| Error::api(error.to_string()))?;
            }
            zip.finish()
                .map_err(|error| Error::api(error.to_string()))?;
            output
                .success(
                    "Theme packaged",
                    &serde_json::json!({"archive": archive, "files": file_count}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::LanguageServer { args } => theme_check::run_language_server(&args).await,
        ThemeCommand::Info { theme, store } => {
            let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| Error::new(ErrorKind::Api, "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete cfy auth login"))?;
            let client =
                ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
            let value = client.get(theme).await.map_err(Error::from)?;
            output
                .success(&format!("Theme {}", value.id), &value)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::Open {
            auth_alias: _,
            development,
            editor,
            environment: _,
            live,
            path: _,
            password,
            store,
            theme,
        } => {
            if usize::from(development) + usize::from(live) + usize::from(theme.is_some()) > 1 {
                return Err(Error::invalid_input(
                    "theme open accepts only one of --development, --live, or --theme",
                ));
            }
            let store = resolve_store(store.as_deref())?;
            let token = password
                .or_else(|| env::var("SHOPIFY_CLI_THEME_TOKEN").ok())
                .ok_or_else(|| Error::new(ErrorKind::Api, "theme authentication is required; pass --password, set SHOPIFY_CLI_THEME_TOKEN, or complete cfy auth login"))?;
            let client =
                ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
            let themes = client.list().await.map_err(Error::from)?;
            let selected = select_theme_for_open(
                &themes,
                theme.as_deref(),
                development,
                live,
                non_interactive,
            )?;
            let preview_url = client.preview_url(selected.id);
            let editor_url = format!("https://{store}/admin/themes/{}/editor", selected.id);
            let requested_url = if editor { &editor_url } else { &preview_url };
            let opened = !non_interactive && open_browser(requested_url);
            output
                .success(
                    &format!("Preview: {preview_url}\nEditor: {editor_url}"),
                    &serde_json::json!({
                        "theme": selected,
                        "preview_url": preview_url,
                        "editor_url": editor_url,
                        "opened": opened,
                    }),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::Share { theme, store } => {
            let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| Error::new(ErrorKind::Api, "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete cfy auth login"))?;
            let client =
                ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
            let url = client.preview_url(theme);
            output
                .success(&url, &serde_json::json!({"url": url, "theme_id": theme}))
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::Rename { theme, store, name } => {
            let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| Error::new(ErrorKind::Api, "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete cfy auth login"))?;
            let client =
                ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
            let value = client.rename(theme, &name).await.map_err(Error::from)?;
            output
                .success(&format!("Renamed theme {}", value.id), &value)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::Duplicate { theme, store, name } => {
            let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| Error::new(ErrorKind::Api, "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete cfy auth login"))?;
            let client =
                ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
            let value = client.duplicate(theme, &name).await.map_err(Error::from)?;
            output
                .success(&format!("Duplicated theme {}", value.id), &value)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::Publish {
            theme,
            store,
            confirm,
        } => {
            if !confirm {
                return Err(Error::invalid_input("theme publish requires --confirm"));
            }
            let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| Error::new(ErrorKind::Api, "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete cfy auth login"))?;
            let client =
                ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
            let value = client.publish(theme).await.map_err(Error::from)?;
            output
                .success(&format!("Published theme {}", value.id), &value)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::Delete {
            theme,
            store,
            confirm,
        } => {
            if !confirm {
                return Err(Error::invalid_input("theme delete requires --confirm"));
            }
            let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| Error::new(ErrorKind::Api, "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete cfy auth login"))?;
            let client =
                ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
            client
                .delete_theme(theme, &Cancellation::default())
                .await
                .map_err(Error::from)?;
            output
                .success(
                    &format!("Deleted theme {theme}"),
                    &serde_json::json!({"theme_id": theme, "deleted": true}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::Preview {
            theme,
            overrides,
            preview_id,
            open,
            auth_alias: _,
            path,
            password,
            store,
            environment,
        } => {
            let root = path.unwrap_or(env::current_dir().map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    "could not resolve current directory",
                    error,
                )
            })?);
            let (environment_store, environment_password) =
                theme_environment_credentials(&root, &environment)?;
            let selected_store = store.or(environment_store);
            let store = resolve_store(selected_store.as_deref())?;
            let target = StoreTarget::parse(&store)?;
            let token = match password.or(environment_password) {
                Some(password) => password,
                None => store_access_token(&target.domain).await?,
            };
            let client = ThemeClient::new(&target.domain, &token, SHOPIFY_API_VERSION)
                .map_err(Error::from)?;
            let themes = client.list().await.map_err(Error::from)?;
            let selected = themes
                .iter()
                .find(|candidate| candidate.id.to_string() == theme || candidate.name == theme)
                .ok_or_else(|| Error::invalid_input(format!("theme `{theme}` was not found")))?;
            let overrides_path = if overrides.is_absolute() {
                overrides
            } else {
                root.join(overrides)
            };
            let overrides_bytes = std::fs::read(&overrides_path).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("could not read overrides file {}", overrides_path.display()),
                    error,
                )
            })?;
            let overrides: serde_json::Value =
                serde_json::from_slice(&overrides_bytes).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        format!(
                            "overrides file {} is not valid JSON",
                            overrides_path.display()
                        ),
                        error,
                    )
                })?;
            let preview = client
                .preview(selected.id, overrides, preview_id.as_deref())
                .await
                .map_err(Error::from)?;
            output
                .success(
                    &format!(
                        "Preview is ready\n{}\nPreview ID: {}",
                        preview.url, preview.preview_identifier
                    ),
                    &preview,
                )
                .map_err(|error| Error::process(error.to_string()))?;
            if open && !non_interactive && !open_browser(&preview.url) {
                output
                    .lifecycle("Browser did not open automatically. Open the preview URL manually.")
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            Ok(0)
        }
        ThemeCommand::Console { args } => Err(backend_unavailable(
            "theme console",
            39,
            format!(
                "the interactive Liquid console adapter is pending ({} forwarded argument(s)); use Shopify CLI for this command for now",
                args.len()
            ),
        )),
        ThemeCommand::Check(_)
        | ThemeCommand::Dev { .. }
        | ThemeCommand::List { .. }
        | ThemeCommand::Pull { .. }
        | ThemeCommand::Push { .. } => Err(Error::process(
            "internal command dispatch error: specialized theme command reached parity fallback",
        )),
        ThemeCommand::Metafields {
            command:
                ThemeMetafieldsCommand::Pull {
                    auth_alias: _,
                    path,
                    password,
                    store,
                    environment,
                    force,
                },
        } => {
            let root = path.unwrap_or(env::current_dir().map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    "could not resolve current directory",
                    error,
                )
            })?);
            let is_theme = [
                "assets",
                "config",
                "layout",
                "sections",
                "snippets",
                "templates",
            ]
            .iter()
            .all(|directory| root.join(directory).is_dir());
            if !is_theme && env::var("SHOPIFY_LANGUAGE_SERVER").as_deref() == Ok("1") {
                return Ok(0);
            }
            if !is_theme && !force {
                return Err(Error::invalid_input(
                    "the target directory does not look like a Shopify theme; pass --force to continue",
                ));
            }
            let (environment_store, environment_password) =
                theme_environment_credentials(&root, &environment)?;
            let selected_store = store.or(environment_store);
            let store = resolve_store(selected_store.as_deref())?;
            let target = StoreTarget::parse(&store)?;
            let token = match password.or(environment_password) {
                Some(password) => password,
                None => store_access_token(&target.domain).await?,
            };
            let backend = AdminStoreBackend::new(&target, &token).map_err(Error::from)?;
            const OWNERS: [(&str, &str); 12] = [
                ("article", "ARTICLE"),
                ("blog", "BLOG"),
                ("collection", "COLLECTION"),
                ("company", "COMPANY"),
                ("company_location", "COMPANY_LOCATION"),
                ("location", "LOCATION"),
                ("market", "MARKET"),
                ("order", "ORDER"),
                ("page", "PAGE"),
                ("product", "PRODUCT"),
                ("variant", "PRODUCTVARIANT"),
                ("shop", "SHOP"),
            ];
            let mut definitions = serde_json::Map::new();
            let mut failed = Vec::new();
            for (handle, owner) in OWNERS {
                match backend.metafield_definitions(owner).await {
                    Ok(values) => {
                        definitions.insert(handle.into(), serde_json::Value::Array(values));
                    }
                    Err(_) => {
                        failed.push(owner);
                        definitions.insert(handle.into(), serde_json::Value::Array(Vec::new()));
                    }
                }
            }
            if failed.len() == OWNERS.len() {
                return Err(Error::api(
                    "failed to fetch metafield definitions for every owner type; check network access and Admin API scopes",
                ));
            }
            let destination = root.join(".shopify").join("metafields.json");
            let bytes = serde_json::to_vec_pretty(&definitions).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    "could not serialize metafield definitions",
                    error,
                )
            })?;
            write_atomic(&destination, &bytes).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("could not write {}", destination.display()),
                    error,
                )
            })?;
            if !failed.is_empty() {
                output
                    .diagnostic(&format!(
                        "failed to fetch metafield definitions for: {}",
                        failed.join(", ")
                    ))
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            output
                .success(
                    "Metafield definitions have been successfully downloaded.",
                    &serde_json::json!({"path": destination, "failed_owner_types": failed}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        ThemeCommand::Profile { args } => Err(backend_unavailable(
            "theme profile",
            39,
            format!(
                "the profiler adapter is pending ({} forwarded argument(s)); use Shopify CLI for this command for now",
                args.len()
            ),
        )),
    }
}

fn config_path() -> PathBuf {
    env::var_os("CFY_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .map(|path| PathBuf::from(path).join("catify/config.toml"))
        })
        .or_else(|| {
            env::var_os("HOME").map(|path| PathBuf::from(path).join(".config/catify/config.toml"))
        })
        .unwrap_or_else(|| PathBuf::from(".catify/config.toml"))
}

fn config_command(command: ConfigCommand, output: &Output) -> Result<u8> {
    match command {
        ConfigCommand::Autoupgrade { mode } => {
            let path = config_path();
            let current = UserSettings::resolve(Some(&path), None);
            match mode.unwrap_or(AutoUpgradeMode::Status) {
                AutoUpgradeMode::Status => output.success(
                    "Automatic upgrade checks status",
                    &serde_json::json!({"autoupgrade": matches!(current.autoupgrade, AutoUpgrade::On)}),
                ),
                AutoUpgradeMode::On | AutoUpgradeMode::Off => {
                    let settings = UserSettings {
                        autoupgrade: if matches!(mode, Some(AutoUpgradeMode::On)) { AutoUpgrade::On } else { AutoUpgrade::Off },
                        ..current
                    };
                    settings.write_user(&path)?;
                    output.success("Automatic upgrade checks updated", &serde_json::json!({"path": path, "autoupgrade": matches!(settings.autoupgrade, AutoUpgrade::On)}))
                }
            }.map_err(|error| Error::process(error.to_string()))?;
        }
        ConfigCommand::Autocorrect { command } => {
            let path = config_path();
            let current = UserSettings::resolve(Some(&path), None);
            match command {
                AutoCorrectCommand::Status => output.success(
                    if matches!(current.autocorrect, AutoCorrect::On) {
                        "Autocorrect on. Catify will automatically run unambiguous command corrections."
                    } else {
                        "Autocorrect off. You'll need to confirm corrections for mistyped commands."
                    },
                    &serde_json::json!({"autocorrect": matches!(current.autocorrect, AutoCorrect::On)}),
                ),
                AutoCorrectCommand::On | AutoCorrectCommand::Off => {
                    let settings = UserSettings {
                        autocorrect: if matches!(command, AutoCorrectCommand::On) { AutoCorrect::On } else { AutoCorrect::Off },
                        ..current
                    };
                    settings.write_user(&path)?;
                    output.success(
                        if matches!(settings.autocorrect, AutoCorrect::On) { "Autocorrect enabled" } else { "Autocorrect disabled" },
                        &serde_json::json!({"path": path, "autocorrect": matches!(settings.autocorrect, AutoCorrect::On)}),
                    )
                }
            }.map_err(|error| Error::process(error.to_string()))?;
        }
    }
    Ok(0)
}

fn cache_command(command: CacheCommand, output: &Output) -> Result<u8> {
    match command {
        CacheCommand::Clear => {
            let mut reclaimed = 0;
            reclaimed += clear_cache_root(&docs_cache_root())?;
            if let Some(root) = env::var_os("CFY_BUILD_CACHE_DIR") {
                reclaimed += clear_cache_root(&PathBuf::from(root))?;
            }
            output
                .success(
                    "Caches cleared",
                    &serde_json::json!({"reclaimed_bytes": reclaimed}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
        }
    }
    Ok(0)
}

fn docs_cache_root() -> PathBuf {
    env::var_os("CFY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME").map(|path| PathBuf::from(path).join("catify/docs"))
        })
        .or_else(|| env::var_os("HOME").map(|path| PathBuf::from(path).join(".cache/catify/docs")))
        .unwrap_or_else(|| PathBuf::from(".catify-cache/docs"))
}

fn notification_command(command: NotificationCommand, output: &Output) -> Result<u8> {
    let (message, enabled) = match command {
        NotificationCommand::Status => ("Notifications status", true),
        NotificationCommand::Clear => ("Notifications cleared", true),
    };
    output.success(message, &serde_json::json!({"supported": enabled, "changed": matches!(command, NotificationCommand::Clear)}))
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Remove Catify caches and report reclaimed bytes.
    Clear,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Enable or disable automatic upgrade checks.
    Autoupgrade { mode: Option<AutoUpgradeMode> },
    /// Manage automatic correction of mistyped commands.
    Autocorrect {
        #[command(subcommand)]
        command: AutoCorrectCommand,
    },
}

#[derive(Debug, Clone, Copy, Subcommand)]
pub enum AutoCorrectCommand {
    /// Check whether autocorrect is enabled.
    Status,
    /// Enable autocorrect.
    On,
    /// Disable autocorrect.
    Off,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum AutoUpgradeMode {
    On,
    Off,
    Status,
}

#[derive(Debug, Subcommand)]
pub enum NotificationCommand {
    /// Report notification support and current state.
    Status,
    /// Clear locally cached notifications.
    Clear,
}

fn docs_cache() -> DocsCache {
    let root = env::var_os("CFY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME").map(|path| PathBuf::from(path).join("catify/docs"))
        })
        .or_else(|| env::var_os("HOME").map(|path| PathBuf::from(path).join(".cache/catify/docs")))
        .unwrap_or_else(|| PathBuf::from(".catify-cache/docs"));
    DocsCache::new(root)
}

fn docs_client() -> Result<DocsClient<HttpDocsTransport>> {
    Ok(DocsClient::new(HttpDocsTransport::new()?))
}

fn docs_command(command: DocCommand, output: &Output) -> Result<u8> {
    match command {
        DocCommand::ClearCache => {
            docs_cache().clear()?;
            output
                .success(
                    "Documentation cache cleared",
                    &serde_json::json!({"cleared": true}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
        }
        DocCommand::Search { query } => {
            let query = query.join(" ");
            let results = docs_client()?.with_cache(docs_cache()).search(&query)?;
            output
                .success(
                    &format!("{} documentation result(s)", results.len()),
                    &results,
                )
                .map_err(|error| Error::process(error.to_string()))?;
        }
        DocCommand::Fetch { url } => {
            let document = docs_client()?.with_cache(docs_cache()).fetch(&url)?;
            output
                .success(&document.url, &document)
                .map_err(|error| Error::process(error.to_string()))?;
        }
    }
    Ok(0)
}

#[derive(Debug, Subcommand)]
pub enum DocCommand {
    /// Search Shopify developer documentation.
    Search { query: Vec<String> },
    /// Fetch a complete document from shopify.dev.
    Fetch { url: String },
    /// Clear the local documentation cache.
    ClearCache,
}

#[derive(Debug, Subcommand)]
pub enum StoreCliCommand {
    /// Authenticate against a store.
    #[command(
        disable_version_flag = true,
        subcommand_negates_reqs = true,
        args_conflicts_with_subcommands = true
    )]
    Auth {
        #[command(subcommand)]
        command: Option<StoreAuthCommand>,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE", required = true)]
        store: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_SCOPES", required = true)]
        scopes: Option<String>,
    },
    /// Run, check, and cancel bulk Admin API operations.
    Bulk {
        #[command(subcommand)]
        command: StoreBulkCommand,
    },
    /// Create Shopify stores.
    Create {
        #[command(subcommand)]
        command: StoreCreateCommand,
    },
    /// List stores in a Shopify organization.
    #[command(disable_version_flag = true)]
    List {
        #[arg(long, env = "SHOPIFY_FLAG_ORGANIZATION_ID")]
        organization_id: Option<String>,
    },
    /// Show store information.
    Info {
        #[arg(long)]
        store: String,
    },
    /// Open the Shopify admin in a browser.
    Open {
        #[arg(long)]
        store: String,
    },
    /// Open Shopify GraphiQL in a browser.
    Graphiql {
        #[arg(long)]
        store: String,
    },
    /// Execute an Admin API request.
    Execute {
        #[arg(long)]
        store: String,
        query: String,
    },
    /// Delete a store. Requires --confirm.
    Delete {
        #[arg(long)]
        store: String,
        #[arg(long)]
        confirm: bool,
    },
    /// Authenticate Stripe for the selected store.
    StripeAuth {
        #[arg(long)]
        store: String,
    },
}

async fn store_command(
    command: StoreCliCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    match command {
        StoreCliCommand::Auth {
            command: Some(StoreAuthCommand::List),
            ..
        } => {
            let entries = StoreAuthRegistry::default()
                .list_current()
                .await?
                .iter()
                .map(cfy_store::store_auth::StoreAuthSummary::public)
                .collect::<Vec<_>>();
            let human = if entries.is_empty() {
                "No stores authenticated directly with `cfy store auth`.".to_owned()
            } else {
                entries
                    .iter()
                    .map(|entry| {
                        format!(
                            "{}\t{}\t{}\t{}",
                            entry.store,
                            entry
                                .associated_user
                                .email
                                .as_deref()
                                .unwrap_or(&entry.user_id),
                            entry.scopes.join(","),
                            entry.acquired_at
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            output
                .success(&human, &entries)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Auth {
            command: None,
            store,
            scopes,
        } => {
            if non_interactive || !io::stdin().is_terminal() {
                return Err(Error::invalid_input(
                    "store auth requires an interactive browser flow",
                ));
            }
            let store = store.ok_or_else(|| Error::invalid_input("store auth requires --store"))?;
            let requested_scopes =
                scopes.ok_or_else(|| Error::invalid_input("store auth requires --scopes"))?;
            let registry = StoreAuthRegistry::default();
            let normalized_store = StoreTarget::parse(&store)?.domain;
            let mut scopes = requested_scopes
                .split([',', ' '])
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if let Some(previous) = registry
                .list()?
                .into_iter()
                .find(|summary| summary.store == normalized_store)
            {
                scopes.extend(previous.scopes);
            }
            scopes.sort();
            scopes.dedup();
            let bootstrap = StoreAuthBootstrap::new(&store, &scopes.join(","))?;
            let callback = StoreAuthCallback::bind(&bootstrap).await?;
            output
                .lifecycle("Opening Shopify store authentication in your browser...")
                .map_err(|error| Error::process(error.to_string()))?;
            let opened = open_browser(&bootstrap.authorization_url);
            if !opened {
                output
                    .lifecycle(&format!(
                        "Open this URL manually:\n{}",
                        bootstrap.authorization_url
                    ))
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            let code = callback.wait(Duration::from_secs(5 * 60)).await?;
            let result = exchange_code(&bootstrap, &code).await?;
            registry.save(&result).await?;
            output
                .success(&format!("Authenticated {}", result.store), &result.public())
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::List { organization_id } => {
            let identity = "default";
            let credential_store = Arc::new(NativeCredentialStore::default());
            let identity_client = Arc::new(IdentityClient::new(
                HttpIdentityTransport::new()?,
                IdentityConfig::from_env(|key| env::var(key).ok())?,
            ));
            let sessions = cfy_auth::SessionManager::new(credential_store, identity_client);
            let session = sessions.session(identity).await?.ok_or_else(|| {
                Error::new(
                    ErrorKind::Api,
                    "no authenticated session; run `cfy auth login` first",
                )
            })?;
            let organizations = BusinessPlatformClient::from_session(&session)
                .await?
                .list_organizations()
                .await?;
            if organizations.is_empty() {
                output
                    .success(
                        "No stores found in your Shopify organization.",
                        &serde_json::json!({"stores": []}),
                    )
                    .map_err(|error| Error::process(error.to_string()))?;
                return Ok(0);
            }
            let organization = if let Some(requested) = organization_id {
                organizations
                    .iter()
                    .find(|organization| organization.id == requested)
                    .cloned()
                    .ok_or_else(|| {
                        let available = organizations
                            .iter()
                            .map(|organization| format!("{} ({})", organization.name, organization.id))
                            .collect::<Vec<_>>()
                            .join(", ");
                        Error::invalid_input(format!(
                            "organization with ID {requested} was not found; available organizations: {available}"
                        ))
                    })?
            } else if organizations.len() == 1 {
                organizations[0].clone()
            } else if non_interactive {
                return Err(Error::invalid_input(
                    "an organization ID is required to list stores non-interactively; pass --organization-id or run `cfy organization list`",
                ));
            } else {
                select_organization(&organizations)?
            };
            let result = OrganizationStoreClient::from_session(&session, &organization.id)
                .await
                .map_err(Error::from)?
                .list()
                .await
                .map_err(Error::from)?;
            if result.truncated {
                output
                    .lifecycle(&format!(
                        "Showing the {} most recent stores in {}. More stores exist.",
                        cfy_store::STORE_LIST_LIMIT,
                        result.organization_name
                    ))
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            let human = if result.stores.is_empty() {
                format!("No stores found in {}.", result.organization_name)
            } else {
                let rows = result
                    .stores
                    .iter()
                    .map(|store| {
                        format!(
                            "{}\t{}\t{}\t{}",
                            store.store,
                            store.name.as_deref().unwrap_or(""),
                            store.store_type.as_deref().unwrap_or(""),
                            store.created_at
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "Organization: {} ({})\nSubdomain\tName\tType\tCreated\n{}",
                    result.organization_name, result.organization_id, rows
                )
            };
            output
                .success(&human, &result)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Create {
            command: StoreCreateCommand::Preview { name, country: _ },
        } => {
            let endpoint = env::var("CFY_PARTNER_API_URL").map_err(|_| {
                Error::new(
                    ErrorKind::Api,
                    "store lifecycle API is not configured; set CFY_PARTNER_API_URL",
                )
            })?;
            let token = store_token()?;
            let backend = StoreManagementBackend::new(&endpoint, &token).map_err(Error::from)?;
            let store = name.unwrap_or_else(|| "Catify preview store".to_owned());
            let value = backend.create_preview(&store).await.map_err(Error::from)?;
            output
                .success("Store created", &value)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Delete { store, confirm } => {
            if !confirm {
                return Err(Error::invalid_input("store delete requires --confirm"));
            }
            let endpoint = env::var("CFY_PARTNER_API_URL").map_err(|_| {
                Error::new(
                    ErrorKind::Api,
                    "store lifecycle API is not configured; set CFY_PARTNER_API_URL",
                )
            })?;
            let token = store_token()?;
            let backend = StoreManagementBackend::new(&endpoint, &token).map_err(Error::from)?;
            let value = backend.delete(&store).await.map_err(Error::from)?;
            output
                .success("Store deleted", &value)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Status { store, id },
        } => {
            let endpoint = env::var("CFY_PARTNER_API_URL").map_err(|_| {
                Error::new(
                    ErrorKind::Api,
                    "store lifecycle API is not configured; set CFY_PARTNER_API_URL",
                )
            })?;
            let token = store_token()?;
            let backend = StoreManagementBackend::new(&endpoint, &token).map_err(Error::from)?;
            let value = backend
                .bulk_status(id.as_deref().unwrap_or(&store))
                .await
                .map_err(Error::from)?;
            output
                .success("Bulk operation status", &value)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Cancel { store: _, id },
        } => {
            let endpoint = env::var("CFY_PARTNER_API_URL").map_err(|_| {
                Error::new(
                    ErrorKind::Api,
                    "store lifecycle API is not configured; set CFY_PARTNER_API_URL",
                )
            })?;
            let token = store_token()?;
            let backend = StoreManagementBackend::new(&endpoint, &token).map_err(Error::from)?;
            let value = backend.bulk_cancel(&id).await.map_err(Error::from)?;
            output
                .success("Bulk operation cancelled", &value)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Info { store } => {
            let target = StoreTarget::parse(&store)?;
            let token = store_access_token(&target.domain).await?;
            let backend = AdminStoreBackend::new(&target, &token).map_err(Error::from)?;
            let info = backend.info(&target).await.map_err(Error::from)?;
            output
                .success(&format!("Store {}", target.domain), &info)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Execute { store, query } => {
            let target = StoreTarget::parse(&store)?;
            let token = store_access_token(&target.domain).await?;
            let backend = AdminStoreBackend::new(&target, &token).map_err(Error::from)?;
            let data = backend
                .execute(&target, &query)
                .await
                .map_err(Error::from)?;
            output
                .success("Store query completed", &data)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Bulk {
            command:
                StoreBulkCommand::Execute {
                    store,
                    query,
                    query_file,
                    variables: _,
                    variable_file: _,
                    output_file: _,
                    watch: _,
                    version: _,
                    allow_mutations: _,
                },
        } => {
            let query = match (query, query_file) {
                (Some(query), None) => query,
                (None, Some(path)) => std::fs::read_to_string(&path).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        format!("could not read {}", path.display()),
                        error,
                    )
                })?,
                _ => {
                    return Err(Error::invalid_input(
                        "store bulk execute requires exactly one of --query or --query-file",
                    ));
                }
            };
            let target = StoreTarget::parse(&store)?;
            let token = store_access_token(&target.domain).await?;
            let backend = AdminStoreBackend::new(&target, &token).map_err(Error::from)?;
            let cancellation = Cancellation::default();
            let mut progress = |_event| {};
            let report = backend
                .bulk_execute(&target, &[query], &mut progress, &cancellation)
                .await
                .map_err(Error::from)?;
            output
                .success("Store bulk operation completed", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        _ => {}
    }
    let (operation, target, destructive, confirm) = match command {
        StoreCliCommand::Open { store } => {
            let target = StoreTarget::parse(&store)?;
            let url = browser_url(StoreOperation::Open, &target, non_interactive)?;
            output
                .success(url.as_ref(), &serde_json::json!({ "url": url }))
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Graphiql { store } => {
            let target = StoreTarget::parse(&store)?;
            let url = browser_url(StoreOperation::Graphiql, &target, non_interactive)?;
            output
                .success(url.as_ref(), &serde_json::json!({ "url": url }))
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Delete { store, confirm } => {
            (StoreOperation::Delete, store, true, confirm)
        }
        StoreCliCommand::Auth { .. } => unreachable!("store auth returns before fallback dispatch"),
        StoreCliCommand::Info { store } => (StoreOperation::Info, store, false, false),
        StoreCliCommand::Execute { store, .. } => (StoreOperation::Execute, store, false, false),
        StoreCliCommand::Create {
            command: StoreCreateCommand::Preview { name, .. },
        } => (
            StoreOperation::CreatePreview,
            name.unwrap_or_else(|| "preview".into()),
            true,
            false,
        ),
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Execute { store, .. },
        } => (StoreOperation::BulkExecute, store, false, false),
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Status { store, .. },
        } => (StoreOperation::BulkStatus, store, false, false),
        StoreCliCommand::Bulk {
            command: StoreBulkCommand::Cancel { store, .. },
        } => (StoreOperation::BulkCancel, store, true, true),
        StoreCliCommand::StripeAuth { store } => (StoreOperation::StripeAuth, store, false, false),
        StoreCliCommand::List { .. } => unreachable!("store list returns before fallback dispatch"),
    };

    let _target = StoreTarget::parse(&target)?;
    cfy_store::ConfirmationPolicy {
        non_interactive,
        confirm,
        destructive,
    }
    .authorize()?;
    Err(cfy_store::StoreError::Unsupported(operation).into())
}

async fn auth_command(command: AuthCommand, non_interactive: bool, output: &Output) -> Result<u8> {
    let store = NativeCredentialStore::default();
    match command {
        AuthCommand::Login { identity, delegate } => {
            let mode = headless_from_env(&identity, |key| env::var(key).ok());
            if non_interactive {
                let LoginMode::Headless {
                    access_token,
                    refresh_token,
                    expires_at_unix,
                } = mode?
                else {
                    return Err(Error::invalid_input("headless login requires a token"));
                };
                let session = Session {
                    identity: identity.clone(),
                    display_name: Some(identity.clone()),
                    access_token,
                    refresh_token: refresh_token.unwrap_or_else(|| cfy_auth::Secret::new("")),
                    expires_at_unix,
                    scopes: Vec::new(),
                };
                store.save(&session).await?;
                output
                    .success(
                        "Headless session saved to the native credential store.",
                        &serde_json::json!({ "identity": identity, "stored": true }),
                    )
                    .map_err(|error| {
                        Error::with_source(
                            ErrorKind::Process,
                            "could not write login result",
                            error,
                        )
                    })?;
                return Ok(0);
            }
            if delegate {
                return delegate_shopify_login(output).await;
            }
            if let Some(session) = store.load(&identity).await?
                && reusable_session(&session, current_unix_time())
            {
                let reuse = if output.mode() == output::OutputMode::Human
                    && io::stdin().is_terminal()
                    && io::stderr().is_terminal()
                {
                    select_auth_account(&session)?
                } else {
                    true
                };
                if reuse {
                    output
                        .success(
                            "Using existing authenticated session",
                            &serde_json::json!({
                                "identity": session.identity,
                                "account": session.display_name,
                                "scopes": session.scopes,
                                "reused": true
                            }),
                        )
                        .map_err(|error| {
                            Error::with_source(
                                ErrorKind::Process,
                                "could not write login result",
                                error,
                            )
                        })?;
                    return Ok(0);
                }
            }
            let config = IdentityConfig::from_env(|key| env::var(key).ok())?;
            let client = IdentityClient::new(HttpIdentityTransport::new()?, config);
            let identity_name = identity.clone();
            let session = client
                .login_and_save_with_notice(&store, &identity, |authorization| {
                    let opened = open_browser(&authorization.verification_uri);
                    let _ = output.lifecycle(if opened {
                        "Opening Shopify authentication in your browser..."
                    } else {
                        "Could not open a browser automatically; use the URL below..."
                    });
                    let _ = output.lifecycle(&format!("URL: {}", authorization.verification_uri));
                    let _ = output.lifecycle(&format!(
                        "Code: {} (waiting for authentication)",
                        authorization.user_code
                    ));
                })
                .await?;
            output
                .success(
                    "Authentication succeeded",
                    &serde_json::json!({"identity": identity_name, "scopes": session.scopes}),
                )
                .map_err(|error| {
                    Error::with_source(ErrorKind::Process, "could not write login result", error)
                })?;
            Ok(0)
        }
        AuthCommand::Logout => {
            let identity = "default";
            store.delete(identity).await?;
            output
                .success(
                    "Logged out from Shopify.",
                    &serde_json::json!({ "identity": identity, "removed": true }),
                )
                .map_err(|error| {
                    Error::with_source(ErrorKind::Process, "could not write logout result", error)
                })?;
            Ok(0)
        }
    }
}

async fn organization_command(command: OrganizationCommand, output: &Output) -> Result<u8> {
    match command {
        OrganizationCommand::List { auth_alias } => {
            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let store = Arc::new(NativeCredentialStore::default());
            let identity_client = Arc::new(IdentityClient::new(
                HttpIdentityTransport::new()?,
                IdentityConfig::from_env(|key| env::var(key).ok())?,
            ));
            let sessions = cfy_auth::SessionManager::new(Arc::clone(&store), identity_client);
            let session = sessions.session(&identity).await?.ok_or_else(|| {
                Error::new(
                    ErrorKind::Api,
                    format!(
                        "no authenticated session for `{identity}`; run `cfy auth login --identity {identity}` first"
                    ),
                )
            })?;
            let organizations = BusinessPlatformClient::from_session(&session)
                .await?
                .list_organizations()
                .await?;
            output
                .success(
                    &format!("{} organization(s)", organizations.len()),
                    &organizations,
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Start browser/device login or consume a headless token.
    Login {
        /// Identity key used for credential storage.
        #[arg(long, default_value = "default")]
        identity: String,
        /// Delegate login to an installed official Shopify CLI instead of using cfy's native flow.
        #[arg(long)]
        delegate: bool,
    },
    /// Log out of the active Shopify account by removing its local session.
    Logout,
}

#[derive(Debug, Subcommand)]
pub enum OrganizationCommand {
    /// List organizations available to the current identity.
    List {
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
    },
}

const COMMAND_INVENTORY: &str = include_str!("../../../inventory/runtime-shopify-cli.json");

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CommandColumn {
    Id,
    Plugin,
    Summary,
    Type,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum CommandSort {
    #[default]
    Id,
    Plugin,
    Summary,
    Type,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
struct CommandRecord {
    name: String,
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    flags: Vec<serde_json::Value>,
    #[serde(default)]
    environment_variables: Vec<String>,
    #[serde(default)]
    summary: String,
    plugin_name: Option<String>,
    plugin_type: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    deprecated: bool,
}

#[derive(serde::Deserialize)]
struct CommandInventory {
    commands: Vec<CommandRecord>,
}

fn print_help(topic: Option<&str>) {
    let mut command = Cli::command();
    if let Some(topic) = topic
        && let Some(subcommand) = command.find_subcommand_mut(topic)
    {
        let _ = subcommand.print_long_help();
        println!();
        return;
    }
    let _ = command.print_long_help();
    println!();
}

fn print_commands(
    columns: Vec<CommandColumn>,
    extended: bool,
    hidden: bool,
    deprecated: bool,
    sort: CommandSort,
    tree: bool,
    output: &Output,
) -> Result<()> {
    let mut commands = serde_json::from_str::<CommandInventory>(COMMAND_INVENTORY)
        .map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "embedded command inventory is invalid",
                error,
            )
        })?
        .commands;
    commands.retain(|command| (hidden || !command.hidden) && (deprecated || !command.deprecated));
    commands.sort_by(|left, right| {
        let ordering = match sort {
            CommandSort::Id => left.name.cmp(&right.name),
            CommandSort::Plugin => left.plugin_name.cmp(&right.plugin_name),
            CommandSort::Summary => left.summary.cmp(&right.summary),
            CommandSort::Type => left.plugin_type.cmp(&right.plugin_type),
        };
        ordering.then_with(|| left.name.cmp(&right.name))
    });

    let columns = if columns.is_empty() {
        if extended {
            vec![
                CommandColumn::Id,
                CommandColumn::Plugin,
                CommandColumn::Summary,
                CommandColumn::Type,
            ]
        } else {
            vec![CommandColumn::Id, CommandColumn::Summary]
        }
    } else {
        columns
    };
    let human = if tree {
        commands
            .iter()
            .map(|command| {
                let depth = command.name.split_whitespace().count().saturating_sub(1);
                let leaf = command
                    .name
                    .split_whitespace()
                    .last()
                    .unwrap_or(&command.name);
                format!("{}{}\t{}", "  ".repeat(depth), leaf, command.summary)
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        let mut rows = vec![
            columns
                .iter()
                .map(command_column_title)
                .collect::<Vec<_>>()
                .join("\t"),
        ];
        rows.extend(commands.iter().map(|command| {
            columns
                .iter()
                .map(|column| command_column_value(command, *column))
                .collect::<Vec<_>>()
                .join("\t")
        }));
        rows.join("\n")
    };
    output.success(&human, &commands).map_err(|error| {
        Error::with_source(
            cfy_core::ErrorKind::Process,
            "could not write command listing",
            error,
        )
    })
}

async fn upgrade(non_interactive: bool, output: &Output) -> Result<()> {
    let provenance = detect_upgrade()?;
    let plan = plan_upgrade(&provenance)
        .map_err(|error| Error::with_source(ErrorKind::Config, error.to_string(), error))?;
    let result = execute_upgrade(
        &plan,
        ExecutionPolicy {
            interactive: !non_interactive,
            approved: !non_interactive,
        },
        &Supervisor::default(),
    )
    .await
    .map_err(|error| Error::with_source(ErrorKind::Process, error.to_string(), error))?;
    if !result.status.success() {
        return Err(Error::process(format!(
            "upgrade command exited with status {:?}",
            result.exit_code()
        )));
    }
    output
        .success(
            "Catify upgraded",
            &serde_json::json!({
                "provenance": provenance.kind().to_string(),
                "exit_code": result.exit_code(),
                "changed": true,
            }),
        )
        .map_err(|error| {
            Error::with_source(
                cfy_core::ErrorKind::Process,
                "could not write upgrade result",
                error,
            )
        })
}

async fn theme_dev(
    requested_theme: Option<u64>,
    explicit_store: Option<&str>,
    source: &Path,
    debounce_ms: u64,
    output: &Output,
) -> Result<()> {
    let source = source.canonicalize().map_err(|error| {
        Error::with_source(
            cfy_core::ErrorKind::Config,
            format!("could not resolve theme directory {}", source.display()),
            error,
        )
    })?;
    if !source.is_dir() {
        return Err(Error::new(
            cfy_core::ErrorKind::Config,
            format!("theme source {} is not a directory", source.display()),
        ));
    }
    let store = resolve_store(explicit_store)?;
    let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| Error::new(
        cfy_core::ErrorKind::Api,
        "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Catify login flow",
    ))?;
    let client = ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
    let cancellation = Cancellation::default();
    let (theme_id, created) = if let Some(id) = requested_theme {
        (id, false)
    } else {
        output
            .lifecycle("Creating development theme...")
            .map_err(|e| {
                Error::with_source(
                    cfy_core::ErrorKind::Process,
                    "could not write lifecycle state",
                    e,
                )
            })?;
        let name = format!("Catify development {}", std::process::id());
        (
            client
                .create_development_theme(&name, &cancellation)
                .await
                .map_err(Error::from)?
                .id,
            true,
        )
    };
    let result = async {
        output.lifecycle("Initial sync in progress...").map_err(|e| Error::with_source(cfy_core::ErrorKind::Process, "could not write lifecycle state", e))?;
        let local = read_theme_files(&source).map_err(|e| Error::with_source(cfy_core::ErrorKind::Config, format!("could not safely scan {}", source.display()), e))?;
        let changes = local.into_iter().map(|(key, contents)| ThemeChange::Upload(ThemeAsset { key, contents })).collect::<Vec<_>>();
        sync_with_retry(&client, theme_id, &changes, &cancellation).await?;
        let preview = format!("https://{store}/?preview_theme_id={theme_id}");
        let editor = format!("https://{store}/admin/themes/{theme_id}/editor");
        output.lifecycle(&format!("Ready and watching {}\nPreview: {preview}\nEditor: {editor}", source.display())).map_err(|e| Error::with_source(cfy_core::ErrorKind::Process, "could not write lifecycle state", e))?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |event| { let _ = tx.send(event); })
            .map_err(|e| Error::with_source(cfy_core::ErrorKind::Process, "could not create filesystem watcher", e))?;
        watcher.watch(&source, RecursiveMode::Recursive).map_err(|e| Error::with_source(cfy_core::ErrorKind::Process, format!("could not watch {}", source.display()), e))?;
        loop {
            let first = tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                event = rx.recv() => event.ok_or_else(|| Error::new(cfy_core::ErrorKind::Process, "filesystem watcher stopped unexpectedly"))?,
            };
            let mut events = vec![first.map_err(|e| Error::with_source(cfy_core::ErrorKind::Process, "filesystem watcher error", e))?];
            tokio::time::sleep(Duration::from_millis(debounce_ms.max(10))).await;
            while let Ok(event) = rx.try_recv() { events.push(event.map_err(|e| Error::with_source(cfy_core::ErrorKind::Process, "filesystem watcher error", e))?); }
            let mut changes = Vec::new();
            for action in coalesce(events.into_iter().flat_map(filesystem_event)) {
                let path = match &action { SyncAction::Upload(path) | SyncAction::Delete(path) => path };
                    let Ok(relative) = path.strip_prefix(&source) else { continue };
                    let Ok(relative) = safe_relative_path(&relative.to_string_lossy()) else { continue };
                    let key = relative.to_string_lossy().replace('\\', "/");
                    match action {
                        SyncAction::Delete(_) => changes.push(ThemeChange::Delete(key)),
                        SyncAction::Upload(path) => {
                            let Ok(metadata) = std::fs::symlink_metadata(&path) else { continue };
                            if metadata.file_type().is_symlink() || !metadata.is_file() { continue; }
                            let Ok(canonical) = path.canonicalize() else { continue };
                            if !canonical.starts_with(&source) { continue; }
                            if let Ok(contents) = std::fs::read(canonical) {
                                changes.push(ThemeChange::Upload(ThemeAsset { key, contents }));
                            }
                        }
                    }
            }
            if !changes.is_empty() { sync_with_retry(&client, theme_id, &changes, &cancellation).await?; }
        }
        drop(watcher);
        Ok(())
    }.await;
    if created {
        output.lifecycle("Cleaning up development theme...").ok();
        if let Err(cleanup) = client
            .delete_theme(theme_id, &Cancellation::default())
            .await
        {
            return Err(Error::new(
                cfy_core::ErrorKind::Api,
                format!("session ended; failed to delete development theme {theme_id}: {cleanup}"),
            ));
        }
    }
    result
}

fn filesystem_event(event: notify::Event) -> Vec<FileEvent> {
    if let EventKind::Modify(ModifyKind::Name(mode)) = event.kind {
        return match (mode, event.paths.as_slice()) {
            (RenameMode::Both, [from, to, ..]) => vec![FileEvent::Rename {
                from: from.clone(),
                to: to.clone(),
            }],
            (RenameMode::From, paths) => paths.iter().cloned().map(FileEvent::Remove).collect(),
            (RenameMode::To, paths) => paths.iter().cloned().map(FileEvent::Upsert).collect(),
            (_, paths) if paths.len() >= 2 => vec![FileEvent::Rename {
                from: paths[0].clone(),
                to: paths[1].clone(),
            }],
            (_, paths) => paths.iter().cloned().map(FileEvent::Upsert).collect(),
        };
    }
    let remove = matches!(event.kind, EventKind::Remove(_));
    event
        .paths
        .into_iter()
        .map(|path| {
            if remove {
                FileEvent::Remove(path)
            } else {
                FileEvent::Upsert(path)
            }
        })
        .collect()
}

async fn sync_with_retry(
    client: &ThemeClient,
    theme_id: u64,
    changes: &[ThemeChange],
    cancellation: &Cancellation,
) -> Result<()> {
    let mut delay = Duration::from_millis(150);
    for attempt in 1..=4 {
        let summary = client.push(theme_id, changes, true, cancellation).await;
        if summary.succeeded() {
            return Ok(());
        }
        if attempt == 4 {
            return Err(Error::new(
                cfy_core::ErrorKind::Api,
                format!(
                    "theme sync failed after {attempt} attempts: {}. Check connectivity and asset paths, then retry",
                    summary.failed.join("; ")
                ),
            ));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(2));
    }
    unreachable!()
}

fn live_push_requires_confirmation(
    is_live: bool,
    force: bool,
    non_interactive: bool,
) -> Result<bool> {
    if !is_live || force {
        return Ok(false);
    }
    if non_interactive {
        return Err(Error::invalid_input(
            "refusing to push to the live theme in non-interactive mode; pass --force to acknowledge the risk",
        ));
    }
    Ok(true)
}

async fn push_theme(
    theme: u64,
    explicit_store: Option<&str>,
    source: &Path,
    allow_delete: bool,
    force: bool,
    non_interactive: bool,
    output: &Output,
) -> Result<()> {
    let store = resolve_store(explicit_store)?;
    let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| Error::new(
        cfy_core::ErrorKind::Api,
        "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Catify login flow",
    ))?;
    let client = ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
    let themes = client.list().await.map_err(Error::from)?;
    let selected = themes
        .iter()
        .find(|candidate| candidate.id == theme)
        .ok_or_else(|| Error::invalid_input(format!("theme {theme} was not found on {store}")))?;
    if live_push_requires_confirmation(selected.role == "main", force, non_interactive)? {
        if !io::stdin().is_terminal() {
            return Err(Error::invalid_input(
                "refusing to prompt for a live theme without an interactive terminal; pass --force to acknowledge the risk",
            ));
        }
        eprint!(
            "Theme {theme} ({}) is live. Push changes? [y/N] ",
            selected.name
        );
        io::stderr().flush().map_err(|error| {
            Error::with_source(
                cfy_core::ErrorKind::Process,
                "could not display confirmation",
                error,
            )
        })?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).map_err(|error| {
            Error::with_source(
                cfy_core::ErrorKind::Process,
                "could not read confirmation",
                error,
            )
        })?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Err(Error::invalid_input("live theme push was not confirmed"));
        }
    }
    let local = read_theme_files(source).map_err(|error| {
        Error::with_source(
            cfy_core::ErrorKind::Config,
            format!("could not read theme files from {}", source.display()),
            error,
        )
    })?;
    let cancellation = Cancellation::default();
    let signal = cancellation.clone();
    let _signal_task = AbortOnDrop(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    }));
    let remote = client
        .pull(theme, &[], &[], &cancellation)
        .await
        .map_err(Error::from)?;
    let changes = diff_assets(&local, &remote);
    let summary = client
        .push(theme, &changes, allow_delete, &cancellation)
        .await;
    if !summary.succeeded() {
        return Err(Error::new(
            cfy_core::ErrorKind::Api,
            format!(
                "theme push partially failed: {} uploaded, {} deleted, {} deletion(s) skipped; failures: {}. Re-run the command after fixing these assets",
                summary.uploaded.len(),
                summary.deleted.len(),
                summary.skipped_deletions.len(),
                summary.failed.join("; ")
            ),
        ));
    }
    output
        .success(
            &format!(
                "Pushed theme {theme}: {} uploaded, {} deleted, {} deletion(s) skipped.",
                summary.uploaded.len(),
                summary.deleted.len(),
                summary.skipped_deletions.len()
            ),
            &summary,
        )
        .map_err(|error| {
            Error::with_source(
                cfy_core::ErrorKind::Process,
                "could not write theme push result",
                error,
            )
        })
}

/// Catify's top-level command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "cfy",
    version,
    about = "A fast, memory-efficient Shopify CLI alternative",
    long_about = None,
    propagate_version = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOptions,

    #[command(subcommand)]
    pub command: Option<Command>,
}

async fn pull_theme(
    theme: u64,
    explicit_store: Option<&str>,
    includes: &[String],
    excludes: &[String],
    destination: &Path,
    output: &Output,
) -> Result<()> {
    let store = resolve_store(explicit_store)?;
    let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| {
        Error::new(
            cfy_core::ErrorKind::Api,
            "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Catify login flow",
        )
    })?;
    let client = ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
    let cancellation = Cancellation::default();
    let signal = cancellation.clone();
    let _signal_task = AbortOnDrop(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    }));
    // The complete remote set is staged in memory. Cancellation or any partial
    // HTTP failure therefore leaves the destination untouched.
    let assets = client
        .pull(theme, includes, excludes, &cancellation)
        .await
        .map_err(Error::from)?;
    let files = assets
        .into_iter()
        .map(|asset| {
            Ok(StagedFile {
                path: safe_relative_path(&asset.key).map_err(|error| {
                    Error::with_source(
                        cfy_core::ErrorKind::Config,
                        format!("Shopify returned an unsafe theme asset path: {}", asset.key),
                        error,
                    )
                })?,
                contents: asset.contents,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    commit_staged_files_cancellable(destination, &files, &cancellation).map_err(|error| {
        Error::with_source(
            cfy_core::ErrorKind::Config,
            format!("could not commit theme assets to {}", destination.display()),
            error,
        )
    })?;
    output
        .success(
            &format!("Pulled {} theme assets to {}.", files.len(), destination.display()),
            &serde_json::json!({ "theme_id": theme, "store": store, "destination": destination, "files": files.len() }),
        )
        .map_err(|error| Error::with_source(cfy_core::ErrorKind::Process, "could not write theme pull result", error))
}

async fn list_themes(explicit_store: Option<&str>, output: &Output) -> Result<()> {
    let store = resolve_store(explicit_store)?;
    let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| {
        Error::new(
            cfy_core::ErrorKind::Api,
            "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Catify login flow",
        )
    })?;
    let client = ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
    let themes = client.list().await.map_err(Error::from)?;
    output
        .success(&format_themes(&themes), &themes)
        .map_err(|error| {
            Error::with_source(
                cfy_core::ErrorKind::Process,
                "could not write theme list",
                error,
            )
        })
}

fn resolve_store(explicit_store: Option<&str>) -> Result<String> {
    let environment = Environment::from_iter(
        ["CFY_STORE", "SHOPIFY_FLAG_STORE"]
            .into_iter()
            .filter_map(|name| env::var(name).ok().map(|value| (name.to_owned(), value))),
    );

    let current = env::current_dir().map_err(|error| {
        Error::with_source(
            cfy_core::ErrorKind::Config,
            "could not read current directory",
            error,
        )
    })?;
    if let Ok(project) = discover(&current, Some(ProjectKind::App)) {
        let selected =
            resolve_environment(project, &ProjectOverrides::default(), &Environment::new())?;
        return select_store(explicit_store, &environment, selected.store.as_deref());
    }

    if let Ok(project) = discover(&current, Some(ProjectKind::Theme)) {
        let selected =
            resolve_environment(project, &ProjectOverrides::default(), &Environment::new())?;
        return select_store(explicit_store, &environment, selected.store.as_deref());
    }

    select_store(explicit_store, &environment, None)
}

fn theme_environment_credentials(
    root: &Path,
    requested: &[String],
) -> Result<(Option<String>, Option<String>)> {
    if requested.len() > 1 {
        return Err(Error::invalid_input(
            "theme metafields pull accepts only one --environment",
        ));
    }
    let path = root.join("shopify.theme.toml");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && requested.is_empty() => {
            return Ok((None, None));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::invalid_input(format!(
                "theme environment `{}` requires {}",
                requested[0],
                path.display()
            )));
        }
        Err(error) => {
            return Err(Error::with_source(
                ErrorKind::Config,
                format!("could not read {}", path.display()),
                error,
            ));
        }
    };
    let document: toml::Value = toml::from_str(&contents).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not parse {}", path.display()),
            error,
        )
    })?;
    let name = requested.first().map(String::as_str).unwrap_or("default");
    let environment = document
        .get("environments")
        .and_then(|value| value.get(name));
    if environment.is_none() && requested.is_empty() {
        return Ok((None, None));
    }
    let environment = environment.ok_or_else(|| {
        Error::invalid_input(format!(
            "theme environment `{name}` was not found in {}",
            path.display()
        ))
    })?;
    Ok((
        environment
            .get("store")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
        environment
            .get("password")
            .and_then(toml::Value::as_str)
            .map(str::to_owned),
    ))
}

fn select_store(
    explicit_store: Option<&str>,
    environment: &Environment,
    configured_store: Option<&str>,
) -> Result<String> {
    if let Some(store) = explicit_store.filter(|store| !store.trim().is_empty()) {
        return Ok(store.to_owned());
    }
    for name in ["CFY_STORE", "SHOPIFY_FLAG_STORE"] {
        if let Some(store) = environment
            .get(name)
            .filter(|store| !store.trim().is_empty())
        {
            return Ok(store.clone());
        }
    }
    if let Some(store) = configured_store.filter(|store| !store.trim().is_empty()) {
        return Ok(store.to_owned());
    }

    Err(Error::invalid_input(
        "no store selected; pass --store, set CFY_STORE/SHOPIFY_FLAG_STORE, or add store to the project configuration",
    ))
}

fn format_themes(themes: &[Theme]) -> String {
    if themes.is_empty() {
        return "No themes found.".to_owned();
    }
    themes
        .iter()
        .map(|theme| format!("{}\t{}\t{}", theme.id, theme.role, theme.name))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Options shared by every Catify command.
#[derive(Debug, Default, Args)]
pub struct GlobalOptions {
    /// Increase diagnostic output; repeat for more detail.
    #[arg(long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Disable ANSI color output.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Emit machine-readable JSON when supported by the command.
    #[arg(long, global = true)]
    pub json: bool,

    /// Never prompt for interactive input.
    #[arg(long, global = true)]
    pub non_interactive: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Display help for Catify.
    Help {
        /// Optional topic or command to describe.
        topic: Option<String>,
    },
    /// List all public Catify commands.
    Commands {
        /// Only show provided columns.
        #[arg(short = 'c', long, value_delimiter = ',')]
        columns: Vec<CommandColumn>,
        /// Show extra columns.
        #[arg(short = 'x', long)]
        extended: bool,
        /// Include deprecated commands.
        #[arg(long)]
        deprecated: bool,
        /// Include hidden commands.
        #[arg(long)]
        hidden: bool,
        /// Do not truncate output. Catify output is never truncated.
        #[arg(long)]
        no_truncate: bool,
        /// Sort the command listing by a field.
        #[arg(long, default_value = "id")]
        sort: CommandSort,
        /// Render commands as a tree.
        #[arg(long)]
        tree: bool,
    },
    /// Manage Shopify apps.
    #[command(alias = "a")]
    App {
        #[command(subcommand)]
        command: AppCommand,
    },

    /// Manage Shopify themes.
    #[command(alias = "th")]
    Theme {
        #[command(subcommand)]
        command: ThemeCommand,
    },

    /// Generate a shell completion script on standard output.
    Completion {
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Print build and runtime version information.
    #[command(alias = "v")]
    Version,

    /// Upgrade Catify through a supported installation channel.
    Upgrade,

    /// Authentication operations.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Catify configuration options.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Show or clear Catify notification state.
    Notification {
        #[command(subcommand)]
        command: NotificationCommand,
    },
    /// Manage local caches.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
    /// Diagnose the local Catify environment and project.
    Doctor {
        #[command(subcommand)]
        command: DoctorCommand,
    },
    /// Search and fetch Shopify documentation.
    Doc {
        #[command(subcommand)]
        command: DocCommand,
    },
    /// Build Hydrogen storefronts.
    Hydrogen {
        /// Hydrogen subcommand and arguments.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// List Shopify organizations.
    Organization {
        #[command(subcommand)]
        command: OrganizationCommand,
    },
    /// Work directly with Shopify stores.
    Store {
        #[command(subcommand)]
        command: StoreCliCommand,
    },
    /// Manage CLI plugins.
    Plugins {
        #[command(subcommand)]
        command: PluginsCommand,
    },
    /// Search Shopify developer documentation.
    Search {
        /// Search query.
        query: Vec<String>,
    },

    #[command(hide = true)]
    Internal {
        #[command(subcommand)]
        command: InternalCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DoctorCommand {
    /// Print runtime, toolchain, and platform diagnostics.
    Env,
    /// Inspect the current project root and config markers.
    Project,
}

fn doctor_command(command: DoctorCommand, output: &Output) -> Result<u8> {
    let value = match command {
        DoctorCommand::Env => serde_json::json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "rust_version": option_env!("RUSTC_VERSION").unwrap_or("unknown"),
            "shell": env::var("SHELL").ok(),
        }),
        DoctorCommand::Project => {
            let cwd = env::current_dir().map_err(|error| Error::api(error.to_string()))?;
            let project = cfy_config::project::discover(&cwd, None).ok();
            serde_json::json!({
                "cwd": cwd,
                "project_found": project.is_some(),
                "project_kind": project.map(|value| format!("{:?}", value.kind())),
            })
        }
    };
    output
        .success("Catify diagnostics", &value)
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

fn selected_app_environment(
    path: Option<PathBuf>,
    config: Option<String>,
    client_id: Option<String>,
    reset: bool,
) -> Result<cfy_config::project::ProjectEnvironment> {
    let cwd = path.unwrap_or(env::current_dir().map_err(|error| Error::api(error.to_string()))?);
    let project = discover(&cwd, Some(ProjectKind::App))?;
    let state_path = app_state_path();
    let mut state = ActiveConfigState::load(&state_path)?;
    if reset {
        state.clear(project.root());
        state.write(&state_path)?;
    }
    let environment = env::vars().collect::<Environment>();
    let explicit_config = config.or_else(|| {
        environment
            .get("CFY_CONFIG")
            .or_else(|| environment.get("SHOPIFY_FLAG_APP_CONFIG"))
            .cloned()
    });
    let client_config = if explicit_config.is_none() {
        client_id
            .map(|client_id| {
                load_local_app_configs(&project)?
                    .into_iter()
                    .find(|choice| choice.client_id == client_id)
                    .map(|choice| choice.file_name)
                    .ok_or_else(|| {
                        Error::invalid_input(
                            "the specified client ID could not be found in any app TOML file",
                        )
                    })
            })
            .transpose()?
    } else {
        None
    };
    let cached_config = if explicit_config.is_none() {
        state.selected(project.root()).map(ToOwned::to_owned)
    } else {
        None
    };
    resolve_environment(
        project,
        &ProjectOverrides {
            config: explicit_config.or(client_config).or(cached_config),
            ..ProjectOverrides::default()
        },
        &environment,
    )
}

fn app_info(
    path: Option<PathBuf>,
    config: Option<String>,
    client_id: Option<String>,
    reset: bool,
    web_env: bool,
    output: &Output,
) -> Result<u8> {
    let selected = selected_app_environment(path, config, client_id, reset)?;
    if web_env {
        let values = app_environment(&selected);
        let rendered = values
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join("\n");
        output
            .success(&rendered, &values)
            .map_err(|error| Error::process(error.to_string()))?;
        return Ok(0);
    }

    let graph =
        cfy_config::graph::AppConfigGraph::load_selected(&selected.project, &selected.config_path)?;
    let app = graph
        .apps
        .first()
        .ok_or_else(|| Error::config("selected app configuration produced no app node"))?;
    let scopes = app
        .config
        .raw
        .get("access_scopes")
        .and_then(toml::Value::as_table)
        .and_then(|table| table.get("scopes"))
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let package_manager =
        if graph.root.join("bun.lock").exists() || graph.root.join("bun.lockb").exists() {
            "bun"
        } else if graph.root.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if graph.root.join("yarn.lock").exists() {
            "yarn"
        } else if graph.root.join("package-lock.json").exists() {
            "npm"
        } else {
            "unknown"
        };
    let extensions = app
        .extensions
        .iter()
        .map(|extension| {
            serde_json::json!({
                "name": extension.name,
                "handle": extension.handle,
                "type": extension.extension_type,
                "family": format!("{:?}", extension.family),
                "path": extension.path,
            })
        })
        .collect::<Vec<_>>();
    let webs = app
        .webs
        .iter()
        .map(|web| {
            serde_json::json!({
                "name": web.name,
                "roles": web.roles,
                "type": web.web_type,
                "path": web.path,
            })
        })
        .collect::<Vec<_>>();
    let diagnostics = graph
        .diagnostics
        .iter()
        .map(|diagnostic| {
            serde_json::json!({
                "severity": format!("{:?}", diagnostic.severity).to_lowercase(),
                "message": diagnostic.message,
                "file": diagnostic.location.file,
                "line": diagnostic.location.line,
                "column": diagnostic.location.column,
            })
        })
        .collect::<Vec<_>>();
    let report = serde_json::json!({
        "project_root": graph.root,
        "config": selected.config_path,
        "app": {
            "name": app.config.name,
            "client_id": app.config.client_id,
            "application_url": app.config.application_url,
            "embedded": app.config.embedded,
            "scopes": scopes,
        },
        "extensions": extensions,
        "webs": webs,
        "system": {
            "package_manager": package_manager,
            "catify_version": env!("CARGO_PKG_VERSION"),
            "os": env::consts::OS,
            "arch": env::consts::ARCH,
        },
        "diagnostics": diagnostics,
    });
    let human = format!(
        "App information\n\nName: {}\nClient ID: {}\nConfiguration: {}\nApplication URL: {}\nEmbedded: {}\nScopes: {}\nExtensions: {}\nWeb components: {}\nPackage manager: {}\nDiagnostics: {}",
        app.config.name.as_deref().unwrap_or("unknown"),
        app.config.client_id.as_deref().unwrap_or("unknown"),
        selected.config_path.display(),
        app.config.application_url.as_deref().unwrap_or("unknown"),
        app.config
            .embedded
            .map_or("unknown".to_owned(), |value| value.to_string()),
        if scopes.is_empty() { "none" } else { scopes },
        app.extensions.len(),
        app.webs.len(),
        package_manager,
        graph.diagnostics.len(),
    );
    output
        .success(&human, &report)
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

async fn app_bulk_client(
    context: AppBulkContext,
    requested_version: Option<&str>,
) -> Result<BulkClient> {
    let selected = selected_app_environment(
        context.path,
        context.config,
        context.client_id,
        context.reset,
    )?;
    let client_id = selected
        .document
        .get("client_id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| Error::invalid_input("selected app configuration has no client_id"))?;
    let store_domain = context
        .store
        .or(selected.store)
        .ok_or_else(|| Error::invalid_input("a development store is required; pass --store"))?;
    let store_domain = BulkStoreDomain::parse(&store_domain)
        .map_err(|error| Error::invalid_input(error.to_string()))?;
    let identity = context.auth_alias.unwrap_or_else(|| "default".to_owned());
    let store = Arc::new(NativeCredentialStore::default());
    let identity_client = Arc::new(IdentityClient::new(
        HttpIdentityTransport::new()?,
        IdentityConfig::from_env(|key| env::var(key).ok())?,
    ));
    let sessions = cfy_auth::SessionManager::new(Arc::clone(&store), identity_client);
    let session = sessions.session(&identity).await?.ok_or_else(|| {
        Error::api(format!(
            "no authenticated session for `{identity}`; run `cfy auth login` first"
        ))
    })?;
    let app_management = AppManagementClient::from_session(&session).await?;
    let credentials = app_management.app_client_credentials(client_id).await?;
    let credentials = BulkAppCredentials::new(
        credentials.client_id,
        credentials.client_secret.expose().to_owned(),
    );
    let token = exchange_client_credentials(&store_domain, &credentials)
        .await
        .map_err(|error| Error::api(error.to_string()))?;
    let version = resolve_api_version(&store_domain, requested_version)
        .await
        .map_err(|error| Error::api(error.to_string()))?;
    BulkClient::new(&store_domain, &version, token.secret())
        .map_err(|error| Error::api(error.to_string()))
}

async fn app_bulk_command(command: AppBulkCommand, output: &Output) -> Result<u8> {
    match command {
        AppBulkCommand::Execute {
            context,
            query,
            query_file,
            variables,
            variable_file,
            output_file,
            watch,
            version,
        } => {
            let document = match (query, query_file) {
                (Some(query), None) => query,
                (None, Some(path)) => std::fs::read_to_string(&path).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        format!("could not read bulk query {}", path.display()),
                        error,
                    )
                })?,
                _ => {
                    return Err(Error::invalid_input(
                        "provide exactly one of --query or --query-file",
                    ));
                }
            };
            let client = app_bulk_client(context, version.as_deref()).await?;
            let operation = match cfy_bulk::operation_kind(&document)
                .map_err(|error| Error::invalid_input(error.to_string()))?
            {
                cfy_bulk::OperationKind::Query => {
                    if !variables.is_empty() || variable_file.is_some() {
                        return Err(Error::invalid_input(
                            "--variables and --variable-file can only be used with mutations",
                        ));
                    }
                    client.execute_query(&document).await
                }
                cfy_bulk::OperationKind::Mutation => {
                    let jsonl = if let Some(path) = variable_file {
                        std::fs::read(&path).map_err(|error| {
                            Error::with_source(
                                ErrorKind::Config,
                                format!("could not read bulk variables {}", path.display()),
                                error,
                            )
                        })?
                    } else {
                        variables.join("\n").into_bytes()
                    };
                    client.execute_mutation(&document, &jsonl).await
                }
            }
            .map_err(|error| Error::api(error.to_string()))?;
            let operation = if watch {
                let cancellation = Cancellation::default();
                client
                    .poll(
                        &BulkOperationId::parse(&operation.id)
                            .map_err(|error| Error::api(error.to_string()))?,
                        cfy_bulk::PollMode::default(),
                        &cancellation,
                    )
                    .await
                    .map_err(|error| Error::api(error.to_string()))?
            } else {
                operation
            };
            if watch
                && operation.status == BulkOperationStatus::Completed
                && operation.url.is_some()
            {
                let results = client
                    .download_jsonl(&operation)
                    .await
                    .map_err(|error| Error::api(error.to_string()))?;
                if let Some(path) = output_file {
                    write_atomic(&path, results.as_bytes()).map_err(|error| {
                        Error::with_source(
                            ErrorKind::Config,
                            format!("could not write bulk results {}", path.display()),
                            error,
                        )
                    })?;
                    output
                        .success(
                            "Bulk results written",
                            &serde_json::json!({"operation": operation, "output_file": path}),
                        )
                        .map_err(|error| Error::process(error.to_string()))?;
                } else {
                    output
                        .success(
                            std::str::from_utf8(results.as_bytes()).unwrap_or_default(),
                            &operation,
                        )
                        .map_err(|error| Error::process(error.to_string()))?;
                }
            } else {
                output
                    .success("Bulk operation", &operation)
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            Ok(0)
        }
        AppBulkCommand::Status { context, id } => {
            let client = app_bulk_client(context, None).await?;
            if let Some(id) = id {
                let id = BulkOperationId::parse(&id)
                    .map_err(|error| Error::invalid_input(error.to_string()))?;
                let operation = client
                    .status(&id)
                    .await
                    .map_err(|error| Error::api(error.to_string()))?;
                output
                    .success("Bulk operation status", &operation)
                    .map_err(|error| Error::process(error.to_string()))?;
            } else {
                let operations = client
                    .list_last_seven_days()
                    .await
                    .map_err(|error| Error::api(error.to_string()))?;
                output
                    .success("Bulk operations", &operations)
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            Ok(0)
        }
        AppBulkCommand::Cancel { context, id } => {
            let client = app_bulk_client(context, Some("2026-01")).await?;
            let id = BulkOperationId::parse(&id)
                .map_err(|error| Error::invalid_input(error.to_string()))?;
            let operation = client
                .cancel(&id)
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            output
                .success("Bulk operation cancellation requested", &operation)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
    }
}

async fn app_webhook_command(
    command: AppWebhookCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    let AppWebhookCommand::Trigger {
        config,
        auth_alias,
        client_id,
        path,
        reset,
        topic,
        api_version,
        delivery_method,
        client_secret,
        address,
    } = command;
    let selected = selected_app_environment(path, config, client_id, reset)?;
    let client_id = selected
        .document
        .get("client_id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| Error::invalid_input("selected app configuration has no client_id"))?;
    let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
    let session = authenticated_session(&identity).await?;
    let app_management = AppManagementClient::from_session(&session).await?;
    let remote_app = app_management.app_by_client_id(client_id).await?;
    let token = exchange_app_management_token(&session).await?;
    let webhook = WebhookClient::for_organization(&token, &remote_app.organization_id)?;

    let api_versions = webhook.api_versions().await?;
    let api_version = if let Some(api_version) = api_version {
        if !api_versions
            .iter()
            .any(|candidate| candidate == &api_version)
        {
            return Err(Error::invalid_input(format!(
                "webhook API version `{api_version}` is not available"
            )));
        }
        api_version
    } else {
        if non_interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(Error::invalid_input(
                "--api-version is required in non-interactive mode",
            ));
        }
        let index = select_text_choice("Which API version would you like to use?", &api_versions)?;
        api_versions[index].clone()
    };

    let topics = webhook.topics(&api_version).await?;
    let topic = if let Some(topic) = topic {
        let normalized = topic.to_ascii_lowercase().replace('_', "/");
        topics
            .iter()
            .find(|candidate| candidate.to_ascii_lowercase() == normalized)
            .cloned()
            .ok_or_else(|| {
                Error::invalid_input(format!(
                    "webhook topic `{topic}` is not available for API version `{api_version}`"
                ))
            })?
    } else {
        if non_interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(Error::invalid_input(
                "--topic is required in non-interactive mode",
            ));
        }
        let index = select_text_choice("Which webhook topic would you like to trigger?", &topics)?;
        topics[index].clone()
    };

    let address = required_interactive_value(address, "Webhook address", non_interactive)?;
    let delivery_method = resolve_delivery_method(&address, delivery_method.map(Into::into))?;
    let credentials = if let Some(client_secret) = client_secret {
        (client_id.to_owned(), Secret::new(client_secret))
    } else {
        let credentials = app_management.app_client_credentials(client_id).await?;
        (credentials.client_id, credentials.client_secret)
    };
    let sample = webhook
        .trigger(
            &topic,
            &api_version,
            &address,
            delivery_method,
            &credentials.1,
            (delivery_method == WebhookDeliveryMethod::EventBridge)
                .then_some(credentials.0.as_str()),
        )
        .await?;
    if !sample.success {
        return Err(Error::api(format!(
            "Shopify could not trigger the sample webhook: {}",
            sample.errors.join(", ")
        )));
    }
    let delivered_locally = if delivery_method == WebhookDeliveryMethod::Localhost {
        if !deliver_local_webhook(&address, &sample).await? {
            return Err(Error::api("localhost webhook delivery failed"));
        }
        true
    } else {
        false
    };
    output
        .success(
            if delivered_locally {
                "Localhost delivery successful"
            } else {
                "Webhook has been enqueued for delivery"
            },
            &serde_json::json!({
                "topic": topic,
                "api_version": api_version,
                "address": address,
                "delivery_method": delivery_method,
                "delivered_locally": delivered_locally,
            }),
        )
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

async fn app_command(command: AppCommand, non_interactive: bool, output: &Output) -> Result<u8> {
    match command {
        AppCommand::Build {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
            skip_dependencies_installation,
        } => {
            if skip_dependencies_installation {
                output.lifecycle(
                    "warning: --skip-dependencies-installation is deprecated; Catify never installs dependencies during app build",
                ).map_err(|error| Error::process(error.to_string()))?;
            }
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let graph = cfy_config::graph::AppConfigGraph::load_selected(
                &selected.project,
                &selected.config_path,
            )?;
            let report = build_app_graph(&graph).await?;
            output
                .success("App build completed", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::Deploy {
            config,
            auth_alias,
            client_id,
            path,
            reset,
            no_release,
            allow_updates,
            allow_deletes,
            no_build,
            message,
            version,
            source_control_url,
        } => {
            if non_interactive && !no_release && !allow_updates && !allow_deletes {
                return Err(Error::invalid_input(
                    "non-interactive deploy requires --allow-updates, --allow-deletes, or --no-release",
                ));
            }
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let client_id = selected
                .document
                .get("client_id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    Error::invalid_input("selected app configuration has no client_id")
                })?;
            let graph = cfy_config::graph::AppConfigGraph::load_selected(
                &selected.project,
                &selected.config_path,
            )?;
            let build = if no_build {
                let path = graph.root.join(".catify/deploy-bundle.tar.br");
                if !path.is_file() {
                    return Err(Error::config(
                        "--no-build requires an existing .catify/deploy-bundle.tar.br",
                    ));
                }
                cfy_build::BuildReport {
                    mode: "cached".into(),
                    skipped: Vec::new(),
                    artifacts: vec![cfy_build::Artifact {
                        extension: "complete-source".into(),
                        path,
                    }],
                    diagnostics: Vec::new(),
                }
            } else {
                let built = build_app_graph(&graph).await?;
                create_deploy_bundle(&graph, &built)?
            };
            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let credential_store = Arc::new(NativeCredentialStore::default());
            let identity_client = Arc::new(IdentityClient::new(
                HttpIdentityTransport::new()?,
                IdentityConfig::from_env(|key| env::var(key).ok())?,
            ));
            let sessions = cfy_auth::SessionManager::new(credential_store, identity_client);
            let session = sessions.session(&identity).await?.ok_or_else(|| {
                Error::api(format!(
                    "no authenticated session for `{identity}`; run `cfy auth login` first"
                ))
            })?;
            let app_management = AppManagementClient::from_session(&session).await?;
            let app = app_management.app_by_client_id(client_id).await?;
            let token = cfy_app::exchange_app_management_token(&session).await?;
            let endpoint = env::var("CFY_APP_MANAGEMENT_URL").unwrap_or_else(|_| {
                "https://app.shopify.com/app_management/unstable/graphql.json".into()
            });
            let backend = DeployBackend::new(&endpoint, token.expose())?;
            let report = deploy_app(
                &backend,
                &DeployOptions {
                    selection: Some(DeploySelection {
                        app: app.id,
                        environment: app.organization_id,
                    }),
                    non_interactive,
                    dry_run: false,
                    release: !no_release,
                    metadata: VersionMetadata {
                        version_tag: version,
                        message,
                        source_control_url,
                    },
                    upload_policy: Default::default(),
                },
                &build,
                &Cancellation::default(),
            )
            .await?;
            output
                .success("App deployed", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::Dev { args, command } => match command {
            Some(AppDevCommand::Clean {
                config,
                auth_alias: _,
                client_id,
                path,
                reset,
                store: _,
            }) => {
                let selected = selected_app_environment(path, config, client_id, reset)?;
                let state = selected.project.root().join(".catify/dev");
                if state.exists() {
                    std::fs::remove_dir_all(&state).map_err(|error| {
                        Error::with_source(
                            ErrorKind::Process,
                            format!("could not remove {}", state.display()),
                            error,
                        )
                    })?;
                }
                output
                    .success(
                        "Development state cleaned",
                        &serde_json::json!({"cleaned": true, "path": state}),
                    )
                    .map_err(|error| Error::process(error.to_string()))?;
                Ok(0)
            }
            None => app_dev(*args, output).await,
        },
        AppCommand::Init { destination } => {
            std::fs::create_dir_all(destination.join("extensions"))
                .map_err(|error| Error::api(format!("could not initialize app: {error}")))?;
            std::fs::create_dir_all(destination.join("web"))
                .map_err(|error| Error::api(format!("could not initialize app: {error}")))?;
            let marker = destination.join("shopify.app.toml");
            if !marker.exists() {
                std::fs::write(&marker, "# Catify app configuration\nname = \"my-app\"\n")
                    .map_err(|error| {
                        Error::api(format!("could not write {}: {error}", marker.display()))
                    })?;
            }
            output
                .success(
                    "App project initialized",
                    &serde_json::json!({"initialized": true}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::Info {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
            web_env,
        } => app_info(path, config, client_id, reset, web_env, output),
        AppCommand::Env { command } => app_env_command(command, output),
        AppCommand::Config { command } => {
            app_config_command(command, non_interactive, output).await
        }
        AppCommand::Function { args } => Err(backend_unavailable(
            "app function",
            25,
            format!(
                "the extension adapter exists but function dispatch is pending ({} forwarded argument(s)); use Shopify CLI for this command for now",
                args.len()
            ),
        )),
        AppCommand::Bulk { command } => app_bulk_command(command, output).await,
        AppCommand::Versions { command } => app_versions_command(command, output).await,
        AppCommand::Logs { args } => Err(backend_unavailable(
            "app logs",
            40,
            format!(
                "the streaming app logs backend is pending ({} forwarded argument(s)); use Shopify CLI for this command for now",
                args.len()
            ),
        )),
        AppCommand::Webhook { command } => {
            app_webhook_command(command, non_interactive, output).await
        }
        AppCommand::Execute {
            context,
            query,
            query_file,
            variables,
            variable_file,
            version,
            output_file,
        } => {
            let document = match (query, query_file) {
                (Some(query), None) => query,
                (None, Some(path)) => std::fs::read_to_string(&path).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        format!("could not read GraphQL document {}", path.display()),
                        error,
                    )
                })?,
                _ => {
                    return Err(Error::invalid_input(
                        "provide exactly one of --query or --query-file",
                    ));
                }
            };
            let variables = match (variables, variable_file) {
                (Some(variables), None) => serde_json::from_str(&variables).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        "--variables must contain a JSON object",
                        error,
                    )
                })?,
                (None, Some(path)) => {
                    let bytes = std::fs::read(&path).map_err(|error| {
                        Error::with_source(
                            ErrorKind::Config,
                            format!("could not read variables file {}", path.display()),
                            error,
                        )
                    })?;
                    serde_json::from_slice(&bytes).map_err(|error| {
                        Error::with_source(
                            ErrorKind::Config,
                            format!("variables file {} is not valid JSON", path.display()),
                            error,
                        )
                    })?
                }
                (None, None) => serde_json::json!({}),
                (Some(_), Some(_)) => unreachable!("clap rejects conflicting variables flags"),
            };
            if !variables.is_object() {
                return Err(Error::invalid_input(
                    "GraphQL variables must be a JSON object",
                ));
            }
            let client = app_bulk_client(context, version.as_deref()).await?;
            let result = client
                .execute_document(&document, variables)
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            if let Some(path) = output_file {
                let bytes = serde_json::to_vec_pretty(&result).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        "could not serialize GraphQL result",
                        error,
                    )
                })?;
                write_atomic(&path, &bytes).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        format!("could not write GraphQL result {}", path.display()),
                        error,
                    )
                })?;
                output
                    .success(
                        "GraphQL result written",
                        &serde_json::json!({"output_file": path, "data": result}),
                    )
                    .map_err(|error| Error::process(error.to_string()))?;
            } else {
                output
                    .success("GraphQL request completed", &result)
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            Ok(0)
        }
        AppCommand::Graphiql {
            context,
            port,
            variables,
            version,
        } => {
            if non_interactive || !io::stdin().is_terminal() {
                return Err(Error::invalid_input(
                    "app graphiql requires an interactive terminal; use `cfy app execute` for automation",
                ));
            }
            if let Some(value) = &variables {
                let parsed: serde_json::Value = serde_json::from_str(value).map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        "--variables must contain valid JSON",
                        error,
                    )
                })?;
                if !parsed.is_object() {
                    return Err(Error::invalid_input(
                        "GraphiQL variables must be a JSON object",
                    ));
                }
            }
            let client = app_bulk_client(context, version.as_deref()).await?;
            let server = GraphiqlServer::bind(client, port.unwrap_or(3457))
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            let url = server
                .url(variables.as_deref())
                .map_err(|error| Error::api(error.to_string()))?;
            output
                .success(
                    &format!("GraphiQL is running at {url}\nPress Ctrl+C to stop."),
                    &serde_json::json!({"url": url}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            if !open_browser(url.as_str()) {
                output
                    .lifecycle("Browser did not open automatically. Open the URL above manually.")
                    .map_err(|error| Error::process(error.to_string()))?;
            }
            let cancellation = Cancellation::default();
            tokio::select! {
                result = server.run(&cancellation) => result.map_err(|error| Error::api(error.to_string()))?,
                _ = tokio::signal::ctrl_c() => cancellation.cancel(),
            }
            Ok(0)
        }
        AppCommand::Release {
            config,
            auth_alias,
            client_id,
            path,
            reset,
            allow_updates,
            allow_deletes,
            version,
        } => {
            if non_interactive && !allow_updates && !allow_deletes {
                return Err(Error::invalid_input(
                    "app release requires --allow-updates or --allow-deletes in non-interactive mode",
                ));
            }
            if !non_interactive && !allow_updates && !allow_deletes {
                eprint!("Release app version `{version}`? [y/N] ");
                io::stderr().flush().ok();
                let mut answer = String::new();
                io::stdin().read_line(&mut answer).map_err(|error| {
                    Error::with_source(ErrorKind::Process, "could not read confirmation", error)
                })?;
                if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                    return Err(Error::invalid_input("app release was cancelled"));
                }
            }
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let client_id = selected
                .document
                .get("client_id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    Error::invalid_input("selected app configuration has no client_id")
                })?;
            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let store = Arc::new(NativeCredentialStore::default());
            let identity_client = Arc::new(IdentityClient::new(
                HttpIdentityTransport::new()?,
                IdentityConfig::from_env(|key| env::var(key).ok())?,
            ));
            let sessions = cfy_auth::SessionManager::new(Arc::clone(&store), identity_client);
            let session = sessions.session(&identity).await?.ok_or_else(|| {
                Error::new(
                    ErrorKind::Api,
                    format!("no authenticated session for `{identity}`; run `cfy auth login --identity {identity}` first"),
                )
            })?;
            let backend = AppManagementClient::from_session(&session).await?;
            let app = backend.app_by_client_id(client_id).await?;
            let report = backend.release_version(&app.id, &version).await?;
            output
                .success("Version released to users", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::ImportExtensions {
            config,
            auth_alias,
            client_id,
            path,
            reset,
        } => {
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let client_id = selected
                .document
                .get("client_id")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| {
                    Error::invalid_input("selected app configuration has no client_id")
                })?;
            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let session = authenticated_session(&identity).await?;
            let organizations = BusinessPlatformClient::from_session(&session)
                .await?
                .list_organizations()
                .await?;
            let organization = if organizations.len() == 1 {
                organizations[0].clone()
            } else if non_interactive {
                return Err(Error::invalid_input(
                    "multiple organizations are available; run interactively to select one",
                ));
            } else {
                select_organization(&organizations)?
            };
            let backend = AppManagementClient::from_session(&session).await?;
            let options = ImportExtensionsOptions {
                app_directory: selected.project.root().to_owned(),
                client_id: client_id.to_owned(),
                organization_id: organization.id,
                selection: ImportSelection::All,
                existing_directory_policy: ExistingDirectoryPolicy::Skip,
            };
            let report = import_extensions(&backend, &options)
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            output
                .success("Extensions imported", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::Generate {
            command:
                AppGenerateCommand::Extension {
                    config,
                    auth_alias: _,
                    client_id,
                    path,
                    reset,
                    template,
                    name,
                    flavor,
                },
        } => {
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let template =
                required_interactive_value(template, "Extension template", non_interactive)?;
            let name = required_interactive_value(name, "Extension name", non_interactive)?;
            let supervisor = Supervisor::default();
            let report = generate_extension(
                &supervisor,
                &GenerateExtensionOptions {
                    app_directory: selected.project.root().to_owned(),
                    name,
                    template,
                    flavor,
                    repository: env::var("CFY_EXTENSION_TEMPLATE_REPO").ok(),
                },
            )
            .await
            .map_err(|error| Error::process(error.to_string()))?;
            output
                .success("Extension generated", &report)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::ImportCustomDataDefinitions => Err(backend_unavailable(
            "app import-custom-data-definitions",
            40,
            "the remote custom-data definitions backend is pending; use Shopify CLI for this command for now",
        )),
    }
}

#[derive(Debug, Subcommand)]
pub enum AppEnvCommand {
    /// Display app and extension environment variables.
    Show {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
    },
    /// Pull app and extension environment variables into a dotenv file.
    Pull {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ENV_FILE")]
        env_file: Option<PathBuf>,
    },
}

fn app_env_command(command: AppEnvCommand, output: &Output) -> Result<u8> {
    match command {
        AppEnvCommand::Show {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
        } => {
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let values = app_environment(&selected);
            output
                .success(
                    "App environment",
                    &serde_json::json!({
                        "config": selected.config_name,
                        "config_path": selected.config_path,
                        "values": redact_app_environment(&values),
                        "remote_values_included": false,
                    }),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppEnvCommand::Pull {
            config,
            auth_alias: _,
            client_id,
            path,
            reset,
            env_file,
        } => {
            let selected = selected_app_environment(path, config, client_id, reset)?;
            let values = app_environment(&selected);
            let destination = env_file
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        selected.project.root().join(path)
                    }
                })
                .unwrap_or_else(|| selected.project.root().join(".env"));
            let existing = std::fs::read_to_string(&destination).unwrap_or_default();
            let contents = merge_dotenv(&existing, &values);
            write_atomic(&destination, contents.as_bytes()).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("could not write {}", destination.display()),
                    error,
                )
            })?;
            output
                .success(
                    "App environment written",
                    &serde_json::json!({
                        "config": selected.config_name,
                        "destination": destination,
                        "variables": values.len(),
                        "remote_values_included": false,
                    }),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AppVersionsCommand {
    /// List deployed versions of the selected app.
    List {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
    },
}

async fn app_versions_command(command: AppVersionsCommand, output: &Output) -> Result<u8> {
    let AppVersionsCommand::List {
        config,
        auth_alias,
        client_id,
        path,
        reset,
    } = command;
    let selected = selected_app_environment(path, config, client_id, reset)?;
    let client_id = selected
        .document
        .get("client_id")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| Error::invalid_input("selected app configuration has no client_id"))?;
    let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
    let store = Arc::new(NativeCredentialStore::default());
    let identity_client = Arc::new(IdentityClient::new(
        HttpIdentityTransport::new()?,
        IdentityConfig::from_env(|key| env::var(key).ok())?,
    ));
    let sessions = cfy_auth::SessionManager::new(Arc::clone(&store), identity_client);
    let session = sessions.session(&identity).await?.ok_or_else(|| {
        Error::new(
            ErrorKind::Api,
            format!("no authenticated session for `{identity}`; run `cfy auth login --identity {identity}` first"),
        )
    })?;
    let backend = AppManagementClient::from_session(&session).await?;
    let app = backend.app_by_client_id(client_id).await?;
    let report = backend.list_versions(&app.id).await?;
    output
        .success("App versions", &report)
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

#[derive(Debug, Subcommand)]
pub enum AppGenerateCommand {
    /// Generate a new app extension.
    #[command(disable_version_flag = true)]
    Extension {
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
        #[arg(short = 't', long, env = "SHOPIFY_FLAG_EXTENSION_TEMPLATE")]
        template: Option<String>,
        #[arg(short = 'n', long, env = "SHOPIFY_FLAG_NAME")]
        name: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_FLAVOR", value_parser = ["vanilla-js", "react", "typescript", "typescript-react", "wasm", "rust"])]
        flavor: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AppCommand {
    /// Build the app, including extensions.
    Build {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(long, env = "SHOPIFY_FLAG_SKIP_DEPENDENCIES_INSTALLATION")]
        skip_dependencies_installation: bool,
    },
    /// Build, upload, create, and optionally release an app version.
    #[command(disable_version_flag = true)]
    Deploy {
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
        #[arg(long, env = "SHOPIFY_FLAG_NO_RELEASE", conflicts_with_all = ["allow_updates", "allow_deletes"])]
        no_release: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_UPDATES")]
        allow_updates: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_DELETES")]
        allow_deletes: bool,
        #[arg(long, env = "SHOPIFY_FLAG_NO_BUILD")]
        no_build: bool,
        #[arg(long, env = "SHOPIFY_FLAG_MESSAGE")]
        message: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_SOURCE_CONTROL_URL")]
        source_control_url: Option<String>,
    },
    /// Run the app locally and watch its declared web processes.
    Dev {
        #[command(flatten)]
        args: Box<AppDevArgs>,
        #[command(subcommand)]
        command: Option<AppDevCommand>,
    },
    /// Print basic information about the app and its extensions.
    #[command(alias = "show")]
    Info {
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
        #[arg(long, env = "SHOPIFY_FLAG_OUTPUT_WEB_ENV")]
        web_env: bool,
    },
    /// Initialize a new app project.
    Init {
        #[arg(long, short = 'd', default_value = ".")]
        destination: PathBuf,
    },
    /// Manage app and extension environment variables.
    Env {
        #[command(subcommand)]
        command: AppEnvCommand,
    },
    /// Manage app configuration.
    Config {
        #[command(subcommand)]
        command: AppConfigCommand,
    },
    /// Build app functions.
    Function {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Execute and manage Admin API bulk operations.
    Bulk {
        #[command(subcommand)]
        command: AppBulkCommand,
    },
    /// Manage deployed app versions.
    Versions {
        #[command(subcommand)]
        command: AppVersionsCommand,
    },
    /// Show application logs.
    Logs {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Work with app webhooks.
    Webhook {
        #[command(subcommand)]
        command: AppWebhookCommand,
    },
    /// Execute an Admin API query for the app.
    #[command(disable_version_flag = true)]
    Execute {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(
            short = 'q',
            long,
            env = "SHOPIFY_FLAG_QUERY",
            conflicts_with = "query_file"
        )]
        query: Option<String>,
        #[arg(
            long,
            env = "SHOPIFY_FLAG_QUERY_FILE",
            required_unless_present = "query"
        )]
        query_file: Option<PathBuf>,
        #[arg(
            short = 'v',
            long,
            env = "SHOPIFY_FLAG_VARIABLES",
            conflicts_with = "variable_file"
        )]
        variables: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VARIABLE_FILE")]
        variable_file: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_OUTPUT_FILE")]
        output_file: Option<PathBuf>,
    },
    /// Open GraphiQL for the app.
    #[command(disable_version_flag = true)]
    Graphiql {
        #[command(flatten)]
        context: AppBulkContext,
        #[arg(long, env = "SHOPIFY_FLAG_PORT", value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
        #[arg(short = 'v', long, env = "SHOPIFY_FLAG_VARIABLES")]
        variables: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: Option<String>,
    },
    /// Release an app version.
    #[command(disable_version_flag = true)]
    Release {
        #[arg(short = 'c', long, env = "SHOPIFY_FLAG_APP_CONFIG")]
        config: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_CLIENT_ID")]
        client_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_FLAG_RESET")]
        reset: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_UPDATES")]
        allow_updates: bool,
        #[arg(long, env = "SHOPIFY_FLAG_ALLOW_DELETES")]
        allow_deletes: bool,
        #[arg(long, env = "SHOPIFY_FLAG_VERSION")]
        version: String,
    },
    /// Import dashboard-managed app extensions.
    #[command(disable_version_flag = true)]
    ImportExtensions {
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
    },
    /// Generate app resources.
    Generate {
        #[command(subcommand)]
        command: AppGenerateCommand,
    },
    /// Import custom data definitions.
    ImportCustomDataDefinitions,
}

#[derive(Debug, Subcommand)]
pub enum ThemeCommand {
    /// Analyze theme code using the official Shopify Theme Check engine
    Check(theme_check::ThemeCheckArgs),
    /// Create or reuse a development theme and continuously sync local changes.
    Dev {
        /// Existing numeric theme ID to reuse. Reused themes are never deleted.
        #[arg(long)]
        theme: Option<u64>,
        /// Store handle or myshopify.com domain.
        #[arg(long)]
        store: Option<String>,
        /// Theme directory. Defaults to the current directory.
        #[arg(long, short = 'd', default_value = ".")]
        source: PathBuf,
        /// Filesystem-event debounce window in milliseconds.
        #[arg(long, default_value_t = 200)]
        debounce_ms: u64,
    },
    /// List themes available on a store.
    List {
        /// Store handle or myshopify.com domain.
        #[arg(long)]
        store: Option<String>,
    },
    /// Download a theme's selected assets into a local directory.
    Pull {
        /// Numeric Shopify theme ID.
        #[arg(long)]
        theme: u64,
        /// Store handle or myshopify.com domain.
        #[arg(long)]
        store: Option<String>,
        /// Include matching asset paths (supports `*` and `?`); repeatable.
        #[arg(long)]
        include: Vec<String>,
        /// Exclude matching asset paths (supports `*` and `?`); repeatable.
        #[arg(long)]
        exclude: Vec<String>,
        /// Destination directory. Defaults to the current directory.
        #[arg(long, short = 'd', default_value = ".")]
        destination: PathBuf,
    },
    /// Upload changed and new assets to a theme.
    Push {
        /// Numeric Shopify theme ID.
        #[arg(long)]
        theme: u64,
        /// Store handle or myshopify.com domain.
        #[arg(long)]
        store: Option<String>,
        /// Theme directory. Defaults to the current directory.
        #[arg(long, short = 'd', default_value = ".")]
        source: PathBuf,
        /// Delete remote assets that do not exist locally.
        #[arg(long)]
        allow_delete: bool,
        /// Push to a live theme without confirmation.
        #[arg(long)]
        force: bool,
    },
    /// Open the theme in Shopify admin.
    #[command(disable_version_flag = true)]
    Open {
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(short = 'd', long, env = "SHOPIFY_FLAG_DEVELOPMENT")]
        development: bool,
        #[arg(short = 'E', long, env = "SHOPIFY_FLAG_EDITOR")]
        editor: bool,
        #[arg(short = 'e', long, env = "SHOPIFY_FLAG_ENVIRONMENT")]
        environment: Vec<String>,
        #[arg(short = 'l', long, env = "SHOPIFY_FLAG_LIVE")]
        live: bool,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_CLI_THEME_TOKEN")]
        password: Option<String>,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: Option<String>,
        #[arg(short = 't', long, env = "SHOPIFY_FLAG_THEME_ID")]
        theme: Option<String>,
    },
    /// Show theme metadata.
    Info {
        #[arg(long)]
        theme: u64,
        #[arg(long)]
        store: String,
    },
    /// Delete a theme. Requires --confirm.
    Delete {
        #[arg(long)]
        theme: u64,
        #[arg(long)]
        store: String,
        #[arg(long)]
        confirm: bool,
    },
    /// Duplicate a theme.
    Duplicate {
        #[arg(long)]
        theme: u64,
        #[arg(long)]
        store: String,
        #[arg(long)]
        name: String,
    },
    /// Rename a theme.
    Rename {
        #[arg(long)]
        theme: u64,
        #[arg(long)]
        store: String,
        #[arg(long)]
        name: String,
    },
    /// Publish a theme. Requires --confirm.
    Publish {
        #[arg(long)]
        theme: u64,
        #[arg(long)]
        store: String,
        #[arg(long)]
        confirm: bool,
    },
    /// Create a shareable preview link.
    Share {
        #[arg(long)]
        theme: u64,
        #[arg(long)]
        store: String,
    },
    /// Preview a theme locally or remotely.
    #[command(disable_version_flag = true)]
    Preview {
        #[arg(short = 't', long, env = "SHOPIFY_FLAG_THEME_ID", required = true)]
        theme: String,
        #[arg(long, env = "SHOPIFY_FLAG_OVERRIDES", required = true)]
        overrides: PathBuf,
        #[arg(long, env = "SHOPIFY_FLAG_PREVIEW_ID")]
        preview_id: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_OPEN")]
        open: bool,
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(long, env = "SHOPIFY_CLI_THEME_TOKEN")]
        password: Option<String>,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: Option<String>,
        #[arg(short = 'e', long, env = "SHOPIFY_FLAG_ENVIRONMENT", action = ArgAction::Append)]
        environment: Vec<String>,
    },
    /// Start the interactive theme console.
    Console {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Clone a Git repository as a starting point for a theme.
    Init {
        name: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH", default_value = ".")]
        path: PathBuf,
        #[arg(short = 'u', long, env = "SHOPIFY_FLAG_CLONE_URL", default_value = cfy_theme_init::SKELETON_THEME_URL)]
        clone_url: String,
        #[arg(short = 'l', long, env = "SHOPIFY_FLAG_LATEST")]
        latest: bool,
    },
    /// Package a theme directory into a zip archive.
    Package {
        #[arg(long, short = 'd', default_value = ".")]
        source: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Run the Theme Language Server adapter.
    LanguageServer {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Manage theme metafields.
    Metafields {
        #[command(subcommand)]
        command: ThemeMetafieldsCommand,
    },
    /// Profile theme operations.
    Profile {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum InternalCommand {
    /// Hold a minimal runtime open for idle RSS benchmarks.
    Idle {
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        /// Attach the native filesystem watcher to this directory while idle.
        #[arg(long)]
        watch: Option<PathBuf>,
    },
}

/// Execute a parsed command.
pub async fn run(cli: Cli, output: &Output) -> Result<u8> {
    match cli.command {
        Some(Command::Help { topic }) => print_help(topic.as_deref()),
        Some(Command::Commands {
            columns,
            extended,
            deprecated,
            hidden,
            no_truncate: _,
            sort,
            tree,
        }) => print_commands(columns, extended, hidden, deprecated, sort, tree, output)?,
        Some(Command::Version) => print_version(output)?,
        Some(Command::Upgrade) => upgrade(cli.global.non_interactive, output).await?,
        Some(Command::Completion { shell }) => print_completion(shell),
        Some(Command::Internal {
            command: InternalCommand::Idle { seconds, watch },
        }) => {
            let mut watcher = if let Some(path) = watch {
                let mut watcher = notify::recommended_watcher(|_| {}).map_err(|error| {
                    Error::api(format!("failed to create benchmark watcher: {error}"))
                })?;
                watcher
                    .watch(&path, RecursiveMode::Recursive)
                    .map_err(|error| {
                        Error::api(format!(
                            "failed to watch benchmark directory {}: {error}",
                            path.display()
                        ))
                    })?;
                Some(watcher)
            } else {
                None
            };
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            drop(watcher.take());
        }
        Some(Command::App { command }) => {
            return app_command(command, cli.global.non_interactive, output).await;
        }
        Some(Command::Auth { command }) => {
            return auth_command(command, cli.global.non_interactive, output).await;
        }
        Some(Command::Organization { command }) => {
            return organization_command(command, output).await;
        }
        Some(Command::Store { command }) => {
            return store_command(command, cli.global.non_interactive, output).await;
        }
        Some(Command::Plugins { command }) => {
            return plugins_command(command, cli.global.non_interactive, output).await;
        }
        Some(Command::Doc { command }) => {
            tokio::task::block_in_place(|| docs_command(command, output))?;
        }
        Some(Command::Search { query }) => {
            tokio::task::block_in_place(|| docs_command(DocCommand::Search { query }, output))?;
        }
        Some(Command::Config { command }) => {
            config_command(command, output)?;
        }
        Some(Command::Cache { command }) => {
            cache_command(command, output)?;
        }
        Some(Command::Doctor { command }) => {
            doctor_command(command, output)?;
        }
        Some(Command::Notification { command }) => {
            notification_command(command, output)?;
        }
        Some(Command::Hydrogen { args }) => {
            let code = run_hydrogen(&args).await?;
            return Ok(code as u8);
        }
        Some(Command::Theme {
            command: ThemeCommand::Check(args),
        }) => return theme_check::run(&args).await,
        Some(Command::Theme {
            command:
                ThemeCommand::Dev {
                    theme,
                    store,
                    source,
                    debounce_ms,
                },
        }) => theme_dev(theme, store.as_deref(), &source, debounce_ms, output).await?,
        Some(Command::Theme {
            command: ThemeCommand::List { store },
        }) => list_themes(store.as_deref(), output).await?,
        Some(Command::Theme {
            command:
                ThemeCommand::Pull {
                    theme,
                    store,
                    include,
                    exclude,
                    destination,
                },
        }) => {
            pull_theme(
                theme,
                store.as_deref(),
                &include,
                &exclude,
                &destination,
                output,
            )
            .await?
        }
        Some(Command::Theme {
            command:
                ThemeCommand::Push {
                    theme,
                    store,
                    source,
                    allow_delete,
                    force,
                },
        }) => {
            push_theme(
                theme,
                store.as_deref(),
                &source,
                allow_delete,
                force,
                cli.global.non_interactive,
                output,
            )
            .await?
        }
        Some(Command::Theme { command }) => {
            return theme_parity_command(command, cli.global.non_interactive, output).await;
        }
        None => {
            Cli::command()
                .print_help()
                .map_err(|error| Error::process(error.to_string()))?;
            println!();
        }
    }

    Ok(0)
}

fn print_version(output: &Output) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Version<'a> {
        name: &'a str,
        version: &'a str,
    }

    output
        .success(
            &format!("cfy {}", env!("CARGO_PKG_VERSION")),
            &Version {
                name: "cfy",
                version: env!("CARGO_PKG_VERSION"),
            },
        )
        .map_err(|error| {
            Error::with_source(
                cfy_core::ErrorKind::Process,
                "could not write output",
                error,
            )
        })
}

fn print_completion(shell: Shell) {
    let mut command = Cli::command();
    generate(shell, &mut command, "cfy", &mut io::stdout());
}

fn backend_unavailable(command: &str, issue: u32, detail: impl AsRef<str>) -> Error {
    Error::new(
        ErrorKind::Api,
        format!(
            "{command} is not available: {}. Tracking: https://github.com/yan-ad/catify/issues/{issue}",
            detail.as_ref()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, Command, ThemeCommand, corrected_command_args, filesystem_event, format_themes,
        live_push_requires_confirmation, reusable_session, select_store, select_theme_for_open,
        update_auth_selection, update_list_selection,
    };
    use cfy_api::theme::Theme;
    use cfy_auth::{Secret, Session};
    use cfy_config::project::Environment;
    use cfy_config::theme_dev::FileEvent;
    use clap::{CommandFactory, Parser, error::ErrorKind};
    use crossterm::event::KeyCode;
    use notify::{
        Event, EventKind,
        event::{ModifyKind, RenameMode},
    };

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn webhook_trigger_uses_the_upstream_nested_command_path() {
        assert!(
            Cli::try_parse_from([
                "cfy",
                "app",
                "webhook",
                "trigger",
                "--topic",
                "orders/create",
                "--api-version",
                "2025-07",
                "--delivery-method",
                "http",
                "--address",
                "https://example.test/webhook",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["cfy", "app", "webhook-trigger"]).is_err());
    }

    #[test]
    fn valid_sessions_are_reused_before_device_login() {
        let valid = Session {
            identity: "account@example.com".to_owned(),
            display_name: Some("account@example.com".to_owned()),
            access_token: Secret::new("access"),
            refresh_token: Secret::new("refresh"),
            expires_at_unix: 1_000,
            scopes: Vec::new(),
        };
        let expired = Session {
            display_name: Some("account@example.com".to_owned()),
            expires_at_unix: 100,
            ..valid.clone()
        };

        assert!(reusable_session(&valid, 900));
        assert!(!reusable_session(&expired, 900));
    }

    #[test]
    fn account_selector_supports_arrows_enter_and_cancel() {
        assert_eq!(
            update_auth_selection(0, KeyCode::Down).unwrap(),
            Some((1, false))
        );
        assert_eq!(
            update_auth_selection(1, KeyCode::Up).unwrap(),
            Some((0, false))
        );
        assert_eq!(
            update_auth_selection(1, KeyCode::Enter).unwrap(),
            Some((1, true))
        );
        assert!(update_auth_selection(0, KeyCode::Esc).is_err());
    }

    #[test]
    fn app_selector_wraps_and_confirms() {
        assert_eq!(
            update_list_selection(0, 3, KeyCode::Up).unwrap(),
            Some((2, false))
        );
        assert_eq!(
            update_list_selection(2, 3, KeyCode::Down).unwrap(),
            Some((0, false))
        );
        assert_eq!(
            update_list_selection(1, 3, KeyCode::Enter).unwrap(),
            Some((1, true))
        );
        assert!(update_list_selection(0, 3, KeyCode::Esc).is_err());
    }

    #[test]
    fn filesystem_rename_events_are_portable() {
        let both = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path("assets/old.css".into())
            .add_path("assets/new.css".into());
        assert_eq!(
            filesystem_event(both),
            vec![FileEvent::Rename {
                from: "assets/old.css".into(),
                to: "assets/new.css".into(),
            }]
        );

        let from = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
            .add_path("assets/old.css".into());
        assert_eq!(
            filesystem_event(from),
            vec![FileEvent::Remove("assets/old.css".into())]
        );

        let to = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::To)))
            .add_path("assets/new.css".into());
        assert_eq!(
            filesystem_event(to),
            vec![FileEvent::Upsert("assets/new.css".into())]
        );
    }

    #[test]
    fn root_help_matches_snapshot() {
        let help = Cli::command()
            .render_long_help()
            .to_string()
            .replace("\r\n", "\n")
            .replace("  -v, --verbose...\n          Increase diagnostic output; repeat for more detail", "  -v, --verbose...       Increase diagnostic output; repeat for more detail")
            .replace("\n\n      --no-color\n          Disable ANSI color output", "\n      --no-color         Disable ANSI color output")
            .replace("\n\n      --json\n          Emit machine-readable JSON when supported by the command", "\n      --json             Emit machine-readable JSON when supported by the command")
            .replace("\n\n      --non-interactive\n          Never prompt for interactive input", "\n      --non-interactive  Never prompt for interactive input")
            .replace("\n\n  -h, --help\n          Print help", "\n  -h, --help             Print help")
            .replace("\n\n  -V, --version\n          Print version", "\n  -V, --version          Print version");
        let snapshot = include_str!("../tests/snapshots/root-help.txt").replace("\r\n", "\n");
        assert_eq!(help, snapshot);
    }

    #[test]
    fn live_push_policy_rejects_non_interactive_without_force() {
        assert!(live_push_requires_confirmation(true, false, true).is_err());
        assert!(!live_push_requires_confirmation(true, true, true).unwrap());
        assert!(!live_push_requires_confirmation(false, false, true).unwrap());
        assert!(live_push_requires_confirmation(true, false, false).unwrap());
    }

    #[test]
    fn global_flags_are_accepted_after_nested_commands() {
        let cli = Cli::try_parse_from([
            "cfy",
            "app",
            "info",
            "--verbose",
            "--json",
            "--no-color",
            "--non-interactive",
        ])
        .expect("global flags should propagate");

        assert_eq!(cli.global.verbose, 1);
        assert!(cli.global.json);
        assert!(cli.global.no_color);
        assert!(cli.global.non_interactive);
    }

    #[test]
    fn command_and_nested_aliases_parse() {
        let cli = Cli::try_parse_from(["cfy", "a", "show"]).expect("aliases should parse");
        assert!(matches!(cli.command, Some(Command::App { .. })));
    }

    #[test]
    fn theme_list_parses_store_and_global_json_flag() {
        let cli =
            Cli::try_parse_from(["cfy", "theme", "list", "--store", "example", "--json"]).unwrap();

        assert!(cli.global.json);
        assert!(matches!(
            cli.command,
            Some(Command::Theme {
                command: ThemeCommand::List { store: Some(store) }
            }) if store == "example"
        ));
    }

    #[test]
    fn theme_pull_parses_filters_and_destination() {
        let cli = Cli::try_parse_from([
            "cfy",
            "theme",
            "pull",
            "--theme",
            "42",
            "--store",
            "example",
            "--include",
            "assets/*",
            "--exclude",
            "*.map",
            "--destination",
            "theme",
        ])
        .unwrap();
        let Some(Command::Theme {
            command:
                ThemeCommand::Pull {
                    theme,
                    store,
                    include,
                    exclude,
                    destination,
                },
        }) = cli.command
        else {
            panic!("expected theme pull")
        };
        assert_eq!(theme, 42);
        assert_eq!(store.as_deref(), Some("example"));
        assert_eq!(include, ["assets/*"]);
        assert_eq!(exclude, ["*.map"]);
        assert_eq!(destination, std::path::PathBuf::from("theme"));
    }

    #[test]
    fn human_theme_output_is_stable_and_complete() {
        let themes = vec![
            Theme {
                id: 10,
                name: "Dawn".to_owned(),
                role: "main".to_owned(),
                created_at: Some("2026-01-01".to_owned()),
                updated_at: Some("2026-01-02".to_owned()),
                previewable: Some(true),
                processing: Some(false),
            },
            Theme {
                id: 20,
                name: "Development".to_owned(),
                role: "development".to_owned(),
                created_at: None,
                updated_at: None,
                previewable: None,
                processing: None,
            },
        ];

        assert_eq!(
            format_themes(&themes),
            "10\tmain\tDawn\n20\tdevelopment\tDevelopment"
        );
        assert_eq!(format_themes(&[]), "No themes found.");
    }

    #[test]
    fn theme_open_resolves_id_name_live_and_development_without_prompting() {
        let themes = vec![
            Theme {
                id: 1,
                name: "Live".into(),
                role: "main".into(),
                created_at: None,
                updated_at: None,
                previewable: Some(true),
                processing: Some(false),
            },
            Theme {
                id: 2,
                name: "Development".into(),
                role: "development".into(),
                created_at: None,
                updated_at: None,
                previewable: Some(true),
                processing: Some(false),
            },
        ];
        assert_eq!(
            select_theme_for_open(&themes, Some("1"), false, false, true)
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            select_theme_for_open(&themes, Some("Development"), false, false, true)
                .unwrap()
                .id,
            2
        );
        assert_eq!(
            select_theme_for_open(&themes, None, false, true, true)
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            select_theme_for_open(&themes, None, true, false, true)
                .unwrap()
                .id,
            2
        );
        assert!(select_theme_for_open(&themes, None, false, false, true).is_err());
    }

    #[test]
    fn store_precedence_is_flag_then_environment_then_config() {
        let environment = Environment::from([
            (
                "CFY_STORE".to_owned(),
                "environment.myshopify.com".to_owned(),
            ),
            (
                "SHOPIFY_FLAG_STORE".to_owned(),
                "compatible.myshopify.com".to_owned(),
            ),
        ]);

        assert_eq!(
            select_store(
                Some("flag.myshopify.com"),
                &environment,
                Some("config.myshopify.com")
            )
            .unwrap(),
            "flag.myshopify.com"
        );
        assert_eq!(
            select_store(None, &environment, Some("config.myshopify.com")).unwrap(),
            "environment.myshopify.com"
        );
        assert_eq!(
            select_store(None, &Environment::new(), Some("config.myshopify.com")).unwrap(),
            "config.myshopify.com"
        );
        assert!(select_store(None, &Environment::new(), None).is_err());
    }

    #[test]
    fn unknown_command_suggests_a_valid_command() {
        let error = Cli::try_parse_from(["cfy", "versoin"]).expect_err("typo should fail");
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        assert!(error.to_string().contains("version"));
    }

    #[test]
    fn autocorrect_only_changes_unique_command_tokens() {
        let corrected = corrected_command_args(&[
            "cfy".into(),
            "config".into(),
            "autocorrect".into(),
            "statsu".into(),
        ])
        .unwrap();
        assert_eq!(corrected[3], "status");
        assert!(corrected_command_args(&["cfy".into(), "--json".into()]).is_none());
    }
}
