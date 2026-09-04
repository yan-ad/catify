use cfy_auth::Secret;
use reqwest::{
    Client, StatusCode, Url,
    header::{
        ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, SET_COOKIE, USER_AGENT,
    },
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::OnceLock};
use thiserror::Error;

const PROFILE_ACCEPT: &str = "application/vnd.speedscope+json";
const PASSWORD_QUERY: &str =
    "query OnlineStorePasswordProtection { onlineStore { passwordProtection { enabled } } }";

#[derive(Debug, Error)]
pub enum ThemeProfileError {
    #[error("invalid theme profile configuration: {0}")]
    Configuration(String),
    #[error("theme profile request failed: {0}")]
    Request(String),
    #[error("storefront password is required; pass --store-password")]
    StorefrontPasswordRequired,
    #[error("the storefront password is invalid")]
    InvalidStorefrontPassword,
    #[error("Shopify returned HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("Shopify returned invalid profile JSON: {0}")]
    InvalidProfile(String),
}

impl LiquidConsole {
    pub async fn evaluate(&mut self, snippet: &str) -> Result<LiquidEvaluation, ThemeProfileError> {
        let snippet = snippet.trim();
        if snippet.starts_with("{{") || snippet.starts_with("{%") {
            return Err(ThemeProfileError::Configuration(
                "Liquid Console does not accept {{ ... }} or {% ... %} delimiters; enter only the inner expression or tag"
                    .into(),
            ));
        }
        if snippet.is_empty() {
            return Ok(LiquidEvaluation::Display(Value::Null));
        }
        if let Some(assignment) = assignment_tag(snippet) {
            let item =
                serde_json::json!({"type": "context", "value": format!("{{% {assignment} %}}")});
            let body = self.render(&item).await?;
            if has_liquid_error(&body) {
                return Err(ThemeProfileError::Request(strip_liquid_error(&body)));
            }
            self.context.push(item);
            return Ok(LiquidEvaluation::Assigned);
        }

        let display = format!(r#"{{ "type": "display", "value": {{{{ {snippet} | json }}}} }}"#);
        let body = self.render_raw(&display).await?;
        if !has_liquid_error(&body) {
            let values: Vec<Value> =
                serde_json::from_str(strip_rendered_body(&body).ok_or_else(|| {
                    ThemeProfileError::InvalidProfile(
                        "Liquid response did not contain a JSON payload".into(),
                    )
                })?)
                .map_err(|error| ThemeProfileError::InvalidProfile(error.to_string()))?;
            if let Some(value) = values
                .into_iter()
                .find(|value| value.get("type").and_then(Value::as_str) == Some("display"))
                .and_then(|value| value.get("value").cloned())
            {
                return Ok(LiquidEvaluation::Display(value));
            }
        }

        let body = self.render_raw(&format!("{{{{ {snippet} }}}}")).await?;
        if has_liquid_error(&body) {
            return Err(ThemeProfileError::Request(strip_liquid_error(&body)));
        }
        let body = self.render_raw(&format!("{{% {snippet} %}}")).await?;
        Err(ThemeProfileError::Request(if has_liquid_error(&body) {
            strip_liquid_error(&body)
        } else {
            format!("Unknown object, property, tag, or filter: '{snippet}'")
        }))
    }

    async fn render(&self, item: &Value) -> Result<String, ThemeProfileError> {
        self.render_raw(&item.to_string()).await
    }

    async fn render_raw(&self, snippet: &str) -> Result<String, ThemeProfileError> {
        let items = self
            .context
            .iter()
            .map(Value::to_string)
            .chain(std::iter::once(snippet.to_owned()))
            .collect::<Vec<_>>()
            .join(",")
            .replace("\\\"", "\"");
        let mut url = self
            .profiler
            .origin
            .join(self.path.trim_start_matches('/'))
            .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("_fd", "0")
            .append_pair("pb", "0")
            .append_pair("section_id", "announcement-bar");
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair(
                "replace_templates[sections/announcement-bar.liquid]",
                "{% render 'eval' %}",
            )
            .append_pair(
                "replace_templates[snippets/eval.liquid]",
                &format!("\n[{items}]\n"),
            )
            .append_pair("_method", "GET")
            .finish();
        let response = self
            .profiler
            .client
            .post(url)
            .headers(
                self.profiler
                    .storefront_headers(Some(&self.cookies), false)?,
            )
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|error| ThemeProfileError::Request(error.to_string()))?;
        let status = response.status();
        let not_found = response
            .headers()
            .get("server-timing")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains("pageType;desc=\"404\""));
        let body = response
            .text()
            .await
            .map_err(|error| ThemeProfileError::Request(error.to_string()))?;
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(ThemeProfileError::Request(
                "Liquid Console session expired".into(),
            ));
        }
        if matches!(status.as_u16(), 429 | 430) {
            return Err(ThemeProfileError::Request(
                "Liquid evaluations limit reached; try again later".into(),
            ));
        }
        if not_found {
            return Err(ThemeProfileError::Request(
                "Page not found; provide a valid --url value".into(),
            ));
        }
        if !status.is_success() {
            return Err(ThemeProfileError::Http {
                status,
                message: bounded_message(&body),
            });
        }
        Ok(body)
    }
}

