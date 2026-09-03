use cfy_api::{GraphQlClient, GraphQlRequest, HttpClient};
use cfy_auth::Secret;
use cfy_core::{Error, ErrorKind, Result};
use reqwest::{
    StatusCode, Url,
    header::{HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};

const SUBSCRIBE_MUTATION: &str = r#"mutation AppLogsSubscribe($shopIds: [Int!]!, $apiKey: String!) {
  appLogsSubscribe(shopIds: $shopIds, apiKey: $apiKey) {
    jwtToken
    success
    errors
  }
}"#;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppLog {
    pub shop_id: i64,
    pub api_client_id: i64,
    pub payload: String,
    pub log_type: String,
    pub source: String,
    pub source_namespace: String,
    pub cursor: String,
    pub status: String,
    pub log_timestamp: String,
}

impl AppLog {
    #[must_use]
    pub fn source_name(&self) -> String {
        format!("{}.{}", self.source_namespace, self.source)
    }

    #[must_use]
    pub fn parsed_payload(&self) -> serde_json::Value {
        serde_json::from_str(&self.payload)
            .unwrap_or_else(|_| serde_json::Value::String(self.payload.clone()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppLogPage {
    pub logs: Vec<AppLog>,
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppLogPollFailure {
    pub status: StatusCode,
    pub messages: Vec<String>,
}

impl AppLogPollFailure {
    #[must_use]
    pub fn is_unauthorized(&self) -> bool {
        self.status == StatusCode::UNAUTHORIZED
    }

    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        self.status == StatusCode::TOO_MANY_REQUESTS
    }

    #[must_use]
    pub fn is_server_error(&self) -> bool {
        self.status.is_server_error()
    }
}

#[derive(Clone)]
pub struct AppLogsClient {
    graphql: GraphQlClient,
    http: reqwest::Client,
    polling_root: Url,
}

impl AppLogsClient {
    pub fn new(graphql_endpoint: &str, polling_root: &str, token: &str) -> Result<Self> {
        let graphql_url = secure_url(graphql_endpoint, "app logs GraphQL endpoint")?;
        let polling_root = secure_url(polling_root, "app logs polling endpoint")?;
        let base = format!(
            "{}://{}{}",
            graphql_url.scheme(),
            graphql_url.host_str().unwrap_or_default(),
            graphql_url
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default()
        );
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|error| Error::config(format!("invalid app logs token: {error}")))?;
        authorization.set_sensitive(true);
        let http_client = HttpClient::new(&base)
            .map_err(|error| Error::api(error.to_string()))?
            .with_sensitive_header(HeaderName::from_static("authorization"), authorization);
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("catify/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| {
                Error::with_source(ErrorKind::Api, "could not create app logs client", error)
            })?;
        Ok(Self {
            graphql: GraphQlClient::new(http_client, graphql_url.path()),
            http,
            polling_root,
        })
    }

    pub fn for_organization(token: &Secret, organization_id: &str) -> Result<Self> {
        let graphql_endpoint = std::env::var("CFY_APP_MANAGEMENT_URL").unwrap_or_else(|_| {
            "https://app.shopify.com/app_management/unstable/graphql.json".into()
        });
        let polling_root = std::env::var("CFY_APP_LOGS_URL").unwrap_or_else(|_| {
            format!(
                "https://app.shopify.com/app_management/unstable/organizations/{organization_id}/app_logs/poll"
            )
        });
        Self::new(&graphql_endpoint, &polling_root, token.expose())
    }

    pub async fn subscribe(&self, shop_ids: &[i64], api_key: &str) -> Result<Secret> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "appLogsSubscribe")]
            result: Option<SubscribeResult>,
        }
        #[derive(Deserialize)]
        struct SubscribeResult {
            #[serde(rename = "jwtToken")]
            jwt_token: Option<String>,
            #[allow(dead_code)]
            success: Option<bool>,
            #[serde(default)]
            errors: Vec<String>,
        }
        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::mutation(
                SUBSCRIBE_MUTATION,
                serde_json::json!({"shopIds": shop_ids, "apiKey": api_key}),
            ))
            .await
            .map_err(|error| Error::api(format!("could not subscribe to app logs: {error}")))?;
        let result = response.data.result.ok_or_else(|| {
            Error::api("Shopify did not return an app logs subscription response")
        })?;
        if !result.errors.is_empty() {
            return Err(Error::api(format!(
                "could not subscribe to app logs: {}",
                result.errors.join(", ")
            )));
        }
        result
            .jwt_token
            .map(Secret::new)
            .ok_or_else(|| Error::api("Shopify did not return an app logs subscription token"))
    }

    pub async fn poll(
        &self,
        subscription: &Secret,
        cursor: Option<&str>,
    ) -> std::result::Result<AppLogPage, AppLogPollFailure> {
        #[derive(Deserialize)]
        struct PollResponse {
            #[serde(default, rename = "app_logs")]
            logs: Vec<AppLog>,
            cursor: Option<String>,
            #[serde(default)]
            errors: Vec<String>,
        }
        let mut url = self.polling_root.clone();
        if let Some(cursor) = cursor.filter(|cursor| !cursor.is_empty()) {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", subscription.expose()))
            .map_err(|error| AppLogPollFailure {
                status: StatusCode::BAD_REQUEST,
                messages: vec![format!("invalid app logs subscription token: {error}")],
            })?;
        authorization.set_sensitive(true);
        let response = self
            .http
            .get(url)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .send()
            .await
            .map_err(|error| AppLogPollFailure {
                status: StatusCode::SERVICE_UNAVAILABLE,
                messages: vec![format!("app logs polling failed: {error}")],
            })?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| AppLogPollFailure {
            status,
            messages: vec![format!("could not read app logs response: {error}")],
        })?;
        let body =
            serde_json::from_slice::<PollResponse>(&bytes).map_err(|error| AppLogPollFailure {
                status,
                messages: vec![format!("Shopify returned malformed app logs JSON: {error}")],
            })?;
        if !status.is_success() {
            return Err(AppLogPollFailure {
                status,
                messages: if body.errors.is_empty() {
                    vec![format!("request failed with status {status}")]
                } else {
                    body.errors
                },
            });
        }
        Ok(AppLogPage {
            logs: body.logs,
            cursor: body.cursor,
        })
    }
}

