//! Shopify API ports. Concrete HTTP clients are adapters added per domain.

use async_trait::async_trait;
use cfy_core::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphQlRequest {
    pub operation_name: String,
    pub document: String,
    pub variables: String,
}

#[async_trait]
pub trait GraphQlClient: Send + Sync {
    async fn execute(&self, request: GraphQlRequest) -> Result<String>;
}
