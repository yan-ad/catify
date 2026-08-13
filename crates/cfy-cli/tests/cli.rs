use std::process::{Command, Output};

fn cfy(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cfy"))
        .args(args)
        .output()
        .expect("cfy should execute")
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("not implemented yet"));
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
