use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cfy_auth::Secret;
use cfy_config::write_atomic;
use cfy_core::{Error, ErrorKind, Result};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::{env, path::PathBuf, sync::OnceLock};
use url::Url;

const INSTANCE_HEADER: HeaderName = HeaderName::from_static("x-shopify-cli-instance");
const VERSION_HEADER: HeaderName = HeaderName::from_static("x-shopify-cli-version");
static TLS_PROVIDER: OnceLock<()> = OnceLock::new();

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PreviewStoreRequest {
    pub name: Option<String>,
    pub country: Option<String>,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub struct PreviewStorePublicResult {
    pub status: &'static str,
    pub message: String,
    pub store: PreviewStorePublicShop,
    pub next_steps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
pub struct PreviewStorePublicShop {
    pub id: String,
    pub name: String,
    pub subdomain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(rename = "storefrontUrl")]
    pub storefront_url: String,
}

pub struct PreviewStoreResult {
    pub shop_id: String,
    pub name: String,
    pub domain: String,
    pub placeholder_account_uuid: Option<String>,
    pub admin_api_token: Secret,
    pub admin_api_scopes: Vec<String>,
    pub access_url: String,
    pub country: Option<String>,
}

impl std::fmt::Debug for PreviewStoreResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewStoreResult")
            .field("shop_id", &self.shop_id)
            .field("name", &self.name)
            .field("domain", &self.domain)
            .field("placeholder_account_uuid", &self.placeholder_account_uuid)
            .field("admin_api_token", &"[REDACTED]")
            .field("admin_api_scopes", &self.admin_api_scopes)
            .field("access_url", &"[REDACTED]")
            .field("country", &self.country)
            .finish()
    }
}

impl PreviewStoreResult {
    #[must_use]
    pub fn public(&self) -> PreviewStorePublicResult {
        let command_store = &self.domain;
        PreviewStorePublicResult {
            status: "success",
            message: format!(
                "Your Shopify store \"{}\" is ready. This store is temporary. Create a free Shopify account to save it and start selling.",
                self.name
            ),
            store: PreviewStorePublicShop {
                id: self.shop_id.clone(),
                name: self.name.clone(),
                subdomain: self.domain.clone(),
                country: self.country.clone(),
                storefront_url: self.access_url.clone(),
            },
            next_steps: vec![
                format!("Use `cfy store open --store {command_store}` to preview the storefront."),
                format!(
                    "Use `cfy store execute --store {command_store}` to add products, collections, pages, and more."
                ),
                format!(
                    "Use `cfy theme pull --store {command_store}` and `cfy theme push --store {command_store}` to edit your store design."
                ),
            ],
        }
    }
}

pub struct PreviewStoreClient {
    http: reqwest::Client,
    endpoint: Url,
    instance_id: String,
}

impl PreviewStoreClient {
    pub fn new() -> Result<Self> {
        let endpoint = env::var("CFY_PREVIEW_STORE_URL")
            .unwrap_or_else(|_| "https://app.shopify.com/services/preview-stores".into());
        let instance_id = load_or_create_instance_id(&default_instance_path())?;
        Self::new_at(&endpoint, instance_id)
    }

