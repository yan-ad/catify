use cfy_api::{GraphQlClient, GraphQlRequest, HttpClient};
use cfy_auth::Secret;
use cfy_core::{Error, ErrorKind, Result};
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, str::FromStr};
use url::Url;

const API_VERSIONS_QUERY: &str = r#"query publicApiVersions {
  publicApiVersions { handle }
}"#;
const TOPICS_QUERY: &str = r#"query availableTopics($apiVersion: String!) {
  availableTopics(apiVersion: $apiVersion)
}"#;
const TRIGGER_MUTATION: &str = r#"mutation CliTesting($address: String!, $apiKey: String, $apiVersion: String!, $deliveryMethod: String!, $sharedSecret: String!, $topic: String!) {
  cliTesting(address: $address, apiKey: $apiKey, apiVersion: $apiVersion, deliveryMethod: $deliveryMethod, sharedSecret: $sharedSecret, topic: $topic) {
    headers
    samplePayload
    success
    errors
  }
}"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WebhookDeliveryMethod {
    Http,
    GooglePubSub,
    EventBridge,
    #[serde(skip)]
    Localhost,
}

impl WebhookDeliveryMethod {
    #[must_use]
    pub const fn api_value(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::GooglePubSub => "google-pub-sub",
            Self::EventBridge => "event-bridge",
            Self::Localhost => "localhost",
        }
    }
}

impl FromStr for WebhookDeliveryMethod {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "http" => Ok(Self::Http),
            "google-pub-sub" => Ok(Self::GooglePubSub),
            "event-bridge" => Ok(Self::EventBridge),
            _ => Err(Error::invalid_input(format!(
                "unsupported webhook delivery method `{value}`"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebhookSample {
    pub payload: String,
    pub headers: BTreeMap<String, String>,
    pub success: bool,
    pub errors: Vec<String>,
}

#[derive(Clone)]
pub struct WebhookClient {
    graphql: GraphQlClient,
}

impl WebhookClient {
    pub fn new(endpoint: &str, token: &str) -> Result<Self> {
        let url = Url::parse(endpoint).map_err(|error| {
            Error::new(
                ErrorKind::Config,
                format!("invalid webhook API endpoint: {error}"),
            )
        })?;
        if url.scheme() != "https"
            && url.host_str() != Some("127.0.0.1")
            && url.host_str() != Some("localhost")
        {
            return Err(Error::new(
                ErrorKind::Config,
                "webhook API endpoint must use HTTPS",
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
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|error| Error::config(format!("invalid webhook API token: {error}")))?;
        authorization.set_sensitive(true);
        let http = HttpClient::new(&base)
            .map_err(|error| Error::api(error.to_string()))?
            .with_sensitive_header(HeaderName::from_static("authorization"), authorization);
        Ok(Self {
            graphql: GraphQlClient::new(http, url.path()),
        })
    }

    pub fn for_organization(token: &Secret, organization_id: &str) -> Result<Self> {
        let endpoint = std::env::var("CFY_WEBHOOK_API_URL").unwrap_or_else(|_| {
            format!(
                "https://app.shopify.com/webhooks/unstable/organizations/{organization_id}/graphql.json"
            )
        });
        Self::new(&endpoint, token.expose())
    }

    pub async fn api_versions(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "publicApiVersions")]
            versions: Vec<ApiVersion>,
        }
        #[derive(Deserialize)]
        struct ApiVersion {
            handle: String,
        }
        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::query(
                API_VERSIONS_QUERY,
                serde_json::json!({}),
            ))
            .await
            .map_err(|error| Error::api(format!("could not list webhook API versions: {error}")))?;
        let mut versions = response
            .data
            .versions
            .into_iter()
            .map(|version| version.handle)
            .collect::<Vec<_>>();
        versions.sort_by(|left, right| right.cmp(left));
        if let Some(index) = versions.iter().position(|version| version == "unstable") {
            let unstable = versions.remove(index);
            versions.push(unstable);
        }
        Ok(versions)
    }

    pub async fn topics(&self, api_version: &str) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "availableTopics")]
            topics: Vec<String>,
        }
        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::query(
                TOPICS_QUERY,
                serde_json::json!({"apiVersion": api_version}),
            ))
            .await
            .map_err(|error| Error::api(format!("could not list webhook topics: {error}")))?;
        let mut topics = response.data.topics;
        topics.sort();
        Ok(topics)
    }

    pub async fn trigger(
        &self,
        topic: &str,
        api_version: &str,
        address: &str,
        delivery_method: WebhookDeliveryMethod,
        client_secret: &Secret,
        client_id: Option<&str>,
    ) -> Result<WebhookSample> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "cliTesting")]
            result: TriggerResult,
        }
        #[derive(Deserialize)]
        struct TriggerResult {
            headers: String,
            #[serde(rename = "samplePayload")]
            sample_payload: String,
            success: bool,
            #[serde(default)]
            errors: Vec<serde_json::Value>,
        }
        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::mutation(
                TRIGGER_MUTATION,
                serde_json::json!({
                    "address": address,
                    "apiKey": client_id,
                    "apiVersion": api_version,
                    "deliveryMethod": delivery_method.api_value(),
                    "sharedSecret": client_secret.expose(),
                    "topic": topic,
                }),
            ))
            .await
            .map_err(|error| Error::api(format!("could not trigger sample webhook: {error}")))?;
        let headers =
            serde_json::from_str::<BTreeMap<String, String>>(&response.data.result.headers)
                .map_err(|error| {
                    Error::with_source(
                        ErrorKind::Api,
                        "Shopify returned invalid sample webhook headers",
                        error,
                    )
                })?;
        Ok(WebhookSample {
            payload: response.data.result.sample_payload,
            headers,
            success: response.data.result.success,
            errors: response
                .data
                .result
                .errors
                .into_iter()
                .map(|error| match error {
                    serde_json::Value::String(message) => message,
                    serde_json::Value::Object(object) => object
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| serde_json::Value::Object(object).to_string()),
                    value => value.to_string(),
                })
                .collect(),
        })
    }
}

