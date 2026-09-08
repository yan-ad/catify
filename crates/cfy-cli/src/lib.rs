mod commands;
pub mod output;
mod theme_check;
mod update_check;

pub use commands::{
    AppBulkCommand, AppBulkContext, AppCommand, AppConfigCommand, AppDevArgs, AppDevCommand,
    AppEnvCommand, AppFunctionCommand, AppFunctionContext, AppGenerateCommand, AppLogStatusArg,
    AppLogsCommand, AppVersionsCommand, AppWebhookCommand, AuthCommand, AutoCorrectCommand,
    AutoUpgradeMode, CacheCommand, CommandColumn, CommandSort, ConfigCommand, DocCommand,
    NotificationCommand, OrganizationCommand, PluginsCommand, StoreAuthCommand, StoreBulkCommand,
    StoreCliCommand, StoreCreateCommand, WebhookDeliveryMethodArg,
};
pub use update_check::{
    is_update_check, maybe_notify_and_refresh, refresh as refresh_update_check,
};

#[cfg(test)]
use crate::commands::{reusable_session, update_auth_selection};
use crate::{
    commands::{
        app_command, auth_command, cache_command, config_command, config_path, docs_command,
        notification_command, organization_command, plugins_command, print_commands, print_help,
        store_command,
    },
    output::Output,
};
use cfy_api::{
    theme::{Theme, ThemeAsset, ThemeChange, ThemeClient, diff_assets},
    theme_profile::{LiquidEvaluation, ThemeProfiler},
};
use cfy_app::{RemoteOrganization, exchange_admin_token, exchange_storefront_renderer_token};
use cfy_auth::{
    NativeCredentialStore, Secret, Session,
    identity::{HttpIdentityTransport, IdentityClient, IdentityConfig},
};
use cfy_bulk::{BulkClient, StoreDomain as BulkStoreDomain, resolve_api_version};
use cfy_config::project::{
    Environment, ProjectKind, ProjectOverrides, discover, resolve_environment,
};
use cfy_config::theme::{
    StagedFile, commit_staged_files_cancellable, read_theme_files, read_theme_files_for_listing,
    safe_relative_path,
};
use cfy_config::theme_dev::{FileEvent, SyncAction, coalesce};
use cfy_config::{AutoCorrect, UserSettings, write_atomic};
use cfy_core::{Cancellation, Error, ErrorKind, Result};
use cfy_hydrogen::run as run_hydrogen;
use cfy_process::Supervisor;
use cfy_store::{AdminStoreBackend, StoreTarget, store_auth::StoreAuthRegistry};
use cfy_theme_init::{ThemeInitRequest, initialize as initialize_theme};
use cfy_upgrade::{
    ExecutionPolicy, detect as detect_upgrade, execute as execute_upgrade, execute_standalone,
    plan as plan_upgrade,
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
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Paragraph},
};
use std::{
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use zip::write::SimpleFileOptions;

pub(crate) const SHOPIFY_API_VERSION: &str = "2026-07";

pub(crate) struct AbortOnDrop(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn explicit_theme_tokens(
    password: Option<String>,
    environment_password: Option<String>,
) -> Result<Option<(Secret, Secret)>> {
    let Some(token) = password.or(environment_password) else {
        return Ok(None);
    };
    if token.is_empty() {
        return Err(Error::invalid_input("theme password cannot be empty"));
    }
    Ok(Some((Secret::new(token.clone()), Secret::new(token))))
}

fn theme_environment_path(root: &Path, name: &str) -> Result<Option<PathBuf>> {
    let path = root.join("shopify.theme.toml");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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
    Ok(document
        .get("environments")
        .and_then(|environments| environments.get(name))
        .and_then(|environment| environment.get("path"))
        .and_then(toml::Value::as_str)
        .map(|value| {
            let value = PathBuf::from(value);
            if value.is_absolute() {
                value
            } else {
                root.join(value)
            }
        }))
}

fn random_share_name() -> Result<String> {
    const ADJECTIVES: &[&str] = &["Brisk", "Bright", "Calm", "Clever", "Vivid", "Warm"];
    const NOUNS: &[&str] = &["Comet", "Harbor", "Maple", "Orbit", "Panda", "Willow"];
    let mut bytes = [0_u8; 4];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::process(format!("could not generate theme name: {error}")))?;
    Ok(format!(
        "{} {} {:02x}{:02x}",
        ADJECTIVES[usize::from(bytes[0]) % ADJECTIVES.len()],
        NOUNS[usize::from(bytes[1]) % NOUNS.len()],
        bytes[2],
        bytes[3],
    ))
}

struct ShareThemeArgs {
    auth_alias: Option<String>,
    environments: Vec<String>,
    force: bool,
    listing: Option<String>,
    explicit_password: Option<String>,
    explicit_path: Option<PathBuf>,
    explicit_store: Option<String>,
}

async fn share_theme(args: ShareThemeArgs, output: &Output) -> Result<u8> {
    let current = env::current_dir().map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not resolve current directory",
            error,
        )
    })?;
    let base_root = args.explicit_path.as_deref().unwrap_or(&current);
    let requested = if args.environments.is_empty() {
        vec![None]
    } else {
        args.environments.iter().map(Some).collect()
    };
    let cancellation = Cancellation::default();
    let signal = cancellation.clone();
    let _signal_task = AbortOnDrop(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    }));
    let mut results = Vec::new();
    for environment_name in requested {
        let environment_values = match environment_name {
            Some(name) => theme_environment_credentials(base_root, std::slice::from_ref(name))?,
            None => theme_environment_credentials(base_root, &[])?,
        };
        let root = match environment_name {
            Some(name) => {
                theme_environment_path(base_root, name)?.unwrap_or_else(|| base_root.to_path_buf())
            }
            None => theme_environment_path(base_root, "default")?
                .unwrap_or_else(|| base_root.to_path_buf()),
        };
        let recognized = [
            "assets",
            "config",
            "layout",
            "locales",
            "sections",
            "snippets",
            "templates",
        ]
        .iter()
        .any(|directory| root.join(directory).is_dir());
        if !recognized && !args.force {
            return Err(Error::invalid_input(format!(
                "{} does not look like a Shopify theme; pass --force to share it anyway",
                root.display()
            )));
        }
        let store = resolve_store(
            args.explicit_store
                .as_deref()
                .or(environment_values.0.as_deref()),
        )?;
        let password = args.explicit_password.clone().or(environment_values.1);
        let token = if let Some(password) = password {
            password
        } else {
            let identity = args
                .auth_alias
                .clone()
                .unwrap_or_else(|| "default".to_owned());
            let session = authenticated_session(&identity).await?;
            exchange_admin_token(&session, &store)
                .await?
                .expose()
                .to_owned()
        };
        let local =
            read_theme_files_for_listing(&root, args.listing.as_deref()).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("could not read theme files from {}", root.display()),
                    error,
                )
            })?;
        let changes = local
            .into_iter()
            .map(|(key, contents)| ThemeChange::Upload(ThemeAsset { key, contents }))
            .collect::<Vec<_>>();
        let client = ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
        let name = random_share_name()?;
        let shared = client
            .share(&name, &changes, &cancellation)
            .await
            .map_err(Error::from)?;
        let theme = shared.theme;
        let summary = shared.summary;
        let preview_url = client.preview_url(theme.id);
        let editor_url = format!("https://{store}/admin/themes/{}/editor", theme.id);
        results.push(serde_json::json!({
            "environment": environment_name,
            "theme": theme,
            "preview_url": preview_url,
            "editor_url": editor_url,
            "uploaded": summary.uploaded,
        }));
    }
    let human = results
        .iter()
        .map(|result| {
            format!(
                "{}\nPreview: {}\nEditor: {}",
                result["theme"]["name"].as_str().unwrap_or("Shared theme"),
                result["preview_url"].as_str().unwrap_or_default(),
                result["editor_url"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    output
        .success(&human, &results)
        .map_err(|error| Error::process(error.to_string()))?;
    Ok(0)
}

pub(crate) async fn store_access_token(store: &str) -> Result<String> {
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

pub(crate) async fn store_bulk_client(store: &str, version: Option<&str>) -> Result<BulkClient> {
    let domain =
        BulkStoreDomain::parse(store).map_err(|error| Error::invalid_input(error.to_string()))?;
    let token = store_access_token(domain.as_str()).await?;
    let version = resolve_api_version(&domain, version)
        .await
        .map_err(|error| Error::api(error.to_string()))?;
    BulkClient::new(&domain, &version, &cfy_bulk::Secret::new(token))
        .map_err(|error| Error::api(error.to_string()))
}

pub(crate) async fn authenticated_session(identity: &str) -> Result<Session> {
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

pub(crate) fn select_text_choice(title: &str, choices: &[String]) -> Result<usize> {
    select_text_choice_with_shortcuts(title, choices, &[])
}

pub(crate) fn select_text_choice_with_shortcuts(
    title: &str,
    choices: &[String],
    shortcuts: &[(char, usize)],
) -> Result<usize> {
    enable_raw_mode().map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not enable selector", error)
    })?;
    let _guard = AuthTerminalGuard;
    execute!(io::stderr(), cursor::Hide).map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not initialize selector", error)
    })?;
    let backend = CrosstermBackend::new(io::stderr());
    let height = u16::try_from(choices.len().saturating_add(4).min(15)).unwrap_or(15);
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    );
    let mut terminal = match terminal {
        Ok(terminal) => terminal,
        Err(_) => return select_text_choice_fallback(title, choices, shortcuts),
    };
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
        {
            if let KeyCode::Char(character) = key.code
                && let Some((_, index)) = shortcuts
                    .iter()
                    .find(|(shortcut, _)| shortcut.eq_ignore_ascii_case(&character))
            {
                terminal.clear().ok();
                return Ok(*index);
            }
            if let Some((next, confirmed)) =
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
}

fn select_text_choice_fallback(
    title: &str,
    choices: &[String],
    shortcuts: &[(char, usize)],
) -> Result<usize> {
    let mut selected = 0usize;
    let lines = choices.len().saturating_add(2);
    loop {
        let mut stderr = io::stderr();
        writeln!(stderr, "? {title}").ok();
        for (index, choice) in choices.iter().enumerate() {
            writeln!(
                stderr,
                "{}  {choice}",
                if index == selected { ">" } else { " " }
            )
            .ok();
        }
        writeln!(stderr, "Press ↑↓ arrows to select, enter to confirm.").ok();
        stderr.flush().ok();

        if let Event::Key(key) = event::read().map_err(|error| {
            Error::with_source(ErrorKind::Process, "could not read selection", error)
        })? && key.kind == KeyEventKind::Press
        {
            if let KeyCode::Char(character) = key.code
                && let Some((_, index)) = shortcuts
                    .iter()
                    .find(|(shortcut, _)| shortcut.eq_ignore_ascii_case(&character))
            {
                return Ok(*index);
            }
            if let Some((next, confirmed)) =
                update_list_selection(selected, choices.len(), key.code)?
            {
                selected = next;
                if confirmed {
                    return Ok(selected);
                }
            }
        }

        execute!(
            io::stderr(),
            cursor::MoveUp(u16::try_from(lines).unwrap_or(u16::MAX)),
            Clear(ClearType::FromCursorDown)
        )
        .ok();
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

pub(crate) fn select_organization(
    organizations: &[RemoteOrganization],
) -> Result<RemoteOrganization> {
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

pub(crate) struct AuthTerminalGuard;

impl Drop for AuthTerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stderr(), cursor::Show, Clear(ClearType::CurrentLine));
    }
}

pub(crate) fn open_browser(url: &str) -> bool {
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

fn open_profile_viewer(profile: &Path) -> bool {
    let executable = env::var_os("CFY_SPEEDSCOPE_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("speedscope"));
    std::process::Command::new(executable)
        .arg(profile)
        .status()
        .is_ok_and(|status| status.success())
}

async fn console_theme(client: &ThemeClient) -> Result<Theme> {
    if let Some(theme) = client
        .list()
        .await
        .map_err(Error::from)?
        .into_iter()
        .find(|theme| {
            theme.role == "development" && theme.name.starts_with("Catify Liquid Console")
        })
    {
        return Ok(theme);
    }
    client
        .create_development_theme(
            &format!("Catify Liquid Console ({})", env!("CARGO_PKG_VERSION")),
            &Cancellation::default(),
        )
        .await
        .map_err(Error::from)
}

async fn prepare_console_theme(client: &ThemeClient, theme_id: u64) -> Result<()> {
    let assets = [
        ("config/settings_data.json", "{}"),
        ("config/settings_schema.json", "[]"),
        ("snippets/eval.liquid", ""),
        (
            "layout/password.liquid",
            "{{ content_for_header }}{{ content_for_layout }}",
        ),
        (
            "layout/theme.liquid",
            "{{ content_for_header }}{{ content_for_layout }}",
        ),
        ("sections/announcement-bar.liquid", ""),
        (
            "templates/index.json",
            r#"{"sections":{"announcement":{"type":"announcement-bar","settings":{}}},"order":["announcement"]}"#,
        ),
    ]
    .into_iter()
    .map(|(key, value)| {
        ThemeChange::Upload(ThemeAsset {
            key: key.to_owned(),
            contents: value.as_bytes().to_vec(),
        })
    })
    .collect::<Vec<_>>();
    let summary = client
        .push(theme_id, &assets, false, &Cancellation::default())
        .await;
    if summary.succeeded() {
        Ok(())
    } else {
        Err(Error::api(format!(
            "could not prepare Liquid Console theme: {}",
            summary.failed.join("; ")
        )))
    }
}

fn read_console_line(history: &[String]) -> Result<Option<String>> {
    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), cursor::Show);
        }
    }

    enable_raw_mode().map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            "could not enable console input mode",
            error,
        )
    })?;
    let _guard = RawModeGuard;
    let mut stdout = io::stdout();
    let mut buffer = String::new();
    let mut cursor_index = 0_usize;
    let mut history_index = history.len();
    execute!(stdout, cursor::Show).ok();

    loop {
        redraw_console_line(&mut stdout, &buffer, cursor_index)?;
        let event = event::read().map_err(|error| {
            Error::with_source(ErrorKind::Process, "could not read console input", error)
        })?;
        let Event::Key(key) = event else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('c')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                println!();
                return Ok(None);
            }
            KeyCode::Char('d')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL)
                    && buffer.is_empty() =>
            {
                println!();
                return Ok(None);
            }
            KeyCode::Enter => {
                println!();
                return Ok(Some(buffer));
            }
            KeyCode::Left if cursor_index > 0 => cursor_index -= 1,
            KeyCode::Right if cursor_index < buffer.chars().count() => cursor_index += 1,
            KeyCode::Home => cursor_index = 0,
            KeyCode::End => cursor_index = buffer.chars().count(),
            KeyCode::Backspace if cursor_index > 0 => {
                remove_character(&mut buffer, cursor_index - 1);
                cursor_index -= 1;
            }
            KeyCode::Delete if cursor_index < buffer.chars().count() => {
                remove_character(&mut buffer, cursor_index);
            }
            KeyCode::Up if !history.is_empty() => {
                history_index = history_index.saturating_sub(1);
                buffer.clone_from(&history[history_index]);
                cursor_index = buffer.chars().count();
            }
            KeyCode::Down if history_index < history.len() => {
                history_index += 1;
                buffer = history.get(history_index).cloned().unwrap_or_default();
                cursor_index = buffer.chars().count();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                insert_character(&mut buffer, cursor_index, character);
                cursor_index += 1;
            }
            _ => {}
        }
    }
}

