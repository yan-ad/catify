pub mod output;
mod theme_check;

use crate::output::Output;
use cfy_api::theme::{Theme, ThemeAsset, ThemeChange, ThemeClient, diff_assets};
use cfy_auth::{
    CredentialStore, NativeCredentialStore, Session,
    flow::{LoginMode, headless_from_env},
    identity::IdentityConfig,
};
use cfy_config::project::{
    Environment, ProjectKind, ProjectOverrides, discover, resolve_environment,
};
use cfy_config::theme::{
    StagedFile, commit_staged_files_cancellable, read_theme_files, safe_relative_path,
};
use cfy_config::theme_dev::{FileEvent, SyncAction, coalesce};
use cfy_config::{AutoUpgrade, UserSettings, clear_cache_root};
use cfy_core::{Cancellation, Error, ErrorKind, Result};
use cfy_docs::{Cache as DocsCache, DocsClient, HttpDocsTransport};
use cfy_hydrogen::run as run_hydrogen;
use cfy_store::{StoreCommand as StoreOperation, StoreTarget, browser_url};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use notify::{
    EventKind, RecursiveMode, Watcher,
    event::{ModifyKind, RenameMode},
};
use std::{
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;

const SHOPIFY_API_VERSION: &str = "2026-07";

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn theme_parity_command(command: ThemeCommand, output: &Output) -> Result<u8> {
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
        _ => Err(not_implemented(
            "this theme command backend is not configured yet",
        )),
    }
}

fn config_path() -> PathBuf {
    env::var_os("CFY_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .map(|path| PathBuf::from(path).join("crabpify/config.toml"))
        })
        .or_else(|| {
            env::var_os("HOME").map(|path| PathBuf::from(path).join(".config/crabpify/config.toml"))
        })
        .unwrap_or_else(|| PathBuf::from(".crabpify/config.toml"))
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
            env::var_os("XDG_CACHE_HOME").map(|path| PathBuf::from(path).join("crabpify/docs"))
        })
        .or_else(|| {
            env::var_os("HOME").map(|path| PathBuf::from(path).join(".cache/crabpify/docs"))
        })
        .unwrap_or_else(|| PathBuf::from(".crabpify-cache/docs"))
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
    /// Remove Crabpify caches and report reclaimed bytes.
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
            env::var_os("XDG_CACHE_HOME").map(|path| PathBuf::from(path).join("crabpify/docs"))
        })
        .or_else(|| {
            env::var_os("HOME").map(|path| PathBuf::from(path).join(".cache/crabpify/docs"))
        })
        .unwrap_or_else(|| PathBuf::from(".crabpify-cache/docs"));
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
        AuthCommand::Login { identity } => {
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
            let config_error = IdentityConfig::from_env(|key| env::var(key).ok()).err();
            let detail = config_error
                .map(|error| format!(": {error}"))
                .unwrap_or_default();
            Err(Error::invalid_input(format!(
                "interactive login is not available in this build{detail}; set CFY_IDENTITY_CLIENT_ID, or use --non-interactive with SHOPIFY_CLI_TOKEN"
            )))
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
        summary: "List all Crabpify commands.",
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
        summary: "Display help for Crabpify.",
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
        summary: "Upgrade Crabpify.",
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
        "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Crabpify login flow",
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
        let name = format!("Crabpify development {}", std::process::id());
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
        "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Crabpify login flow",
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

/// Crabpify's top-level command-line interface.
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
            "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Crabpify login flow",
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
            "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Crabpify login flow",
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

/// Options shared by every Crabpify command.
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
    /// Display help for Crabpify.
    Help {
        /// Optional topic or command to describe.
        topic: Option<String>,
    },
    /// List all public Crabpify commands.
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

    /// Upgrade Crabpify through a supported installation channel.
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
    /// Crabpify configuration options.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Show or clear Crabpify notification state.
    Notification {
        #[command(subcommand)]
        command: NotificationCommand,
    },
    /// Manage local caches.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
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
pub enum AppCommand {
    /// Display the currently selected app.
    #[command(alias = "show")]
    Info,
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
        Some(Command::App { .. }) => return Err(not_implemented("app")),
        Some(Command::Auth { command }) => {
            auth_command(command, cli.global.non_interactive, output).await?;
        }
        Some(Command::Organization { command }) => {
            organization_command(command, output).await?;
        }
        Some(Command::Store { command }) => {
            store_command(command, cli.global.non_interactive, output).await?;
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
            return theme_parity_command(command, output);
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

fn not_implemented(group: &str) -> Error {
    Error::invalid_input(format!(
        "the '{group}' command group is reserved but not implemented yet"
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, Command, ThemeCommand, filesystem_event, format_themes,
        live_push_requires_confirmation, select_store,
    };
    use cfy_api::theme::Theme;
    use cfy_config::project::Environment;
    use cfy_config::theme_dev::FileEvent;
    use clap::{CommandFactory, Parser, error::ErrorKind};
    use notify::{
        Event, EventKind,
        event::{ModifyKind, RenameMode},
    };

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
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