pub fn resolve_delivery_method(
    address: &str,
    requested: Option<WebhookDeliveryMethod>,
) -> Result<WebhookDeliveryMethod> {
    let trimmed = address.trim();
    let inferred = if trimmed.starts_with("pubsub://") {
        WebhookDeliveryMethod::GooglePubSub
    } else if trimmed.starts_with("arn:aws:events:") {
        WebhookDeliveryMethod::EventBridge
    } else {
        let url = Url::parse(trimmed)
            .map_err(|error| Error::invalid_input(format!("invalid webhook address: {error}")))?;
        match (url.scheme(), url.host_str()) {
            ("http", Some("localhost" | "127.0.0.1")) => WebhookDeliveryMethod::Localhost,
            ("https", Some(_)) => WebhookDeliveryMethod::Http,
            _ => {
                return Err(Error::invalid_input(
                    "HTTP webhook addresses must use HTTPS, except localhost HTTP URLs",
                ));
            }
        }
    };
    if let Some(requested) = requested
        && requested != inferred
        && !(requested == WebhookDeliveryMethod::Http
            && inferred == WebhookDeliveryMethod::Localhost)
    {
        return Err(Error::invalid_input(format!(
            "webhook address is incompatible with delivery method `{}`",
            requested.api_value()
        )));
    }
    Ok(inferred)
}

pub async fn deliver_local_webhook(address: &str, sample: &WebhookSample) -> Result<bool> {
    let url = Url::parse(address)
        .map_err(|error| Error::invalid_input(format!("invalid localhost address: {error}")))?;
    if url.scheme() != "http" || !matches!(url.host_str(), Some("localhost" | "127.0.0.1")) {
        return Err(Error::invalid_input(
            "local webhook delivery is restricted to http://localhost or http://127.0.0.1",
        ));
    }
    let client = reqwest::Client::new();
    let mut request = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(sample.payload.clone());
    for (name, value) in &sample.headers {
        let name = reqwest::header::HeaderName::from_str(name).map_err(|error| {
            Error::invalid_input(format!(
                "Shopify returned invalid webhook header name: {error}"
            ))
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| {
            Error::invalid_input(format!(
                "Shopify returned invalid webhook header value: {error}"
            ))
        })?;
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(|error| {
        Error::with_source(ErrorKind::Api, "local webhook delivery failed", error)
    })?;
    Ok(response.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn serve_graphql(
        responses: Vec<&'static str>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut responses = VecDeque::from(responses);
            let mut requests = Vec::new();
            while let Some(response) = responses.pop_front() {
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
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or_default();
                    if bytes.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&bytes).into_owned());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.len(),
                    response
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}/graphql"), task)
    }

    #[test]
    fn delivery_method_validation_is_strict() {
        assert_eq!(
            resolve_delivery_method("http://localhost:3000/webhook", None).unwrap(),
            WebhookDeliveryMethod::Localhost
        );
        assert_eq!(
            resolve_delivery_method("https://example.test/webhook", None).unwrap(),
            WebhookDeliveryMethod::Http
        );
        assert!(resolve_delivery_method("http://example.test/webhook", None).is_err());
        assert!(
            resolve_delivery_method("pubsub://project:topic", Some(WebhookDeliveryMethod::Http))
                .is_err()
        );
    }

    #[test]
    fn secret_is_not_exposed_by_debug_types() {
        let secret = Secret::new("webhook-secret");
        assert!(!format!("{secret:?}").contains("webhook-secret"));
    }

    #[tokio::test]
    async fn executes_version_topic_and_sample_operations() {
        let (endpoint, server) = serve_graphql(vec![
            r#"{"data":{"publicApiVersions":[{"handle":"2025-07"},{"handle":"unstable"}]}}"#,
            r#"{"data":{"availableTopics":["orders/updated","orders/create"]}}"#,
            r#"{"data":{"cliTesting":{"headers":"{\"X-Shopify-Test\":\"true\"}","samplePayload":"{\"id\":1}","success":true,"errors":[]}}}"#,
        ])
        .await;
        let client = WebhookClient::new(&endpoint, "webhook-token").unwrap();
        assert_eq!(
            client.api_versions().await.unwrap(),
            ["2025-07", "unstable"]
        );
        assert_eq!(
            client.topics("2025-07").await.unwrap(),
            ["orders/create", "orders/updated"]
        );
        let sample = client
            .trigger(
                "orders/create",
                "2025-07",
                "https://example.test/webhook",
                WebhookDeliveryMethod::Http,
                &Secret::new("shared-secret"),
                Some("client-id"),
            )
            .await
            .unwrap();
        assert!(sample.success);
        assert_eq!(sample.payload, r#"{"id":1}"#);
        assert_eq!(sample.headers["X-Shopify-Test"], "true");

        let requests = server.await.unwrap();
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer webhook-token")
        }));
        assert!(requests[0].contains("publicApiVersions"));
        assert!(requests[1].contains("availableTopics"));
        assert!(requests[2].contains("shared-secret"));
    }
}