fn assignment_tag(snippet: &str) -> Option<String> {
    let trimmed = snippet.trim();
    if trimmed.starts_with("assign ") && trimmed.contains('=') {
        return Some(trimmed.to_owned());
    }
    let (name, value) = trimmed.split_once('=')?;
    let name = name.trim();
    if !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '[' | ']')
        })
        && !value.trim().is_empty()
    {
        Some(format!("assign {name} = {}", value.trim()))
    } else {
        None
    }
}

fn strip_rendered_body(body: &str) -> Option<&str> {
    let body = body.strip_prefix('\n').unwrap_or(body);
    body.strip_suffix('\n')
        .or(Some(body))
        .filter(|body| !body.is_empty())
}

fn has_liquid_error(body: &str) -> bool {
    body.contains("Liquid syntax error")
}

fn strip_liquid_error(body: &str) -> String {
    body.replace(" (snippets/eval line 1)", "")
        .trim()
        .to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum LiquidEvaluation {
    Display(Value),
    Assigned,
}

pub struct LiquidConsole {
    profiler: ThemeProfiler,
    theme_id: u64,
    path: String,
    cookies: BTreeMap<String, String>,
    context: Vec<Value>,
}

impl std::fmt::Debug for LiquidConsole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiquidConsole")
            .field("profiler", &self.profiler)
            .field("theme_id", &self.theme_id)
            .field("path", &self.path)
            .field("cookies", &"[REDACTED]")
            .field("context_items", &self.context.len())
            .finish()
    }
}

#[derive(Clone)]
pub struct ThemeProfiler {
    client: Client,
    origin: Url,
    store: String,
    api_version: String,
    admin_token: Secret,
    storefront_token: Secret,
}

impl std::fmt::Debug for ThemeProfiler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ThemeProfiler")
            .field("origin", &self.origin)
            .field("store", &self.store)
            .field("api_version", &self.api_version)
            .field("admin_token", &"[REDACTED]")
            .field("storefront_token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ThemeProfile {
    pub theme_id: u64,
    pub path: String,
    pub profile: Value,
    #[serde(skip)]
    raw: String,
}

impl ThemeProfile {
    #[must_use]
    pub fn raw_json(&self) -> &str {
        &self.raw
    }
}

#[derive(Deserialize)]
struct PasswordEnvelope {
    data: Option<PasswordData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct PasswordData {
    #[serde(rename = "onlineStore")]
    online_store: Option<OnlineStore>,
}

#[derive(Deserialize)]
struct OnlineStore {
    #[serde(rename = "passwordProtection")]
    password_protection: PasswordProtection,
}

#[derive(Deserialize)]
struct PasswordProtection {
    enabled: bool,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
}

impl ThemeProfiler {
    pub fn new(
        store: &str,
        admin_token: Secret,
        storefront_token: Secret,
        api_version: &str,
    ) -> Result<Self, ThemeProfileError> {
        let store = normalize_store(store)?;
        let origin = Url::parse(&format!("https://{store}/"))
            .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?;
        Self::with_origin(origin, store, admin_token, storefront_token, api_version)
    }