fn redraw_console_line(stdout: &mut io::Stdout, buffer: &str, cursor_index: usize) -> Result<()> {
    execute!(
        stdout,
        cursor::MoveToColumn(0),
        Clear(ClearType::CurrentLine)
    )
    .map_err(|error| Error::with_source(ErrorKind::Process, "could not redraw console", error))?;
    write!(stdout, "> {buffer}")
        .map_err(|error| Error::with_source(ErrorKind::Process, "could not draw console", error))?;
    execute!(stdout, cursor::MoveToColumn((cursor_index + 2) as u16)).map_err(|error| {
        Error::with_source(ErrorKind::Process, "could not move console cursor", error)
    })?;
    stdout
        .flush()
        .map_err(|error| Error::with_source(ErrorKind::Process, "could not flush console", error))
}

fn insert_character(buffer: &mut String, index: usize, character: char) {
    let byte_index = buffer
        .char_indices()
        .nth(index)
        .map_or(buffer.len(), |(index, _)| index);
    buffer.insert(byte_index, character);
}

fn remove_character(buffer: &mut String, index: usize) {
    let Some((start, _)) = buffer.char_indices().nth(index) else {
        return;
    };
    let end = buffer
        .char_indices()
        .nth(index + 1)
        .map_or(buffer.len(), |(index, _)| index);
    buffer.replace_range(start..end, "");
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
        ThemeCommand::Share {
            auth_alias,
            environment,
            force,
            listing,
            password,
            path,
            store,
        } => {
            share_theme(
                ShareThemeArgs {
                    auth_alias,
                    environments: environment,
                    force,
                    listing,
                    explicit_password: password,
                    explicit_path: path,
                    explicit_store: store,
                },
                output,
            )
            .await
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
        ThemeCommand::Console {
            auth_alias,
            environment,
            password,
            path,
            store,
            store_password,
            url,
        } => {
            if non_interactive || !io::stdin().is_terminal() {
                return Err(Error::invalid_input(
                    "theme console requires an interactive terminal",
                ));
            }
            let root = path.unwrap_or(env::current_dir().map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    "could not resolve current directory",
                    error,
                )
            })?);
            let (environment_store, environment_password) =
                theme_environment_credentials(&root, &environment)?;
            let store = resolve_store(store.or(environment_store).as_deref())?;
            let (admin_token, storefront_token) =
                match explicit_theme_tokens(password, environment_password)? {
                    Some(tokens) => tokens,
                    None => {
                        let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
                        let session = authenticated_session(&identity).await?;
                        (
                            exchange_admin_token(&session, &store).await?,
                            exchange_storefront_renderer_token(&session).await?,
                        )
                    }
                };
            let theme_client = ThemeClient::new(&store, admin_token.expose(), SHOPIFY_API_VERSION)
                .map_err(Error::from)?;
            let console_theme = console_theme(&theme_client).await?;
            prepare_console_theme(&theme_client, console_theme.id).await?;
            let profiler =
                ThemeProfiler::new(&store, admin_token, storefront_token, SHOPIFY_API_VERSION)
                    .map_err(|error| Error::api(error.to_string()))?;
            let mut console = profiler
                .console(console_theme.id, &url, store_password.as_deref())
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            println!("Liquid Console ready. Enter expressions without Liquid delimiters.");
            println!("Press Ctrl-D or Ctrl-C to exit.");
            let mut history = Vec::new();
            while let Some(input) = read_console_line(&history)? {
                if input.trim().is_empty() {
                    continue;
                }
                history.push(input.clone());
                match console.evaluate(&input).await {
                    Ok(LiquidEvaluation::Display(value)) => println!(
                        "{}",
                        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
                    ),
                    Ok(LiquidEvaluation::Assigned) => {}
                    Err(error) => eprintln!("{error}"),
                }
            }
            Ok(0)
        }
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
        ThemeCommand::Profile {
            auth_alias,
            environment,
            json,
            password,
            path,
            store,
            store_password,
            theme,
            url,
        } => {
            if password.is_some() {
                return Err(Error::invalid_input(
                    "theme profile cannot use --password; authenticate with `cfy auth login` instead",
                ));
            }
            let root = path.unwrap_or(env::current_dir().map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    "could not resolve current directory",
                    error,
                )
            })?);
            let (environment_store, _) = theme_environment_credentials(&root, &environment)?;
            let store = resolve_store(store.or(environment_store).as_deref())?;
            let identity = auth_alias.unwrap_or_else(|| "default".to_owned());
            let session = authenticated_session(&identity).await?;
            let admin_token = exchange_admin_token(&session, &store).await?;
            let storefront_token = exchange_storefront_renderer_token(&session).await?;
            let theme_client = ThemeClient::new(&store, admin_token.expose(), SHOPIFY_API_VERSION)
                .map_err(Error::from)?;
            let themes = theme_client.list().await.map_err(Error::from)?;
            let selected = select_theme_for_open(
                &themes,
                theme.as_deref(),
                false,
                theme.is_none(),
                non_interactive,
            )?;
            let profiler =
                ThemeProfiler::new(&store, admin_token, storefront_token, SHOPIFY_API_VERSION)
                    .map_err(|error| Error::api(error.to_string()))?;
            let profile = profiler
                .profile(selected.id, &url, store_password.as_deref())
                .await
                .map_err(|error| Error::api(error.to_string()))?;
            if json {
                use std::io::Write as _;
                let mut stdout = io::stdout().lock();
                stdout
                    .write_all(profile.raw_json().as_bytes())
                    .map_err(|error| {
                        Error::with_source(
                            ErrorKind::Process,
                            "could not write profile JSON",
                            error,
                        )
                    })?;
                stdout.write_all(b"\n").map_err(|error| {
                    Error::with_source(ErrorKind::Process, "could not write profile JSON", error)
                })?;
                return Ok(0);
            }
            let destination = env::temp_dir().join(format!(
                "catify-liquid-profile-{}-{}.json",
                selected.id,
                std::process::id()
            ));
            write_atomic(&destination, profile.raw_json().as_bytes()).map_err(|error| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("could not write profile file {}", destination.display()),
                    error,
                )
            })?;
            let opened = !non_interactive && open_profile_viewer(&destination);
            output
                .success(
                    &format!("Liquid profile saved to {}", destination.display()),
                    &serde_json::json!({
                        "theme": selected,
                        "url": url,
                        "profile_path": destination,
                        "opened": opened,
                    }),
                )
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
    }
}

