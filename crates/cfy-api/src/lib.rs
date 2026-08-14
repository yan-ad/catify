//! Reusable HTTP and GraphQL clients for Shopify APIs.

use std::{
    fmt,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{
    Method, StatusCode, Url,
    header::{HeaderMap, HeaderName, HeaderValue, RETRY_AFTER},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;

const REQUEST_ID_HEADERS: [&str; 2] = ["x-request-id", "x-shopify-request-id"];
const REDACTED: &str = "[REDACTED]";
static TLS_PROVIDER: OnceLock<Result<(), String>> = OnceLock::new();

/// Controls retry count and exponential backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

fn install_tls_provider() -> Result<(), ApiError> {
    TLS_PROVIDER
        .get_or_init(|| {
            rustls::crypto::ring::default_provider()
                .install_default()
                .map_err(|_| "a different Rustls crypto provider is already installed".to_owned())
        })
        .clone()
        .map_err(ApiError::Configuration)
}

impl<D> fmt::Debug for GraphQlResponse<D> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GraphQlResponse")
            .field("data", &REDACTED)
            .field("request_id", &self.request_id)
            .field("extensions", &REDACTED)
            .finish()
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &REDACTED)
            .field("body", &REDACTED)
            .field("request_id", &self.request_id)
            .finish()
    }
}

impl<V> fmt::Debug for GraphQlRequest<V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GraphQlRequest")
            .field("query", &self.query)
            .field("variables", &REDACTED)
            .field("operation_name", &self.operation_name)
            .field("operation", &self.operation)
            .field(
                "idempotency_key",
                &self.idempotency_key.as_ref().map(|_| REDACTED),
            )
            .finish()
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("path", &self.path)
            .field("headers", &REDACTED)
            .field("body", &self.body.as_ref().map(|_| REDACTED))
            .field("retry_safety", &self.retry_safety)
            .finish()
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(5),
        }
    }
}

/// Declares whether replaying a request is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrySafety {
    /// Retry only methods that are idempotent by HTTP semantics.
    Automatic,
    /// The caller guarantees replay safety, normally using an idempotency key.
    Idempotent,
    /// Never retry the request.
    Unsafe,
}

/// A transport-level request independent of a specific Shopify API.
#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Option<Value>,
    pub retry_safety: RetrySafety,
}

impl HttpRequest {
    #[must_use]
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            headers: HeaderMap::new(),
            body: None,
            retry_safety: RetrySafety::Automatic,
        }
    }
}

/// A successful HTTP response with Shopify's request identifier preserved.
#[derive(Clone)]
pub struct HttpResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub request_id: Option<String>,
}

impl HttpResponse {
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, ApiError> {
        serde_json::from_slice(&self.body).map_err(|source| ApiError::MalformedJson {
            request_id: self.request_id.clone(),
            source,
        })
    }
}

/// Structured failures returned by the transport and Shopify APIs.
#[derive(Debug, Error)]
pub enum ApiError {
    #[error("HTTP transport failed: {source}")]
    Transport {
        #[source]
        source: reqwest::Error,
    },
    #[error("Shopify API returned HTTP {status}: {message}")]
    Http {
        status: StatusCode,
        request_id: Option<String>,
        message: String,
        details: Option<Value>,
    },
    #[error("Shopify GraphQL returned errors: {errors:?}")]
    GraphQl {
        request_id: Option<String>,
        errors: Vec<GraphQlError>,
    },
    #[error("Shopify GraphQL response was invalid: {message}")]
    GraphQlResponse {
        request_id: Option<String>,
        message: String,
    },
    #[error("Shopify returned malformed JSON: {source}")]
    MalformedJson {
        request_id: Option<String>,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid API client configuration: {0}")]
    Configuration(String),
}

impl ApiError {
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Http { request_id, .. }
            | Self::GraphQl { request_id, .. }
            | Self::GraphQlResponse { request_id, .. }
            | Self::MalformedJson { request_id, .. } => request_id.as_deref(),
            Self::Transport { .. } | Self::Configuration(_) => None,
        }
    }
}

impl From<ApiError> for cfy_core::Error {
    fn from(error: ApiError) -> Self {
        let request_id = error
            .request_id()
            .map(|id| format!(" (request ID: {id})"))
            .unwrap_or_default();
        cfy_core::Error::with_source(
            cfy_core::ErrorKind::Api,
            format!("request failed{request_id}"),
            error,
        )
    }
}

/// Shared asynchronous HTTP client with bounded retries.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    base_url: Url,
    default_headers: HeaderMap,
    retry_policy: RetryPolicy,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("base_url", &self.base_url)
            .field("default_headers", &REDACTED)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

