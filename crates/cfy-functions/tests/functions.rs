use cfy_config::graph::{AppConfigGraph, AppNode, ExtensionConfig, ExtensionFamily};
use cfy_functions::*;
use cfy_process::Supervisor;
use serde_json::Value as JsonValue;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;
use tempfile::tempdir;
use toml::Table;

fn extension(directory: &Path, source: &str, family: ExtensionFamily) -> ExtensionConfig {
    let raw: Table = source.parse().unwrap();
    ExtensionConfig {
        path: directory.join("shopify.extension.toml"),
        directory: directory.into(),
        name: raw.get("name").and_then(|v| v.as_str()).map(str::to_owned),
        handle: raw
            .get("handle")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        uid: None,
        extension_type: raw.get("type").and_then(|v| v.as_str()).map(str::to_owned),
        api_version: raw
            .get("api_version")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        family,
        raw,
        unknown: Table::new(),
    }
}

fn spec(directory: &Path) -> FunctionSpec {
    FunctionSpec::from_extension(&extension(
        directory,
        r#"name = "Discount"
type = "product_discounts"
handle = "discount"
api_version = "2025-01"
[build]
path = "build/function.wasm"
[[targeting]]
target = "purchase.product-discount.run"
input_query = "src/run.graphql"
export = "run"
"#,
        ExtensionFamily::Function,
    ))
    .unwrap()
}

#[test]
fn parses_legacy_and_modern_function_configuration() {
    let dir = Path::new("extensions/discount");
    let legacy = spec(dir);
    assert_eq!(legacy.name.as_deref(), Some("Discount"));
    assert_eq!(legacy.wasm_path, dir.join("build/function.wasm"));
    assert_eq!(legacy.schema_path, dir.join("schema.graphql"));
    assert_eq!(
        legacy.targeting[0].input_query,
        Some(dir.join("src/run.graphql"))
    );

    let modern = FunctionSpec::from_extension(&extension(
        dir,
        r#"[[extensions]]
name = "Modern"
type = "cart_transform"
handle = "modern"
api_version = "unstable"
[extensions.build]
command = "cargo build"
typegen_command = "cargo xtask typegen"
[[extensions.targeting]]
target = "cart.transform.run"
input_query = "run.graphql"
export = "run"
"#,
        ExtensionFamily::Unsupported,
    ))
    .unwrap();
    assert_eq!(modern.name.as_deref(), Some("Modern"));
    assert_eq!(modern.build.command.as_deref(), Some("cargo build"));
    assert_eq!(modern.wasm_path, dir.join("dist/index.wasm"));
}

#[test]
fn selection_is_exact_or_returns_choices() {
    let a = extension(
        Path::new("extensions/a"),
        "type='cart_transform'\nname='A'",
        ExtensionFamily::Function,
    );
    let b = extension(
        Path::new("extensions/b"),
        "type='cart_transform'\nname='B'",
        ExtensionFamily::Function,
    );
    let graph = AppConfigGraph {
        root: PathBuf::from("."),
        apps: vec![AppNode {
            config: cfy_config::graph::AppConfig {
                path: "shopify.app.toml".into(),
                name: None,
                client_id: None,
                application_url: None,
                embedded: None,
                extension_directories: vec![],
                web_directories: vec![],
                build: Default::default(),
                raw: Table::new(),
                unknown: Table::new(),
            },
            extensions: vec![a, b],
            webs: vec![],
        }],
        diagnostics: vec![],
    };
    assert_eq!(
        select_function(&graph, Some(Path::new("extensions/b")))
            .unwrap()
            .name
            .as_deref(),
        Some("B")
    );
    assert!(
        matches!(select_function(&graph, None), Err(FunctionsError::FunctionSelectionRequired(v)) if v.len() == 2)
    );
}

#[tokio::test]
async fn build_runs_configured_command_and_checks_output() {
    let temp = tempdir().unwrap();
    let mut function = spec(temp.path());
    function.build.command = Some(
        if cfg!(windows) {
            "mkdir build && echo wasm>build\\function.wasm"
        } else {
            "mkdir -p build && printf wasm > build/function.wasm"
        }
        .into(),
    );
    let output = build(&function, &Supervisor::default())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    assert!(function.wasm_path.is_file());
}

#[tokio::test]
async fn build_without_command_reuses_wasm_or_is_actionable() {
    let temp = tempdir().unwrap();
    let mut function = spec(temp.path());
    function.build.command = None;
    assert!(matches!(
        build(&function, &Supervisor::default()).await,
        Err(FunctionsError::MissingBuild { .. })
    ));
    fs::create_dir_all(function.wasm_path.parent().unwrap()).unwrap();
    fs::write(&function.wasm_path, b"wasm").unwrap();
    assert!(
        build(&function, &Supervisor::default())
            .await
            .unwrap()
            .is_none()
    );
}

