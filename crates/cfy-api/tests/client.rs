use std::{collections::VecDeque, sync::Arc, time::Duration};

use cfy_api::{
    ApiError, GraphQlClient, GraphQlRequest, HttpClient, HttpRequest, RetryPolicy, RetrySafety,
};
use reqwest::{Method, header::HeaderValue};
use serde::Deserialize;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

#[derive(Clone)]
struct MockResponse {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
}

#[tokio::test]
async fn rejects_secrets_in_url_queries() {
    let server = MockServer::start(vec![]).await;
    let error = fast_client(&server.url)
        .execute(&HttpRequest::new(
            Method::GET,
            "/products?access_token=shpat_query_secret",
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, ApiError::Configuration(_)));
    assert_eq!(server.request_count().await, 0);
    assert!(!format!("{error:?} {error}").contains("shpat_query_secret"));
}

struct MockServer {
    url: String,
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
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let responses = Arc::clone(&responses);
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 16 * 1024];
                    let length = stream.read(&mut buffer).await.unwrap();
                    recorded
                        .lock()
                        .await
                        .push(String::from_utf8_lossy(&buffer[..length]).into_owned());
                    let response = responses.lock().await.pop_front().unwrap_or(MockResponse {
                        status: 500,
                        headers: vec![],
                        body: r#"{"error":"unexpected request"}"#,
                    });
                    let reason = match response.status {
                        200 => "OK",
                        400 => "Bad Request",
                        429 => "Too Many Requests",
                        500 => "Internal Server Error",
                        _ => "Test Response",
                    };
                    let headers = response
                        .headers
                        .iter()
                        .map(|(name, value)| format!("{name}: {value}\r\n"))
                        .collect::<String>();
                    let wire = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}",
                        response.status,
                        reason,
                        response.body.len(),
                        headers,
                        response.body
                    );
                    stream.write_all(wire.as_bytes()).await.unwrap();
                });
            }
        });

        Self {
            url: format!("http://{address}/"),
            requests,
        }
    }

    async fn request_count(&self) -> usize {
        self.requests.lock().await.len()
    }

    async fn request(&self, index: usize) -> String {
        self.requests.lock().await[index].clone()
    }
}

fn fast_client(url: &str) -> HttpClient {
    HttpClient::new(url)
        .unwrap()
        .with_retry_policy(RetryPolicy {
            max_retries: 3,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        })
}

#[tokio::test]
async fn retries_429_and_5xx_then_preserves_request_id() {
    let server = MockServer::start(vec![
        MockResponse {
            status: 429,
            headers: vec![],
            body: r#"{"errors":"throttled"}"#,
        },
        MockResponse {
            status: 500,
            headers: vec![],
            body: r#"{"errors":"temporary"}"#,
        },
        MockResponse {
            status: 200,
            headers: vec![("X-Shopify-Request-Id", "req-123")],
            body: r#"{"ok":true}"#,
        },
    ])
    .await;

    let response = fast_client(&server.url)
        .execute(&HttpRequest::new(Method::GET, "/products"))
        .await
        .unwrap();

    assert_eq!(response.request_id.as_deref(), Some("req-123"));
    assert_eq!(server.request_count().await, 3);
}

#[tokio::test]
async fn does_not_retry_unsafe_post() {
    let server = MockServer::start(vec![
        MockResponse {
            status: 500,
            headers: vec![],
            body: r#"{"errors":"failed"}"#,
        },
        MockResponse {
            status: 200,
            headers: vec![],
            body: r#"{"ok":true}"#,
        },
    ])
    .await;
    let mut request = HttpRequest::new(Method::POST, "/mutation");
    request.retry_safety = RetrySafety::Unsafe;

    let error = fast_client(&server.url)
        .execute(&request)
        .await
        .unwrap_err();

    assert!(matches!(error, ApiError::Http { .. }));
    assert_eq!(server.request_count().await, 1);
}

#[derive(Debug, Deserialize, PartialEq)]
struct ShopData {
    shop: Shop,
}

#[derive(Debug, Deserialize, PartialEq)]
struct Shop {
    name: String,
}