    pub fn new_at(endpoint: &str, instance_id: String) -> Result<Self> {
        TLS_PROVIDER.get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
        let endpoint = Url::parse(endpoint).map_err(|source| {
            Error::with_source(ErrorKind::Config, "invalid preview-store endpoint", source)
        })?;
        if endpoint.scheme() != "https"
            && endpoint.host_str() != Some("127.0.0.1")
            && endpoint.host_str() != Some("localhost")
        {
            return Err(Error::config("preview-store endpoint must use HTTPS"));
        }
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(concat!("Catify; v=", env!("CARGO_PKG_VERSION")))
                .map_err(|_| Error::config("invalid Catify user agent"))?,
        );
        headers.insert(
            INSTANCE_HEADER,
            HeaderValue::from_str(&instance_id)
                .map_err(|_| Error::config("preview-store instance ID is invalid"))?,
        );
        headers.insert(
            VERSION_HEADER,
            HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|source| {
                Error::with_source(
                    ErrorKind::Api,
                    "could not create preview-store client",
                    source,
                )
            })?;
        Ok(Self {
            http,
            endpoint,
            instance_id,
        })
    }

    #[must_use]
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub async fn create(&self, request: PreviewStoreRequest) -> Result<PreviewStoreResult> {
        let country = request
            .country
            .map(|country| validate_country(&country))
            .transpose()?;
        let mut body = serde_json::Map::new();
        if let Some(name) = request.name.filter(|name| !name.trim().is_empty()) {
            body.insert("name".into(), serde_json::Value::String(name));
        }
        if let Some(country) = &country {
            body.insert(
                "variables".into(),
                serde_json::json!({"storeCreatePayload": {"country": country}}),
            );
        }
        let response = self
            .http
            .post(self.endpoint.clone())
            .json(&body)
            .send()
            .await
            .map_err(|source| {
                Error::with_source(ErrorKind::Api, "preview store creation failed", source)
            })?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|source| {
            Error::with_source(
                ErrorKind::Api,
                "could not read preview-store response",
                source,
            )
        })?;
        if !status.is_success() {
            return Err(preview_error(status.as_u16(), &bytes));
        }
        let raw: RawResponse = serde_json::from_slice(&bytes).map_err(|source| {
            Error::with_source(
                ErrorKind::Api,
                "preview store creation returned an invalid JSON response",
                source,
            )
        })?;
        let shop = raw.shop.ok_or_else(missing_response_fields)?;
        let shop_id = scalar_string(shop.id).ok_or_else(missing_response_fields)?;
        let name = shop.name.ok_or_else(missing_response_fields)?;
        let domain = shop.domain.ok_or_else(missing_response_fields)?;
        let domain = crate::StoreTarget::parse(&domain)?.domain;
        let token = raw.admin_api_token.ok_or_else(missing_response_fields)?;
        let scopes = raw.admin_api_scopes.ok_or_else(missing_response_fields)?;
        let access_url = raw.access_url.ok_or_else(missing_response_fields)?;
        let access = Url::parse(&access_url)
            .map_err(|_| Error::api("preview-store access URL is invalid"))?;
        if access.scheme() != "https" {
            return Err(Error::api("preview-store access URL must use HTTPS"));
        }
        Ok(PreviewStoreResult {
            shop_id,
            name,
            domain,
            placeholder_account_uuid: raw.placeholder_account_uuid,
            admin_api_token: Secret::new(token),
            admin_api_scopes: scopes,
            access_url,
            country,
        })
    }
}

#[derive(Deserialize)]
struct RawResponse {
    shop: Option<RawShop>,
    placeholder_account_uuid: Option<String>,
    admin_api_token: Option<String>,
    admin_api_scopes: Option<Vec<String>>,
    access_url: Option<String>,
}

#[derive(Deserialize)]
struct RawShop {
    id: Option<serde_json::Value>,
    name: Option<String>,
    domain: Option<String>,
}

fn scalar_string(value: Option<serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(value) => Some(value),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn missing_response_fields() -> Error {
    Error::api("preview store creation response is missing required fields")
}

fn validate_country(value: &str) -> Result<String> {
    let country = value.trim().to_ascii_uppercase();
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Err(Error::invalid_input(
            "--country must be a two-letter ISO 3166-1 alpha-2 code",
        ));
    }
    Ok(country)
}

fn preview_error(status: u16, bytes: &[u8]) -> Error {
    #[derive(Deserialize)]
    struct ErrorBody {
        error_code: Option<String>,
    }
    let code = serde_json::from_slice::<ErrorBody>(bytes)
        .ok()
        .and_then(|body| body.error_code);
    let message = match code.as_deref() {
        Some("not_in_rollout") => "preview store creation is not enabled yet; try again later",
        Some("service_unavailable") => {
            "preview store creation is temporarily unavailable; try again later"
        }
        Some("rate_limited") => "too many preview store creation requests; try again later",
        Some("preview_store_create_failed") => "preview store creation failed; try again later",
        Some("shop_name_banned_keyword" | "shop_name_invalid") => {
            "the preview store name was rejected; use a different name"
        }
        Some("country_invalid") => {
            "the preview store country was rejected; use a different country"
        }
        _ => return Error::api(format!("preview store creation failed with HTTP {status}")),
    };
    Error::api(message)
}

