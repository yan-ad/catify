use cfy_upgrade::{
    CARGO_PACKAGE, DetectionContext, ExecutionPolicy, HOMEBREW_FORMULA, InstallProvenance,
    NPM_PACKAGE, UpdateCache, UpgradeError, UpgradePlan, detect_with, fetch_latest_version, plan,
    read_update_cache, write_update_cache,
};
use std::{fs, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn context(executable: impl Into<PathBuf>) -> DetectionContext {
    DetectionContext {
        executable: executable.into(),
        cargo_home: None,
        home: None,
        homebrew_prefix: None,
        install_channel: None,
    }
}

#[test]
fn detects_long_catify_command_in_cargo_home() {
    let mut context = context("/users/me/.cargo/bin/catify");
    context.cargo_home = Some("/users/me/.cargo".into());
    assert!(matches!(
        detect_with(&context),
        InstallProvenance::Cargo { .. }
    ));
}

#[test]
fn detects_homebrew_cellar_and_builds_exact_plan() {
    let provenance = detect_with(&context("/opt/homebrew/Cellar/catify/1.2.3/bin/cfy"));
    assert_eq!(provenance.kind().to_string(), "homebrew");
    let plan = plan(&provenance).unwrap();
    assert_eq!(plan.command().unwrap().display(), "brew upgrade catify");
    assert!(
        matches!(plan, UpgradePlan::Homebrew { ref formula, .. } if formula == HOMEBREW_FORMULA)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn execution_returns_the_childs_exact_nonzero_exit_status() {
    let plan = UpgradePlan::Cargo {
        package: CARGO_PACKAGE.into(),
        command: cfy_upgrade::UpgradeCommand {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 23".into()],
        },
    };
    let output = cfy_upgrade::execute(
        &plan,
        ExecutionPolicy::INTERACTIVE,
        &cfy_process::Supervisor::default(),
    )
    .await
    .unwrap();
    assert_eq!(output.exit_code(), Some(23));
}

#[test]
fn detects_configured_homebrew_prefix() {
    let mut context = context("/brew/Cellar/catify/1.0/bin/cfy");
    context.homebrew_prefix = Some("/brew".into());
    assert!(matches!(
        detect_with(&context),
        InstallProvenance::Homebrew { .. }
    ));
}

#[test]
fn detects_cargo_home_and_builds_exact_locked_plan() {
    let mut context = context("/users/me/.cargo/bin/cfy");
    context.cargo_home = Some("/users/me/.cargo".into());
    let provenance = detect_with(&context);
    assert!(
        matches!(provenance, InstallProvenance::Cargo { ref package, .. } if package == CARGO_PACKAGE)
    );
    assert_eq!(
        plan(&provenance).unwrap().command().unwrap().display(),
        "cargo install cfy-cli --locked"
    );
}

#[test]
fn standalone_requires_archive_version_marker() {
    let root = std::env::temp_dir().join(format!("cfy-upgrade-standalone-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("cfy"), b"binary").unwrap();
    fs::write(root.join("VERSION"), b"1.2.3\n").unwrap();
    let provenance = detect_with(&context(root.join("cfy")));
    assert!(matches!(provenance, InstallProvenance::Standalone { .. }));
    assert!(matches!(
        plan(&provenance).unwrap(),
        UpgradePlan::Standalone { .. }
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn source_checkout_wins_over_channel_hint_and_is_refused() {
    let root = std::env::temp_dir().join(format!("cfy-upgrade-source-{}", std::process::id()));
    let executable = root.join("target/debug/cfy");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    fs::write(&executable, b"binary").unwrap();
    let mut context = context(&executable);
    context.install_channel = Some("cargo".into());
    let provenance = detect_with(&context);
    assert!(matches!(provenance, InstallProvenance::Source { .. }));
    assert!(matches!(
        plan(&provenance),
        Err(UpgradeError::SourceInstall { .. })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unknown_install_is_typed_and_refused() {
    let provenance = detect_with(&context("/some/custom/bin/cfy"));
    assert!(matches!(provenance, InstallProvenance::Unknown { .. }));
    assert!(matches!(
        plan(&provenance),
        Err(UpgradeError::UnknownInstall { .. })
    ));
}

#[test]
fn npm_channel_builds_global_package_upgrade_plan() {
    let mut context = context("/some/custom/bin/cfy");
    context.install_channel = Some("npm".into());
    let provenance = detect_with(&context);
    assert!(matches!(
        provenance,
        InstallProvenance::Npm { ref package, .. } if package == NPM_PACKAGE
    ));
    assert_eq!(
        plan(&provenance).unwrap().command().unwrap().display(),
        "npm install --global catify-cli@latest"
    );
}

#[test]
fn update_cache_is_fresh_and_only_reports_newer_semver() {
    let cache = UpdateCache {
        checked_at: 1_000,
        latest_version: Some("1.3.0".into()),
    };
    assert!(cache.is_fresh_at(1_100));
    assert!(!cache.is_fresh_at(1_000 + 24 * 60 * 60));
    assert_eq!(cache.available_version("1.2.9"), Some("1.3.0"));
    assert_eq!(cache.available_version("1.3.0"), None);
    assert_eq!(cache.available_version("2.0.0"), None);
}

#[test]
fn update_cache_round_trips_atomically() {
    let root = std::env::temp_dir().join(format!(
        "cfy-update-cache-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = root.join("nested/update.json");
    let cache = UpdateCache {
        checked_at: 42,
        latest_version: Some("9.8.7".into()),
    };
    write_update_cache(&path, &cache).unwrap();
    assert_eq!(read_update_cache(&path).unwrap(), Some(cache));
    let replacement = UpdateCache {
        checked_at: 43,
        latest_version: None,
    };
    write_update_cache(&path, &replacement).unwrap();
    assert_eq!(read_update_cache(&path).unwrap(), Some(replacement));
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn fetches_latest_github_release_tag_as_semver() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        let body = r#"{"tag_name":"v2.4.1"}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let latest = fetch_latest_version(&format!("http://{address}/latest"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.to_string(), "2.4.1");
    server.await.unwrap();
}

#[tokio::test]
async fn noninteractive_execution_requires_explicit_approval_before_spawning() {
    let plan = UpgradePlan::Cargo {
        package: CARGO_PACKAGE.into(),
        command: cfy_upgrade::UpgradeCommand {
            program: "this-program-must-not-run".into(),
            args: vec![],
        },
    };
    let result = cfy_upgrade::execute(
        &plan,
        ExecutionPolicy::NON_INTERACTIVE_REFUSE,
        &cfy_process::Supervisor::default(),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        UpgradeError::NonInteractiveApprovalRequired
    );
}
