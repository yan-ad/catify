//! Store command contracts: selection, safety, progress, and adapters.

use async_trait::async_trait;
use cfy_core::{Cancellation, Error, ErrorKind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error as ThisError;
use url::Url;

type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StoreTarget {
    pub handle: String,
    pub domain: String,
}

impl StoreTarget {
    pub fn parse(value: &str) -> Result<Self> {
        let trimmed = value.trim().trim_end_matches('/');
        if trimmed.is_empty() || trimmed.contains('/') || trimmed.contains('@') {
            return Err(StoreError::InvalidTarget(value.into()));
        }
        let domain = if trimmed.ends_with(".myshopify.com") {
            trimmed.to_owned()
        } else {
            format!("{trimmed}.myshopify.com")
        };
        let handle = domain.trim_end_matches(".myshopify.com").to_owned();
        if handle.is_empty()
            || !handle
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(StoreError::InvalidTarget(value.into()));
        }
        Ok(Self { handle, domain })
    }

    pub fn admin_url(&self) -> Url {
        Url::parse(&format!("https://{}/admin", self.domain)).expect("normalized Shopify domain")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfirmationPolicy {
    pub non_interactive: bool,
    pub confirm: bool,
    pub destructive: bool,
}

impl ConfirmationPolicy {
    pub fn authorize(&self) -> Result<()> {
        if !self.destructive || self.confirm {
            return Ok(());
        }
        if self.non_interactive {
            return Err(StoreError::ConfirmationRequired(
                "destructive store operation requires --confirm in non-interactive mode".into(),
            ));
        }
        Err(StoreError::ConfirmationRequired(
            "destructive store operation requires explicit confirmation".into(),
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProgressEvent {
    pub operation: String,
    pub completed: usize,
    pub total: usize,
    pub failed: usize,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PartialFailure {
    pub item: String,
    pub error: String,
    pub retryable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BulkReport {
    pub operation: String,
    pub completed: usize,
    pub failed: Vec<PartialFailure>,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StoreCommand {
    Auth,
    AuthList,
    List,
    Info,
    Open,
    Execute,
    Graphiql,
    CreateDev,
    CreatePreview,
    Delete,
    BulkExecute,
    BulkStatus,
    BulkCancel,
    StripeAuth,
}

#[derive(Debug, ThisError)]
pub enum StoreError {
    #[error("invalid Shopify store target `{0}`; use a handle or *.myshopify.com domain")]
    InvalidTarget(String),
    #[error("{0}")]
    ConfirmationRequired(String),
    #[error(
        "browser operation unavailable in headless mode; use the printed URL or an API-backed command"
    )]
    HeadlessBrowser,
    #[error("store command `{0:?}` is not supported by the configured backend")]
    Unsupported(StoreCommand),
    #[error("store operation failed: {0}")]
    Backend(String),
}

impl From<StoreError> for Error {
    fn from(error: StoreError) -> Self {
        let kind = match error {
            StoreError::InvalidTarget(_) | StoreError::ConfirmationRequired(_) => ErrorKind::Config,
            StoreError::HeadlessBrowser | StoreError::Unsupported(_) | StoreError::Backend(_) => {
                ErrorKind::Api
            }
        };
        Error::new(kind, error.to_string())
    }
}

#[async_trait]
pub trait StoreBackend: Send + Sync {
    async fn info(&self, target: &StoreTarget) -> Result<HashMap<String, String>>;
    async fn execute(&self, target: &StoreTarget, query: &str) -> Result<serde_json::Value>;
    async fn bulk_execute(
        &self,
        target: &StoreTarget,
        items: &[String],
        progress: &mut (dyn FnMut(ProgressEvent) + Send),
        cancellation: &Cancellation,
    ) -> Result<BulkReport>;
}

pub fn browser_url(command: StoreCommand, target: &StoreTarget, headless: bool) -> Result<Url> {
    if headless {
        return Err(StoreError::HeadlessBrowser);
    }
    let path = match command {
        StoreCommand::Open => "/",
        StoreCommand::Graphiql => "/admin/api/graphiql",
        _ => return Err(StoreError::Unsupported(command)),
    };
    Ok(Url::parse(&format!("https://{}{}", target.domain, path)).expect("normalized Shopify URL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_handles_and_rejects_urls() {
        let target = StoreTarget::parse("demo").unwrap();
        assert_eq!(target.domain, "demo.myshopify.com");
        assert_eq!(
            StoreTarget::parse("demo.myshopify.com").unwrap().handle,
            "demo"
        );
        assert!(matches!(
            StoreTarget::parse("https://demo.myshopify.com"),
            Err(StoreError::InvalidTarget(_))
        ));
    }

    #[test]
    fn destructive_operations_require_explicit_confirmation() {
        assert!(
            ConfirmationPolicy {
                non_interactive: true,
                confirm: false,
                destructive: true
            }
            .authorize()
            .is_err()
        );
        assert!(
            ConfirmationPolicy {
                non_interactive: true,
                confirm: true,
                destructive: true
            }
            .authorize()
            .is_ok()
        );
        assert!(
            ConfirmationPolicy {
                non_interactive: true,
                confirm: false,
                destructive: false
            }
            .authorize()
            .is_ok()
        );
    }

    #[test]
    fn browser_commands_degrade_headlessly() {
        let target = StoreTarget::parse("demo").unwrap();
        assert!(matches!(
            browser_url(StoreCommand::Open, &target, true),
            Err(StoreError::HeadlessBrowser)
        ));
        assert_eq!(
            browser_url(StoreCommand::Graphiql, &target, false)
                .unwrap()
                .path(),
            "/admin/api/graphiql"
        );
    }
}