fn default_instance_path() -> PathBuf {
    if let Some(path) = env::var_os("CFY_PREVIEW_STORE_STATE") {
        return PathBuf::from(path);
    }
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(env::temp_dir);
    base.join("catify").join("preview-store.json")
}

fn load_or_create_instance_id(path: &std::path::Path) -> Result<String> {
    #[derive(Deserialize, Serialize)]
    struct State {
        cli_instance_id: String,
    }
    if let Ok(bytes) = std::fs::read(path)
        && let Ok(state) = serde_json::from_slice::<State>(&bytes)
        && !state.cli_instance_id.is_empty()
    {
        return Ok(state.cli_instance_id);
    }
    let mut random = [0_u8; 24];
    getrandom::fill(&mut random).map_err(|error| {
        Error::api(format!(
            "could not create preview-store instance ID: {error}"
        ))
    })?;
    let id = URL_SAFE_NO_PAD.encode(random);
    let bytes = serde_json::to_vec_pretty(&State {
        cli_instance_id: id.clone(),
    })
    .map_err(|source| {
        Error::with_source(
            ErrorKind::Config,
            "could not serialize preview-store state",
            source,
        )
    })?;
    write_atomic(path, &bytes).map_err(|source| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not persist preview-store state: {}", path.display()),
            source,
        )
    })?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn validates_country_and_redacts_result_debug() {
        assert_eq!(validate_country("id").unwrap(), "ID");
        assert!(validate_country("Indonesia").is_err());
        let result = PreviewStoreResult {
            shop_id: "123".into(),
            name: "Preview".into(),
            domain: "preview.myshopify.com".into(),
            placeholder_account_uuid: None,
            admin_api_token: Secret::new("shpat-secret"),
            admin_api_scopes: vec!["read_products".into()],
            access_url: "https://app.shopify.com/auth/preview-store?token=secret".into(),
            country: Some("ID".into()),
        };
        let debug = format!("{result:?}");
        assert!(!debug.contains("shpat-secret"));
        assert!(!debug.contains("token=secret"));
    }

    #[test]
    fn instance_id_is_stable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        let first = load_or_create_instance_id(&path).unwrap();
        assert_eq!(load_or_create_instance_id(&path).unwrap(), first);
    }

    #[tokio::test]
    async fn creates_preview_store_without_auth_and_redacts_public_result() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = stream.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                let text = String::from_utf8_lossy(&bytes);
                let Some(header_end) = text.find("\r\n\r\n") else {
                    continue;
                };
                let content_length = text[..header_end]
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if bytes.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(request.starts_with("POST /services/preview-stores HTTP/1.1"));
            assert!(request.contains("x-shopify-cli-instance: instance-1"));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            assert!(request.contains(r#""name":"Lavender Candles""#));
            assert!(request.contains(r#""country":"US""#));
            let body = r#"{"shop":{"id":123,"name":"Lavender Candles","domain":"preview.myshopify.com"},"placeholder_account_uuid":"placeholder","admin_api_token":"shpat-secret","admin_api_scopes":["read_products"],"access_url":"https://app.shopify.com/auth/preview-store?token=secret"}"#;
            let response = format!(
                "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = PreviewStoreClient::new_at(
            &format!("http://{address}/services/preview-stores"),
            "instance-1".into(),
        )
        .unwrap();
        let result = client
            .create(PreviewStoreRequest {
                name: Some("Lavender Candles".into()),
                country: Some("us".into()),
            })
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(result.domain, "preview.myshopify.com");
        let public = serde_json::to_string(&result.public()).unwrap();
        assert!(!public.contains("shpat-secret"));
        assert!(public.contains("storefrontUrl"));
        assert!(public.contains("token=secret"));
    }
}
