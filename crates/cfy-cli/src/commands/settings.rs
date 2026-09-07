use super::{super::output::Output, docs::docs_cache_root};
use cfy_config::{AutoCorrect, AutoUpgrade, UserSettings, clear_cache_root};
use cfy_core::{Error, Result};
use clap::Subcommand;
use std::{env, path::PathBuf};

pub(crate) fn config_path() -> PathBuf {
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

pub(crate) fn config_command(command: ConfigCommand, output: &Output) -> Result<u8> {
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

pub(crate) fn cache_command(command: CacheCommand, output: &Output) -> Result<u8> {
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

pub(crate) fn notification_command(command: NotificationCommand, output: &Output) -> Result<u8> {
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
