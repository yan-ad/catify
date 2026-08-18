use std::{env, path::PathBuf, time::Duration};

use cfy_core::{Error, ErrorKind, Result};
use cfy_process::{OutputMode, ProcessOutput, ProcessSpec, Supervisor};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HydrogenError {
    #[error(
        "Hydrogen tooling is not installed; install @shopify/cli-hydrogen or set CFY_HYDROGEN_BIN"
    )]
    NotInstalled,
    #[error("Hydrogen executable path is invalid: {0}")]
    InvalidExecutable(String),
    #[error("Hydrogen command failed: {0}")]
    Process(String),
}

impl From<HydrogenError> for Error {
    fn from(error: HydrogenError) -> Self {
        Error::with_source(ErrorKind::Process, "Hydrogen adapter failed", error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HydrogenTool {
    pub executable: PathBuf,
    pub version: Option<String>,
}

impl HydrogenTool {
    pub fn discover() -> std::result::Result<Self, HydrogenError> {
        if let Some(path) = env::var_os("CFY_HYDROGEN_BIN") {
            let path = PathBuf::from(path);
            if path.as_os_str().is_empty() {
                return Err(HydrogenError::InvalidExecutable("empty path".into()));
            }
            return Ok(Self {
                executable: path,
                version: None,
            });
        }
        for candidate in ["shopify", "npx"] {
            if which(candidate).is_some() {
                return Ok(Self {
                    executable: PathBuf::from(candidate),
                    version: None,
                });
            }
        }
        Err(HydrogenError::NotInstalled)
    }

    pub fn command_args(&self, args: &[String]) -> Vec<String> {
        if self.executable.file_name().and_then(|x| x.to_str()) == Some("npx") {
            let mut command = vec!["--no-install".into(), "shopify".into(), "hydrogen".into()];
            command.extend(args.iter().cloned());
            command
        } else if self.executable.file_name().and_then(|x| x.to_str()) == Some("shopify") {
            let mut command = vec!["hydrogen".into()];
            command.extend(args.iter().cloned());
            command
        } else {
            args.to_vec()
        }
    }

    pub async fn run(&self, args: &[String], supervisor: &Supervisor) -> Result<ProcessOutput> {
        let process = supervisor.spawn(
            ProcessSpec::new(self.executable.to_string_lossy())
                .args(self.command_args(args))
                .output(OutputMode::CaptureAndStream),
        )?;
        process.wait().await
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub async fn run(args: &[String]) -> Result<i32> {
    let tool = HydrogenTool::discover()?;
    let supervisor = Supervisor::new(Duration::from_secs(2));
    let output = tool.run(args, &supervisor).await?;
    Ok(output.exit_code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_shopify_and_npx_invocations() {
        let shopify = HydrogenTool {
            executable: PathBuf::from("shopify"),
            version: None,
        };
        assert_eq!(
            shopify.command_args(&["build".into()]),
            vec!["hydrogen", "build"]
        );
        let npx = HydrogenTool {
            executable: PathBuf::from("npx"),
            version: None,
        };
        assert_eq!(
            npx.command_args(&["dev".into()]),
            vec!["--no-install", "shopify", "hydrogen", "dev"]
        );
    }
}
