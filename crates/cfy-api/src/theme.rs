//! Shopify theme metadata listing over the Admin REST API.

use crate::{ApiError, HttpClient, HttpRequest};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use cfy_core::Cancellation;
use reqwest::{
    Method, StatusCode, Url,
    header::{HeaderName, HeaderValue, LINK},
};
use serde::{Deserialize, Serialize};

const TOKEN_HEADER: &str = "x-shopify-access-token";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Theme {
    pub id: u64,
    pub name: String,
    pub role: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub previewable: Option<bool>,
    #[serde(default)]
    pub processing: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeAsset {
    pub key: String,
    pub contents: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct AssetsEnvelope {
    assets: Vec<AssetMetadata>,
}

#[derive(Debug, Deserialize)]
struct AssetMetadata {
    key: String,
}

#[derive(Debug, Deserialize)]
struct AssetEnvelope {
    asset: AssetPayload,
}

#[derive(Debug, Deserialize)]
struct AssetPayload {
    key: String,
    value: Option<String>,
    attachment: Option<String>,
}

fn theme_api_error(error: ApiError) -> ApiError {
    match error {
        ApiError::Http {
            status: StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN,
            ..
        } => {
            ApiError::Configuration(
                "theme access was denied; refresh SHOPIFY_CLI_THEME_TOKEN and verify theme read permissions for this store"
                    .to_owned(),
            )
        }
        error => error,
    }
}

fn selected(key: &str, includes: &[String], excludes: &[String]) -> bool {
    (includes.is_empty() || includes.iter().any(|pattern| matches_pattern(pattern, key)))
        && !excludes.iter().any(|pattern| matches_pattern(pattern, key))
}

fn matches_pattern(pattern: &str, value: &str) -> bool {
    let (mut pattern, mut value) = (pattern.as_bytes(), value.as_bytes());
    let (mut star_pattern, mut star_value) = (None, &[][..]);
    while !value.is_empty() {
        if !pattern.is_empty() && (pattern[0] == b'?' || pattern[0] == value[0]) {
            pattern = &pattern[1..];
            value = &value[1..];
        } else if !pattern.is_empty() && pattern[0] == b'*' {
            star_pattern = Some(&pattern[1..]);
            pattern = &pattern[1..];
            star_value = value;
        } else if let Some(after_star) = star_pattern {
            if star_value.is_empty() {
                return false;
            }
            star_value = &star_value[1..];
            value = star_value;
            pattern = after_star;
        } else {
            return false;
        }
    }
    pattern.iter().all(|byte| *byte == b'*')
}

#[derive(Debug, Deserialize)]
struct ThemesEnvelope {
    themes: Vec<Theme>,
}

#[derive(Debug, Clone)]
pub struct ThemeClient {
    http: HttpClient,
    origin: Url,
    first_path: String,
    api_root: String,
}

impl ThemeClient {
    pub fn new(store: &str, token: &str, api_version: &str) -> Result<Self, ApiError> {
        let store = normalize_store(store)?;
        if token.trim().is_empty() {
            return Err(ApiError::Configuration(
                "theme access token is empty; provide SHOPIFY_CLI_THEME_TOKEN".to_owned(),
            ));
        }
        let origin = Url::parse(&format!("https://{store}/"))
            .map_err(|error| ApiError::Configuration(format!("invalid store URL: {error}")))?;
        let token = HeaderValue::from_str(token)
            .map_err(|error| ApiError::Configuration(format!("invalid theme token: {error}")))?;
        let http = HttpClient::new(origin.as_str())?
            .with_sensitive_header(HeaderName::from_static(TOKEN_HEADER), token);
        Ok(Self {
            http,
            origin,
            first_path: format!("admin/api/{api_version}/themes.json?limit=250"),
            api_root: format!("admin/api/{api_version}"),
        })
    }

    #[cfg(test)]
    fn with_http(http: HttpClient, origin: Url, first_path: impl Into<String>) -> Self {
        Self {
            http,
            origin,
            first_path: first_path.into(),
            api_root: String::new(),
        }
    }

    pub async fn list(&self) -> Result<Vec<Theme>, ApiError> {
        let mut themes = Vec::new();
        let mut path = self.first_path.clone();
        loop {
            let response = self
                .http
                .execute(&HttpRequest::new(Method::GET, &path))
                .await
                .map_err(theme_api_error)?;
            let next = response
                .headers
                .get(LINK)
                .and_then(|value| value.to_str().ok())
                .and_then(next_link);
            themes.extend(response.json::<ThemesEnvelope>()?.themes);
            let Some(next) = next else { break };
            path = same_origin_path(&self.origin, &next)?;
        }
        Ok(themes)
    }

    /// Download all selected assets before returning them to the caller.
    pub async fn pull(
        &self,
        theme_id: u64,
        includes: &[String],
        excludes: &[String],
        cancellation: &Cancellation,
    ) -> Result<Vec<ThemeAsset>, ApiError> {
        check_cancelled(cancellation)?;
        let path = format!("{}/themes/{theme_id}/assets.json?fields=key", self.api_root);
        let response = self
            .http
            .execute(&HttpRequest::new(Method::GET, path))
            .await
            .map_err(theme_api_error)?;
        let metadata = response.json::<AssetsEnvelope>()?.assets;
        let mut assets = Vec::new();
        for asset in metadata {
            check_cancelled(cancellation)?;
            if !selected(&asset.key, includes, excludes) {
                continue;
            }
            let mut url = Url::parse("https://placeholder.invalid/").expect("valid URL");
            url.query_pairs_mut().append_pair("asset[key]", &asset.key);
            let query = url.query().unwrap_or_default();
            let path = format!("{}/themes/{theme_id}/assets.json?{query}", self.api_root);
            let response = self
                .http
                .execute(&HttpRequest::new(Method::GET, path))
                .await
                .map_err(theme_api_error)?;
            let payload = response.json::<AssetEnvelope>()?.asset;
            if payload.key != asset.key {
                return Err(ApiError::Configuration(format!(
                    "Shopify returned asset {:?} while {:?} was requested",
                    payload.key, asset.key
                )));
            }
            let contents = match (payload.value, payload.attachment) {
                (Some(value), None) => value.into_bytes(),
                (None, Some(attachment)) => BASE64.decode(attachment).map_err(|error| {
                    ApiError::Configuration(format!(
                        "invalid base64 attachment for {}: {error}",
                        payload.key
                    ))
                })?,
                _ => {
                    return Err(ApiError::Configuration(format!(
                        "asset {} did not contain exactly one of value or attachment",
                        payload.key
                    )));
                }
            };
            assets.push(ThemeAsset {
                key: payload.key,
                contents,
            });
        }
        Ok(assets)
    }
}

fn check_cancelled(cancellation: &Cancellation) -> Result<(), ApiError> {
    if cancellation.is_cancelled() {
        Err(ApiError::Configuration(
            "theme pull was cancelled".to_owned(),
        ))
    } else {
        Ok(())
    }
}

pub fn normalize_store(store: &str) -> Result<String, ApiError> {
    let store = store.trim().trim_end_matches('/');
    let host = if store.contains("://") {
        Url::parse(store)
            .map_err(|error| ApiError::Configuration(format!("invalid store URL: {error}")))?
            .host_str()
            .map(str::to_owned)
    } else {
        Some(store.to_owned())
    }
    .ok_or_else(|| ApiError::Configuration("store URL has no host".to_owned()))?;
    let host = host.to_ascii_lowercase();
    let host = if host.contains('.') {
        host
    } else {
        format!("{host}.myshopify.com")
    };
    if !host.ends_with(".myshopify.com") || host == ".myshopify.com" {
        return Err(ApiError::Configuration(
            "store must be a myshopify.com domain or store handle".to_owned(),
        ));
    }
    Ok(host)
}

fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, parameters) = part.trim().split_once(';')?;
        parameters.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_owned()
        })
    })
}

