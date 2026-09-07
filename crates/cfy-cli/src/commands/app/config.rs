use super::super::super::{
    AuthTerminalGuard, output::Output, select_organization, update_list_selection,
};
use cfy_app::{
    AppManagementClient, BusinessPlatformClient, LinkOptions, RemoteAppSummary, write_linked_config,
};
use cfy_auth::{
    NativeCredentialStore,
    identity::{HttpIdentityTransport, IdentityClient, IdentityConfig},
};
use cfy_config::{
    active_config::ActiveConfigState,
    project::{ProjectKind, discover},
};
use cfy_core::{Error, ErrorKind, Result};
use clap::Subcommand;
use crossterm::{
    cursor,
    event::{self, Event, KeyEventKind},
    execute,
    terminal::enable_raw_mode,
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use std::{
    env,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    sync::Arc,
};

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

pub(super) fn app_state_path() -> PathBuf {
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
pub(super) struct LocalAppConfig {
    path: PathBuf,
    pub(super) file_name: String,
    pub(super) client_id: String,
}

pub(super) fn load_local_app_configs(
    project: &cfy_config::project::Project,
) -> Result<Vec<LocalAppConfig>> {
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

pub(super) async fn app_config_command(
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