async fn upgrade(non_interactive: bool, output: &Output) -> Result<()> {
    let provenance = detect_upgrade()?;
    let plan = plan_upgrade(&provenance)
        .map_err(|error| Error::with_source(ErrorKind::Config, error.to_string(), error))?;
    if matches!(plan, cfy_upgrade::UpgradePlan::Standalone { .. }) {
        if non_interactive {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "standalone upgrade requires an interactive terminal",
            ));
        }
        let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).map_err(|error| {
            Error::with_source(ErrorKind::Config, "invalid Catify build version", error)
        })?;
        let releases_url = env::var("CFY_RELEASES_API_URL")
            .unwrap_or_else(|_| cfy_upgrade::DEFAULT_RELEASES_API_URL.into());
        let result = execute_standalone(&plan, &current, &releases_url)
            .await
            .map_err(|error| Error::with_source(ErrorKind::Process, error.to_string(), error))?;
        return output
            .success(
                if result.changed {
                    "Catify upgraded"
                } else {
                    "Catify is already up to date"
                },
                &result,
            )
            .map_err(|error| Error::process(error.to_string()));
    }
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
    #[command(disable_version_flag = true)]
    Share {
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(short = 'e', long, env = "SHOPIFY_FLAG_ENVIRONMENT", action = ArgAction::Append)]
        environment: Vec<String>,
        #[arg(short = 'f', long, env = "SHOPIFY_FLAG_FORCE", hide = true)]
        force: bool,
        #[arg(long, env = "SHOPIFY_FLAG_LISTING")]
        listing: Option<String>,
        #[arg(long, env = "SHOPIFY_CLI_THEME_TOKEN")]
        password: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: Option<String>,
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
    /// Start the interactive Shopify Liquid REPL.
    #[command(disable_version_flag = true)]
    Console {
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(short = 'e', long, env = "SHOPIFY_FLAG_ENVIRONMENT", action = ArgAction::Append)]
        environment: Vec<String>,
        #[arg(long, env = "SHOPIFY_CLI_THEME_TOKEN")]
        password: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_STORE_PASSWORD")]
        store_password: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_URL", default_value = "/")]
        url: String,
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
    /// Profile the Liquid rendering of a theme page.
    #[command(disable_version_flag = true)]
    Profile {
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
        #[arg(short = 'e', long, env = "SHOPIFY_FLAG_ENVIRONMENT", action = ArgAction::Append)]
        environment: Vec<String>,
        #[arg(short = 'j', long, env = "SHOPIFY_FLAG_JSON")]
        json: bool,
        #[arg(long, env = "SHOPIFY_CLI_THEME_TOKEN")]
        password: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_PATH")]
        path: Option<PathBuf>,
        #[arg(short = 's', long, env = "SHOPIFY_FLAG_STORE")]
        store: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_STORE_PASSWORD")]
        store_password: Option<String>,
        #[arg(short = 't', long, env = "SHOPIFY_FLAG_THEME_ID")]
        theme: Option<String>,
        #[arg(long, env = "SHOPIFY_FLAG_URL", default_value = "/")]
        url: String,
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
    /// Refresh the cached latest-release metadata.
    #[command(hide = true)]
    UpdateCheck,
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
        Some(Command::Internal {
            command: InternalCommand::UpdateCheck,
        }) => update_check::refresh().await,
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

/// Shared entry point for the `cfy` and `catify` executable names.
/// Runs the CLI on a stack large enough for Clap's complete compatibility tree.
/// Windows executable threads default to a 1 MiB stack, which is too small for
/// the intentionally broad Shopify-compatible command graph.
#[must_use]
pub fn run_main() -> std::process::ExitCode {
    std::thread::Builder::new()
        .name("catify-cli".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Catify could not initialize its async runtime")
                .block_on(main_entry())
        })
        .expect("Catify could not initialize its CLI thread")
        .join()
        .unwrap_or(std::process::ExitCode::FAILURE)
}

