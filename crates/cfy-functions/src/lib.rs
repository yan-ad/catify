//! Native Shopify Functions command services.
//!
//! Configuration, selection, replay-log handling, downloads, and Functions API
//! transport are implemented here. Subprocesses are limited to the function's
//! configured compiler/type generator and Shopify's function-runner.

use cfy_config::graph::{AppConfigGraph, ExtensionConfig, ExtensionFamily};
use cfy_process::{OutputMode, ProcessOutput, ProcessSpec, Supervisor};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Once;
use toml::{Table, Value};
use url::Url;

pub const FUNCTION_RUNNER_VERSION: &str = "9.2.2";
pub const FUNCTIONS_API_BASE: &str = "https://app.shopify.com/services/partners/api/functions/unstable";
const LOG_SELECTOR_LIMIT: usize = 100;
const SCHEMA_BY_TARGET: &str = r#"query SchemaDefinitionByTarget($handle: String!, $version: String!) {
  target(handle: $handle) {
    api {
      schema(version: $version) {
        definition
      }

pub fn replay_process_spec(spec: &FunctionSpec, runner: &Path, replay: &ReplayRun, json: bool) -> Result<ProcessSpec> {
    let (input, export) = replay_input(replay)?;
    let mut options = RunOptions { export, json, ..RunOptions::default() };
    if let Some(target) = spec.targeting.first() {
        options.schema_path = spec.schema_path.is_file().then(|| spec.schema_path.clone());
        options.query_path = target.input_query.clone().filter(|path| path.is_file());
        if options.schema_path.is_some() != options.query_path.is_some() {
            options.schema_path = None;
            options.query_path = None;
        }
    }
    let mut process = run_process_spec(spec, runner, &options)?;
    process.stdin = Some(input);
    process.output = OutputMode::CaptureAndStream;
    Ok(process)
}
    }
  }
}"#;
const SCHEMA_BY_API_TYPE: &str = r#"query SchemaDefinitionByApiType($type: String!, $version: String!) {
  api(type: $type) {
    schema(version: $version) {
      definition
    }
  }
}"#;

pub type Result<T> = std::result::Result<T, FunctionsError>;

