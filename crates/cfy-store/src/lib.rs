//! Store command contracts: selection, safety, progress, and adapters.

use async_trait::async_trait;
use cfy_api::{GraphQlClient, GraphQlRequest, HttpClient};
use cfy_app::exchange_business_platform_token;
use cfy_auth::Session;
use cfy_core::{Cancellation, Error, ErrorKind};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error as ThisError;
use url::Url;

type Result<T> = std::result::Result<T, StoreError>;

const STORE_LIST_QUERY: &str = r#"
query ListAccessibleShops($first: Int!) {
  organization {
    id
    name
    accessibleShops(
      first: $first
      sort: SHOP_CREATED_AT_DESC
      filters: [{field: STORE_STATUS, operator: EQUALS, value: "active"}]
    ) {
      edges {
        node { id shopifyShopId name storeType primaryDomain url createdAt }
      }
      pageInfo { hasNextPage }
    }
  }
}
"#;

pub const STORE_LIST_LIMIT: usize = 250;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrganizationStore {
    pub id: Option<String>,
    pub store: String,
    pub created_at: String,
    pub organization_id: String,
    pub organization_name: String,
    pub name: Option<String>,
    pub store_type: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrganizationStoreList {
    pub stores: Vec<OrganizationStore>,
    pub organization_id: String,
    pub organization_name: String,
    pub truncated: bool,
}

pub struct OrganizationStoreClient {
    graphql: GraphQlClient,
    organization_id: String,
}

impl OrganizationStoreClient {
    pub async fn from_session(session: &Session, organization_id: &str) -> Result<Self> {
        let token = exchange_business_platform_token(session)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let endpoint = std::env::var("CFY_BUSINESS_PLATFORM_ORGANIZATIONS_URL")
            .unwrap_or_else(|_| {
                format!(
                    "https://destinations.shopifysvc.com/organizations/api/unstable/organization/{organization_id}/graphql"
                )
            });
        Self::new(&endpoint, token.expose(), organization_id)
    }

    pub fn new(endpoint: &str, token: &str, organization_id: &str) -> Result<Self> {
        let url = Url::parse(endpoint).map_err(|error| {
            StoreError::Backend(format!("invalid organization API URL: {error}"))
        })?;
        if url.scheme() != "https"
            && url.host_str() != Some("127.0.0.1")
            && url.host_str() != Some("localhost")
        {
            return Err(StoreError::Backend(
                "organization API URL must use HTTPS".into(),
            ));
        }
        let base = format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or_default(),
            url.port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default()
        );
        let http = HttpClient::new(&base)
            .map_err(|error| StoreError::Backend(error.to_string()))?
            .with_sensitive_header(
                reqwest::header::HeaderName::from_static("authorization"),
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|error| StoreError::Backend(format!("invalid token: {error}")))?,
            );
        Ok(Self {
            graphql: GraphQlClient::new(http, url.path()),
            organization_id: organization_id.to_owned(),
        })
    }

    pub async fn list(&self) -> Result<OrganizationStoreList> {
        #[derive(Deserialize)]
        struct Data {
            organization: Option<Organization>,
        }
        #[derive(Deserialize)]
        struct Organization {
            name: String,
            #[serde(rename = "accessibleShops")]
            shops: Option<Connection>,
        }
        #[derive(Deserialize)]
        struct Connection {
            edges: Vec<Edge>,
            #[serde(rename = "pageInfo")]
            page_info: PageInfo,
        }
        #[derive(Deserialize)]
        struct Edge {
            node: Node,
        }
        #[derive(Deserialize)]
        struct Node {
            #[serde(rename = "shopifyShopId")]
            shopify_shop_id: Option<String>,
            name: Option<String>,
            #[serde(rename = "storeType")]
            store_type: Option<String>,
            #[serde(rename = "primaryDomain")]
            primary_domain: Option<String>,
            url: Option<String>,
            #[serde(rename = "createdAt")]
            created_at: String,
        }
        #[derive(Deserialize)]
        struct PageInfo {
            #[serde(rename = "hasNextPage")]
            has_next_page: bool,
        }

        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::query(
                STORE_LIST_QUERY,
                serde_json::json!({"first": STORE_LIST_LIMIT}),
            ))
            .await
            .map_err(|error| StoreError::Backend(format!("could not list stores: {error}")))?;
        let organization = response
            .data
            .organization
            .ok_or_else(|| StoreError::Backend("organization was not returned".into()))?;
        let connection = organization.shops.unwrap_or(Connection {
            edges: Vec::new(),
            page_info: PageInfo {
                has_next_page: false,
            },
        });
        let mut stores = connection
            .edges
            .into_iter()
            .filter_map(|edge| {
                let raw_store = edge.node.url.or(edge.node.primary_domain)?;
                let store = Url::parse(&raw_store)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned))
                    .unwrap_or(raw_store);
                Some(OrganizationStore {
                    id: edge
                        .node
                        .shopify_shop_id
                        .map(|id| format!("gid://shopify/Shop/{id}")),
                    store,
                    created_at: edge.node.created_at,
                    organization_id: self.organization_id.clone(),
                    organization_name: organization.name.clone(),
                    name: edge.node.name,
                    store_type: edge.node.store_type.map(normalize_store_type),
                })
            })
            .collect::<Vec<_>>();
        stores.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.store.cmp(&right.store))
        });
        let truncated = connection.page_info.has_next_page || stores.len() > STORE_LIST_LIMIT;
        stores.truncate(STORE_LIST_LIMIT);
        Ok(OrganizationStoreList {
            stores,
            organization_id: self.organization_id.clone(),
            organization_name: organization.name,
            truncated,
        })
    }
}