impl HttpClient {
    pub fn new(base_url: &str) -> Result<Self, ApiError> {
        install_tls_provider()?;
        let base_url = Url::parse(base_url)
            .map_err(|error| ApiError::Configuration(format!("invalid base URL: {error}")))?;
        let client = reqwest::Client::builder()
            .user_agent(concat!("crabpify/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|source| ApiError::Transport { source })?;
        Ok(Self {
            client,
            base_url,
            default_headers: HeaderMap::new(),
            retry_policy: RetryPolicy::default(),
        })
    }

    #[must_use]
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Adds a secret header and marks it sensitive so dependency debug output cannot expose it.
    #[must_use]
    pub fn with_sensitive_header(mut self, name: HeaderName, mut value: HeaderValue) -> Self {
        value.set_sensitive(true);
        self.default_headers.insert(name, value);
        self
    }

    pub async fn execute(&self, request: &HttpRequest) -> Result<HttpResponse, ApiError> {
        let url = self
            .base_url
            .join(request.path.trim_start_matches('/'))
            .map_err(|error| ApiError::Configuration(format!("invalid request path: {error}")))?;
        if url
            .query_pairs()
            .any(|(key, _)| sensitive_key(key.as_ref()))
        {
            return Err(ApiError::Configuration(
                "sensitive values must be sent in headers or request bodies, not URL queries"
                    .to_owned(),
            ));
        }
        let retryable = request_is_retryable(request);
        let mut attempt = 0;

        loop {
            let mut builder = self
                .client
                .request(request.method.clone(), url.clone())
                .headers(self.default_headers.clone())
                .headers(request.headers.clone());
            if let Some(body) = &request.body {
                builder = builder.json(body);
            }

            match builder.send().await {
                Ok(response) => {
                    let status = response.status();
                    let headers = response.headers().clone();
                    let request_id = request_id(&headers);
                    if retryable && retry_status(status) && attempt < self.retry_policy.max_retries
                    {
                        sleep_before_retry(self.retry_policy, attempt, &headers).await;
                        attempt += 1;
                        continue;
                    }
                    let body = response
                        .bytes()
                        .await
                        .map_err(|source| ApiError::Transport { source })?
                        .to_vec();
                    if !status.is_success() {
                        return Err(http_error(status, request_id, &body));
                    }
                    return Ok(HttpResponse {
                        status,
                        headers,
                        body,
                        request_id,
                    });
                }
                Err(source)
                    if retryable
                        && (source.is_connect() || source.is_timeout())
                        && attempt < self.retry_policy.max_retries =>
                {
                    sleep_before_retry(self.retry_policy, attempt, &HeaderMap::new()).await;
                    attempt += 1;
                }
                Err(source) => return Err(ApiError::Transport { source }),
            }
        }
    }
}

/// Whether a GraphQL operation may be replayed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphQlOperation {
    Query,
    Mutation,
}

#[derive(Clone, Serialize)]
pub struct GraphQlRequest<V> {
    pub query: String,
    pub variables: V,
    #[serde(rename = "operationName", skip_serializing_if = "Option::is_none")]
    pub operation_name: Option<String>,
    #[serde(skip)]
    pub operation: GraphQlOperation,
    #[serde(skip)]
    pub idempotency_key: Option<String>,
}

impl<V> GraphQlRequest<V> {
    #[must_use]
    pub fn query(query: impl Into<String>, variables: V) -> Self {
        Self {
            query: query.into(),
            variables,
            operation_name: None,
            operation: GraphQlOperation::Query,
            idempotency_key: None,
        }
    }

