use crate::{Cli, Command, InternalCommand};
use cfy_config::{AutoUpgrade, UserSettings};
use cfy_upgrade::{
    DEFAULT_RELEASE_API_URL, InstallProvenance, UpdateCache, detect, fetch_latest_version, plan,
    read_update_cache, unix_timestamp, update_cache_path, write_update_cache,
};
use std::{
    env,
    io::IsTerminal,
    process::{Command as ProcessCommand, Stdio},
};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn maybe_notify_and_refresh(cli: &Cli) {
    if !eligible(cli) {
        return;
    }
    let cache_path = update_cache_path();
    let now = unix_timestamp();
    let cache = read_update_cache(&cache_path).ok().flatten();
    if let Some(latest) = cache
        .as_ref()
        .and_then(|cached| cached.available_version(CURRENT_VERSION))
    {
        eprintln!(
            "Update available: Catify {CURRENT_VERSION} → {latest}. Run `{}`.",
            upgrade_instruction()
        );
    }
    if cache.as_ref().is_some_and(|cached| cached.is_fresh_at(now)) {
        return;
    }
    let Ok(executable) = env::current_exe() else {
        return;
    };
    let Ok(mut child) = ProcessCommand::new(executable)
        .args(["internal", "update-check"])
        .env("CFY_UPDATE_CHECK_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn eligible(cli: &Cli) -> bool {
    !cli.global.json
        && !cli.global.non_interactive
        && std::io::stderr().is_terminal()
        && env::var_os("CI").is_none()
        && env::var_os("CFY_NO_UPDATE_CHECK").is_none()
        && env::var_os("CFY_UPDATE_CHECK_CHILD").is_none()
        && !matches!(
            cli.command,
            Some(Command::Completion { .. } | Command::Internal { .. })
        )
        && matches!(
            UserSettings::resolve(Some(&crate::config_path()), None).autoupgrade,
            AutoUpgrade::On
        )
}

fn upgrade_instruction() -> String {
    match detect().ok() {
        Some(InstallProvenance::Standalone { .. } | InstallProvenance::Unknown { .. }) | None => {
            "rerun the Catify installer".into()
        }
        Some(provenance) => plan(&provenance)
            .ok()
            .and_then(|plan| plan.command().map(|command| command.display()))
            .unwrap_or_else(|| "cfy upgrade".into()),
    }
}

pub async fn refresh() {
    let url =
        env::var("CFY_RELEASE_API_URL").unwrap_or_else(|_| DEFAULT_RELEASE_API_URL.to_owned());
    let latest = fetch_latest_version(&url).await.ok().flatten();
    let cache = UpdateCache {
        checked_at: unix_timestamp(),
        latest_version: latest.map(|version| version.to_string()),
    };
    let _ = write_update_cache(&update_cache_path(), &cache);
}

pub const fn is_update_check(command: &InternalCommand) -> bool {
    matches!(command, InternalCommand::UpdateCheck)
}