#[test]
fn typegen_uses_configured_command_or_lockfile_manager() {
    let temp = tempdir().unwrap();
    let mut function = spec(temp.path());
    function.build.typegen_command = Some("custom typegen --fast".into());
    let configured = typegen_process_spec(&function);
    assert!(
        configured
            .args
            .iter()
            .any(|arg| arg.contains("custom typegen"))
    );

    function.build.typegen_command = None;
    fs::write(temp.path().join("pnpm-lock.yaml"), "").unwrap();
    let discovered = typegen_process_spec(&function);
    assert_eq!(discovered.program, "pnpm");
    assert_eq!(
        discovered.args,
        ["exec", "graphql-code-generator", "--config", "package.json"]
    );
}

#[test]
fn runner_urls_and_override_are_portable() {
    assert!(
        runner_download_url(RunnerPlatform::MacArm)
            .ends_with("function-runner-arm-macos-v9.2.2.gz")
    );
    assert!(
        runner_download_url(RunnerPlatform::LinuxX86_64)
            .ends_with("function-runner-x86_64-linux-v9.2.2.gz")
    );
    assert!(
        runner_download_url(RunnerPlatform::WindowsX86_64)
            .ends_with("function-runner-x86_64-windows-v9.2.2.gz")
    );
    assert!(runner_platform("windows", "aarch64").is_err());
    let temp = tempdir().unwrap();
    let runner = temp.path().join("runner");
    fs::write(&runner, "").unwrap();
    assert_eq!(
        resolve_runner_with_override(temp.path(), Some(runner.as_os_str())).unwrap(),
        runner
    );
}

#[test]
fn parses_lists_and_selects_replay_logs() {
    let temp = tempdir().unwrap();
    let valid = "20240522_150641_827Z_extensions_discount_abcdef.json";
    assert_eq!(parse_log_filename(valid).unwrap().identifier, "abcdef");
    fs::write(
        temp.path().join(valid),
        r#"{"payload":{"input":{"cart":1},"export":"run"}}"#,
    )
    .unwrap();
    fs::write(
        temp.path()
            .join("20240522_150642_827Z_extensions_discount_ghijkl.json"),
        r#"{"payload":{"input":null}}"#,
    )
    .unwrap();
    let runs = list_replay_runs(temp.path(), "discount").unwrap();
    assert_eq!(runs.len(), 1);
    let selected = select_replay_run(temp.path(), "discount", Some("abcdef")).unwrap();
    let (input, export) = replay_input(&selected).unwrap();
    assert_eq!(
        serde_json::from_slice::<JsonValue>(&input).unwrap(),
        serde_json::json!({"cart":1})
    );
    assert_eq!(export.as_deref(), Some("run"));

    let runner = temp.path().join("function-runner");
    let process = replay_process_spec(&spec(temp.path()), &runner, &selected, true).unwrap();
    assert_eq!(process.stdin.as_deref(), Some(input.as_slice()));
    assert!(process.args.iter().any(|argument| argument == "--json"));
}

#[test]
fn info_is_camel_case_and_only_reports_existing_schema() {
    let temp = tempdir().unwrap();
    let function = spec(temp.path());
    let value = serde_json::to_value(function_info(&function, PathBuf::from("runner"))).unwrap();
    assert_eq!(value["apiVersion"], "2025-01");
    assert!(value.get("schemaPath").is_none());
    assert_eq!(
        value["targeting"]["purchase.product-discount.run"]["export"],
        "run"
    );
}

fn schema_server(
    token: &'static str,
    response_body: String,
) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..header_end + 4]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if bytes.len() >= header_end + 4 + length {
                    break;
                }
            }
        }
        let request = String::from_utf8_lossy(&bytes).into_owned();
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {token}").to_ascii_lowercase())
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream.write_all(response.as_bytes()).unwrap();
        request
    });
    (format!("http://{address}"), handle)
}

#[test]
fn schema_client_uses_target_query_writes_atomically_and_redacts() {
    let token = "super-secret-token";
    let body = serde_json::json!({"data":{"target":{"api":{"schema":{"definition":"type Query { ok: Boolean }\n"}}}}}).to_string();
    let (base, handle) = schema_server(token, body);
    let temp = tempdir().unwrap();
    let function = spec(temp.path());
    let client = FunctionsApiClient::with_base(token, "org", "app", &base).unwrap();
    assert!(!format!("{client:?}").contains(token));
    let schema = client.write_schema(&function, false).unwrap();
    assert!(schema.starts_with("# schema-version: 2025-01\n"));
    assert_eq!(fs::read_to_string(&function.schema_path).unwrap(), schema);
    let request = handle.join().unwrap();
    assert!(request.contains("SchemaDefinitionByTarget"));
    assert!(request.contains("purchase.product-discount.run"));
}

#[test]
fn schema_errors_do_not_expose_token() {
    let token = "never-print-me";
    let body = serde_json::json!({"errors":[{"message":format!("bad {token}")}]}).to_string();
    let (base, handle) = schema_server(token, body);
    let temp = tempdir().unwrap();
    let error = FunctionsApiClient::with_base(token, "org", "app", &base)
        .unwrap()
        .fetch_schema(&spec(temp.path()))
        .unwrap_err();
    let rendered = error.to_string();
    assert!(!rendered.contains(token));
    assert!(rendered.contains("[REDACTED]"));
    handle.join().unwrap();
}
