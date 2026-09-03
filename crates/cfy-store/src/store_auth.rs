use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cfy_auth::{CredentialStore, NativeCredentialStore, Secret, Session};
use cfy_config::write_atomic;
use cfy_core::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    path::PathBuf,
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};
use url::Url;

pub const STORE_AUTH_APP_CLIENT_ID: &str = "7e9cb568cfd431c538f36d1ad3f2b4f6";
pub const STORE_AUTH_PORT: u16 = 13_387;
const CALLBACK_PATH: &str = "/auth/callback";
const STORE_AUTH_SERVICE: &str = "dev.catify.cfy.store-auth";
static TLS_PROVIDER: OnceLock<std::result::Result<(), String>> = OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreAuthBootstrap {
    pub store: String,
    pub scopes: Vec<String>,
    pub state: String,
    pub redirect_uri: String,
    pub authorization_url: String,
    code_verifier: String,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub struct StoreAuthListEntry {
    pub store: String,
    pub user_id: String,
    pub scopes: Vec<String>,
    pub acquired_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token_expires_at: Option<String>,
    pub associated_user: AssociatedUser,
}

impl StoreAuthSummary {
    #[must_use]
    pub fn public(&self) -> StoreAuthListEntry {
        StoreAuthListEntry {
            store: self.store.clone(),
            user_id: self.user_id.clone(),
            scopes: self.scopes.clone(),
            acquired_at: self.acquired_at.clone(),
            expires_at: self.expires_at.clone(),
            refresh_token_expires_at: self.refresh_token_expires_at.clone(),
            associated_user: self.associated_user.clone(),
        }
    }
}

impl StoreAuthBootstrap {
    pub fn new(store: &str, scopes: &str) -> Result<Self> {
        let store = crate::StoreTarget::parse(store)?.domain;
        let scopes = parse_scopes(scopes)?;
        let state = random_base64url(32)?;
        let code_verifier = random_base64url(32)?;
        let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
        let redirect_uri = format!("http://127.0.0.1:{STORE_AUTH_PORT}{CALLBACK_PATH}");
        let mut authorization = Url::parse(&format!("https://{store}/admin/oauth/authorize"))
            .map_err(|source| {
                Error::with_source(
                    ErrorKind::Config,
                    "could not build store authorization URL",
                    source,
                )
            })?;
        authorization
            .query_pairs_mut()
            .append_pair("client_id", STORE_AUTH_APP_CLIENT_ID)
            .append_pair("scope", &scopes.join(","))
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("state", &state)
            .append_pair("response_type", "code")
            .append_pair("code_challenge", &code_challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(Self {
            store,
            scopes,
            state,
            redirect_uri,
            authorization_url: authorization.into(),
            code_verifier,
        })
    }

    #[must_use]
    pub fn code_verifier(&self) -> &str {
        &self.code_verifier
    }
}

pub struct StoreAuthCallback {
    listener: TcpListener,
    store: String,
    state: String,
}

impl StoreAuthCallback {
    pub async fn bind(bootstrap: &StoreAuthBootstrap) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", STORE_AUTH_PORT))
            .await
            .map_err(|source| {
                Error::with_source(
                    ErrorKind::Config,
                    format!(
                        "port {STORE_AUTH_PORT} is already in use; free it and retry store auth"
                    ),
                    source,
                )
            })?;
        Ok(Self {
            listener,
            store: bootstrap.store.clone(),
            state: bootstrap.state.clone(),
        })
    }

    pub async fn wait(self, wait: Duration) -> Result<String> {
        timeout(wait, self.wait_inner()).await.map_err(|_| {
            Error::new(
                ErrorKind::Api,
                "timed out waiting for Shopify OAuth callback",
            )
        })?
    }

    async fn wait_inner(self) -> Result<String> {
        let (mut socket, _) = self.listener.accept().await.map_err(|source| {
            Error::with_source(
                ErrorKind::Api,
                "could not accept Shopify OAuth callback",
                source,
            )
        })?;
        let mut request = vec![0_u8; 16 * 1024];
        let read = socket.read(&mut request).await.map_err(|source| {
            Error::with_source(
                ErrorKind::Api,
                "could not read Shopify OAuth callback",
                source,
            )
        })?;
        let first_line = String::from_utf8_lossy(&request[..read])
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        let target = first_line
            .split_whitespace()
            .nth(1)
            .ok_or_else(|| Error::invalid_input("OAuth callback request was malformed"))?;
        let callback = Url::parse(&format!("http://127.0.0.1:{STORE_AUTH_PORT}{target}")).map_err(
            |source| {
                Error::with_source(ErrorKind::Config, "OAuth callback URL was invalid", source)
            },
        )?;
        let result = validate_callback(&callback, &self.store, &self.state);
        let (status, title, message) = match &result {
            Ok(_) => (
                "200 OK",
                "Authentication succeeded",
                "Close this window and return to the terminal.",
            ),
            Err(error) => ("400 Bad Request", "Authentication failed", error.message()),
        };
        let html = callback_page(title, message);
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
            html.len(),
            html
        );
        socket
            .write_all(response.as_bytes())
            .await
            .map_err(|source| {
                Error::with_source(
                    ErrorKind::Api,
                    "could not write OAuth callback response",
                    source,
                )
            })?;
        result
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    refresh_token_expires_in: Option<u64>,
    associated_user: Option<AssociatedUser>,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    refresh_token_expires_in: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct AssociatedUser {
    pub id: u64,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, rename = "first_name")]
    pub first_name: Option<String>,
    #[serde(default, rename = "last_name")]
    pub last_name: Option<String>,
    #[serde(default, rename = "account_owner")]
    pub account_owner: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct StoreAuthSummary {
    pub store: String,
    pub user_id: String,
    pub scopes: Vec<String>,
    pub acquired_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token_expires_at: Option<String>,
    pub associated_user: AssociatedUser,
    credential_identity: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct StoreAuthIndex {
    sessions: BTreeMap<String, StoreAuthSummary>,
}

pub struct StoreAuthRegistry {
    index_path: PathBuf,
    credentials: NativeCredentialStore,
}

impl Default for StoreAuthRegistry {
    fn default() -> Self {
        Self::new(default_index_path())
    }
}

impl StoreAuthRegistry {
    #[must_use]
    pub fn new(index_path: PathBuf) -> Self {
        Self {
            index_path,
            credentials: NativeCredentialStore::new(STORE_AUTH_SERVICE),
        }
    }

    pub fn list(&self) -> Result<Vec<StoreAuthSummary>> {
        let mut summaries = self
            .load_index()?
            .sessions
            .into_values()
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| {
            right
                .acquired_at
                .cmp(&left.acquired_at)
                .then_with(|| left.store.cmp(&right.store))
        });
        Ok(summaries)
    }

    pub async fn list_current(&self) -> Result<Vec<StoreAuthSummary>> {
        let mut index = self.load_index()?;
        let mut summaries = Vec::new();
        let stores = index.sessions.keys().cloned().collect::<Vec<_>>();
        let mut changed = false;
        for store in stores {
            let Some(summary) = index.sessions.get(&store).cloned() else {
                continue;
            };
            if self
                .credentials
                .load(&summary.credential_identity)
                .await?
                .is_some()
            {
                summaries.push(summary);
            } else {
                index.sessions.remove(&store);
                changed = true;
            }
        }
        if changed {
            self.save_index(&index)?;
        }
        summaries.sort_by(|left, right| {
            right
                .acquired_at
                .cmp(&left.acquired_at)
                .then_with(|| left.store.cmp(&right.store))
        });
        Ok(summaries)
    }

    pub async fn save(&self, result: &StoreAuthResult) -> Result<()> {
        let identity = credential_identity(&result.store, &result.user_id);
        let session = Session {
            identity: identity.clone(),
            display_name: result.associated_user.email.clone(),
            access_token: result.access_token.clone(),
            refresh_token: result
                .refresh_token
                .clone()
                .unwrap_or_else(|| Secret::new("")),
            expires_at_unix: result.expires_at_unix.unwrap_or(u64::MAX),
            scopes: result.scopes.clone(),
        };
        self.credentials.save(&session).await?;
        let mut index = self.load_index()?;
        index.sessions.insert(
            result.store.clone(),
            StoreAuthSummary {
                store: result.store.clone(),
                user_id: result.user_id.clone(),
                scopes: result.scopes.clone(),
                acquired_at: format_unix(result.acquired_at_unix),
                expires_at: result.expires_at_unix.map(format_unix),
                refresh_token_expires_at: result.refresh_token_expires_at_unix.map(format_unix),
                associated_user: result.associated_user.clone(),
                credential_identity: identity.clone(),
            },
        );
        if let Err(error) = self.save_index(&index) {
            let _ = self.credentials.delete(&identity).await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn load(&self, store: &str) -> Result<Option<Session>> {
        let store = crate::StoreTarget::parse(store)?.domain;
        let Some(summary) = self.load_index()?.sessions.get(&store).cloned() else {
            return Ok(None);
        };
        self.credentials.load(&summary.credential_identity).await
    }

    pub async fn access_token(&self, store: &str) -> Result<Option<Secret>> {
        let store = crate::StoreTarget::parse(store)?.domain;
        let mut index = self.load_index()?;
        let Some(summary) = index.sessions.get_mut(&store) else {
            return Ok(None);
        };
        let Some(mut session) = self.credentials.load(&summary.credential_identity).await? else {
            index.sessions.remove(&store);
            self.save_index(&index)?;
            return Ok(None);
        };
        let now = unix_now();
        if session.is_valid_at(now, 240) {
            return Ok(Some(session.access_token.clone()));
        }
        if session.refresh_token.expose().is_empty() {
            return Err(Error::new(
                ErrorKind::Api,
                format!(
                    "store-auth session for `{store}` expired without a refresh token; run `cfy store auth` again"
                ),
            ));
        }
        let refreshed =
            refresh_token_at(&token_endpoint(&store)?, session.refresh_token.expose()).await?;
        session.access_token = Secret::new(refreshed.access_token);
        if let Some(refresh_token) = refreshed.refresh_token {
            session.refresh_token = Secret::new(refresh_token);
        }
        session.expires_at_unix = refreshed
            .expires_in
            .map(|seconds| now.saturating_add(seconds))
            .unwrap_or(u64::MAX);
        summary.expires_at = refreshed
            .expires_in
            .map(|seconds| format_unix(now.saturating_add(seconds)));
        summary.refresh_token_expires_at = refreshed
            .refresh_token_expires_in
            .map(|seconds| format_unix(now.saturating_add(seconds)));
        self.credentials.save(&session).await?;
        self.save_index(&index)?;
        Ok(Some(session.access_token.clone()))
    }

    fn load_index(&self) -> Result<StoreAuthIndex> {
        match std::fs::read(&self.index_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| {
                Error::with_source(
                    ErrorKind::Config,
                    format!("store auth index is invalid: {}", self.index_path.display()),
                    source,
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(StoreAuthIndex::default())
            }
            Err(source) => Err(Error::with_source(
                ErrorKind::Config,
                format!(
                    "could not read store auth index: {}",
                    self.index_path.display()
                ),
                source,
            )),
        }
    }

    fn save_index(&self, index: &StoreAuthIndex) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(index).map_err(|source| {
            Error::with_source(
                ErrorKind::Config,
                "could not serialize store auth index",
                source,
            )
        })?;
        write_atomic(&self.index_path, &bytes).map_err(|source| {
            Error::with_source(
                ErrorKind::Config,
                format!(
                    "could not persist store auth index: {}",
                    self.index_path.display()
                ),
                source,
            )
        })
    }
}

pub struct StoreAuthResult {
    pub store: String,
    pub user_id: String,
    pub scopes: Vec<String>,
    pub acquired_at_unix: u64,
    pub expires_at_unix: Option<u64>,
    pub refresh_token_expires_at_unix: Option<u64>,
    pub associated_user: AssociatedUser,
    access_token: Secret,
    refresh_token: Option<Secret>,
}

impl std::fmt::Debug for StoreAuthResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoreAuthResult")
            .field("store", &self.store)
            .field("user_id", &self.user_id)
            .field("scopes", &self.scopes)
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Serialize)]
pub struct StoreAuthPublicResult {
    pub store: String,
    pub user_id: String,
    pub scopes: Vec<String>,
    pub acquired_at: String,
    pub expires_at: Option<String>,
    pub refresh_token_expires_at: Option<String>,
    pub has_refresh_token: bool,
    pub associated_user: AssociatedUser,
}

impl StoreAuthResult {
    #[must_use]
    pub fn public(&self) -> StoreAuthPublicResult {
        StoreAuthPublicResult {
            store: self.store.clone(),
            user_id: self.user_id.clone(),
            scopes: self.scopes.clone(),
            acquired_at: format_unix(self.acquired_at_unix),
            expires_at: self.expires_at_unix.map(format_unix),
            refresh_token_expires_at: self.refresh_token_expires_at_unix.map(format_unix),
            has_refresh_token: self.refresh_token.is_some(),
            associated_user: self.associated_user.clone(),
        }
    }
}

pub async fn exchange_code(bootstrap: &StoreAuthBootstrap, code: &str) -> Result<StoreAuthResult> {
    exchange_code_at(bootstrap, code, &token_endpoint(&bootstrap.store)?).await
}

async fn exchange_code_at(
    bootstrap: &StoreAuthBootstrap,
    code: &str,
    endpoint: &Url,
) -> Result<StoreAuthResult> {
    install_tls_provider()?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("catify/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|source| {
            Error::with_source(ErrorKind::Api, "could not create OAuth client", source)
        })?;
    let response = client
        .post(endpoint.clone())
        .json(&serde_json::json!({
            "client_id": STORE_AUTH_APP_CLIENT_ID,
            "code": code,
            "code_verifier": bootstrap.code_verifier(),
            "redirect_uri": bootstrap.redirect_uri,
        }))
        .send()
        .await
        .map_err(|source| {
            Error::with_source(ErrorKind::Api, "store token exchange failed", source)
        })?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|source| {
        Error::with_source(
            ErrorKind::Api,
            "could not read store token response",
            source,
        )
    })?;
    if !status.is_success() {
        return Err(Error::new(
            ErrorKind::Api,
            format!("store token exchange failed with HTTP {status}"),
        ));
    }
    let token: TokenResponse = serde_json::from_slice(&bytes).map_err(|source| {
        Error::with_source(
            ErrorKind::Api,
            "Shopify returned an invalid store token response",
            source,
        )
    })?;
    let scopes = resolve_granted_scopes(token.scope.as_deref(), &bootstrap.scopes)?;
    let associated_user = token.associated_user.ok_or_else(|| {
        Error::new(
            ErrorKind::Api,
            "Shopify did not return associated user information for the online access token",
        )
    })?;
    let now = unix_now();
    Ok(StoreAuthResult {
        store: bootstrap.store.clone(),
        user_id: associated_user.id.to_string(),
        scopes,
        acquired_at_unix: now,
        expires_at_unix: token.expires_in.map(|seconds| now.saturating_add(seconds)),
        refresh_token_expires_at_unix: token
            .refresh_token_expires_in
            .map(|seconds| now.saturating_add(seconds)),
        associated_user,
        access_token: Secret::new(token.access_token),
        refresh_token: token.refresh_token.map(Secret::new),
    })
}