#[derive(Debug, thiserror::Error)]
pub enum FunctionsError {
    #[error("the extension at {0} is not a Shopify Function")]
    NotAFunction(PathBuf),
    #[error("no Shopify Function extensions were found; add a Function extension or pass its directory")]
    NoFunctions,
    #[error("no function matches directory {directory}; available functions: {choices:?}")]
    FunctionNotFound { directory: PathBuf, choices: Vec<FunctionChoice> },
    #[error("more than one function is available; select one by exact directory: {0:?}")]
    FunctionSelectionRequired(Vec<FunctionChoice>),
    #[error("invalid function configuration: {0}")]
    InvalidConfig(String),
    #[error("{program} exited unsuccessfully ({description})")]
    ProcessFailed { program: String, description: String },
    #[error("function WebAssembly output is missing at {wasm}; configure build.command or build the function externally")]
    MissingBuild { wasm: PathBuf },
    #[error("configured build completed but did not create WebAssembly output at {0}")]
    MissingBuildOutput(PathBuf),
    #[error("unsupported function-runner platform {os}/{arch}")]
    UnsupportedPlatform { os: String, arch: String },
    #[error("function-runner override does not exist or is not a file: {0}")]
    InvalidRunnerOverride(PathBuf),
    #[error("failed to download function-runner from {url}: {message}")]
    RunnerDownload { url: String, message: String },
    #[error("no log found for '{identifier}'; searched {directory} for function {function_handle}")]
    LogNotFound { identifier: String, directory: PathBuf, function_handle: String },
    #[error("no replayable logs found in {0}")]
    NoReplayLogs(PathBuf),
    #[error("more than one replayable log is available; select an exact identifier: {0:?}")]
    ReplaySelectionRequired(Vec<ReplayChoice>),
    #[error("invalid function log {path}: {message}")]
    InvalidLog { path: PathBuf, message: String },
    #[error("Functions API request failed: {0}")]
    Api(String),
    #[error("I/O error while {context}: {source}")]
    Io { context: String, #[source] source: io::Error },
    #[error(transparent)]
    Process(#[from] cfy_core::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetingEntry {
    pub target: String,
    pub input_query: Option<PathBuf>,
    pub export: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionBuild {
    pub command: Option<String>,
    pub path: Option<PathBuf>,
    pub typegen_command: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSpec {
    pub directory: PathBuf,
    pub name: Option<String>,
    pub handle: Option<String>,
    pub api_version: Option<String>,
    pub extension_type: Option<String>,
    pub build: FunctionBuild,
    pub targeting: Vec<TargetingEntry>,
    pub schema_path: PathBuf,
    pub wasm_path: PathBuf,
}

impl FunctionSpec {
    pub fn from_extension(extension: &ExtensionConfig) -> Result<Self> {
        let table = configuration_table(&extension.raw)?;
        let nested_type = string(table, "type");
        if extension.family != ExtensionFamily::Function
            && nested_type.as_deref().map(is_function_type) != Some(true)
        {
            return Err(FunctionsError::NotAFunction(extension.directory.clone()));
        }
        let build_table = table.get("build").and_then(Value::as_table);
        let build_path = build_table.and_then(|t| path_value(t, "path"));
        let directory = extension.directory.clone();
        let targeting = table
            .get("targeting")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_table)
            .map(|target| TargetingEntry {
                target: string(target, "target").unwrap_or_default(),
                input_query: path_value(target, "input_query").map(|p| directory.join(p)),
                export: string(target, "export"),
            })
            .filter(|target| !target.target.is_empty())
            .collect();
        let wasm_path = build_path
            .as_ref()
            .map(|path| directory.join(path))
            .unwrap_or_else(|| directory.join("dist/index.wasm"));
        Ok(Self {
            directory: directory.clone(),
            name: string(table, "name").or_else(|| extension.name.clone()),
            handle: string(table, "handle").or_else(|| extension.handle.clone()),
            api_version: string(table, "api_version").or_else(|| extension.api_version.clone()),
            extension_type: nested_type.or_else(|| extension.extension_type.clone()),
            build: FunctionBuild {
                command: build_table.and_then(|t| nonempty_string(t, "command")),
                path: build_path,
                typegen_command: build_table.and_then(|t| nonempty_string(t, "typegen_command")),
            },
            targeting,
            schema_path: directory.join("schema.graphql"),
            wasm_path,
        })
    }
}

fn configuration_table(raw: &Table) -> Result<&Table> {
    if let Some(extensions) = raw.get("extensions").and_then(Value::as_array) {
        return extensions
            .iter()
            .filter_map(Value::as_table)
            .find(|table| string(table, "type").as_deref().map(is_function_type) == Some(true))
            .ok_or_else(|| FunctionsError::InvalidConfig("[[extensions]] contains no Function entry".into()));
    }
    Ok(raw)
}

fn string(table: &Table, key: &str) -> Option<String> {
    table.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn nonempty_string(table: &Table, key: &str) -> Option<String> {
    string(table, key).filter(|value| !value.trim().is_empty())
}
fn path_value(table: &Table, key: &str) -> Option<PathBuf> {
    string(table, key).map(PathBuf::from)
}
fn is_function_type(value: &str) -> bool {
    value == "function"
        || value.ends_with("_discounts")
        || ["cart_checkout_validation", "cart_transform", "delivery_customization", "payment_customization", "fulfillment_constraints", "order_routing_location_rule", "local_pickup_delivery_option_generator", "pickup_point_delivery_option_generator"].contains(&value)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionChoice {
    pub directory: PathBuf,
    pub name: Option<String>,
    pub handle: Option<String>,
}

pub fn function_specs(graph: &AppConfigGraph) -> Result<Vec<FunctionSpec>> {
    graph.apps.iter().flat_map(|app| &app.extensions)
        .filter(|extension| extension.family == ExtensionFamily::Function || modern_function(extension))
        .map(FunctionSpec::from_extension).collect()
}

fn modern_function(extension: &ExtensionConfig) -> bool {
    extension.raw.get("extensions").and_then(Value::as_array).into_iter().flatten()
        .filter_map(Value::as_table).any(|table| string(table, "type").as_deref().map(is_function_type) == Some(true))
}

pub fn select_function(graph: &AppConfigGraph, directory: Option<&Path>) -> Result<FunctionSpec> {
    let functions = function_specs(graph)?;
    if functions.is_empty() { return Err(FunctionsError::NoFunctions); }
    if let Some(directory) = directory {
        if let Some(found) = functions.iter().find(|function| function.directory == directory) {
            return Ok(found.clone());
        }
        return Err(FunctionsError::FunctionNotFound { directory: directory.to_path_buf(), choices: choices(&functions) });
    }
    if functions.len() == 1 { return Ok(functions[0].clone()); }
    Err(FunctionsError::FunctionSelectionRequired(choices(&functions)))
}

fn choices(functions: &[FunctionSpec]) -> Vec<FunctionChoice> {
    functions.iter().map(|f| FunctionChoice { directory: f.directory.clone(), name: f.name.clone(), handle: f.handle.clone() }).collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionInfo {
    pub handle: Option<String>,
    pub name: Option<String>,
    pub api_version: Option<String>,
    pub targeting: BTreeMap<String, TargetInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_path: Option<PathBuf>,
    pub wasm_path: PathBuf,
    pub function_runner_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub input_query: Option<PathBuf>,
    pub export: Option<String>,
}

pub fn function_info(spec: &FunctionSpec, runner_path: PathBuf) -> FunctionInfo {
    FunctionInfo {
        handle: spec.handle.clone(), name: spec.name.clone(), api_version: spec.api_version.clone(),
        targeting: spec.targeting.iter().map(|t| (t.target.clone(), TargetInfo { input_query: t.input_query.clone(), export: t.export.clone() })).collect(),
        schema_path: spec.schema_path.exists().then(|| spec.schema_path.clone()), wasm_path: spec.wasm_path.clone(), function_runner_path: runner_path,
    }
}

pub async fn build(spec: &FunctionSpec, supervisor: &Supervisor) -> Result<Option<ProcessOutput>> {
    let Some(command) = spec.build.command.as_deref() else {
        return if spec.wasm_path.is_file() { Ok(None) } else { Err(FunctionsError::MissingBuild { wasm: spec.wasm_path.clone() }) };
    };
    let (program, args) = shell_command(command);
    let output = supervisor.spawn(ProcessSpec::new(program.clone()).args(args).current_dir(&spec.directory).output(OutputMode::Inherit))?.wait_with_signal_forwarding().await?;
    ensure_success(&program, &output.status)?;
    if !spec.wasm_path.is_file() { return Err(FunctionsError::MissingBuildOutput(spec.wasm_path.clone())); }
    Ok(Some(output))
}

pub fn typegen_process_spec(spec: &FunctionSpec) -> ProcessSpec {
    if let Some(command) = spec.build.typegen_command.as_deref() {
        let (program, args) = shell_command(command);
        return ProcessSpec::new(program).args(args).current_dir(&spec.directory).output(OutputMode::Inherit);
    }
    let program = discover_typegen_program(&spec.directory);
    let args = match program.as_str() {
        "bunx" => vec!["graphql-code-generator", "--config", "package.json"],
        "pnpm" | "yarn" => vec!["exec", "graphql-code-generator", "--config", "package.json"],
        _ => vec!["graphql-code-generator", "--config", "package.json"],
    };
    ProcessSpec::new(program).args(args).current_dir(&spec.directory).output(OutputMode::Inherit)
}

pub async fn typegen(spec: &FunctionSpec, supervisor: &Supervisor) -> Result<ProcessOutput> {
    let process = typegen_process_spec(spec);
    let program = process.program.clone();
    let output = supervisor.spawn(process)?.wait_with_signal_forwarding().await?;
    ensure_success(&program, &output.status)?;
    Ok(output)
}

pub fn discover_typegen_program(directory: &Path) -> String {
    for ancestor in directory.ancestors() {
        if ancestor.join("bun.lock").is_file() || ancestor.join("bun.lockb").is_file() { return "bunx".into(); }
        if ancestor.join("pnpm-lock.yaml").is_file() { return "pnpm".into(); }
        if ancestor.join("yarn.lock").is_file() { return "yarn".into(); }
    }
    "npx".into()
}

fn shell_command(command: &str) -> (String, Vec<String>) {
    if cfg!(windows) { ("cmd".into(), vec!["/C".into(), command.into()]) }
    else { ("sh".into(), vec!["-c".into(), command.into()]) }
}

fn ensure_success(program: &str, status: &ExitStatus) -> Result<()> {
    if status.success() { return Ok(()); }
    #[cfg(unix)]
    let description = { use std::os::unix::process::ExitStatusExt; status.code().map(|c| format!("exit code {c}")).or_else(|| status.signal().map(|s| format!("signal {s}"))).unwrap_or_else(|| "unknown status".into()) };
    #[cfg(not(unix))]
    let description = status.code().map(|c| format!("exit code {c}")).unwrap_or_else(|| "terminated by signal".into());
    Err(FunctionsError::ProcessFailed { program: program.into(), description })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerPlatform { MacArm, MacX86_64, LinuxArm, LinuxX86_64, WindowsX86_64 }

pub fn runner_download_url(platform: RunnerPlatform) -> String {
    let target = match platform { RunnerPlatform::MacArm => "arm-macos", RunnerPlatform::MacX86_64 => "x86_64-macos", RunnerPlatform::LinuxArm => "arm-linux", RunnerPlatform::LinuxX86_64 => "x86_64-linux", RunnerPlatform::WindowsX86_64 => "x86_64-windows" };
    format!("https://github.com/Shopify/function-runner/releases/download/v{FUNCTION_RUNNER_VERSION}/function-runner-{target}-v{FUNCTION_RUNNER_VERSION}.gz")
}

pub fn current_runner_platform() -> Result<RunnerPlatform> { runner_platform(env::consts::OS, env::consts::ARCH) }
pub fn runner_platform(os: &str, arch: &str) -> Result<RunnerPlatform> {
    match (os, arch) {
        ("macos", "aarch64") => Ok(RunnerPlatform::MacArm), ("macos", "x86_64") => Ok(RunnerPlatform::MacX86_64),
        ("linux", "aarch64") => Ok(RunnerPlatform::LinuxArm), ("linux", "x86_64") => Ok(RunnerPlatform::LinuxX86_64),
        ("windows", "x86_64") => Ok(RunnerPlatform::WindowsX86_64),
        _ => Err(FunctionsError::UnsupportedPlatform { os: os.into(), arch: arch.into() }),
    }
}

pub fn cached_runner_path(cache_root: &Path) -> PathBuf {
    let name = if cfg!(windows) { format!("function-runner-{FUNCTION_RUNNER_VERSION}.exe") } else { format!("function-runner-{FUNCTION_RUNNER_VERSION}") };
    cache_root.join("functions").join(name)
}

pub fn runner_override_from_env() -> Result<Option<PathBuf>> {
    let Some(value) = env::var_os("CFY_FUNCTION_RUNNER_BIN") else { return Ok(None) };
    let path = PathBuf::from(value);
    if !path.is_file() { return Err(FunctionsError::InvalidRunnerOverride(path)); }
    Ok(Some(path))
}

pub fn resolve_runner(cache_root: &Path) -> Result<PathBuf> {
    resolve_runner_with_override(cache_root, env::var_os("CFY_FUNCTION_RUNNER_BIN").as_deref())
}

pub fn resolve_runner_with_override(cache_root: &Path, override_path: Option<&OsStr>) -> Result<PathBuf> {
    if let Some(value) = override_path {
        let path = PathBuf::from(value);
        if !path.is_file() { return Err(FunctionsError::InvalidRunnerOverride(path)); }
        return Ok(path);
    }
    let destination = cached_runner_path(cache_root);
    if destination.is_file() { return Ok(destination); }
    download_runner(&destination, &runner_download_url(current_runner_platform()?))?;
    Ok(destination)
}

pub fn download_runner(destination: &Path, url: &str) -> Result<()> {
    let parsed = Url::parse(url).map_err(|e| FunctionsError::RunnerDownload { url: url.into(), message: e.to_string() })?;
    if !matches!(parsed.scheme(), "https" | "http") || parsed.path_segments().into_iter().flatten().any(|p| p == "..") {
        return Err(FunctionsError::RunnerDownload { url: url.into(), message: "unsafe download URL".into() });
    }
    install_crypto_provider();
    let response = reqwest::blocking::Client::builder().build().map_err(|e| FunctionsError::RunnerDownload { url: url.into(), message: e.to_string() })?.get(parsed).send().map_err(|e| FunctionsError::RunnerDownload { url: url.into(), message: e.to_string() })?;
    if !response.status().is_success() { return Err(FunctionsError::RunnerDownload { url: url.into(), message: format!("HTTP {}", response.status()) }); }
    let parent = destination.parent().ok_or_else(|| FunctionsError::InvalidConfig("runner cache path has no parent".into()))?;
    fs::create_dir_all(parent).map_err(|source| io_error("creating runner cache", source))?;
    let temp = sibling_temp_path(destination);
    let result = (|| -> Result<()> {
        let mut decoder = GzDecoder::new(response);
        let mut output = File::create(&temp).map_err(|source| io_error("creating runner temporary file", source))?;
        io::copy(&mut decoder, &mut output).map_err(|source| io_error("decompressing function-runner", source))?;
        output.sync_all().map_err(|source| io_error("syncing function-runner", source))?;
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(&temp, fs::Permissions::from_mode(0o755)).map_err(|source| io_error("marking function-runner executable", source))?; }
        atomic_replace(&temp, destination).map_err(|source| io_error("installing function-runner", source))
    })();
    if result.is_err() { let _ = fs::remove_file(&temp); }
    result
}

fn sibling_temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_else(|| OsStr::new("function-runner")).to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)] { if destination.exists() { fs::remove_file(destination)?; } }
    fs::rename(source, destination)
}
fn io_error(context: impl Into<String>, source: io::Error) -> FunctionsError { FunctionsError::Io { context: context.into(), source } }

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    pub input: Option<PathBuf>, pub export: Option<String>, pub json: bool, pub profile: bool,
    pub schema_path: Option<PathBuf>, pub query_path: Option<PathBuf>,
}

pub fn run_process_spec(spec: &FunctionSpec, runner: &Path, options: &RunOptions) -> Result<ProcessSpec> {
    if options.schema_path.is_some() != options.query_path.is_some() { return Err(FunctionsError::InvalidConfig("schema_path and query_path must be supplied together".into())); }
    let mut args = vec!["-f".into(), spec.wasm_path.to_string_lossy().into_owned()];
    if let Some(input) = &options.input { args.extend(["--input".into(), input.to_string_lossy().into_owned()]); }
    if let Some(export) = &options.export { args.extend(["--export".into(), export.clone()]); }
    if options.json { args.push("--json".into()); } if options.profile { args.push("--profile".into()); }
    if let (Some(schema), Some(query)) = (&options.schema_path, &options.query_path) { args.extend(["--schema-path".into(), schema.to_string_lossy().into_owned(), "--query-path".into(), query.to_string_lossy().into_owned()]); }
    Ok(ProcessSpec::new(runner.to_string_lossy().into_owned()).args(args).current_dir(&spec.directory).output(OutputMode::Inherit))
}

pub async fn run(spec: &FunctionSpec, runner: &Path, options: &RunOptions, supervisor: &Supervisor) -> Result<ProcessOutput> {
    let output = supervisor.spawn(run_process_spec(spec, runner, options)?)?.wait_with_signal_forwarding().await?;
    ensure_success(&runner.to_string_lossy(), &output.status)?; Ok(output)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogFileMetadata { pub namespace: String, pub function_handle: String, pub identifier: String }

pub fn parse_log_filename(filename: &str) -> Option<LogFileMetadata> {
    let fields: Vec<_> = filename.split(&['_', '.'][..]).collect();
    (fields.len() >= 6).then(|| LogFileMetadata { namespace: fields[3].into(), function_handle: fields[4].into(), identifier: fields[5].into() })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionRunData {
    #[serde(default)] pub payload: FunctionRunPayload,
    #[serde(default)] pub identifier: String,
    #[serde(flatten)] pub other: BTreeMap<String, JsonValue>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FunctionRunPayload {
    pub input: Option<JsonValue>, #[serde(rename = "export")] pub export_name: Option<String>, #[serde(flatten)] pub other: BTreeMap<String, JsonValue>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayChoice { pub identifier: String, pub path: PathBuf }
#[derive(Clone, Debug)]
pub struct ReplayRun { pub path: PathBuf, pub data: FunctionRunData }

pub fn list_replay_runs(log_directory: &Path, function_handle: &str) -> Result<Vec<ReplayRun>> {
    if !log_directory.is_dir() { return Ok(Vec::new()); }
    let mut names: Vec<_> = fs::read_dir(log_directory).map_err(|source| io_error("reading function logs", source))?
        .filter_map(std::result::Result::ok).filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false)).map(|entry| entry.file_name()).collect();
    names.sort(); names.reverse();
    let mut runs = Vec::new();
    for name in names {
        if runs.len() >= LOG_SELECTOR_LIMIT { break; }
        let name = name.to_string_lossy();
        let Some(meta) = parse_log_filename(&name) else { continue };
        if meta.namespace != "extensions" || meta.function_handle != function_handle { continue; }
        let path = log_directory.join(name.as_ref());
        let text = fs::read_to_string(&path).map_err(|source| io_error("reading function log", source))?;
        let mut data: FunctionRunData = serde_json::from_str(&text).map_err(|e| FunctionsError::InvalidLog { path: path.clone(), message: e.to_string() })?;
        if data.payload.input.is_none() { continue; }
        data.identifier = meta.identifier;
        runs.push(ReplayRun { path, data });
    }
    Ok(runs)
}

pub fn select_replay_run(log_directory: &Path, function_handle: &str, identifier: Option<&str>) -> Result<ReplayRun> {
    let runs = list_replay_runs(log_directory, function_handle)?;
    if let Some(identifier) = identifier {
        return runs.into_iter().find(|run| run.data.identifier == identifier).ok_or_else(|| FunctionsError::LogNotFound { identifier: identifier.into(), directory: log_directory.into(), function_handle: function_handle.into() });
    }
    match runs.len() { 0 => Err(FunctionsError::NoReplayLogs(log_directory.into())), 1 => Ok(runs.into_iter().next().unwrap()), _ => Err(FunctionsError::ReplaySelectionRequired(runs.iter().map(|r| ReplayChoice { identifier: r.data.identifier.clone(), path: r.path.clone() }).collect())) }
}

pub fn replay_input(run: &ReplayRun) -> Result<(Vec<u8>, Option<String>)> {
    let input = run.data.payload.input.as_ref().ok_or_else(|| FunctionsError::InvalidLog { path: run.path.clone(), message: "payload.input is null".into() })?;
    Ok((serde_json::to_vec(input).map_err(|e| FunctionsError::InvalidLog { path: run.path.clone(), message: e.to_string() })?, run.data.payload.export_name.clone()))
}

pub fn replay_watch_paths(spec: &FunctionSpec, run: &ReplayRun) -> Vec<PathBuf> {
    let mut paths = vec![spec.directory.clone(), run.path.clone()]; paths.sort(); paths.dedup(); paths
}

#[derive(Clone)]
pub struct FunctionsApiClient { client: reqwest::blocking::Client, endpoint: Url, token: SecretToken }
#[derive(Clone)]
struct SecretToken(String);
impl fmt::Debug for SecretToken { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str("[REDACTED]") } }
impl fmt::Debug for FunctionsApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.debug_struct("FunctionsApiClient").field("endpoint", &self.endpoint).field("token", &self.token).finish() }
}

