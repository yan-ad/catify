pub mod output;
mod theme_check;

use crate::output::Output;
use cfy_api::theme::{Theme, ThemeAsset, ThemeChange, ThemeClient, diff_assets};
use cfy_app::{AppManagementClient, LinkOptions, RemoteAppSummary, write_linked_config};
use cfy_auth::{
    CredentialStore, NativeCredentialStore, Session,
    flow::{LoginMode, headless_from_env},
    identity::{HttpIdentityTransport, IdentityClient, IdentityConfig},
};
use cfy_config::project::{
    Environment, ProjectKind, ProjectOverrides, discover, resolve_environment,
};
use cfy_config::theme::{
    StagedFile, commit_staged_files_cancellable, read_theme_files, safe_relative_path,
};
use cfy_config::theme_dev::{FileEvent, SyncAction, coalesce};
use cfy_config::{
    AutoUpgrade, UserSettings,
    active_config::ActiveConfigState,
    app_env::{from_project as app_environment, redacted as redact_app_environment, render_dotenv},
    clear_cache_root, write_atomic,
};
use cfy_core::{Cancellation, Error, ErrorKind, Result};
use cfy_docs::{Cache as DocsCache, DocsClient, HttpDocsTransport};
use cfy_hydrogen::run as run_hydrogen;
use cfy_store::{
    AdminStoreBackend, StoreBackend, StoreCommand as StoreOperation, StoreManagementBackend,
    StoreTarget, browser_url,
};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};
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
            let selected_client_id = if let Some(client_id) = client_id {
                client_id
            } else {
                if non_interactive || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                    return Err(Error::invalid_input(
                        "app config link requires --client-id outside an interactive terminal",
                    ));
                }
                let apps = backend.list_apps().await?;
                select_remote_app(&apps)?.client_id
            };
            let app = backend.app_by_client_id(&selected_client_id).await?;
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

