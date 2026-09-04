//! Native Shopify Admin bulk-operation engine.
//!
//! This crate deliberately contains no CLI integration. It owns the typed HTTP,
//! GraphQL, polling, credential-exchange, identifier, and raw JSONL contracts.

use std::{fmt, net::SocketAddr, sync::OnceLock, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cfy_core::Cancellation;
use reqwest::{StatusCode, Url, header::HeaderValue};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use zeroize::{Zeroize, ZeroizeOnDrop};

const ACCESS_TOKEN_HEADER: &str = "x-shopify-access-token";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
static TLS_PROVIDER: OnceLock<()> = OnceLock::new();

/// Failures exposed by the bulk engine. Secrets and response bodies are never
/// retained in printable variants.
#[derive(Debug, Error)]
pub enum BulkError {
    #[error("invalid Shopify store: {0}")]
    InvalidStore(String),
    #[error("invalid Shopify API version: {0}")]
    InvalidApiVersion(String),
    #[error("Shopify API version {requested} is not supported (available: {available:?})")]
    UnsupportedApiVersion {
        requested: String,
        available: Vec<String>,
    },
    #[error("invalid bulk operation ID: {0}")]
    InvalidOperationId(String),
    #[error("bulk query input must contain exactly one GraphQL query operation")]
    QueryRequired,
    #[error("bulk mutations require JSONL variables")]
    MutationVariablesRequired,
    #[error("request transport failed: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("local GraphiQL server failed: {0}")]
    ServerIo(#[source] std::io::Error),
    #[error("GraphQL mutations are disabled for this GraphiQL session")]
    MutationsDisabled,
    #[error("Shopify returned HTTP {status} (request ID: {request_id:?})")]
    Http {
        status: StatusCode,
        request_id: Option<String>,
    },
    #[error("Shopify returned malformed JSON (request ID: {request_id:?}): {source}")]
    MalformedJson {
        request_id: Option<String>,
        #[source]
        source: serde_json::Error,
    },
    #[error("Shopify GraphQL returned errors (request ID: {request_id:?}): {messages:?}")]
    GraphQl {
        request_id: Option<String>,
        messages: Vec<String>,
    },
    #[error("Shopify rejected the bulk operation: {0:?}")]
    UserErrors(Vec<UserError>),
    #[error("Shopify response omitted {0}")]
    MissingField(&'static str),
    #[error("bulk operation polling was cancelled")]
    Cancelled,
    #[error("unsafe result URL returned by Shopify")]
    UnsafeResultUrl,
    #[error("result is not valid UTF-8 JSONL")]
    InvalidJsonlEncoding,
}

pub struct GraphiqlServer {
    listener: TcpListener,
    client: BulkClient,
    key: String,
    mutation_policy: MutationPolicy,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MutationPolicy {
    Deny,
    Allow,
    #[default]
    DevelopmentStoresOnly,
}

impl GraphiqlServer {
    pub async fn bind(client: BulkClient, port: u16) -> Result<Self> {
        Self::bind_with_policy(client, port, MutationPolicy::default()).await
    }

    pub async fn bind_with_policy(
        client: BulkClient,
        port: u16,
        mutation_policy: MutationPolicy,
    ) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|error| {
                BulkError::InvalidStore(format!("could not bind GraphiQL server: {error}"))
            })?;
        let mut key = [0_u8; 32];
        getrandom::fill(&mut key).map_err(|error| {
            BulkError::InvalidStore(format!("could not create GraphiQL key: {error}"))
        })?;
        Ok(Self {
            listener,
            client,
            key: URL_SAFE_NO_PAD.encode(key),
            mutation_policy,
        })
    }

    pub fn url(&self, initial_variables: Option<&str>) -> Result<Url> {
        let mut url = Url::parse(&format!("http://{}/", self.address()?))
            .map_err(|_| BulkError::InvalidStore("could not build GraphiQL URL".into()))?;
        url.query_pairs_mut().append_pair("key", &self.key);
        if let Some(variables) = initial_variables {
            url.query_pairs_mut().append_pair("variables", variables);
        }
        Ok(url)
    }

    pub fn address(&self) -> Result<SocketAddr> {
        self.listener.local_addr().map_err(|error| {
            BulkError::InvalidStore(format!("could not inspect GraphiQL address: {error}"))
        })
    }

    pub async fn run(self, cancellation: &Cancellation) -> Result<()> {
        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            if let Ok(accepted) =
                tokio::time::timeout(Duration::from_millis(100), self.listener.accept()).await
            {
                let (stream, _) = accepted.map_err(BulkError::ServerIo)?;
                let client = self.client.clone();
                let key = self.key.clone();
                let mutation_policy = self.mutation_policy;
                tokio::spawn(async move {
                    let _ = serve_graphiql_connection(stream, client, key, mutation_policy).await;
                });
            }
        }
    }
}

async fn serve_graphiql_connection(
    mut stream: TcpStream,
    client: BulkClient,
    key: String,
    mutation_policy: MutationPolicy,
) -> Result<()> {
    const MAX_REQUEST: usize = 1024 * 1024;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.map_err(BulkError::ServerIo)?;
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_REQUEST {
            return write_http(&mut stream, 413, "text/plain", b"Request too large").await;
        }
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let first_line = headers.lines().next().unwrap_or_default();
    let mut request = first_line.split_whitespace();
    let method = request.next().unwrap_or_default().to_owned();
    let path = request.next().unwrap_or_default().to_owned();
    let requested_url = Url::parse(&format!("http://127.0.0.1{path}"))
        .map_err(|_| BulkError::InvalidStore("GraphiQL request URL was invalid".into()))?;
    let query_key = requested_url
        .query_pairs()
        .find_map(|(name, value)| (name == "key").then(|| value.into_owned()));
    let header_key = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("x-catify-graphiql-key")
            .then(|| value.trim().to_owned())
    });
    if query_key.as_deref() != Some(&key) && header_key.as_deref() != Some(&key) {
        return write_http(&mut stream, 403, "text/plain", b"Forbidden").await;
    }
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    if content_length > MAX_REQUEST {
        return write_http(&mut stream, 413, "text/plain", b"Request too large").await;
    }
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk).await.map_err(BulkError::ServerIo)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    match (method.as_str(), requested_url.path()) {
        ("GET", "/") => {
            let html = GRAPHIQL_HTML.replace("__CATIFY_KEY__", &key);
            write_http(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                html.as_bytes(),
            )
            .await
        }
        ("POST", "/graphql") => {
            #[derive(Deserialize)]
            struct Request {
                query: String,
                #[serde(default)]
                variables: serde_json::Value,
            }
            let body = bytes
                .get(header_end..header_end + content_length)
                .unwrap_or_default();
            let request: Request =
                serde_json::from_slice(body).map_err(|source| BulkError::MalformedJson {
                    request_id: None,
                    source,
                })?;
            let payload = match client
                .execute_document_with_policy(&request.query, request.variables, mutation_policy)
                .await
            {
                Ok(data) => serde_json::json!({"data": data}),
                Err(error) => serde_json::json!({"errors": [{"message": error.to_string()}]}),
            };
            let body = serde_json::to_vec(&payload).map_err(|source| BulkError::MalformedJson {
                request_id: None,
                source,
            })?;
            write_http(&mut stream, 200, "application/json", &body).await
        }
        _ => write_http(&mut stream, 404, "text/plain", b"Not found").await,
    }
}

