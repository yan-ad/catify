//! Stable domain primitives shared by Crabpify crates.

use std::process::ExitCode;
use thiserror::Error;

/// Errors crossing a command boundary.
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("external process failed: {0}")]
    Process(String),
    #[error("Shopify API request failed: {0}")]
    Api(String),
}

impl Error {
    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::InvalidInput(_) | Self::Config(_) => ExitCode::from(2),
            Self::Process(_) | Self::Api(_) => ExitCode::from(1),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn invalid_input_uses_cli_usage_exit_code() {
        assert_eq!(Error::InvalidInput("x".into()).exit_code(), 2.into());
    }
}