fn normalize_store_type(value: String) -> String {
    value.trim().to_ascii_lowercase().replace('_', "-")
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StoreTarget {
    pub handle: String,
    pub domain: String,
}

/// Partner/organization API boundary for store lifecycle commands.
/// It is intentionally separate from the Admin API backend because store
/// creation and deletion are not Admin API operations.
pub struct StoreManagementBackend {
    client: GraphQlClient,
}

impl StoreManagementBackend {
    pub fn new(base_url: &str, token: &str) -> std::result::Result<Self, StoreError> {
        let base = Url::parse(base_url)
            .map_err(|error| StoreError::Backend(format!("invalid partner API URL: {error}")))?;
        if base.scheme() != "https" {
            return Err(StoreError::Backend("partner API URL must use HTTPS".into()));
        }

        let http = HttpClient::new(base.as_str())
            .map_err(|error| StoreError::Backend(error.to_string()))?
            .with_sensitive_header(
                reqwest::header::HeaderName::from_static("authorization"),
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|error| StoreError::Backend(format!("invalid token: {error}")))?,
            );
        Ok(Self {
            client: GraphQlClient::new(http, base.path()),
        })
    }

    async fn mutation(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let request = GraphQlRequest::mutation(query, variables);
        self.client
            .execute::<_, serde_json::Value>(&request)
            .await
            .map(|response| response.data)
            .map_err(|error| StoreError::Backend(error.to_string()))
    }

    async fn query(&self, query: &str, variables: serde_json::Value) -> Result<serde_json::Value> {
        let request = GraphQlRequest::query(query, variables);
        self.client
            .execute::<_, serde_json::Value>(&request)
            .await
            .map(|response| response.data)
            .map_err(|error| StoreError::Backend(error.to_string()))
    }

    pub async fn create_development(&self, handle: &str) -> Result<serde_json::Value> {
        self.mutation(
            "mutation CreateDevelopmentStore($handle: String!) { developmentStoreCreate(input: { handle: $handle }) { store { id handle } userErrors { field message } } }",
            serde_json::json!({"handle": handle}),
        )
        .await
    }

    pub async fn create_preview(&self, handle: &str) -> Result<serde_json::Value> {
        self.mutation(
            "mutation CreatePreviewStore($handle: String!) { previewStoreCreate(input: { handle: $handle }) { store { id handle } userErrors { field message } } }",
            serde_json::json!({"handle": handle}),
        )
        .await
    }

    pub async fn delete(&self, store_id: &str) -> Result<serde_json::Value> {
        self.mutation(
            "mutation DeleteStore($id: ID!) { storeDelete(id: $id) { deletedStoreId userErrors { field message } } }",
            serde_json::json!({"id": store_id}),
        )
        .await
    }

    pub async fn bulk_status(&self, operation_id: &str) -> Result<serde_json::Value> {
        self.query(
            "query BulkOperationStatus($id: ID!) { bulkOperation(id: $id) { id status errorCode objectCount url } }",
            serde_json::json!({"id": operation_id}),
        )
        .await
    }

    pub async fn bulk_cancel(&self, operation_id: &str) -> Result<serde_json::Value> {
        self.mutation(
            "mutation BulkOperationCancel($id: ID!) { bulkOperationCancel(id: $id) { bulkOperation { id status } userErrors { field message } } }",
            serde_json::json!({"id": operation_id}),
        )
        .await
    }
}

pub struct AdminStoreBackend {
    client: GraphQlClient,
}