async fn refresh_token_at(endpoint: &Url, refresh_token: &str) -> Result<RefreshResponse> {
    install_tls_provider()?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("catify/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|source| {
            Error::with_source(ErrorKind::Api, "could not create OAuth client", source)
        })?;
    let response = client
        .post(endpoint.clone())
        .json(&serde_json::json!({
            "client_id": STORE_AUTH_APP_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|source| {
            Error::with_source(ErrorKind::Api, "store token refresh failed", source)
        })?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(|source| {
        Error::with_source(
            ErrorKind::Api,
            "could not read store token refresh response",
            source,
        )
    })?;
    if !status.is_success() {
        return Err(Error::new(
            ErrorKind::Api,
            format!("store token refresh failed with HTTP {status}"),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|source| {
        Error::with_source(
            ErrorKind::Api,
            "Shopify returned an invalid refresh response",
            source,
        )
    })
}

fn token_endpoint(store: &str) -> Result<Url> {
    let endpoint = env::var("CFY_STORE_AUTH_TOKEN_URL")
        .unwrap_or_else(|_| format!("https://{store}/admin/oauth/access_token"));
    let url = Url::parse(&endpoint).map_err(|source| {
        Error::with_source(
            ErrorKind::Config,
            "store OAuth token endpoint is invalid",
            source,
        )
    })?;
    if url.scheme() != "https"
        && url.host_str() != Some("127.0.0.1")
        && url.host_str() != Some("localhost")
    {
        return Err(Error::invalid_input(
            "store OAuth token endpoint must use HTTPS",
        ));
    }
    Ok(url)
}

fn validate_callback(callback: &Url, expected_store: &str, expected_state: &str) -> Result<String> {
    if callback.path() != CALLBACK_PATH {
        return Err(Error::invalid_input(
            "OAuth callback path was not recognized",
        ));
    }
    let values = callback.query_pairs().collect::<BTreeMap<_, _>>();
    let returned_store = values
        .get("shop")
        .ok_or_else(|| Error::invalid_input("OAuth callback did not include a store"))?;
    let returned_store = crate::StoreTarget::parse(returned_store)?.domain;
    if returned_store != expected_store {
        return Err(Error::invalid_input(
            "OAuth callback store does not match the requested store",
        ));
    }
    let returned_state = values
        .get("state")
        .ok_or_else(|| Error::invalid_input("OAuth callback did not include state"))?;
    if !constant_time_equal(returned_state.as_bytes(), expected_state.as_bytes()) {
        return Err(Error::invalid_input(
            "OAuth callback state does not match the request",
        ));
    }
    if let Some(error) = values.get("error") {
        return Err(Error::new(
            ErrorKind::Api,
            format!("Shopify returned OAuth error: {error}"),
        ));
    }
    values
        .get("code")
        .map(|code| code.to_string())
        .ok_or_else(|| Error::invalid_input("OAuth callback did not include an authorization code"))
}

fn parse_scopes(input: &str) -> Result<Vec<String>> {
    let mut scopes = input
        .split([',', ' '])
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    if scopes.is_empty() {
        return Err(Error::invalid_input(
            "at least one scope is required; pass --scopes as a comma-separated list",
        ));
    }
    Ok(scopes)
}

fn resolve_granted_scopes(granted: Option<&str>, requested: &[String]) -> Result<Vec<String>> {
    let Some(granted) = granted else {
        return Ok(requested.to_vec());
    };
    let granted = parse_scopes(granted)?;
    let expanded = granted
        .iter()
        .flat_map(|scope| {
            let mut values = vec![scope.clone()];
            if let Some(suffix) = scope.strip_prefix("write_") {
                values.push(format!("read_{suffix}"));
            }
            values
        })
        .collect::<Vec<_>>();
    let missing = requested
        .iter()
        .filter(|scope| !expanded.contains(scope))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(Error::new(
            ErrorKind::Api,
            format!(
                "Shopify granted fewer scopes than requested; missing: {}",
                missing.join(", ")
            ),
        ));
    }
    Ok(granted)
}

fn random_base64url(length: usize) -> Result<String> {
    let mut bytes = vec![0_u8; length];
    getrandom::fill(&mut bytes).map_err(|source| {
        Error::new(
            ErrorKind::Config,
            format!("could not generate OAuth randomness: {source}"),
        )
    })?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn install_tls_provider() -> Result<()> {
    TLS_PROVIDER
        .get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            Ok(())
        })
        .clone()
        .map_err(|error| Error::new(ErrorKind::Config, error))
}

fn credential_identity(store: &str, user_id: &str) -> String {
    format!("{store}::{user_id}")
}

fn default_index_path() -> PathBuf {
    if let Some(path) = env::var_os("CFY_STORE_AUTH_INDEX") {
        return PathBuf::from(path);
    }
    let root = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    root.join("catify").join("store-auth.json")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn format_unix(unix: u64) -> String {
    OffsetDateTime::from_unix_timestamp(i64::try_from(unix).unwrap_or(i64::MAX))
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| unix.to_string())
}

fn callback_page(title: &str, message: &str) -> String {
    fn escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head><body><main><h1>{}</h1><p>{}</p></main></body></html>",
        escape(title),
        escape(title),
        escape(message)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn builds_shopify_pkce_url_and_normalizes_scopes() {
        let bootstrap =
            StoreAuthBootstrap::new("demo", "write_products, read_products write_products")
                .unwrap();
        assert_eq!(bootstrap.store, "demo.myshopify.com");
        assert_eq!(bootstrap.scopes, ["read_products", "write_products"]);
        assert!(
            bootstrap
                .authorization_url
                .contains(STORE_AUTH_APP_CLIENT_ID)
        );
        assert!(
            bootstrap
                .authorization_url
                .contains("code_challenge_method=S256")
        );
        assert!(!bootstrap.code_verifier().is_empty());
    }

    #[test]
    fn callback_rejects_wrong_store_and_state() {
        let url = Url::parse(
            "http://127.0.0.1:13387/auth/callback?shop=other.myshopify.com&state=wrong&code=secret",
        )
        .unwrap();
        assert!(validate_callback(&url, "demo.myshopify.com", "expected").is_err());
    }

    #[test]
    fn granted_write_scope_implies_read_scope() {
        let scopes = resolve_granted_scopes(
            Some("write_products"),
            &["read_products".into(), "write_products".into()],
        )
        .unwrap();
        assert_eq!(scopes, ["write_products"]);
    }

    #[test]
    fn result_debug_never_exposes_tokens() {
        let result = StoreAuthResult {
            store: "demo.myshopify.com".into(),
            user_id: "1".into(),
            scopes: vec!["read_products".into()],
            acquired_at_unix: 1,
            expires_at_unix: Some(240),
            refresh_token_expires_at_unix: None,
            associated_user: AssociatedUser {
                id: 1,
                email: Some("dev@example.com".into()),
                first_name: None,
                last_name: None,
                account_owner: None,
            },
            access_token: Secret::new("access-secret"),
            refresh_token: Some(Secret::new("refresh-secret")),
        };
        let debug = format!("{result:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
    }

    #[test]
    fn registry_index_is_non_secret_and_sorted() {
        let fixture = tempfile::tempdir().unwrap();
        let registry = StoreAuthRegistry::new(fixture.path().join("store-auth.json"));
        assert!(registry.list().unwrap().is_empty());
        assert!(!Path::new(&registry.index_path).exists());
    }

    #[tokio::test]
    async fn exchanges_authorization_code_without_exposing_tokens() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 16 * 1024];
            let read = socket.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("\"code\":\"oauth-code\""));
            assert!(request.contains(STORE_AUTH_APP_CLIENT_ID));
            let body = r#"{"access_token":"access-secret","refresh_token":"refresh-secret","expires_in":3600,"refresh_token_expires_in":7200,"scope":"write_products","associated_user":{"id":99,"email":"dev@example.com"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let bootstrap = StoreAuthBootstrap::new("demo", "read_products,write_products").unwrap();
        let endpoint = Url::parse(&format!("http://{address}/token")).unwrap();
        let result = exchange_code_at(&bootstrap, "oauth-code", &endpoint)
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(result.user_id, "99");
        assert_eq!(result.scopes, ["write_products"]);
        let public = serde_json::to_string(&result.public()).unwrap();
        assert!(!public.contains("access-secret"));
        assert!(!public.contains("refresh-secret"));
    }
}