impl FunctionsApiClient {
    pub fn new(token: impl Into<String>, org_id: &str, app_id: &str) -> Result<Self> { Self::with_base(token, org_id, app_id, FUNCTIONS_API_BASE) }
    pub fn with_base(token: impl Into<String>, org_id: &str, app_id: &str, base: &str) -> Result<Self> {
        let base = base.trim_end_matches('/');
        let endpoint = Url::parse(&format!("{base}/organizations/{org_id}/{app_id}/graphql")).map_err(|e| FunctionsError::Api(format!("invalid Functions API endpoint: {e}")))?;
        install_crypto_provider();
        let client = reqwest::blocking::Client::builder().build().map_err(|e| FunctionsError::Api(e.to_string()))?;
        Ok(Self { client, endpoint, token: SecretToken(token.into()) })
    }
    pub fn fetch_schema(&self, spec: &FunctionSpec) -> Result<String> {
        let version = spec.api_version.as_deref().ok_or_else(|| FunctionsError::InvalidConfig("function api_version is required to fetch a schema".into()))?;
        let (query, variables, field_path) = if let Some(target) = spec.targeting.first() {
            (SCHEMA_BY_TARGET, json!({"handle": target.target, "version": version}), &["target", "api", "schema", "definition"][..])
        } else {
            let extension_type = spec.extension_type.as_deref().ok_or_else(|| FunctionsError::InvalidConfig("function type is required to fetch a schema".into()))?;
            (SCHEMA_BY_API_TYPE, json!({"type": extension_type, "version": version}), &["api", "schema", "definition"][..])
        };
        let response = self.client.post(self.endpoint.clone()).bearer_auth(&self.token.0).json(&json!({"query": query, "variables": variables})).send().map_err(|e| FunctionsError::Api(redact(&e.to_string(), &self.token.0)))?;
        let status = response.status();
        let body: JsonValue = response.json().map_err(|e| FunctionsError::Api(redact(&format!("HTTP {status}: invalid JSON response: {e}"), &self.token.0)))?;
        if !status.is_success() { return Err(FunctionsError::Api(redact(&format!("HTTP {status}: {body}"), &self.token.0))); }
        if let Some(errors) = body.get("errors") { return Err(FunctionsError::Api(redact(&format!("GraphQL errors: {errors}"), &self.token.0))); }
        let mut definition = body.get("data");
        for field in field_path { definition = definition.and_then(|value| value.get(field)); }
        let definition = definition.and_then(JsonValue::as_str).ok_or_else(|| FunctionsError::Api("the Functions API returned no schema; check the Function target/type and version".into()))?;
        Ok(format!("# schema-version: {version}\n{definition}"))
    }
    pub fn write_schema(&self, spec: &FunctionSpec, stdout: bool) -> Result<String> {
        let schema = self.fetch_schema(spec)?;
        if !stdout { atomic_write(&spec.schema_path, schema.as_bytes())?; }
        Ok(schema)
    }
}

fn install_crypto_provider() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn redact(message: &str, token: &str) -> String { if token.is_empty() { message.into() } else { message.replace(token, "[REDACTED]") } }
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| FunctionsError::InvalidConfig(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(parent).map_err(|source| io_error("creating output directory", source))?;
    let temp = sibling_temp_path(path);
    let result = (|| -> Result<()> { let mut file = File::create(&temp).map_err(|source| io_error("creating temporary output", source))?; file.write_all(contents).map_err(|source| io_error("writing temporary output", source))?; file.sync_all().map_err(|source| io_error("syncing temporary output", source))?; atomic_replace(&temp, path).map_err(|source| io_error("installing output", source)) })();
    if result.is_err() { let _ = fs::remove_file(temp); } result
}

/// Returns false for absolute paths or paths containing parent/root components.
pub fn is_safe_relative_path(path: &Path) -> bool {
    !path.is_absolute() && path.components().all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}
