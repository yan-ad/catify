use std::process::{Command, Output};

fn cfy(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(args)
        .output()
        .expect("cfy should execute")
}

#[cfg(unix)]
#[test]
fn auth_login_can_explicitly_delegate_to_official_shopify_cli() {
    let executable = fake_shopify(
        "#!/bin/sh\n[ \"$1 $2\" = \"auth login\" ] || exit 9\nprintf 'delegated-login\\n'\nexit 0\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["auth", "login", "--delegate"])
        .env_remove("CFY_IDENTITY_CLIENT_ID")
        .env("CFY_SHOPIFY_BIN", executable)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("delegated-login"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Delegating authentication to the official Shopify CLI"));
    assert!(stderr.contains("session remains managed by Shopify CLI"));
}

#[cfg(unix)]
fn fake_shopify(script: &str) -> std::path::PathBuf {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };
    let directory = std::env::temp_dir().join(format!(
        "cfy-theme-check-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&directory).unwrap();
    let executable = directory.join("shopify");
    fs::write(&executable, script).unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();
    executable
}

#[cfg(unix)]
#[test]
fn theme_check_preserves_streams_and_child_exit_code() {
    let executable = fake_shopify(
        "#!/bin/sh\nif [ \"$1\" = version ]; then echo 3.90.0; exit 0; fi\nprintf 'theme-json\\n'\nprintf 'theme-warning\\n' >&2\nexit 7\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args([
            "theme",
            "check",
            "--path",
            "tests/fixtures/theme-check/failing",
            "--output",
            "json",
        ])
        .env("CFY_THEME_CHECK_BIN", executable)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "theme-json\n");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "theme-warning\n");
}

#[cfg(unix)]
#[test]
fn theme_check_version_mismatch_is_actionable() {
    let executable = fake_shopify("#!/bin/sh\necho 1.0.0\n");
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["theme", "check"])
        .env("CFY_THEME_CHECK_BIN", executable)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported Theme Check adapter version"));
    assert!(stderr.contains("CFY_THEME_CHECK_BIN"));
}

#[test]
fn theme_check_missing_dependency_is_actionable() {
    let output = Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(["theme", "check"])
        .env(
            "CFY_THEME_CHECK_BIN",
            "cfy-definitely-missing-theme-check-adapter",
        )
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("npm install -g @shopify/cli @shopify/theme"));
    assert!(stderr.contains("CFY_THEME_CHECK_BIN"));
}

#[test]
fn json_runtime_errors_leave_stdout_empty() {
    let output = cfy(&["app", "info", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());

    let diagnostic: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("stderr should contain one JSON diagnostic");
    assert_eq!(diagnostic["error"]["code"], "invalid_input");
}

#[test]
fn verbose_diagnostics_are_explicit_and_stay_out_of_stdout() {
    let output = cfy(&["version", "--json", "--verbose"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        concat!(
            "{\"name\":\"cfy\",\"version\":\"",
            env!("CARGO_PKG_VERSION"),
            "\"}\n"
        )
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "debug: debug diagnostics enabled\n"
    );
}

#[test]
fn root_help_is_successful() {
    let output = cfy(&["--help"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage: cfy"));
}

#[test]
fn invalid_command_has_stable_usage_exit_code_and_suggestion() {
    let output = cfy(&["versoin"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("version"));
}

#[test]
fn runtime_command_error_uses_core_exit_code() {
    let output = cfy(&["a", "show"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("app info requires a Shopify app project"));
    assert!(!stderr.contains("reserved but not implemented yet"));
}

#[test]
fn unavailable_backends_have_command_specific_diagnostics() {
    let app = cfy(&["app", "env"]);
    assert_eq!(app.status.code(), Some(1));
    let app_stderr = String::from_utf8_lossy(&app.stderr);
    assert!(app_stderr.contains("app env show is not available"));
    assert!(app_stderr.contains("issues/40"));
    assert!(!app_stderr.contains("reserved but not implemented yet"));

    let theme = cfy(&["theme", "preview"]);
    assert_eq!(theme.status.code(), Some(1));
    let theme_stderr = String::from_utf8_lossy(&theme.stderr);
    assert!(theme_stderr.contains("theme preview is not available"));
    assert!(theme_stderr.contains("issues/39"));
    assert!(!theme_stderr.contains("reserved but not implemented yet"));
}

#[test]
fn version_supports_machine_readable_output() {
    let output = cfy(&["version", "--json"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        concat!(
            "{\"name\":\"cfy\",\"version\":\"",
            env!("CARGO_PKG_VERSION"),
            "\"}\n"
        )
    );
}

#[test]
fn completion_generates_a_shell_script() {
    let output = cfy(&["completion", "bash"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("_cfy"));
}
