//! Explicit subprocess boundary for Node, esbuild, cloudflared, and other tools.

use cfy_core::{Error, Result};
use std::process::ExitStatus;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
}

pub async fn run(spec: &ProcessSpec) -> Result<ExitStatus> {
    Command::new(&spec.program)
        .args(&spec.args)
        .status()
        .await
        .map_err(|error| Error::Process(format!("{}: {error}", spec.program)))
}