async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        403 => "Forbidden",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(BulkError::ServerIo)?;
    stream.write_all(body).await.map_err(BulkError::ServerIo)
}

const GRAPHIQL_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Catify GraphiQL</title>
<style>html,body,#root{height:100%;margin:0}body{font-family:system-ui;background:#0a1314;color:#f1f2f2}textarea{width:95%;height:45%;margin:1rem;background:#132426;color:#fff;padding:1rem}button{margin-left:1rem;padding:.6rem 1rem}pre{margin:1rem;white-space:pre-wrap}</style></head>
<body><textarea id="query">query { shop { name myshopifyDomain } }</textarea><textarea id="variables">{}</textarea><button id="run">Run</button><pre id="result"></pre>
<script>const key='__CATIFY_KEY__';const initial=new URLSearchParams(location.search).get('variables');if(initial)document.getElementById('variables').value=initial;document.getElementById('run').onclick=async()=>{const query=document.getElementById('query').value;let variables;try{variables=JSON.parse(document.getElementById('variables').value||'{}')}catch(error){document.getElementById('result').textContent='Invalid variables JSON: '+error;return}const response=await fetch('/graphql',{method:'POST',headers:{'content-type':'application/json','x-catify-graphiql-key':key},body:JSON.stringify({query,variables})});document.getElementById('result').textContent=JSON.stringify(await response.json(),null,2)};</script></body></html>"#;
#[derive(Deserialize)]
struct ShopPlanData {
    shop: ShopPlanShop,
}
#[derive(Deserialize)]
struct ShopPlanShop {
    plan: ShopPlan,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShopPlan {
    partner_development: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunMutationData {
    bulk_operation_run_mutation: OperationPayload,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListData {
    bulk_operations: BulkConnection,
}
#[derive(Deserialize)]
struct BulkConnection {
    nodes: Vec<BulkOperation>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StagedUploadData {
    staged_uploads_create: StagedUploadPayload,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StagedUploadPayload {
    staged_targets: Vec<StagedTarget>,
    #[serde(default)]
    user_errors: Vec<UserError>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StagedTarget {
    url: String,
    #[allow(dead_code)]
    resource_url: Option<String>,
    parameters: Vec<StagedParameter>,
}
#[derive(Deserialize)]
struct StagedParameter {
    name: String,
    value: String,
}

pub type Result<T> = std::result::Result<T, BulkError>;

/// A normalized `*.myshopify.com` hostname.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct StoreDomain(String);

impl StoreDomain {
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim().trim_end_matches('/');
        let host = if trimmed.contains("://") {
            let url = Url::parse(trimmed).map_err(|_| BulkError::InvalidStore(input.into()))?;
            if url.scheme() != "https"
                || url.port().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(BulkError::InvalidStore(input.into()));
            }

            url.host_str()
                .ok_or_else(|| BulkError::InvalidStore(input.into()))?
                .to_owned()
        } else {
            trimmed.to_owned()
        };
        let host = host.to_ascii_lowercase();
        let host = if host.ends_with(".myshopify.com") {
            host
        } else {
            format!("{host}.myshopify.com")
        };
        let handle = host.strip_suffix(".myshopify.com").unwrap_or_default();
        if handle.is_empty()
            || handle.contains('.')
            || !handle
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(BulkError::InvalidStore(input.into()));
        }
        Ok(Self(host))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StoreDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StoreDomain").field(&self.0).finish()
    }
}
impl fmt::Display for StoreDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated public Admin API version (`YYYY-MM`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ApiVersion(String);

impl ApiVersion {
    pub fn parse(input: &str) -> Result<Self> {
        let bytes = input.as_bytes();
        if bytes.len() != 7
            || bytes[4] != b'-'
            || !bytes[..4].iter().all(u8::is_ascii_digit)
            || !bytes[5..].iter().all(u8::is_ascii_digit)
        {
            return Err(BulkError::InvalidApiVersion(input.into()));
        }
        let month: u8 = input[5..]
            .parse()
            .map_err(|_| BulkError::InvalidApiVersion(input.into()))?;
        if !matches!(month, 1 | 4 | 7 | 10) {
            return Err(BulkError::InvalidApiVersion(input.into()));
        }
        Ok(Self(input.into()))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Normalized BulkOperation GraphQL global ID. A numeric ID is accepted as a
/// convenience; unrelated GID types are rejected.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BulkOperationId(String);

impl BulkOperationId {
    pub fn parse(input: &str) -> Result<Self> {
        let value = input.trim();
        let numeric = if value.bytes().all(|b| b.is_ascii_digit()) && !value.is_empty() {
            value
        } else if let Some(id) = value.strip_prefix("gid://shopify/BulkOperation/") {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                return Err(BulkError::InvalidOperationId(input.into()));
            }
            id
        } else {
            return Err(BulkError::InvalidOperationId(input.into()));
        };
        Ok(Self(format!("gid://shopify/BulkOperation/{numeric}")))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Secret wrapper whose memory is zeroed on drop and whose debug output is redacted.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Secret(String);
impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl AppCredentials {
    #[must_use]
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: Secret::new(client_secret),
        }
    }
}

impl AccessToken {
    #[must_use]
    pub fn secret(&self) -> &Secret {
        &self.token
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

#[derive(Clone, Debug)]
pub struct AppCredentials {
    pub client_id: String,
    pub client_secret: Secret,
}

#[derive(Clone, Debug)]
pub struct AccessToken {
    pub token: Secret,
    pub scope: Option<String>,
    pub expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct VersionEnvelope {
    versions: Vec<VersionRecord>,
}
#[derive(Debug, Deserialize)]
struct VersionRecord {
    handle: String,
    #[serde(default = "default_true")]
    supported: bool,
}
const fn default_true() -> bool {
    true
}

/// Resolve a requested version, or the newest supported stable public version,
/// via Shopify's public Admin versions endpoint.
pub async fn resolve_api_version(
    store: &StoreDomain,
    requested: Option<&str>,
) -> Result<ApiVersion> {
    let base = Url::parse(&format!("https://{store}/")).expect("normalized host");
    resolve_api_version_at(&reqwest_client()?, &base, requested).await
}

async fn resolve_api_version_at(
    client: &reqwest::Client,
    base: &Url,
    requested: Option<&str>,
) -> Result<ApiVersion> {
    let url = base
        .join("admin/api/versions.json")
        .map_err(|_| BulkError::InvalidStore(base.to_string()))?;
    let response = send(client.get(url)).await?;
    let request_id = request_id(response.headers());
    let status = response.status();
    if !status.is_success() {
        return Err(BulkError::Http { status, request_id });
    }
    let body = response.bytes().await.map_err(BulkError::Transport)?;
    let envelope: VersionEnvelope = serde_json::from_slice(&body)
        .map_err(|source| BulkError::MalformedJson { request_id, source })?;
    let mut versions = envelope
        .versions
        .into_iter()
        .filter(|v| v.supported)
        .filter_map(|v| ApiVersion::parse(&v.handle).ok())
        .collect::<Vec<_>>();
    versions.sort();
    versions.dedup();
    if let Some(requested) = requested {
        let requested = ApiVersion::parse(requested)?;
        if versions.contains(&requested) {
            return Ok(requested);
        }
        return Err(BulkError::UnsupportedApiVersion {
            requested: requested.0,
            available: versions.into_iter().map(|v| v.0).collect(),
        });
    }
    versions.pop().ok_or_else(|| {
        BulkError::InvalidApiVersion("Shopify returned no supported stable versions".into())
    })
}

/// Exchange app client credentials for an Admin API access token. Credentials
/// are body-only and are never included in URLs or printable errors.
pub async fn exchange_client_credentials(
    store: &StoreDomain,
    credentials: &AppCredentials,
) -> Result<AccessToken> {
    let base = Url::parse(&format!("https://{store}/")).expect("normalized host");
    exchange_client_credentials_at(&reqwest_client()?, &base, credentials).await
}

async fn exchange_client_credentials_at(
    client: &reqwest::Client,
    base: &Url,
    credentials: &AppCredentials,
) -> Result<AccessToken> {
    let url = base
        .join("admin/oauth/access_token")
        .map_err(|_| BulkError::InvalidStore(base.to_string()))?;
    let response = send(client.post(url).json(&serde_json::json!({
        "grant_type": "client_credentials",
        "client_id": credentials.client_id,
        "client_secret": credentials.client_secret.expose(),
    })))
    .await?;
    let request_id = request_id(response.headers());
    let status = response.status();
    if !status.is_success() {
        return Err(BulkError::Http { status, request_id });
    }
    let body = response.bytes().await.map_err(BulkError::Transport)?;
    let token: TokenResponse = serde_json::from_slice(&body)
        .map_err(|source| BulkError::MalformedJson { request_id, source })?;
    Ok(AccessToken {
        token: Secret::new(token.access_token),
        scope: token.scope,
        expires_in: token.expires_in,
    })
}

/// Typed Admin GraphQL bulk client.
#[derive(Clone)]
pub struct BulkClient {
    http: reqwest::Client,
    graphql_url: Url,
}

impl fmt::Debug for BulkClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BulkClient")
            .field("graphql_url", &self.graphql_url)
            .field("http", &"[REDACTED]")
            .finish()
    }
}

impl BulkClient {
    pub fn new(store: &StoreDomain, version: &ApiVersion, token: &Secret) -> Result<Self> {
        let base = Url::parse(&format!("https://{store}/")).expect("normalized host");
        Self::new_at(base, version, token)
    }

    fn new_at(base: Url, version: &ApiVersion, token: &Secret) -> Result<Self> {
        TLS_PROVIDER.get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        let mut headers = reqwest::header::HeaderMap::new();
        let mut value = HeaderValue::from_str(token.expose()).map_err(|_| {
            BulkError::InvalidStore("access token contains invalid header bytes".into())
        })?;
        value.set_sensitive(true);
        headers.insert(ACCESS_TOKEN_HEADER, value);
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .user_agent(concat!("catify-bulk/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(BulkError::Transport)?;
        let graphql_url = base
            .join(&format!("admin/api/{}/graphql.json", version.as_str()))
            .map_err(|_| BulkError::InvalidStore(base.to_string()))?;
        Ok(Self { http, graphql_url })
    }

    /// Start a bulk query.
    pub async fn execute_query(&self, document: &str) -> Result<BulkOperation> {
        match operation_kind(document)? {
            OperationKind::Query => {}
            OperationKind::Mutation => return Err(BulkError::QueryRequired),
        }
        let data: RunQueryData = self
            .graphql(RUN_QUERY, serde_json::json!({ "query": document }))
            .await?;
        reject_user_errors(data.bulk_operation_run_query.user_errors)?;
        data.bulk_operation_run_query
            .bulk_operation
            .ok_or(BulkError::MissingField(
                "bulkOperationRunQuery.bulkOperation",
            ))
    }

    /// Execute a regular Admin GraphQL query or mutation. Mutations are guarded
    /// to partner development stores, matching Shopify CLI safety semantics.
    pub async fn execute_document(
        &self,
        document: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.execute_document_with_policy(
            document,
            variables,
            MutationPolicy::DevelopmentStoresOnly,
        )
        .await
    }

    pub async fn execute_document_with_policy(
        &self,
        document: &str,
        variables: serde_json::Value,
        mutation_policy: MutationPolicy,
    ) -> Result<serde_json::Value> {
        if operation_kind(document)? == OperationKind::Mutation {
            match mutation_policy {
                MutationPolicy::Deny => return Err(BulkError::MutationsDisabled),
                MutationPolicy::Allow => {}
                MutationPolicy::DevelopmentStoresOnly => {
                    let shop: ShopPlanData =
                        self.graphql(SHOP_PLAN_QUERY, serde_json::json!({})).await?;
                    if !shop.shop.plan.partner_development {
                        return Err(BulkError::InvalidStore(
                            "mutations are only allowed on partner development stores".into(),
                        ));
                    }
                }
            }
        }
        self.graphql(document, variables).await
    }

    pub async fn list_recent(&self, since: &str) -> Result<Vec<BulkOperation>> {
        let data: ListData = self
            .graphql(
                LIST_QUERY,
                serde_json::json!({
                    "query": format!("created_at:>={since}"),
                    "first": 100,
                    "sortKey": "CREATED_AT"
                }),
            )
            .await?;
        Ok(data.bulk_operations.nodes)
    }

    pub async fn list_last_seven_days(&self) -> Result<Vec<BulkOperation>> {
        let since = time::OffsetDateTime::now_utc() - time::Duration::days(7);
        let format = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]")
            .expect("bulk date format is static and valid");
        let date = since
            .format(&format)
            .map_err(|_| BulkError::InvalidApiVersion("could not format bulk date".into()))?;
        self.list_recent(&date).await
    }

    /// Stage JSONL variables and start a bulk mutation.
    pub async fn execute_mutation(
        &self,
        document: &str,
        variables_jsonl: &[u8],
    ) -> Result<BulkOperation> {
        if operation_kind(document)? != OperationKind::Mutation {
            return Err(BulkError::QueryRequired);
        }
        if variables_jsonl.is_empty() {
            return Err(BulkError::MutationVariablesRequired);
        }
        let shop: ShopPlanData = self.graphql(SHOP_PLAN_QUERY, serde_json::json!({})).await?;
        if !shop.shop.plan.partner_development {
            return Err(BulkError::InvalidStore(
                "bulk mutations are only allowed on partner development stores".into(),
            ));
        }
        let staged: StagedUploadData = self
            .graphql(
                STAGED_UPLOADS_CREATE,
                serde_json::json!({"input": [{
                    "filename": "bulk-variables.jsonl",
                    "fileSize": variables_jsonl.len().to_string(),
                    "httpMethod": "POST",
                    "mimeType": "text/jsonl",
                    "resource": "BULK_MUTATION_VARIABLES"
                }]}),
            )
            .await?;
        reject_user_errors(staged.staged_uploads_create.user_errors)?;
        let target = staged
            .staged_uploads_create
            .staged_targets
            .into_iter()
            .next()
            .ok_or(BulkError::MissingField(
                "stagedUploadsCreate.stagedTargets[0]",
            ))?;
        let key = target
            .parameters
            .iter()
            .find(|parameter| parameter.name == "key")
            .map(|parameter| parameter.value.clone())
            .ok_or(BulkError::MissingField("staged upload key"))?;
        let mut form = reqwest::multipart::Form::new();
        for parameter in target.parameters {
            form = form.text(parameter.name, parameter.value);
        }
        form = form.part(
            "file",
            reqwest::multipart::Part::bytes(variables_jsonl.to_vec())
                .file_name("bulk-variables.jsonl")
                .mime_str("text/jsonl")
                .map_err(BulkError::Transport)?,
        );
        let response = send(reqwest_client()?.post(target.url).multipart(form)).await?;
        let request_id = request_id(response.headers());
        if !response.status().is_success() {
            return Err(BulkError::Http {
                status: response.status(),
                request_id,
            });
        }
        let data: RunMutationData = self
            .graphql(
                RUN_MUTATION,
                serde_json::json!({"mutation": document, "stagedUploadPath": key}),
            )
            .await?;
        reject_user_errors(data.bulk_operation_run_mutation.user_errors)?;
        data.bulk_operation_run_mutation
            .bulk_operation
            .ok_or(BulkError::MissingField(
                "bulkOperationRunMutation.bulkOperation",
            ))
    }

    pub async fn status(&self, id: &BulkOperationId) -> Result<BulkOperation> {
        let data: StatusData = self
            .graphql(STATUS_QUERY, serde_json::json!({ "id": id.as_str() }))
            .await?;
        data.node.ok_or(BulkError::MissingField("node"))
    }

    pub async fn cancel(&self, id: &BulkOperationId) -> Result<BulkOperation> {
        let data: CancelData = self
            .graphql(CANCEL_MUTATION, serde_json::json!({ "id": id.as_str() }))
            .await?;
        reject_user_errors(data.bulk_operation_cancel.user_errors)?;
        data.bulk_operation_cancel
            .bulk_operation
            .ok_or(BulkError::MissingField("bulkOperationCancel.bulkOperation"))
    }

    /// Fetch once (`Quick`) or poll until terminal status (`Watch`). Cancellation
    /// is checked before every request and while sleeping.
    pub async fn poll(
        &self,
        id: &BulkOperationId,
        mode: PollMode,
        cancellation: &Cancellation,
    ) -> Result<BulkOperation> {
        let interval = match mode {
            PollMode::Quick => return self.status(id).await,
            PollMode::Watch { interval } => interval,
        };
        loop {
            if cancellation.is_cancelled() {
                return Err(BulkError::Cancelled);
            }
            let operation = self.status(id).await?;
            if operation.status.is_terminal() {
                return Ok(operation);
            }
            cancellable_sleep(interval, cancellation).await?;
        }
    }

    /// Download Shopify's signed JSONL result without transforming records.
    /// The access-token-bearing Admin client is not used for this cross-origin
    /// request, preventing credential leakage to object storage.
    pub async fn download_jsonl(&self, operation: &BulkOperation) -> Result<RawJsonl> {
        let raw = operation
            .url
            .as_deref()
            .ok_or(BulkError::MissingField("bulkOperation.url"))?;
        let url = safe_result_url(raw)?;
        let response = send(reqwest_client()?.get(url)).await?;
        let request_id = request_id(response.headers());
        let status = response.status();
        if !status.is_success() {
            return Err(BulkError::Http { status, request_id });
        }
        let bytes = response
            .bytes()
            .await
            .map_err(BulkError::Transport)?
            .to_vec();
        RawJsonl::new(bytes)
    }

    async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        let response = send(
            self.http
                .post(self.graphql_url.clone())
                .json(&GraphQlBody { query, variables }),
        )
        .await?;
        let request_id = request_id(response.headers());
        let status = response.status();
        if !status.is_success() {
            return Err(BulkError::Http { status, request_id });
        }
        let body = response.bytes().await.map_err(BulkError::Transport)?;
        let envelope: GraphQlEnvelope<T> =
            serde_json::from_slice(&body).map_err(|source| BulkError::MalformedJson {
                request_id: request_id.clone(),
                source,
            })?;
        if !envelope.errors.is_empty() {
            return Err(BulkError::GraphQl {
                request_id,
                messages: envelope.errors.into_iter().map(|e| e.message).collect(),
            });
        }
        envelope.data.ok_or(BulkError::MissingField("data"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PollMode {
    Quick,
    Watch { interval: Duration },
}
impl Default for PollMode {
    fn default() -> Self {
        Self::Watch {
            interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BulkOperationStatus {
    Created,
    Running,
    Canceling,
    Canceled,
    Completed,
    Failed,
    Expired,
}
impl BulkOperationStatus {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Canceled | Self::Completed | Self::Failed | Self::Expired
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BulkOperation {
    pub id: String,
    pub status: BulkOperationStatus,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub object_count: String,
    #[serde(default)]
    pub root_object_count: String,
    #[serde(default)]
    pub file_size: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub partial_data_url: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UserError {
    #[serde(default)]
    pub field: Vec<String>,
    pub message: String,
}

/// Untouched result bytes plus zero-copy line access. Empty trailing lines are
/// omitted, while each non-empty line remains byte-for-byte unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawJsonl {
    bytes: Vec<u8>,
}
impl RawJsonl {
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        std::str::from_utf8(&bytes).map_err(|_| BulkError::InvalidJsonlEncoding)?;
        Ok(Self { bytes })
    }
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        // Encoding was checked by new.
        std::str::from_utf8(&self.bytes)
            .expect("validated UTF-8")
            .lines()
    }
}

#[derive(Serialize)]
struct GraphQlBody<'a> {
    query: &'a str,
    variables: serde_json::Value,
}
#[derive(Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}
#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunQueryData {
    bulk_operation_run_query: OperationPayload,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelData {
    bulk_operation_cancel: OperationPayload,
}
#[derive(Deserialize)]
struct StatusData {
    node: Option<BulkOperation>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OperationPayload {
    bulk_operation: Option<BulkOperation>,
    #[serde(default)]
    user_errors: Vec<UserError>,
}

#[cfg(test)]
const FIELDS: &str = "id status errorCode objectCount rootObjectCount fileSize url partialDataUrl createdAt completedAt";
const RUN_QUERY: &str = "mutation BulkOperationRunQuery($query: String!) { bulkOperationRunQuery(query: $query) { bulkOperation { id status errorCode objectCount rootObjectCount fileSize url partialDataUrl createdAt completedAt } userErrors { field message } } }";
const RUN_MUTATION: &str = "mutation BulkOperationRunMutation($mutation: String!, $stagedUploadPath: String!) { bulkOperationRunMutation(mutation: $mutation, stagedUploadPath: $stagedUploadPath) { bulkOperation { id status errorCode objectCount rootObjectCount fileSize url partialDataUrl createdAt completedAt } userErrors { field message } } }";
const STAGED_UPLOADS_CREATE: &str = "mutation StagedUploadsCreate($input: [StagedUploadInput!]!) { stagedUploadsCreate(input: $input) { stagedTargets { url resourceUrl parameters { name value } } userErrors { field message } } }";
const SHOP_PLAN_QUERY: &str = "query BulkMutationShopPlan { shop { plan { partnerDevelopment } } }";
const STATUS_QUERY: &str = "query BulkOperationStatus($id: ID!) { node(id: $id) { ... on BulkOperation { id status errorCode objectCount rootObjectCount fileSize url partialDataUrl createdAt completedAt } } }";
const LIST_QUERY: &str = "query ListBulkOperations($query: String, $first: Int!, $sortKey: BulkOperationsSortKeys!) { bulkOperations(first: $first, query: $query, sortKey: $sortKey) { nodes { id status errorCode objectCount rootObjectCount fileSize url partialDataUrl createdAt completedAt } } }";
const CANCEL_MUTATION: &str = "mutation BulkOperationCancel($id: ID!) { bulkOperationCancel(id: $id) { bulkOperation { id status errorCode objectCount rootObjectCount fileSize url partialDataUrl createdAt completedAt } userErrors { field message } } }";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationKind {
    Query,
    Mutation,
}

pub fn operation_kind(document: &str) -> Result<OperationKind> {
    // Skip whitespace, commas, comments, and an optional leading BOM. This is
    // intentionally a conservative validator, not a GraphQL parser: shorthand
    // selection sets are queries; subscriptions and multiple operations fail.
    let mut text = document.trim_start_matches('\u{feff}');
    loop {
        text = text.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if let Some(comment) = text.strip_prefix('#') {
            text = comment.split_once('\n').map_or("", |(_, rest)| rest);
        } else {
            break;
        }
    }
    if text.starts_with('{') {
        return Ok(OperationKind::Query);
    }
    let keyword = text
        .split(|c: char| c.is_whitespace() || c == '{' || c == '(')
        .next()
        .unwrap_or("");
    match keyword {
        "query" => Ok(OperationKind::Query),
        "mutation" => Ok(OperationKind::Mutation),
        _ => Err(BulkError::QueryRequired),
    }
}

fn reject_user_errors(errors: Vec<UserError>) -> Result<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(BulkError::UserErrors(errors))
    }
}

fn reqwest_client() -> Result<reqwest::Client> {
    TLS_PROVIDER.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::builder()
        .user_agent(concat!("catify-bulk/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(BulkError::Transport)
}

async fn send(builder: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    builder.send().await.map_err(BulkError::Transport)
}

fn request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("x-shopify-request-id")
        .or_else(|| headers.get("x-request-id"))?
        .to_str()
        .ok()
        .map(str::to_owned)
}

fn safe_result_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw).map_err(|_| BulkError::UnsafeResultUrl)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err(BulkError::UnsafeResultUrl);
    }
    Ok(url)
}

async fn cancellable_sleep(duration: Duration, cancellation: &Cancellation) -> Result<()> {
    let quantum = Duration::from_millis(50);
    let mut elapsed = Duration::ZERO;
    while elapsed < duration {
        if cancellation.is_cancelled() {
            return Err(BulkError::Cancelled);
        }
        let step = quantum.min(duration - elapsed);
        tokio::time::sleep(step).await;
        elapsed += step;
    }
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use std::{collections::VecDeque, sync::Arc};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };

    #[derive(Clone)]
    struct MockResponse {
        status: u16,
        body: &'static str,
        headers: Vec<(&'static str, &'static str)>,
    }

    struct MockServer {
        base: Url,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockServer {
        async fn start(responses: Vec<MockResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&requests);
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let responses = Arc::clone(&responses);
                    let recorded = Arc::clone(&recorded);
                    tokio::spawn(async move {
                        let mut bytes = Vec::new();
                        let mut chunk = [0_u8; 4096];
                        let header_end;
                        loop {
                            let count = stream.read(&mut chunk).await.unwrap();
                            if count == 0 {
                                return;
                            }
                            bytes.extend_from_slice(&chunk[..count]);
                            if let Some(position) =
                                bytes.windows(4).position(|window| window == b"\r\n\r\n")
                            {
                                header_end = position + 4;
                                break;
                            }
                        }
                        let headers = String::from_utf8_lossy(&bytes[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        while bytes.len() < header_end + content_length {
                            let count = stream.read(&mut chunk).await.unwrap();
                            if count == 0 {
                                break;
                            }
                            bytes.extend_from_slice(&chunk[..count]);
                        }
                        recorded
                            .lock()
                            .await
                            .push(String::from_utf8_lossy(&bytes).into_owned());
                        let response = responses.lock().await.pop_front().unwrap();
                        let extra = response
                            .headers
                            .iter()
                            .map(|(name, value)| format!("{name}: {value}\r\n"))
                            .collect::<String>();
                        let wire = format!(
                            "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
                            response.status,
                            response.body.len(),
                            extra,
                            response.body
                        );
                        stream.write_all(wire.as_bytes()).await.unwrap();
                    });
                }
            });
            Self {
                base: Url::parse(&format!("http://{address}/")).unwrap(),
                requests,
            }
        }

        async fn request(&self, index: usize) -> String {
            self.requests.lock().await[index].clone()
        }
        async fn count(&self) -> usize {
            self.requests.lock().await.len()
        }
    }

    fn response(body: &'static str) -> MockResponse {
        MockResponse {
            status: 200,
            body,
            headers: vec![],
        }
    }

    fn operation(status: &str) -> String {
        format!(
            r#"{{"id":"gid://shopify/BulkOperation/42","status":"{status}","objectCount":"3","rootObjectCount":"2"}}"#
        )
    }

    #[test]
    fn normalizes_domains_ids_and_versions() {
        assert_eq!(
            StoreDomain::parse("Example").unwrap().as_str(),
            "example.myshopify.com"
        );
        assert_eq!(
            StoreDomain::parse("https://EXAMPLE.myshopify.com/")
                .unwrap()
                .as_str(),
            "example.myshopify.com"
        );
        assert!(StoreDomain::parse("example.com").is_err());
        assert_eq!(
            BulkOperationId::parse("42").unwrap().as_str(),
            "gid://shopify/BulkOperation/42"
        );
        assert!(BulkOperationId::parse("gid://shopify/Product/42").is_err());
        assert!(ApiVersion::parse("2025-01").is_ok());
        assert!(ApiVersion::parse("2025-02").is_err());
    }

    #[test]
    fn validates_document_kind_without_echoing_document() {
        assert_eq!(
            operation_kind("# hi\n query Products { products { id } }").unwrap(),
            OperationKind::Query
        );
        assert_eq!(
            operation_kind("{ shop { name } }").unwrap(),
            OperationKind::Query
        );
        assert_eq!(
            operation_kind("mutation M { productDelete(input: {}) { id } }").unwrap(),
            OperationKind::Mutation
        );
        assert!(matches!(
            operation_kind("subscription S { x }"),
            Err(BulkError::QueryRequired)
        ));
    }

    #[test]
    fn secrets_are_redacted_and_jsonl_is_raw() {
        let secret = Secret::new("very-secret");
        assert!(!format!("{secret:?}").contains("very-secret"));
        let raw = RawJsonl::new(b"{\"id\":1}\n{not parsed}\n".to_vec()).unwrap();
        assert_eq!(
            raw.lines().collect::<Vec<_>>(),
            vec!["{\"id\":1}", "{not parsed}"]
        );
        assert_eq!(raw.as_bytes(), b"{\"id\":1}\n{not parsed}\n");
        assert!(RawJsonl::new(vec![0xff]).is_err());
    }

    #[test]
    fn result_url_requires_credential_free_https() {
        assert!(safe_result_url("https://storage.example/result.jsonl?sig=x").is_ok());
        assert!(safe_result_url("http://storage.example/result").is_err());
        assert!(safe_result_url("https://user:pass@storage.example/result").is_err());
    }

    #[test]
    fn field_constant_tracks_documents() {
        for field in FIELDS.split_whitespace() {
            assert!(RUN_QUERY.contains(field));
            assert!(STATUS_QUERY.contains(field));
            assert!(CANCEL_MUTATION.contains(field));
        }
    }
    #[tokio::test]
    async fn resolves_newest_public_version_and_rejects_unsupported_request() {
        let versions = r#"{"versions":[{"handle":"unstable","supported":true},{"handle":"2024-10","supported":false},{"handle":"2025-01","supported":true},{"handle":"2025-07","supported":true}]}"#;
        let server = MockServer::start(vec![response(versions), response(versions)]).await;
        let client = reqwest_client().unwrap();
        assert_eq!(
            resolve_api_version_at(&client, &server.base, None)
                .await
                .unwrap()
                .as_str(),
            "2025-07"
        );
        let error = resolve_api_version_at(&client, &server.base, Some("2025-04"))
            .await
            .unwrap_err();
        assert!(matches!(error, BulkError::UnsupportedApiVersion { .. }));
        assert!(
            server
                .request(0)
                .await
                .starts_with("GET /admin/api/versions.json ")
        );
    }

    #[tokio::test]
    async fn exchanges_client_credentials_in_json_body_only() {
        let server = MockServer::start(vec![response(
            r#"{"access_token":"shpat_returned","scope":"read_products","expires_in":86399}"#,
        )])
        .await;
        let credentials = AppCredentials {
            client_id: "client-id".into(),
            client_secret: Secret::new("client secret&value"),
        };
        let token =
            exchange_client_credentials_at(&reqwest_client().unwrap(), &server.base, &credentials)
                .await
                .unwrap();
        assert_eq!(token.scope.as_deref(), Some("read_products"));
        assert_eq!(token.expires_in, Some(86399));
        assert!(!format!("{token:?}").contains("shpat_returned"));
        let request = server.request(0).await;
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /admin/oauth/access_token "));
        assert!(!headers.contains("client secret"));
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["grant_type"], "client_credentials");
        assert_eq!(json["client_secret"], "client secret&value");
    }

    #[tokio::test]
    async fn executes_statuses_and_cancels_with_typed_models_and_secret_header() {
        let run = format!(
            r#"{{"data":{{"bulkOperationRunQuery":{{"bulkOperation":{},"userErrors":[]}}}}}}"#,
            operation("CREATED")
        );
        let status = format!(r#"{{"data":{{"node":{}}}}}"#, operation("RUNNING"));
        let cancel = format!(
            r#"{{"data":{{"bulkOperationCancel":{{"bulkOperation":{},"userErrors":[]}}}}}}"#,
            operation("CANCELING")
        );
        let run: &'static str = Box::leak(run.into_boxed_str());
        let status: &'static str = Box::leak(status.into_boxed_str());
        let cancel: &'static str = Box::leak(cancel.into_boxed_str());
        let server =
            MockServer::start(vec![response(run), response(status), response(cancel)]).await;
        let client = BulkClient::new_at(
            server.base.clone(),
            &ApiVersion::parse("2025-01").unwrap(),
            &Secret::new("shpat_secret"),
        )
        .unwrap();
        let id = BulkOperationId::parse("42").unwrap();
        assert_eq!(
            client
                .execute_query("query Q { products { edges { node { id } } } }")
                .await
                .unwrap()
                .status,
            BulkOperationStatus::Created
        );
        assert_eq!(
            client.status(&id).await.unwrap().status,
            BulkOperationStatus::Running
        );
        assert_eq!(
            client.cancel(&id).await.unwrap().status,
            BulkOperationStatus::Canceling
        );
        let request = server.request(0).await;
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-shopify-access-token: shpat_secret")
        );
        assert!(request.contains("bulkOperationRunQuery"));
        assert!(request.contains("query Q"));
        assert!(
            server
                .request(1)
                .await
                .contains("gid://shopify/BulkOperation/42")
        );
    }

    #[tokio::test]
    async fn reports_graphql_and_user_errors_and_rejects_mutation_before_http() {
        let server = MockServer::start(vec![
            response(r#"{"errors":[{"message":"denied"}]}"#),
            response(r#"{"data":{"bulkOperationRunQuery":{"bulkOperation":null,"userErrors":[{"field":["query"],"message":"bad query"}]}}}"#),
        ]).await;
        let client = BulkClient::new_at(
            server.base.clone(),
            &ApiVersion::parse("2025-01").unwrap(),
            &Secret::new("token"),
        )
        .unwrap();
        let id = BulkOperationId::parse("42").unwrap();
        assert!(matches!(
            client.status(&id).await,
            Err(BulkError::GraphQl { .. })
        ));
        assert!(matches!(
            client.execute_query("query Q { shop { name } }").await,
            Err(BulkError::UserErrors(_))
        ));
        assert!(matches!(
            client.execute_query("mutation M { x }").await,
            Err(BulkError::QueryRequired)
        ));
        assert_eq!(server.count().await, 2);
    }

    #[tokio::test]
    async fn executes_regular_documents_and_guards_mutations_to_dev_stores() {
        let server = MockServer::start(vec![
            MockResponse {
                status: 200,
                body: r#"{"data":{"shop":{"name":"Demo"}}}"#,
                headers: vec![],
            },
            MockResponse {
                status: 200,
                body: r#"{"data":{"shop":{"plan":{"partnerDevelopment":false}}}}"#,
                headers: vec![],
            },
        ])
        .await;
        let client = BulkClient::new_at(
            server.base.clone(),
            &ApiVersion::parse("2026-01").unwrap(),
            &Secret::new("admin-secret"),
        )
        .unwrap();

        let data = client
            .execute_document(
                "query ShopName($unused: String) { shop { name } }",
                serde_json::json!({"unused": "value"}),
            )
            .await
            .unwrap();
        assert_eq!(data["shop"]["name"], "Demo");
        let request = server.request(0).await;
        assert!(request.contains("query ShopName"));
        assert!(request.contains("\"unused\":\"value\""));
        assert!(request.contains("x-shopify-access-token: admin-secret"));

        let error = client
            .execute_document(
                "mutation UpdateShop { shopUpdate(input: {}) { userErrors { message } } }",
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("partner development stores"));
    }

    #[tokio::test]
    async fn explicit_graphiql_policy_denies_or_allows_mutations_before_http() {
        let server = MockServer::start(vec![MockResponse {
            status: 200,
            body: r#"{"data":{"shopUpdate":{"shop":{"name":"Updated"}}}}"#,
            headers: vec![],
        }])
        .await;
        let client = BulkClient::new_at(
            server.base.clone(),
            &ApiVersion::parse("2026-01").unwrap(),
            &Secret::new("admin-secret"),
        )
        .unwrap();
        let mutation = "mutation { shopUpdate(input: {name: \"Updated\"}) { shop { name } } }";

        assert!(matches!(
            client
                .execute_document_with_policy(
                    mutation,
                    serde_json::json!({}),
                    MutationPolicy::Deny,
                )
                .await,
            Err(BulkError::MutationsDisabled)
        ));
        assert_eq!(server.count().await, 0);

        let data = client
            .execute_document_with_policy(mutation, serde_json::json!({}), MutationPolicy::Allow)
            .await
            .unwrap();
        assert_eq!(data["shopUpdate"]["shop"]["name"], "Updated");
        assert_eq!(server.count().await, 1);
    }

    #[tokio::test]
    async fn graphiql_server_requires_key_and_proxies_authenticated_queries() {
        let backend = MockServer::start(vec![MockResponse {
            status: 200,
            body: r#"{"data":{"shop":{"name":"Demo"}}}"#,
            headers: vec![],
        }])
        .await;
        let client = BulkClient::new_at(
            backend.base.clone(),
            &ApiVersion::parse("2026-01").unwrap(),
            &Secret::new("admin-secret"),
        )
        .unwrap();
        let server = GraphiqlServer::bind(client, 0).await.unwrap();
        let address = server.address().unwrap();
        let url = server.url(Some(r#"{"id":"1"}"#)).unwrap();
        let key = url
            .query_pairs()
            .find_map(|(name, value)| (name == "key").then(|| value.into_owned()))
            .unwrap();
        let cancellation = Cancellation::default();
        let signal = cancellation.clone();
        let task = tokio::spawn(async move { server.run(&signal).await });

        async fn request(address: SocketAddr, request: String) -> String {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        }

        let forbidden = request(
            address,
            "GET / HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n".into(),
        )
        .await;
        assert!(forbidden.starts_with("HTTP/1.1 403"));

        let page = request(
            address,
            format!("GET /?key={key} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n"),
        )
        .await;
        assert!(page.starts_with("HTTP/1.1 200"));
        assert!(page.contains("Catify GraphiQL"));
        assert!(!page.contains("admin-secret"));

        let body = r#"{"query":"query { shop { name } }","variables":{}}"#;
        let response = request(
            address,
            format!(
                "POST /graphql HTTP/1.1\r\nhost: localhost\r\nx-catify-graphiql-key: {key}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("\"name\":\"Demo\""));
        assert!(
            backend
                .request(0)
                .await
                .contains("x-shopify-access-token: admin-secret")
        );

        cancellation.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn watch_polling_reaches_terminal_state_and_honors_cancellation() {
        let running = format!(r#"{{"data":{{"node":{}}}}}"#, operation("RUNNING"));
        let completed = format!(r#"{{"data":{{"node":{}}}}}"#, operation("COMPLETED"));
        let running: &'static str = Box::leak(running.into_boxed_str());
        let completed: &'static str = Box::leak(completed.into_boxed_str());
        let server = MockServer::start(vec![response(running), response(completed)]).await;
        let client = BulkClient::new_at(
            server.base,
            &ApiVersion::parse("2025-01").unwrap(),
            &Secret::new("token"),
        )
        .unwrap();
        let result = client
            .poll(
                &BulkOperationId::parse("42").unwrap(),
                PollMode::Watch {
                    interval: Duration::from_millis(1),
                },
                &Cancellation::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.status, BulkOperationStatus::Completed);

        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(matches!(
            client
                .poll(
                    &BulkOperationId::parse("42").unwrap(),
                    PollMode::Watch {
                        interval: Duration::ZERO
                    },
                    &cancellation
                )
                .await,
            Err(BulkError::Cancelled)
        ));
    }
}
