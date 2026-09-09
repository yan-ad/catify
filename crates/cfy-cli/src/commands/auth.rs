use super::super::{AuthTerminalGuard, open_browser, output, output::Output};
use cfy_app::BusinessPlatformClient;
use cfy_auth::{
    CredentialStore, NativeCredentialStore, Session,
    flow::{LoginMode, headless_from_env},
    identity::{HttpIdentityTransport, IdentityClient, IdentityConfig},
};
use cfy_core::{Error, ErrorKind, Result};
use clap::Subcommand;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::enable_raw_mode,
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
    io::{self, IsTerminal},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

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

fn format_organizations(organizations: &[cfy_app::RemoteOrganization]) -> String {
    if organizations.is_empty() {
        return "No organizations found.".to_owned();
    }
    let id_width = organizations
        .iter()
        .map(|organization| organization.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let mut rows = vec![format!("{:<id_width$}  NAME", "ID")];
    rows.extend(
        organizations
            .iter()
            .map(|organization| format!("{:<id_width$}  {}", organization.id, organization.name)),
    );
    rows.join("\n")
}

#[derive(Debug, Subcommand)]
pub enum OrganizationCommand {
    /// List organizations available to the current identity.
    List {
        #[arg(long, env = "SHOPIFY_FLAG_AUTH_ALIAS")]
        auth_alias: Option<String>,
    },
}

pub(crate) async fn auth_command(
    command: AuthCommand,
    non_interactive: bool,
    output: &Output,
) -> Result<u8> {
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

pub(crate) async fn organization_command(
    command: OrganizationCommand,
    output: &Output,
) -> Result<u8> {
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
            let human = format_organizations(&organizations);
            output
                .success(&human, &organizations)
                .map_err(|error| Error::process(error.to_string()))?;
            Ok(0)
        }
    }
}

pub(crate) fn update_auth_selection(
    selected: usize,
    code: KeyCode,
) -> Result<Option<(usize, bool)>> {
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

pub(crate) fn reusable_session(session: &Session, now_unix: u64) -> bool {
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