    pub fn with_origin(
        origin: Url,
        store: String,
        admin_token: Secret,
        storefront_token: Secret,
        api_version: &str,
    ) -> Result<Self, ThemeProfileError> {
        if origin.scheme() != "https" && !origin.host_str().is_some_and(is_loopback) {
            return Err(ThemeProfileError::Configuration(
                "theme profile endpoint must use HTTPS unless it is loopback".into(),
            ));
        }
        if admin_token.expose().is_empty() || storefront_token.expose().is_empty() {
            return Err(ThemeProfileError::Configuration(
                "theme profile tokens cannot be empty".into(),
            ));
        }
        install_tls()?;
        let client = Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?;
        Ok(Self {
            client,
            origin,
            store,
            api_version: api_version.to_owned(),
            admin_token,
            storefront_token,
        })
    }

    pub async fn console(
        &self,
        theme_id: u64,
        path: &str,
        storefront_password: Option<&str>,
    ) -> Result<LiquidConsole, ThemeProfileError> {
        let path = normalize_path(path)?;
        let protected = self.password_protected().await?;
        let mut cookies = self.session_cookies(theme_id).await?;
        if protected {
            let password =
                storefront_password.ok_or(ThemeProfileError::StorefrontPasswordRequired)?;
            cookies.extend(self.authenticate_storefront(password, &cookies).await?);
        }
        Ok(LiquidConsole {
            profiler: self.clone(),
            theme_id,
            path,
            cookies,
            context: Vec::new(),
        })
    }

    pub async fn profile(
        &self,
        theme_id: u64,
        path: &str,
        storefront_password: Option<&str>,
    ) -> Result<ThemeProfile, ThemeProfileError> {
        let path = normalize_path(path)?;
        let protected = self.password_protected().await?;
        let mut cookies = self.session_cookies(theme_id).await?;
        if protected {
            let password =
                storefront_password.ok_or(ThemeProfileError::StorefrontPasswordRequired)?;
            cookies.extend(self.authenticate_storefront(password, &cookies).await?);
        }

        let mut url = self
            .origin
            .join(path.trim_start_matches('/'))
            .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?;
        url.query_pairs_mut()
            .append_pair("_fd", "0")
            .append_pair("pb", "0");
        let response = self
            .client
            .get(url)
            .headers(self.storefront_headers(Some(&cookies), false)?)
            .header(ACCEPT, PROFILE_ACCEPT)
            .send()
            .await
            .map_err(|error| ThemeProfileError::Request(error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| ThemeProfileError::Request(error.to_string()))?;
        if status != StatusCode::OK {
            return Err(ThemeProfileError::Http {
                status,
                message: bounded_message(&body),
            });
        }
        let profile = serde_json::from_str(&body)
            .map_err(|error| ThemeProfileError::InvalidProfile(error.to_string()))?;
        Ok(ThemeProfile {
            theme_id,
            path,
            profile,
            raw: body,
        })
    }

    async fn password_protected(&self) -> Result<bool, ThemeProfileError> {
        let endpoint = self
            .origin
            .join(&format!("admin/api/{}/graphql.json", self.api_version))
            .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?;
        let response = self
            .client
            .post(endpoint)
            .header("x-shopify-access-token", self.admin_token.expose())
            .json(&serde_json::json!({"query": PASSWORD_QUERY}))
            .send()
            .await
            .map_err(|error| ThemeProfileError::Request(error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| ThemeProfileError::Request(error.to_string()))?;
        if !status.is_success() {
            return Err(ThemeProfileError::Http {
                status,
                message: bounded_message(&body),
            });
        }
        let envelope: PasswordEnvelope = serde_json::from_str(&body).map_err(|error| {
            ThemeProfileError::Request(format!("invalid Admin GraphQL response: {error}"))
        })?;
        if !envelope.errors.is_empty() {
            return Err(ThemeProfileError::Request(
                envelope
                    .errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        envelope
            .data
            .and_then(|data| data.online_store)
            .map(|store| store.password_protection.enabled)
            .ok_or_else(|| {
                ThemeProfileError::Request(
                    "Admin GraphQL omitted onlineStore password protection".into(),
                )
            })
    }

    async fn session_cookies(
        &self,
        theme_id: u64,
    ) -> Result<BTreeMap<String, String>, ThemeProfileError> {
        let mut url = self.origin.clone();
        url.query_pairs_mut()
            .append_pair("preview_theme_id", &theme_id.to_string())
            .append_pair("_fd", "0")
            .append_pair("pb", "0");
        let response = self
            .client
            .head(url)
            .headers(self.storefront_headers(None, true)?)
            .send()
            .await
            .map_err(|error| ThemeProfileError::Request(error.to_string()))?;
        let cookies = response_cookies(response.headers());
        if !response.status().is_success() && !response.status().is_redirection() {
            return Err(ThemeProfileError::Http {
                status: response.status(),
                message: "could not establish storefront rendering session".into(),
            });
        }
        if !cookies.contains_key("_shopify_essential") {
            return Err(ThemeProfileError::Request(
                "Shopify did not return the required _shopify_essential cookie".into(),
            ));
        }
        Ok(cookies)
    }

    async fn authenticate_storefront(
        &self,
        password: &str,
        cookies: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, ThemeProfileError> {
        let endpoint = self
            .origin
            .join("password")
            .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?;
        let response = self
            .client
            .post(endpoint)
            .headers(self.storefront_headers(Some(cookies), true)?)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(
                url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("password", password)
                    .finish(),
            )
            .send()
            .await
            .map_err(|error| ThemeProfileError::Request(error.to_string()))?;
        let valid_redirect = response.status() == StatusCode::FOUND
            && response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|location| self.origin.join(location).ok())
                .is_some_and(|location| location.origin() == self.origin.origin());
        if !valid_redirect {
            return Err(ThemeProfileError::InvalidStorefrontPassword);
        }
        Ok(response_cookies(response.headers()))
    }

    fn storefront_headers(
        &self,
        cookies: Option<&BTreeMap<String, String>>,
        include_admin: bool,
    ) -> Result<HeaderMap, ThemeProfileError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("Catify/", env!("CARGO_PKG_VERSION"))),
        );
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.storefront_token.expose()))
                .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?,
        );
        if include_admin {
            headers.insert(
                "x-shopify-shop",
                HeaderValue::from_str(&self.store)
                    .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?,
            );
            headers.insert(
                "x-shopify-access-token",
                HeaderValue::from_str(self.admin_token.expose())
                    .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?,
            );
        }
        if let Some(cookies) = cookies {
            headers.insert(
                COOKIE,
                HeaderValue::from_str(&serialize_cookies(cookies))
                    .map_err(|error| ThemeProfileError::Configuration(error.to_string()))?,
            );
        }
        Ok(headers)
    }
}