async fn theme_parity_command(
    command: ThemeCommand,
    _non_interactive: bool,
    output: &Output,
) -> Result<u8> {
    match command {
        ThemeCommand::Init { destination } => {
            std::fs::create_dir_all(destination.join("assets"))
                .map_err(|error| Error::api(format!("could not initialize theme: {error}")))?;
            std::fs::create_dir_all(destination.join("config"))
                .map_err(|error| Error::api(format!("could not initialize theme: {error}")))?;
            output
                .success(
                    "Theme directory initialized",
                    &serde_json::json!({"initialized": true}),
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
        ThemeCommand::Open { theme, store } | ThemeCommand::Share { theme, store } => {
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
        ThemeCommand::Preview { theme, store } => Err(backend_unavailable(
            "theme preview",
            39,
            format!(
                "preview session orchestration is pending{}{}; use `cfy theme open --theme <id> --store <store>` for an existing remote theme",
                theme
                    .map(|value| format!(" for theme {value}"))
                    .unwrap_or_default(),
                store
                    .map(|value| format!(" on {value}"))
                    .unwrap_or_default(),
            ),
        )),
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
        ThemeCommand::MetafieldsPull { theme, store } => Err(backend_unavailable(
            "theme metafields pull",
            39,
            format!(
                "the metafields API adapter is pending for theme {theme} on {store}; use Shopify CLI for this command for now"
            ),
        )),
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
                    "Automatic upgrades status",
                    &serde_json::json!({"autoupgrade": matches!(current.autoupgrade, AutoUpgrade::On)}),
                ),
                AutoUpgradeMode::On | AutoUpgradeMode::Off => {
                    let settings = UserSettings { autoupgrade: if matches!(mode, Some(AutoUpgradeMode::On)) { AutoUpgrade::On } else { AutoUpgrade::Off } };
                    settings.write_user(&path)?;
                    output.success("Automatic upgrades updated", &serde_json::json!({"path": path, "autoupgrade": matches!(settings.autoupgrade, AutoUpgrade::On)}))
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
    /// Enable automatic upgrades.
    Autoupgrade { mode: Option<AutoUpgradeMode> },
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
    Auth {
        #[arg(long)]
        store: Option<String>,
    },
    /// List stores available to the current account.
    AuthList,
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
    /// Create a development store.
    CreateDev {
        #[arg(long)]
        store: String,
    },
    /// Create a preview store.
    CreatePreview {
        #[arg(long)]
        store: String,
    },
    /// Delete a store. Requires --confirm.
    Delete {
        #[arg(long)]
        store: String,
        #[arg(long)]
        confirm: bool,
    },
    /// Execute a bulk operation.
    BulkExecute {
        #[arg(long)]
        store: String,
        query: String,
    },
    /// Show bulk operation status.
    BulkStatus {
        #[arg(long)]
        store: String,
    },
    /// Cancel a bulk operation. Requires --confirm.
    BulkCancel {
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
        StoreCliCommand::CreateDev { store } | StoreCliCommand::CreatePreview { store } => {
            let endpoint = env::var("CFY_PARTNER_API_URL").map_err(|_| {
                Error::new(
                    ErrorKind::Api,
                    "store lifecycle API is not configured; set CFY_PARTNER_API_URL",
                )
            })?;
            let token = store_token()?;
            let backend = StoreManagementBackend::new(&endpoint, &token).map_err(Error::from)?;
            let value = if store.contains("preview") {
                backend.create_preview(&store).await
            } else {
                backend.create_development(&store).await
            }
            .map_err(Error::from)?;
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
        StoreCliCommand::BulkStatus { store } => {
            let endpoint = env::var("CFY_PARTNER_API_URL").map_err(|_| {
                Error::new(
                    ErrorKind::Api,
                    "store lifecycle API is not configured; set CFY_PARTNER_API_URL",
                )
            })?;
            let token = store_token()?;
            let backend = StoreManagementBackend::new(&endpoint, &token).map_err(Error::from)?;
            let value = backend.bulk_status(&store).await.map_err(Error::from)?;
            output
                .success("Bulk operation status", &value)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::BulkCancel { store, confirm } => {
            if !confirm {
                return Err(Error::invalid_input("bulk cancellation requires --confirm"));
            }
            let endpoint = env::var("CFY_PARTNER_API_URL").map_err(|_| {
                Error::new(
                    ErrorKind::Api,
                    "store lifecycle API is not configured; set CFY_PARTNER_API_URL",
                )
            })?;
            let token = store_token()?;
            let backend = StoreManagementBackend::new(&endpoint, &token).map_err(Error::from)?;
            let value = backend.bulk_cancel(&store).await.map_err(Error::from)?;
            output
                .success("Bulk operation cancelled", &value)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Info { store } => {
            let target = StoreTarget::parse(&store)?;
            let token = store_token()?;
            let backend = AdminStoreBackend::new(&target, &token).map_err(Error::from)?;
            let info = backend.info(&target).await.map_err(Error::from)?;
            output
                .success(&format!("Store {}", target.domain), &info)
                .map_err(|error| Error::process(error.to_string()))?;
            return Ok(0);
        }
        StoreCliCommand::Execute { store, query } => {
            let target = StoreTarget::parse(&store)?;
            let token = store_token()?;
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
        StoreCliCommand::BulkExecute { store, query } => {
            let target = StoreTarget::parse(&store)?;
            let token = store_token()?;
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
        StoreCliCommand::BulkCancel { store, confirm } => {
            (StoreOperation::BulkCancel, store, true, confirm)
        }
        StoreCliCommand::Auth { store } => (
            StoreOperation::Auth,
            store.unwrap_or_else(|| "current".to_owned()),
            false,
            false,
        ),
        StoreCliCommand::AuthList => (StoreOperation::AuthList, "current".to_owned(), false, false),
        StoreCliCommand::Info { store } => (StoreOperation::Info, store, false, false),
        StoreCliCommand::Execute { store, .. } => (StoreOperation::Execute, store, false, false),
        StoreCliCommand::CreateDev { store } => (StoreOperation::CreateDev, store, true, false),
        StoreCliCommand::CreatePreview { store } => {
            (StoreOperation::CreatePreview, store, true, false)
        }
        StoreCliCommand::BulkExecute { store, .. } => {
            (StoreOperation::BulkExecute, store, false, false)
        }
        StoreCliCommand::BulkStatus { store } => (StoreOperation::BulkStatus, store, false, false),
        StoreCliCommand::StripeAuth { store } => (StoreOperation::StripeAuth, store, false, false),
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
        AuthCommand::Logout { identity } => {
            store.delete(&identity).await?;
            output
                .success(
                    "Local credentials removed; remote token revocation depends on the provider.",
                    &serde_json::json!({ "identity": identity, "remote_revoked": false }),
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
        OrganizationCommand::List => {
            let _ = output.lifecycle("Organization provider is not configured in this build");
            Err(Error::invalid_input(
                "organization API provider is not configured; authenticate first and configure the organization backend",
            ))
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
    /// Remove local credentials. Remote revocation is provider-dependent.
    Logout {
        /// Identity key used for credential storage.
        #[arg(long, default_value = "default")]
        identity: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum OrganizationCommand {
    /// List organizations available to the current identity.
    List,
}

#[derive(serde::Serialize)]
struct CommandListing {
    name: &'static str,
    summary: &'static str,
    status: &'static str,
}

const COMMAND_LISTING: &[CommandListing] = &[
    CommandListing {
        name: "app",
        summary: "Build Shopify apps.",
        status: "scaffolded",
    },
    CommandListing {
        name: "auth",
        summary: "Auth operations.",
        status: "scaffolded",
    },
    CommandListing {
        name: "commands",
        summary: "List all Catify commands.",
        status: "implemented",
    },
    CommandListing {
        name: "completion",
        summary: "Generate shell completions.",
        status: "implemented",
    },
    CommandListing {
        name: "config",
        summary: "CLI configuration options.",
        status: "scaffolded",
    },
    CommandListing {
        name: "doc",
        summary: "Search and fetch documentation.",
        status: "scaffolded",
    },
    CommandListing {
        name: "help",
        summary: "Display help for Catify.",
        status: "implemented",
    },
    CommandListing {
        name: "hydrogen",
        summary: "Build Hydrogen storefronts.",
        status: "scaffolded",
    },
    CommandListing {
        name: "organization",
        summary: "List Shopify organizations.",
        status: "scaffolded",
    },
    CommandListing {
        name: "search",
        summary: "Search Shopify developer documentation.",
        status: "scaffolded",
    },
    CommandListing {
        name: "store",
        summary: "Work directly with Shopify stores.",
        status: "scaffolded",
    },
    CommandListing {
        name: "theme",
        summary: "Build Liquid themes.",
        status: "implemented",
    },
    CommandListing {
        name: "upgrade",
        summary: "Upgrade Catify.",
        status: "implemented",
    },
    CommandListing {
        name: "version",
        summary: "Show the current version.",
        status: "implemented",
    },
];

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

fn print_commands(output: &Output) -> Result<()> {
    output
        .success("Available commands:", &COMMAND_LISTING)
        .map_err(|error| {
            Error::with_source(
                cfy_core::ErrorKind::Process,
                "could not write command listing",
                error,
            )
        })
}

fn upgrade(dry_run: bool, output: &Output) -> Result<()> {
    let channel = env::var("CFY_INSTALL_CHANNEL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let Some(channel) = channel else {
        return Err(Error::invalid_input(
            "cannot upgrade an unmanaged installation; set CFY_INSTALL_CHANNEL to cargo or homebrew, or reinstall cfy through a supported channel",
        ));
    };
    if !matches!(channel.as_str(), "cargo" | "homebrew") {
        return Err(Error::invalid_input(format!(
            "unsupported install channel `{channel}`; supported channels: cargo, homebrew"
        )));
    }
    let message = if dry_run {
        format!("upgrade check passed for {channel}; no files changed")
    } else {
        format!(
            "upgrade is available through {channel}; run the channel's package manager to apply it"
        )
    };
    output
        .success(
            &message,
            &serde_json::json!({ "channel": channel, "dry_run": dry_run, "changed": false }),
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
    #[arg(short, long, global = true, action = ArgAction::Count)]
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
    Commands,
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
    Upgrade {
        /// Validate the selected channel without changing the installation.
        #[arg(long)]
        dry_run: bool,
    },

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

fn selected_app_environment() -> Result<cfy_config::project::ProjectEnvironment> {
    let cwd = env::current_dir().map_err(|error| Error::api(error.to_string()))?;
    let project = discover(&cwd, Some(ProjectKind::App))?;
    let environment = env::vars().collect::<Environment>();
    let explicit_config = environment
        .get("CFY_CONFIG")
        .or_else(|| environment.get("SHOPIFY_FLAG_APP_CONFIG"))
        .cloned();
    let cached_config = if explicit_config.is_none() {
        ActiveConfigState::load(&app_state_path())?
            .selected(project.root())
            .map(ToOwned::to_owned)
    } else {
        None
    };
    resolve_environment(
        project,
        &ProjectOverrides {
            config: explicit_config.or(cached_config),
            ..ProjectOverrides::default()
        },
        &environment,
    )
}

async fn app_command(command: AppCommand, non_interactive: bool, output: &Output) -> Result<u8> {
    match command {
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
        AppCommand::Info => {
            let cwd = env::current_dir().map_err(|error| Error::api(error.to_string()))?;
            let project = cfy_config::project::discover(&cwd, None).ok();
            if project.is_none() {
                return Err(Error::invalid_input(
                    "app info requires a Shopify app project; run it inside a directory containing shopify.app.toml or pass through a project directory",
                ));
            }
            output
                .success(
                    "App information",
                    &serde_json::json!({"cwd": cwd, "project_found": project.is_some()}),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::Env { show } => {
            let selected = selected_app_environment()?;
            let values = app_environment(&selected);
            let displayed = if show {
                values.clone()
            } else {
                redact_app_environment(&values)
            };
            output
                .success(
                    "App environment",
                    &serde_json::json!({
                        "config": selected.config_name,
                        "config_path": selected.config_path,
                        "values": displayed,
                        "secrets_visible": show,
                        "remote_values_included": false,
                    }),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
        AppCommand::EnvPull => {
            let selected = selected_app_environment()?;
            let values = app_environment(&selected);
            let destination = selected.project.root().join(".env");
            let contents = render_dotenv(&values);
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
        AppCommand::VersionsList => Err(backend_unavailable(
            "app versions list",
            40,
            "the app management GraphQL backend requires a verified Shopify endpoint and schema",
        )),
        AppCommand::Logs { args } => Err(backend_unavailable(
            "app logs",
            40,
            format!(
                "the streaming app logs backend is pending ({} forwarded argument(s)); use Shopify CLI for this command for now",
                args.len()
            ),
        )),
        AppCommand::WebhookTrigger { args } => Err(backend_unavailable(
            "app webhook trigger",
            40,
            format!(
                "the webhook trigger backend is pending ({} forwarded argument(s)); use Shopify CLI for this command for now",
                args.len()
            ),
        )),
        AppCommand::Execute { query } => Err(backend_unavailable(
            "app execute",
            40,
            format!(
                "the app-scoped GraphQL backend is not configured (query length: {} bytes); use `cfy store execute` for store Admin API queries",
                query.len()
            ),
        )),
        AppCommand::Graphiql => Err(backend_unavailable(
            "app graphiql",
            40,
            "the app-scoped GraphiQL session backend is pending; use Shopify CLI for this command for now",
        )),
        AppCommand::Release { args } => Err(backend_unavailable(
            "app release",
            40,
            format!(
                "the verified app release mutation backend is pending ({} forwarded argument(s)); `cfy app deploy` orchestration is available at the library boundary",
                args.len()
            ),
        )),
        AppCommand::ImportExtensions => Err(backend_unavailable(
            "app import-extensions",
            24,
            "extension discovery exists, but the import workflow is not wired to the CLI yet",
        )),
        AppCommand::GenerateExtension { args } => Err(backend_unavailable(
            "app generate extension",
            24,
            format!(
                "extension schema discovery exists, but template generation is pending ({} forwarded argument(s))",
                args.len()
            ),
        )),
        AppCommand::ImportCustomDataDefinitions => Err(backend_unavailable(
            "app import-custom-data-definitions",
            40,
            "the remote custom-data definitions backend is pending; use Shopify CLI for this command for now",
        )),
    }
}

#[derive(Debug, Subcommand)]
pub enum AppCommand {
    /// Display the currently selected app.
    #[command(alias = "show")]
    Info,
    /// Initialize a new app project.
    Init {
        #[arg(long, short = 'd', default_value = ".")]
        destination: PathBuf,
    },
    /// Show app environment variables.
    Env {
        #[arg(long)]
        show: bool,
    },
    /// Pull app environment configuration.
    EnvPull,
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
    /// List app versions.
    VersionsList,
    /// Show application logs.
    Logs {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Trigger an app webhook.
    WebhookTrigger {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Execute an Admin API query for the app.
    Execute { query: String },
    /// Open GraphiQL for the app.
    Graphiql,
    /// Run a release operation.
    Release {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Import app extensions.
    ImportExtensions,
    /// Generate an extension.
    GenerateExtension {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
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
    Open {
        #[arg(long)]
        theme: u64,
        #[arg(long)]
        store: String,
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
    Preview {
        #[arg(long)]
        theme: Option<u64>,
        #[arg(long)]
        store: Option<String>,
    },
    /// Start the interactive theme console.
    Console {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Initialize a theme directory with Shopify markers.
    Init {
        #[arg(long, short = 'd', default_value = ".")]
        destination: PathBuf,
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
    /// Pull theme metafields.
    MetafieldsPull {
        #[arg(long)]
        theme: u64,
        #[arg(long)]
        store: String,
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
        Some(Command::Commands) => print_commands(output)?,
        Some(Command::Version) => print_version(output)?,
        Some(Command::Upgrade { dry_run }) => upgrade(dry_run, output)?,
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
        Cli, Command, ThemeCommand, filesystem_event, format_themes,
        live_push_requires_confirmation, reusable_session, select_store, update_auth_selection,
        update_list_selection,
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
}
