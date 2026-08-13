//! Stable domain primitives shared by Crabpify crates.

use std::{error::Error as StdError, process::ExitCode};
use thiserror::Error;

pub type BoxError = Box<dyn StdError + Send + Sync + 'static>;

/// Stable error categories exposed at the command boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    InvalidInput,
    Config,
    Process,
    Api,
}

impl ErrorKind {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Config => "config",
            Self::Process => "process",
            Self::Api => "api",
        }
    }

    #[must_use]
    pub const fn exit_status(self) -> u8 {
        match self {
            Self::InvalidInput | Self::Config => 2,
            Self::Process | Self::Api => 1,
        }
    }
}

/// Errors crossing a command boundary while retaining their underlying cause.
#[derive(Debug, Error)]
#[error("{kind}: {message}")]
pub struct Error {
    kind: ErrorKind,
    message: String,
    #[source]
    source: Option<BoxError>,
}

impl Error {
    #[must_use]
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    #[must_use]
    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }

    #[must_use]
    pub fn process(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Process, message)
    }

    #[must_use]
    pub fn api(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Api, message)
    }

    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    #[must_use]
    pub fn with_source<E>(kind: ErrorKind, message: impl Into<String>, source: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.kind
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.kind.exit_status())
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::InvalidInput => "invalid input",
            Self::Config => "configuration error",
            Self::Process => "external process failed",
            Self::Api => "Shopify API request failed",
        };
        formatter.write_str(label)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::{Error, ErrorKind};
    use std::{error::Error as _, io};

    #[test]
    fn invalid_input_uses_cli_usage_exit_code() {
        assert_eq!(Error::invalid_input("x").exit_code(), 2.into());
    }

    #[test]
    fn errors_retain_kind_and_source() {
        let error = Error::with_source(
            ErrorKind::Config,
            "could not read config",
            io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        );

        assert_eq!(error.kind(), ErrorKind::Config);
        assert_eq!(error.source().unwrap().to_string(), "denied");
    }
}
