//! Shopify theme metadata listing over the Admin REST API.

use crate::{ApiError, HttpClient, HttpRequest};
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

#[derive(Debug, Deserialize)]
struct ThemesEnvelope {
    themes: Vec<Theme>,
}

#[derive(Debug, Clone)]
pub struct ThemeClient {
    http: HttpClient,
    origin: Url,
    first_path: String,
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
        })
    }

    #[cfg(test)]
    fn with_http(http: HttpClient, origin: Url, first_path: impl Into<String>) -> Self {
        Self {
            http,
            origin,
            first_path: first_path.into(),
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
