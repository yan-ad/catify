use cfy_upgrade::{
    CARGO_PACKAGE, DetectionContext, ExecutionPolicy, HOMEBREW_FORMULA, InstallProvenance,
    UpgradeError, UpgradePlan, detect_with, plan,
};
use std::{fs, path::PathBuf};

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
fn unsupported_channel_does_not_become_a_managed_install() {
    let mut context = context("/some/custom/bin/cfy");
    context.install_channel = Some("npm".into());
    let InstallProvenance::Unknown { reason, .. } = detect_with(&context) else {
        panic!("expected unknown")
    };
    assert!(reason.contains("npm"));
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