#[tokio::test]
async fn graphql_query_parses_data_and_retries_safely() {
    let server = MockServer::start(vec![
        MockResponse {
            status: 500,
            headers: vec![],
            body: r#"{"errors":"temporary"}"#,
        },
        MockResponse {
            status: 200,
            headers: vec![("X-Request-Id", "graphql-42")],
            body: r#"{"data":{"shop":{"name":"Crab Shop"}},"extensions":{"cost":1}}"#,
        },
    ])
    .await;
    let client = GraphQlClient::new(fast_client(&server.url), "/graphql.json");
    let request = GraphQlRequest::query("query Shop { shop { name } }", json!({}));

    let response = client.execute::<_, ShopData>(&request).await.unwrap();

    assert_eq!(response.data.shop.name, "Crab Shop");
    assert_eq!(response.request_id.as_deref(), Some("graphql-42"));
    assert_eq!(server.request_count().await, 2);
}

#[tokio::test]
async fn graphql_mutation_retries_only_with_idempotency_key() {
    let server = MockServer::start(vec![
        MockResponse {
            status: 500,
            headers: vec![],
            body: r#"{"errors":"temporary"}"#,
        },
        MockResponse {
            status: 200,
            headers: vec![],
            body: r#"{"data":{"shop":{"name":"Crab Shop"}}}"#,
        },
    ])
    .await;
    let client = GraphQlClient::new(fast_client(&server.url), "/graphql.json");
    let mut request = GraphQlRequest::mutation("mutation Update { shop { name } }", json!({}));
    request.idempotency_key = Some("idempotent-123".to_owned());

    client.execute::<_, ShopData>(&request).await.unwrap();

    assert_eq!(server.request_count().await, 2);
    assert!(
        server
            .request(0)
            .await
            .contains("idempotency-key: idempotent-123")
    );
}

#[tokio::test]
async fn returns_structured_redacted_graphql_errors() {
    let server = MockServer::start(vec![MockResponse {
        status: 200,
        headers: vec![("X-Request-Id", "graphql-error-7")],
        body: r#"{"data":null,"errors":[{"message":"token shpat_supersecret rejected","extensions":{"access_token":"shpat_supersecret","code":"DENIED"}}]}"#,
    }])
    .await;
    let client = GraphQlClient::new(fast_client(&server.url), "/graphql.json");

    let error = client
        .execute::<_, ShopData>(&GraphQlRequest::query(
            "query Shop { shop { name } }",
            json!({}),
        ))
        .await
        .unwrap_err();
    let rendered = format!("{error:?} {error}");

    assert_eq!(error.request_id(), Some("graphql-error-7"));
    assert!(!rendered.contains("shpat_supersecret"));
    assert!(rendered.contains("[REDACTED]"));
}

#[tokio::test]
async fn malformed_json_retains_request_id() {
    let server = MockServer::start(vec![MockResponse {
        status: 200,
        headers: vec![("X-Request-Id", "bad-json-9")],
        body: "not json",
    }])
    .await;
    let client = GraphQlClient::new(fast_client(&server.url), "/graphql.json");

    let error = client
        .execute::<_, ShopData>(&GraphQlRequest::query(
            "query Shop { shop { name } }",
            json!({}),
        ))
        .await
        .unwrap_err();

    assert!(matches!(error, ApiError::MalformedJson { .. }));
    assert_eq!(error.request_id(), Some("bad-json-9"));
}

#[tokio::test]
async fn debug_output_never_contains_headers_bodies_or_variables() {
    let token = "shpat_never_print_me";
    let client = HttpClient::new(&MockServer::start(vec![]).await.url)
        .unwrap()
        .with_sensitive_header(
            "x-shopify-access-token".parse().unwrap(),
            HeaderValue::from_str(token).unwrap(),
        );
    let mut http_request = HttpRequest::new(Method::POST, "/graphql.json");
    http_request.body = Some(json!({"access_token": token}));
    let graphql_request = GraphQlRequest::query(
        "query Test { shop { name } }",
        json!({
            "access_token": token
        }),
    );

    let rendered = format!("{client:?} {http_request:?} {graphql_request:?}");

    assert!(!rendered.contains(token));
    assert!(rendered.contains("[REDACTED]"));
}

#[tokio::test]
async fn redacts_sensitive_fields_in_http_errors() {
    let token = "shpat_http_secret";
    let server = MockServer::start(vec![MockResponse {
        status: 400,
        headers: vec![("X-Request-Id", "http-error-1")],
        body: r#"{"errors":{"message":"token shpat_http_secret rejected"},"access_token":"shpat_http_secret"}"#,
    }])
    .await;

    let error = fast_client(&server.url)
        .execute(&HttpRequest::new(Method::GET, "/products"))
        .await
        .unwrap_err();
    let rendered = format!("{error:?} {error}");

    assert_eq!(error.request_id(), Some("http-error-1"));
    assert!(!rendered.contains(token));
    assert!(rendered.contains("[REDACTED]"));
}