fn secure_url(value: &str, label: &str) -> Result<Url> {
    let url =
        Url::parse(value).map_err(|error| Error::config(format!("invalid {label}: {error}")))?;
    if url.scheme() != "https" && !matches!(url.host_str(), Some("localhost" | "127.0.0.1")) {
        return Err(Error::config(format!("{label} must use HTTPS")));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn server(
        responses: Vec<(&'static str, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut responses = VecDeque::from(responses);
            let mut requests = Vec::new();
            while let Some((status, body)) = responses.pop_front() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    bytes.extend_from_slice(&buffer[..read]);
                    let Some(header_end) =
                        bytes.windows(4).position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&bytes[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")?
                                .trim()
                                .parse::<usize>()
                                .ok()
                        })
                        .unwrap_or_default();
                    if bytes.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&bytes).into_owned());
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn subscribes_and_polls_without_exposing_tokens() {
        let (base, task) = server(vec![
            ("200 OK", r#"{"data":{"appLogsSubscribe":{"jwtToken":"jwt-secret","success":true,"errors":[]}}}"#),
            ("200 OK", r#"{"app_logs":[{"shop_id":7,"api_client_id":9,"payload":"{\"input\":1}","log_type":"function_run","source":"discount","source_namespace":"extensions","cursor":"next","status":"success","log_timestamp":"2026-09-03T00:00:00Z"}],"cursor":"next"}"#),
        ]).await;
        let client = AppLogsClient::new(
            &format!("{base}/graphql"),
            &format!("{base}/poll"),
            "app-token",
        )
        .unwrap();
        let subscription = client.subscribe(&[7], "client-id").await.unwrap();
        assert!(!format!("{subscription:?}").contains("jwt-secret"));
        let page = client.poll(&subscription, None).await.unwrap();
        assert_eq!(page.logs[0].source_name(), "extensions.discount");
        assert_eq!(page.logs[0].parsed_payload()["input"], 1);
        let requests = task.await.unwrap();
        assert!(
            requests[0].contains("authorization: Bearer app-token")
                || requests[0].contains("authorization: bearer app-token")
        );
        assert!(requests[1].contains("Bearer jwt-secret"));
    }
}