    #[must_use]
    pub fn mutation(query: impl Into<String>, variables: V) -> Self {
        Self {
            operation: GraphQlOperation::Mutation,
            ..Self::query(query, variables)
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct GraphQlError {
    pub message: String,
    #[serde(default)]
    pub path: Vec<Value>,
    #[serde(default)]
    pub extensions: Value,
}

#[derive(Clone, PartialEq)]
pub struct GraphQlResponse<D> {
    pub data: D,
    pub request_id: Option<String>,
    pub extensions: Value,
}

#[derive(Debug, Deserialize)]
struct GraphQlEnvelope<D> {
    data: Option<D>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
    #[serde(default)]
    extensions: Value,
}

/// GraphQL wrapper over the shared HTTP transport.
#[derive(Debug, Clone)]
pub struct GraphQlClient {
    http: HttpClient,
    path: String,
}

impl GraphQlClient {
    #[must_use]
    pub fn new(http: HttpClient, path: impl Into<String>) -> Self {
        Self {
            http,
            path: path.into(),
        }
    }

    pub async fn execute<V, D>(
        &self,
        request: &GraphQlRequest<V>,
    ) -> Result<GraphQlResponse<D>, ApiError>
    where
        V: Serialize,
        D: DeserializeOwned,
    {
        let mut http_request = HttpRequest::new(Method::POST, &self.path);
        http_request.body = Some(serde_json::to_value(request).map_err(|source| {
            ApiError::Configuration(format!("could not serialize GraphQL request: {source}"))
        })?);
        http_request.retry_safety = match request.operation {
            GraphQlOperation::Query => RetrySafety::Idempotent,
            GraphQlOperation::Mutation if request.idempotency_key.is_some() => {
                RetrySafety::Idempotent
            }
            GraphQlOperation::Mutation => RetrySafety::Unsafe,
        };
        if let Some(key) = &request.idempotency_key {
            let value = HeaderValue::from_str(key).map_err(|error| {
                ApiError::Configuration(format!("invalid idempotency key: {error}"))
            })?;
            http_request.headers.insert("Idempotency-Key", value);
        }

        let response = self.http.execute(&http_request).await?;
        let request_id = response.request_id.clone();
        let envelope: GraphQlEnvelope<D> = response.json()?;
        if !envelope.errors.is_empty() {
            return Err(ApiError::GraphQl {
                request_id,
                errors: envelope
                    .errors
                    .into_iter()
                    .map(redact_graphql_error)
                    .collect(),
            });
        }
        let data = envelope.data.ok_or_else(|| ApiError::GraphQlResponse {
            request_id: request_id.clone(),
            message: "response omitted data".to_owned(),
        })?;
        Ok(GraphQlResponse {
            data,
            request_id,
            extensions: envelope.extensions,
        })
    }
}

fn request_is_retryable(request: &HttpRequest) -> bool {
    match request.retry_safety {
        RetrySafety::Idempotent => true,
        RetrySafety::Unsafe => false,
        RetrySafety::Automatic => matches!(
            request.method,
            Method::GET | Method::HEAD | Method::OPTIONS | Method::PUT | Method::DELETE
        ),
    }
}

fn retry_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

async fn sleep_before_retry(policy: RetryPolicy, attempt: u32, headers: &HeaderMap) {
    let retry_after = headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs);
    let exponential = policy
        .base_delay
        .saturating_mul(2_u32.saturating_pow(attempt))
        .min(policy.max_delay);
    let jitter_window = exponential / 5;
    let jitter = if jitter_window.is_zero() {
        Duration::ZERO
    } else {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        jitter_window.mul_f64(f64::from(nanos % 1_000) / 1_000.0)
    };
    tokio::time::sleep(
        retry_after
            .unwrap_or(exponential + jitter)
            .min(policy.max_delay),
    )
    .await;
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    REQUEST_ID_HEADERS.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    })
}

fn http_error(status: StatusCode, request_id: Option<String>, body: &[u8]) -> ApiError {
    let details = serde_json::from_slice::<Value>(body).ok().map(redact_json);
    let message = details.as_ref().and_then(error_message).unwrap_or_else(|| {
        status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_owned()
    });
    ApiError::Http {
        status,
        request_id,
        message,
        details,
    }
}

fn error_message(value: &Value) -> Option<String> {
    value
        .get("errors")
        .or_else(|| value.get("error"))
        .and_then(|value| match value {
            Value::String(message) => Some(message.clone()),
            Value::Object(object) => object
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        })
        .map(|message| redact_text(&message))
}

fn redact_graphql_error(mut error: GraphQlError) -> GraphQlError {
    error.message = redact_text(&error.message);
    error.extensions = redact_json(error.extensions);
    error
}

fn redact_json(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            for (key, value) in object {
                if sensitive_key(key) {
                    *value = Value::String(REDACTED.to_owned());
                } else {
                    *value = redact_json(value.take());
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = redact_json(value.take());
            }
        }
        Value::String(text) => *text = redact_text(text),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    value
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "token",
        "password",
        "secret",
        "authorization",
        "accesstoken",
    ]
    .iter()
    .any(|sensitive| key.contains(sensitive))
}

fn redact_text(text: &str) -> String {
    let parts = text.split_whitespace().collect::<Vec<_>>();
    let mut redacted = Vec::with_capacity(parts.len());
    let mut hide_next = false;
    for part in parts {
        if hide_next {
            redacted.push(REDACTED);
            hide_next = false;
        } else if part.eq_ignore_ascii_case("bearer") {
            redacted.push(REDACTED);
            hide_next = true;
        } else if part.starts_with("shpat_")
            || part.starts_with("shpca_")
            || part.starts_with("shpss_")
        {
            redacted.push(REDACTED);
        } else {
            redacted.push(part);
        }
    }
    redacted.join(" ")
}

#[cfg(test)]
mod tests {
    use super::{HttpRequest, RetrySafety, redact_json, request_is_retryable};
    use reqwest::Method;
    use serde_json::json;

    #[test]
    fn retries_safe_methods_and_explicitly_idempotent_posts() {
        assert!(request_is_retryable(&HttpRequest::new(Method::GET, "/")));
        assert!(!request_is_retryable(&HttpRequest::new(Method::POST, "/")));
        let mut request = HttpRequest::new(Method::POST, "/");
        request.retry_safety = RetrySafety::Idempotent;
        assert!(request_is_retryable(&request));
    }

    #[test]
    fn redacts_sensitive_keys_recursively() {
        let value = redact_json(json!({
            "access_token": "shpat_secret",
            "nested": {"password": "hunter2"},
            "safe": "visible"
        }));
        assert_eq!(value["access_token"], "[REDACTED]");
        assert_eq!(value["nested"]["password"], "[REDACTED]");
        assert_eq!(value["safe"], "visible");
    }
}