pub async fn main_entry() -> std::process::ExitCode {
    let cli = parse_cli();
    if let Some(Command::Internal { command }) = cli.command.as_ref()
        && is_update_check(command)
    {
        refresh_update_check().await;
        return std::process::ExitCode::SUCCESS;
    }
    maybe_notify_and_refresh(&cli);
    let output = Output::new(cli.global.json, cli.global.verbose);
    let _ = output.diagnostic("debug diagnostics enabled");
    match run(cli, &output).await {
        Ok(code) => std::process::ExitCode::from(code),
        Err(error) => {
            let _ = output.error(&error);
            error.exit_code()
        }
    }
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

#[cfg(test)]
mod tests {
    use super::{
        AppCommand, Cli, Command, ThemeCommand, corrected_command_args, explicit_theme_tokens,
        filesystem_event, format_themes, insert_character, live_push_requires_confirmation,
        remove_character, reusable_session, select_store, select_theme_for_open,
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
    fn theme_access_password_is_reused_for_admin_and_storefront_sessions() {
        let (admin, storefront) = explicit_theme_tokens(Some("theme-secret".into()), None)
            .unwrap()
            .unwrap();
        assert_eq!(admin.expose(), "theme-secret");
        assert_eq!(storefront.expose(), "theme-secret");
        assert!(explicit_theme_tokens(None, None).unwrap().is_none());
        assert!(explicit_theme_tokens(Some(String::new()), None).is_err());
    }

    #[test]
    fn function_commands_use_the_upstream_nested_paths_and_flags() {
        for args in [
            vec!["cfy", "app", "function", "build", "--path", "extensions/fn"],
            vec!["cfy", "app", "function", "info", "--json"],
            vec![
                "cfy",
                "app",
                "function",
                "replay",
                "--log",
                "abc",
                "--no-watch",
            ],
            vec![
                "cfy",
                "app",
                "function",
                "run",
                "--input",
                "input.json",
                "--export",
                "run",
                "--profile",
            ],
            vec!["cfy", "app", "function", "schema", "--stdout"],
            vec!["cfy", "app", "function", "typegen", "--reset"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        assert!(Cli::try_parse_from(["cfy", "app", "function-build"]).is_err());
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
    fn app_log_sources_uses_the_upstream_nested_command_path() {
        assert!(
            Cli::try_parse_from(["cfy", "app", "logs", "sources", "--config", "development",])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["cfy", "app", "logs-sources"]).is_err());
    }

    #[test]
    fn theme_share_matches_upstream_flags_without_legacy_theme_id() {
        assert!(
            Cli::try_parse_from([
                "cfy",
                "theme",
                "share",
                "--store",
                "example.myshopify.com",
                "--path",
                "theme",
                "--listing",
                "modern",
                "--environment",
                "staging",
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["cfy", "theme", "share", "--theme", "42"]).is_err());
    }

    #[test]
    fn store_open_matches_upstream_short_store_flag() {
        assert!(
            Cli::try_parse_from(["cfy", "store", "open", "-s", "example.myshopify.com"]).is_ok()
        );
        assert!(Cli::try_parse_from(["cfy", "store", "open", "--store", "example"]).is_ok());
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
    fn console_editor_handles_unicode_insertion_and_deletion() {
        let mut buffer = "ac".to_owned();
        insert_character(&mut buffer, 1, '🙂');
        assert_eq!(buffer, "a🙂c");
        remove_character(&mut buffer, 1);
        assert_eq!(buffer, "ac");
    }

    #[test]
    fn theme_profile_uses_shopify_compatible_flags() {
        let cli = Cli::try_parse_from([
            "cfy",
            "theme",
            "profile",
            "--auth-alias",
            "work",
            "--environment",
            "staging",
            "--json",
            "--path",
            ".",
            "--store",
            "example",
            "--store-password",
            "password",
            "--theme",
            "123",
            "--url",
            "/products/example",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Theme {
                command: ThemeCommand::Profile {
                    auth_alias: Some(_),
                    environment,
                    json: true,
                    password: None,
                    path: Some(_),
                    store: Some(_),
                    store_password: Some(_),
                    theme: Some(_),
                    url,
                }
            }) if environment == ["staging"] && url == "/products/example"
        ));
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
    fn custom_data_import_matches_shopify_command_path_and_flags() {
        let parsed = Cli::try_parse_from([
            "cfy",
            "app",
            "import-custom-data-definitions",
            "--store",
            "demo.myshopify.com",
            "--include-existing",
            "--config",
            "staging",
        ])
        .unwrap();
        let Some(Command::App {
            command:
                AppCommand::ImportCustomDataDefinitions {
                    context,
                    include_existing,
                },
        }) = parsed.command
        else {
            panic!("expected import-custom-data-definitions command");
        };
        assert_eq!(context.store.as_deref(), Some("demo.myshopify.com"));
        assert_eq!(context.config.as_deref(), Some("staging"));
        assert!(include_existing);
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
