//! Versioned protocol and supervised runner for extension build adapters.

use cfy_core::{Error, ErrorKind, Result};
use cfy_process::{OutputMode, ProcessSpec, Supervisor};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{sync::Semaphore, task::JoinSet};

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_ERROR_DETAIL_BYTES: usize = 8 * 1024;
pub const INFO_ARGUMENT: &str = "--cfy-adapter-info";
pub const BUILD_ARGUMENT: &str = "--cfy-build-adapter";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterCommand {
    pub program: PathBuf,
    pub arguments: Vec<String>,
}

impl AdapterCommand {
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
        }
    }

    #[must_use]
    pub fn arguments(mut self, arguments: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AdapterInfo {
    pub protocol_version: u32,
    pub name: String,
    pub adapter_version: String,
    #[serde(default)]
    pub extension_types: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BuildRequest {
    pub protocol_version: u32,
    pub extension_type: String,
    pub extension_dir: PathBuf,
    pub output_dir: PathBuf,
    #[serde(default)]
    pub configuration: serde_json::Value,
}

impl BuildRequest {
    #[must_use]
    pub fn new(
        extension_type: impl Into<String>,
        extension_dir: impl Into<PathBuf>,
        output_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            extension_type: extension_type.into(),
            extension_dir: extension_dir.into(),
            output_dir: output_dir.into(),
            configuration: serde_json::Value::Null,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct BuildResponse {
    pub protocol_version: u32,
    #[serde(default)]
    pub artifacts: Vec<PathBuf>,
    #[serde(default)]
    pub diagnostics: Vec<AdapterDiagnostic>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AdapterDiagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct Adapter {
    command: AdapterCommand,
    info: AdapterInfo,
}

impl Adapter {
    #[must_use]
    pub fn info(&self) -> &AdapterInfo {
        &self.info
    }

    pub async fn discover(
        supervisor: &Supervisor,
        command: AdapterCommand,
        required_version: Option<&VersionReq>,
    ) -> Result<Self> {
        let executable = find_executable(&command.program).ok_or_else(|| {
            Error::config(format!(
                "extension adapter `{}` was not found; install it or configure an absolute executable path",
                command.program.display()
            ))
        })?;
        let mut discovered = command;
        discovered.program = executable;
        let output = run(supervisor, &discovered, INFO_ARGUMENT, None).await?;
        let info: AdapterInfo = decode_output(&discovered, "discovery", &output)?;
        check_protocol(&discovered, info.protocol_version, "discovery")?;
        let version = Version::parse(&info.adapter_version).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!(
                    "extension adapter `{}` reported invalid version `{}`; adapters must report a semantic version",
                    info.name, info.adapter_version
                ),
                error,
            )
        })?;
        if let Some(required) = required_version
            && !required.matches(&version)
        {
            return Err(Error::config(format!(
                "extension adapter `{}` version {} does not satisfy required version {}; install a compatible adapter or update the configured requirement",
                info.name, version, required
            )));
        }
        Ok(Self {
            command: discovered,
            info,
        })
    }

    pub async fn build(
        &self,
        supervisor: &Supervisor,
        request: &BuildRequest,
    ) -> Result<BuildResponse> {
        if request.protocol_version != PROTOCOL_VERSION {
            return Err(Error::invalid_input(format!(
                "build request uses protocol version {}, but this CLI supports version {PROTOCOL_VERSION}",
                request.protocol_version
            )));
        }
        if !self.info.extension_types.is_empty()
            && !self.info.extension_types.contains(&request.extension_type)
        {
            return Err(Error::config(format!(
                "extension adapter `{}` does not support extension type `{}`; supported types: {}",
                self.info.name,
                request.extension_type,
                self.info.extension_types.join(", ")
            )));
        }
        let json = serde_json::to_string(request).map_err(|error| {
            Error::with_source(
                ErrorKind::InvalidInput,
                "could not encode extension adapter build request",
                error,
            )
        })?;
        let output = run(
            supervisor,
            &self.command,
            BUILD_ARGUMENT,
            Some(json.into_bytes()),
        )
        .await?;
        let response: BuildResponse = decode_output(&self.command, "build", &output)?;
        check_protocol(&self.command, response.protocol_version, "build response")?;
        Ok(response)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parallelism {
    pub max_jobs: usize,
    pub max_memory_mb: u32,
}

impl Parallelism {
    pub fn validate(self) -> Result<Self> {
        if self.max_jobs == 0 {
            return Err(Error::invalid_input("adapter max_jobs must be at least 1"));
        }
        if self.max_memory_mb == 0 {
            return Err(Error::invalid_input(
                "adapter max_memory_mb must be at least 1",
            ));
        }
        Ok(self)
    }
}

#[derive(Clone, Debug)]
pub struct BuildJob {
    pub request: BuildRequest,
    pub memory_mb: u32,
}

pub async fn build_all(
    supervisor: &Supervisor,
    adapter: &Adapter,
    jobs: Vec<BuildJob>,
    parallelism: Parallelism,
) -> Result<Vec<BuildResponse>> {
    let parallelism = parallelism.validate()?;
    for job in &jobs {
        if job.memory_mb == 0 {
            return Err(Error::invalid_input(
                "an adapter build job's memory_mb must be at least 1",
            ));
        }
        if job.memory_mb > parallelism.max_memory_mb {
            return Err(Error::invalid_input(format!(
                "adapter build job for `{}` requires {} MiB, exceeding the configured {} MiB limit; increase max_memory_mb or lower the job estimate",
                job.request.extension_type, job.memory_mb, parallelism.max_memory_mb
            )));
        }
    }

    let job_slots = Arc::new(Semaphore::new(parallelism.max_jobs));
    let memory = Arc::new(Semaphore::new(parallelism.max_memory_mb as usize));
    let mut tasks = JoinSet::new();
    let result_count = jobs.len();
    for (index, job) in jobs.into_iter().enumerate() {
        let job_slots = Arc::clone(&job_slots);
        let memory = Arc::clone(&memory);
        let supervisor = supervisor.clone();
        let adapter = adapter.clone();
        tasks.spawn(async move {
            let _job_slot = job_slots.acquire_owned().await.map_err(|_| {
                Error::process("extension adapter job scheduler closed unexpectedly")
            })?;
            let _memory = memory
                .acquire_many_owned(job.memory_mb)
                .await
                .map_err(|_| {
                    Error::process("extension adapter memory scheduler closed unexpectedly")
                })?;
            let result = adapter.build(&supervisor, &job.request).await;
            Ok::<_, Error>((index, result))
        });
    }

    let mut responses = (0..result_count).map(|_| None).collect::<Vec<_>>();
    let mut first_error = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok((index, Ok(response)))) => responses[index] = Some(response),
            Ok(Ok((_, Err(error)))) | Ok(Err(error)) => {
                first_error.get_or_insert(error);
            }
            Err(error) => {
                first_error.get_or_insert_with(|| {
                    Error::with_source(
                        ErrorKind::Process,
                        "extension adapter build task failed",
                        error,
                    )
                });
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(responses
        .into_iter()
        .map(|response| response.expect("successful adapter task did not store its response"))
        .collect())
}

async fn run(
    supervisor: &Supervisor,
    command: &AdapterCommand,
    protocol_argument: &str,
    request: Option<Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut arguments = command.arguments.clone();
    arguments.push(protocol_argument.into());
    let mut spec = ProcessSpec::new(command.program.to_string_lossy())
        .args(arguments)
        .output(OutputMode::Capture);
    if let Some(request) = request {
        spec = spec.stdin(request);
    }
    let output = supervisor
        .spawn(spec)?
        .wait_with_signal_forwarding()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(
            &output.stderr[..output.stderr.len().min(MAX_ERROR_DETAIL_BYTES)],
        );
        let detail = stderr.trim();
        return Err(Error::process(if detail.is_empty() {
            format!(
                "extension adapter `{}` failed during {protocol_argument} with status {}",
                command.program.display(),
                output.status
            )
        } else {
            format!(
                "extension adapter `{}` failed during {protocol_argument}: {detail}",
                command.program.display()
            )
        }));
    }
    Ok(output.stdout)
}

fn decode_output<T: DeserializeOwned>(
    command: &AdapterCommand,
    operation: &str,
    output: &[u8],
) -> Result<T> {
    serde_json::from_slice(output).map_err(|error| {
        Error::with_source(
            ErrorKind::Process,
            format!(
                "extension adapter `{}` returned invalid JSON for {operation}; ensure it implements protocol version {PROTOCOL_VERSION} and writes only its response to stdout",
                command.program.display()
            ),
            error,
        )
    })
}

fn check_protocol(command: &AdapterCommand, actual: u32, operation: &str) -> Result<()> {
    if actual == PROTOCOL_VERSION {
        return Ok(());
    }
    Err(Error::config(format!(
        "extension adapter `{}` returned protocol version {actual} for {operation}, but this CLI supports version {PROTOCOL_VERSION}; install a compatible adapter",
        command.program.display()
    )))
}

fn find_executable(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 || program.is_absolute() {
        return program.is_file().then(|| program.to_path_buf());
    }
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        executable_candidates(&directory.join(program))
            .into_iter()
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(windows)]
fn executable_candidates(path: &Path) -> Vec<PathBuf> {
    if path.extension().is_some() {
        return vec![path.to_path_buf()];
    }
    env::var_os("PATHEXT")
        .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
        .to_string_lossy()
        .split(';')
        .map(|extension| path.with_extension(extension.trim_start_matches('.')))
        .collect()
}

#[cfg(not(windows))]
fn executable_candidates(path: &Path) -> Vec<PathBuf> {
    vec![path.to_path_buf()]
}