impl AdminStoreBackend {
    pub fn new(target: &StoreTarget, token: &str) -> std::result::Result<Self, StoreError> {
        let http = HttpClient::new(&format!("https://{}", target.domain))
            .map_err(|error| StoreError::Backend(error.to_string()))?
            .with_sensitive_header(
                reqwest::header::HeaderName::from_static("x-shopify-access-token"),
                reqwest::header::HeaderValue::from_str(token)
                    .map_err(|error| StoreError::Backend(format!("invalid token: {error}")))?,
            );
        Ok(Self {
            client: GraphQlClient::new(http, "/admin/api/2025-01/graphql.json"),
        })
    }

    pub async fn execute_query(&self, query: &str) -> Result<serde_json::Value> {
        let request = GraphQlRequest::query(query, serde_json::json!({}));
        let response = self
            .client
            .execute::<_, serde_json::Value>(&request)
            .await
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(response.data)
    }
}

#[async_trait]
impl StoreBackend for AdminStoreBackend {
    async fn info(&self, target: &StoreTarget) -> Result<HashMap<String, String>> {
        let data = self
            .execute_query("query StoreInfo { shop { name myshopifyDomain plan { displayName } } }")
            .await?;
        let shop = data
            .get("shop")
            .ok_or_else(|| StoreError::Backend("response omitted shop data".into()))?;
        let mut info = HashMap::new();
        info.insert("domain".into(), target.domain.clone());
        if let Some(value) = shop.get("name").and_then(serde_json::Value::as_str) {
            info.insert("name".into(), value.into());
        }
        if let Some(value) = shop
            .get("myshopifyDomain")
            .and_then(serde_json::Value::as_str)
        {
            info.insert("myshopify_domain".into(), value.into());
        }
        if let Some(value) = shop
            .pointer("/plan/displayName")
            .and_then(serde_json::Value::as_str)
        {
            info.insert("plan".into(), value.into());
        }
        Ok(info)
    }

    async fn execute(&self, _target: &StoreTarget, query: &str) -> Result<serde_json::Value> {
        self.execute_query(query).await
    }

    async fn bulk_execute(
        &self,
        target: &StoreTarget,
        items: &[String],
        progress: &mut (dyn FnMut(ProgressEvent) + Send),
        cancellation: &Cancellation,
    ) -> Result<BulkReport> {
        let mut completed = 0;
        let mut failed = Vec::new();
        for query in items {
            if cancellation.is_cancelled() {
                return Ok(BulkReport {
                    operation: "store.execute".into(),
                    completed,
                    failed,
                    cancelled: true,
                });
            }
            match self.execute(target, query).await {
                Ok(_) => {
                    completed += 1;
                    progress(ProgressEvent {
                        operation: "store.execute".into(),
                        completed,
                        total: items.len(),
                        failed: failed.len(),
                        detail: Some("completed".into()),
                    });
                }
                Err(error) => failed.push(PartialFailure {
                    item: query.clone(),
                    error: error.to_string(),
                    retryable: false,
                }),
            }
        }
        Ok(BulkReport {
            operation: "store.execute".into(),
            completed,
            failed,
            cancelled: false,
        })
    }
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
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

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

    #[tokio::test]
    async fn lists_organization_stores_with_canonical_fields() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16 * 1024];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("ListAccessibleShops"));
            assert!(request.contains("\"first\":250"));
            assert!(request.contains("authorization: Bearer secret-token"));
            let body_start = request.find("\r\n\r\n").unwrap_or(request.len());
            assert!(!request[body_start..].contains("secret-token"));
            let body = r#"{"data":{"organization":{"name":"Example Org","accessibleShops":{"edges":[{"node":{"shopifyShopId":"22","name":"Newest","storeType":"APP_DEVELOPMENT","primaryDomain":"newest.myshopify.com","url":"https://newest.myshopify.com","createdAt":"2026-02-02T00:00:00Z"}},{"node":{"shopifyShopId":"11","name":"Older","storeType":"SHOPIFY_PLUS","primaryDomain":"older.myshopify.com","url":null,"createdAt":"2026-01-01T00:00:00Z"}}],"pageInfo":{"hasNextPage":true}}}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let client =
            OrganizationStoreClient::new(&format!("http://{address}/graphql"), "secret-token", "7")
                .unwrap();
        let result = client.list().await.unwrap();
        server.await.unwrap();

        assert_eq!(result.organization_id, "7");
        assert_eq!(result.organization_name, "Example Org");
        assert!(result.truncated);
        assert_eq!(result.stores[0].store, "newest.myshopify.com");
        assert_eq!(
            result.stores[0].store_type.as_deref(),
            Some("app-development")
        );
        assert_eq!(
            result.stores[0].id.as_deref(),
            Some("gid://shopify/Shop/22")
        );
        assert_eq!(result.stores[1].store, "older.myshopify.com");
    }
}