fn same_origin_path(origin: &Url, next: &str) -> Result<String, ApiError> {
    let next = Url::parse(next)
        .map_err(|error| ApiError::Configuration(format!("invalid pagination URL: {error}")))?;
    if next.scheme() != origin.scheme()
        || next.host_str() != origin.host_str()
        || next.port_or_known_default() != origin.port_or_known_default()
    {
        return Err(ApiError::Configuration(
            "Shopify pagination attempted to change API origin".to_owned(),
        ));
    }
    Ok(match next.query() {
        Some(query) => format!("{}?{query}", next.path().trim_start_matches('/')),
        None => next.path().trim_start_matches('/').to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RetryPolicy;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn normalizes_handle_and_url() {
        assert_eq!(normalize_store("example").unwrap(), "example.myshopify.com");
        assert_eq!(
            normalize_store("https://EXAMPLE.myshopify.com/").unwrap(),
            "example.myshopify.com"
        );
        assert!(normalize_store("example.com").is_err());
    }

    #[test]
    fn parses_only_next_link_and_rejects_foreign_origin() {
        let header = "<https://shop.myshopify.com/a?page_info=old>; rel=\"previous\", <https://shop.myshopify.com/a?page_info=next>; rel=\"next\"";
        assert!(next_link(header).unwrap().contains("page_info=next"));
        let origin = Url::parse("https://shop.myshopify.com/").unwrap();
        assert!(same_origin_path(&origin, "https://evil.example/a").is_err());
    }

    #[test]
    fn include_and_exclude_patterns_select_expected_assets() {
        let include = vec!["assets/*".to_owned(), "sections/?.liquid".to_owned()];
        let exclude = vec!["*.map".to_owned()];
        assert!(selected("assets/theme.js", &include, &exclude));
        assert!(!selected("assets/theme.js.map", &include, &exclude));
        assert!(selected("sections/a.liquid", &include, &exclude));
        assert!(!selected("templates/index.json", &include, &exclude));
    }

    #[tokio::test]
    async fn pulls_filtered_text_and_binary_assets_from_rest_fixtures() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let bodies = [
                r#"{"assets":[{"key":"assets/theme.js"},{"key":"assets/logo.bin"},{"key":"config/settings.json"}]}"#,
                r#"{"asset":{"key":"assets/theme.js","value":"console.log('ok');","attachment":null}}"#,
                r#"{"asset":{"key":"assets/logo.bin","value":null,"attachment":"AJ//Cg=="}}"#,
            ];
            for (index, body) in bodies.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                let read = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.contains("GET /themes/42/assets.json"));
                if index == 1 {
                    assert!(request.contains("asset%5Bkey%5D=assets%2Ftheme.js"));
                }
                if index == 2 {
                    assert!(request.contains("asset%5Bkey%5D=assets%2Flogo.bin"));
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let base = format!("http://{address}/");
        let http = HttpClient::new(&base)
            .unwrap()
            .with_retry_policy(RetryPolicy {
                max_retries: 0,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            });
        let client = ThemeClient::with_http(http, Url::parse(&base).unwrap(), "themes");
        let assets = client
            .pull(
                42,
                &["assets/*".to_owned()],
                &["*.map".to_owned()],
                &Cancellation::default(),
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(assets[0].contents, b"console.log('ok');");
        assert_eq!(assets[1].contents, vec![0, 159, 255, 10]);
    }

    #[tokio::test]
    async fn pull_failure_returns_no_partial_asset_set() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for (status, body) in [
                ("200 OK", r#"{"assets":[{"key":"a"},{"key":"b"}]}"#),
                (
                    "200 OK",
                    r#"{"asset":{"key":"a","value":"downloaded","attachment":null}}"#,
                ),
                ("500 Internal Server Error", r#"{"errors":"failed"}"#),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                let _ = socket.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let base = format!("http://{address}/");
        let http = HttpClient::new(&base)
            .unwrap()
            .with_retry_policy(RetryPolicy {
                max_retries: 0,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            });
        let client = ThemeClient::with_http(http, Url::parse(&base).unwrap(), "themes");
        assert!(
            client
                .pull(42, &[], &[], &Cancellation::default())
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn lists_all_paginated_theme_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for page in 1..=2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 4096];
                let read = socket.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.contains("GET /themes"));
                let link = if page == 1 {
                    format!("Link: <http://{address}/themes?page_info=two>; rel=\"next\"\r\n")
                } else {
                    String::new()
                };
                let body = format!(
                    r#"{{"themes":[{{"id":{page},"name":"Theme {page}","role":"{}"}}]}}"#,
                    if page == 1 { "main" } else { "unpublished" }
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{link}\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let base = format!("http://{address}/");
        let http = HttpClient::new(&base)
            .unwrap()
            .with_retry_policy(RetryPolicy {
                max_retries: 0,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            });
        let client = ThemeClient::with_http(http, Url::parse(&base).unwrap(), "themes");
        let themes = client.list().await.unwrap();
        server.await.unwrap();
        assert_eq!(themes.len(), 2);
        assert_eq!(themes[0].role, "main");
        assert_eq!(themes[1].id, 2);
    }

    #[tokio::test]
    async fn permission_errors_include_remediation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let body = r#"{"errors":"forbidden"}"#;
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let base = format!("http://{address}/");
        let http = HttpClient::new(&base)
            .unwrap()
            .with_retry_policy(RetryPolicy {
                max_retries: 0,
                base_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            });
        let client = ThemeClient::with_http(http, Url::parse(&base).unwrap(), "themes");

        let error = client.list().await.unwrap_err();
        server.await.unwrap();

        assert!(
            error
                .to_string()
                .contains("refresh SHOPIFY_CLI_THEME_TOKEN")
        );
        assert!(error.to_string().contains("theme read permissions"));
    }
}