fn response_cookies(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .filter_map(|cookie| cookie.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

fn serialize_cookies(cookies: &BTreeMap<String, String>) -> String {
    cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn normalize_store(store: &str) -> Result<String, ThemeProfileError> {
    let store = store
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let store = if store.contains('.') {
        store.to_owned()
    } else {
        format!("{store}.myshopify.com")
    };
    if !store.ends_with(".myshopify.com") {
        return Err(ThemeProfileError::Configuration(format!(
            "invalid Shopify store domain `{store}`"
        )));
    }
    Ok(store)
}

fn normalize_path(path: &str) -> Result<String, ThemeProfileError> {
    if path.starts_with("http://")
        || path.starts_with("https://")
        || path.contains('\n')
        || path.contains('\r')
    {
        return Err(ThemeProfileError::Configuration(
            "profile URL must be a relative storefront path".into(),
        ));
    }
    Ok(format!("/{}", path.trim_start_matches('/')))
}

fn bounded_message(message: &str) -> String {
    message.chars().take(512).collect()
}

fn install_tls() -> Result<(), ThemeProfileError> {
    static TLS: OnceLock<()> = OnceLock::new();
    TLS.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    Ok(())
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn rejects_absolute_profile_urls_and_redacts_tokens() {
        let profiler = ThemeProfiler::with_origin(
            Url::parse("http://127.0.0.1:3000/").unwrap(),
            "test.myshopify.com".into(),
            Secret::new("admin-secret"),
            Secret::new("storefront-secret"),
            "2025-07",
        )
        .unwrap();
        let debug = format!("{profiler:?}");
        assert!(!debug.contains("admin-secret"));
        assert!(!debug.contains("storefront-secret"));
        assert!(normalize_path("https://evil.example").is_err());
    }

    #[test]
    fn parses_and_serializes_cookie_headers() {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("_shopify_essential=abc; Path=/"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("storefront_digest=def; HttpOnly"),
        );
        let cookies = response_cookies(&headers);
        assert_eq!(cookies["_shopify_essential"], "abc");
        assert_eq!(
            serialize_cookies(&cookies),
            "_shopify_essential=abc; storefront_digest=def"
        );
    }

    #[tokio::test]
    async fn creates_storefront_session_and_fetches_speedscope_profile() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for index in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                let (status, headers, body) = match index {
                    0 => {
                        assert!(request.starts_with("post /admin/api/2025-07/graphql.json"));
                        assert!(request.contains("x-shopify-access-token: admin-secret"));
                        (
                            "200 OK",
                            "content-type: application/json\r\n",
                            r#"{"data":{"onlineStore":{"passwordProtection":{"enabled":false}}}}"#,
                        )
                    }
                    1 => {
                        assert!(request.starts_with("head /?preview_theme_id=42&_fd=0&pb=0"));
                        assert!(request.contains("authorization: bearer storefront-secret"));
                        assert!(request.contains("x-shopify-access-token: admin-secret"));
                        (
                            "200 OK",
                            "set-cookie: _shopify_essential=session-value; Path=/\r\n",
                            "",
                        )
                    }
                    _ => {
                        assert!(request.starts_with("get /products/example?_fd=0&pb=0"));
                        assert!(request.contains("accept: application/vnd.speedscope+json"));
                        assert!(request.contains("authorization: bearer storefront-secret"));
                        assert!(request.contains("cookie: _shopify_essential=session-value"));
                        assert!(!request.contains("x-shopify-access-token"));
                        (
                            "200 OK",
                            "content-type: application/json\r\n",
                            r#"{"$schema":"https://www.speedscope.app/file-format-schema.json","profiles":[]}"#,
                        )
                    }
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let profiler = ThemeProfiler::with_origin(
            Url::parse(&format!("http://{address}/")).unwrap(),
            "fixture.myshopify.com".into(),
            Secret::new("admin-secret"),
            Secret::new("storefront-secret"),
            "2025-07",
        )
        .unwrap();
        let profile = profiler
            .profile(42, "/products/example", None)
            .await
            .unwrap();
        assert_eq!(profile.theme_id, 42);
        assert_eq!(profile.path, "/products/example");
        assert_eq!(profile.profile["profiles"], serde_json::json!([]));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn evaluates_liquid_and_persists_assignment_context() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for index in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let headers_end = request.windows(4).position(|part| part == b"\r\n\r\n");
                    if let Some(headers_end) = headers_end {
                        let headers_end = headers_end + 4;
                        let headers = String::from_utf8_lossy(&request[..headers_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if request.len() >= headers_end + content_length {
                            break;
                        }
                    }
                }
                let request_text = String::from_utf8_lossy(&request);
                let lower = request_text.to_ascii_lowercase();
                let (headers, body) = match index {
                    0 => (
                        "content-type: application/json\r\n",
                        r#"{"data":{"onlineStore":{"passwordProtection":{"enabled":false}}}}"#,
                    ),
                    1 => (
                        "set-cookie: _shopify_essential=session-value; Path=/\r\n",
                        "",
                    ),
                    2 => {
                        assert!(lower.starts_with(
                            "post /products/example?_fd=0&pb=0&section_id=announcement-bar"
                        ));
                        assert!(request_text.contains("shop.name"));
                        (
                            "content-type: text/html\r\n",
                            "\n[{\"type\":\"display\",\"value\":\"Demo\"}]\n",
                        )
                    }
                    _ => {
                        assert!(
                            request_text.contains("assign+x+%3D+1")
                                || request_text.contains("assign%20x%20%3D%201")
                        );
                        (
                            "content-type: text/html\r\n",
                            "\n[{\"type\":\"context\",\"value\":\"{% assign x = 1 %}\"}]\n",
                        )
                    }
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let profiler = ThemeProfiler::with_origin(
            Url::parse(&format!("http://{address}/")).unwrap(),
            "fixture.myshopify.com".into(),
            Secret::new("admin-secret"),
            Secret::new("storefront-secret"),
            "2025-07",
        )
        .unwrap();
        let mut console = profiler
            .console(42, "/products/example", None)
            .await
            .unwrap();
        assert_eq!(
            console.evaluate("shop.name").await.unwrap(),
            LiquidEvaluation::Display(Value::String("Demo".into()))
        );
        assert_eq!(
            console.evaluate("x = 1").await.unwrap(),
            LiquidEvaluation::Assigned
        );
        assert_eq!(console.context.len(), 1);
        server.await.unwrap();
    }
}
