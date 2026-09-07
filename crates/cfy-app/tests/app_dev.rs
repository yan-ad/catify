use cfy_app::{AppDevClient, AppDevCreateSessionRequest, AppDevUpdateSessionRequest};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn fixture(
    response_status: &str,
    response_body: Value,
) -> (String, tokio::task::JoinHandle<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let status = response_status.to_owned();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2048];
        let header_end = loop {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "connection closed before request headers");
            request.extend_from_slice(&buffer[..read]);
            if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        assert!(headers.starts_with("POST /app_dev/unstable/graphql.json HTTP/1.1"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer dev-token")
        );
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap();
        while request.len() - header_end < content_length {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "connection closed before request body");
            request.extend_from_slice(&buffer[..read]);
        }
        let request_body =
            serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap();
        let body = serde_json::to_string(&response_body).unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        request_body
    });
    (format!("http://{address}"), server)
}

#[tokio::test]
async fn creates_session_with_upstream_payload_and_typed_response() {
    let (store, server) = fixture(
        "200 OK",
        json!({"data":{"devSessionCreate":{
            "devSession":{
                "websocketUrl":"wss://example.test/dev",
                "updatedAt":"2025-01-02T03:04:05Z",
                "user":{"id":"gid://shopify/User/7","email":"dev@example.test"},
                "app":{"id":"gid://shopify/App/42","key":"client-key"}
            },
            "warnings":[{"message":"Another session was replaced","code":"SESSION_TAKEOVER"}],
            "userErrors":[]
        }}}),
    )
    .await;
    let client = AppDevClient::new(&store, "dev-token").unwrap();
    let response = client
        .create_session(&AppDevCreateSessionRequest {
            app_id: "gid://shopify/App/42".into(),
            assets_url: None,
            websocket_url: Some("wss://localhost:3456".into()),
        })
        .await
        .unwrap();

    assert_eq!(response.session.unwrap().app.key, "client-key");
    assert_eq!(response.warnings[0].code, "SESSION_TAKEOVER");
    assert!(response.user_errors.is_empty());
    let request = server.await.unwrap();
    assert!(
        request["query"]
            .as_str()
            .unwrap()
            .contains("mutation DevSessionCreate")
    );
    assert_eq!(
        request["variables"],
        json!({"appId":"42","assetsUrl":"","websocketUrl":"wss://localhost:3456"})
    );
}

#[tokio::test]
async fn updates_session_with_optional_assets_manifest_and_inherited_uids() {
    let (store, server) = fixture(
        "200 OK",
        json!({"data":{"devSessionUpdate":{
            "devSession":null,
            "userErrors":[]
        }}}),
    )
    .await;
    let client = AppDevClient::new(&store, "dev-token").unwrap();
    let response = client
        .update_session(&AppDevUpdateSessionRequest {
            app_id: "42".into(),
            assets_url: None,
            manifest: json!({"name":"example","modules":[{"uid":"changed"}]}),
            inherited_module_uids: vec!["unchanged-1".into(), "unchanged-2".into()],
        })
        .await
        .unwrap();

    assert!(response.session.is_none());
    assert!(response.user_errors.is_empty());
    let request = server.await.unwrap();
    assert!(
        request["query"]
            .as_str()
            .unwrap()
            .contains("mutation DevSessionUpdate")
    );
    assert_eq!(request["variables"]["appId"], "42");
    assert!(request["variables"]["assetsUrl"].is_null());
    assert_eq!(
        request["variables"]["manifest"],
        serde_json::to_string(&json!({"name":"example","modules":[{"uid":"changed"}]})).unwrap()
    );
    assert_eq!(
        request["variables"]["inheritedModuleUids"],
        json!(["unchanged-1", "unchanged-2"])
    );
}

#[tokio::test]
async fn returns_typed_user_errors_and_redacts_retry_diagnostics() {
    let assets_url = "https://storage.example.test/upload?signature=asset-secret";
    let (store, server) = fixture(
        "200 OK",
        json!({"data":{"devSessionUpdate":{
            "devSession":null,
            "userErrors":[{
                "message":format!("failed using dev-token at {assets_url}"),
                "on":{"authorization":"Bearer dev-token","url":assets_url},
                "field":["manifest"],
                "category":"remote"
            }]
        }}}),
    )
    .await;
    let client = AppDevClient::new(&store, "dev-token").unwrap();
    let response = client
        .update_session(&AppDevUpdateSessionRequest {
            app_id: "gid://shopify/App/42".into(),
            assets_url: Some(assets_url.into()),
            manifest: json!({}),
            inherited_module_uids: vec![],
        })
        .await
        .unwrap();

    let diagnostic = format!("{:?}", response.user_errors[0]);
    assert!(diagnostic.contains("[REDACTED]"));
    assert!(!diagnostic.contains("dev-token"));
    assert!(!diagnostic.contains("asset-secret"));
    server.await.unwrap();
}

#[tokio::test]
async fn redacts_transport_errors_and_rejects_non_numeric_ids_before_io() {
    let assets_url = "https://storage.example.test/upload?signature=asset-secret";
    let (store, server) = fixture(
        "500 Internal Server Error",
        json!({"errors":format!("Bearer dev-token failed for {assets_url}")}),
    )
    .await;
    let client = AppDevClient::new(&store, "dev-token").unwrap();
    let error = client
        .create_session(&AppDevCreateSessionRequest {
            app_id: "42".into(),
            assets_url: Some(assets_url.into()),
            websocket_url: None,
        })
        .await
        .unwrap_err();
    let diagnostic = error.to_string();
    assert!(!diagnostic.contains("dev-token"));
    assert!(!diagnostic.contains("asset-secret"));
    assert!(diagnostic.contains("[REDACTED"));
    server.await.unwrap();

    let client = AppDevClient::new("http://127.0.0.1:1", "dev-token").unwrap();
    let error = client
        .create_session(&AppDevCreateSessionRequest {
            app_id: "gid://shopify/App/not-a-number".into(),
            assets_url: None,
            websocket_url: None,
        })
        .await
        .unwrap_err();
    assert!(error.message().contains("numeric identifier"));
}

#[tokio::test]
async fn deletes_numeric_app_session_and_reports_user_errors() {
    let (store, server) = fixture(
        "200 OK",
        json!({"data":{"devSessionDelete":{"userErrors":[]}}}),
    )
    .await;
    let client = AppDevClient::new(&store, "dev-token").unwrap();
    client.delete_session("gid://shopify/App/42").await.unwrap();
    let request = server.await.unwrap();
    assert!(
        request["query"]
            .as_str()
            .unwrap()
            .contains("mutation DevSessionDelete")
    );
    assert_eq!(request["variables"], json!({"appId":"42"}));

    let (store, server) = fixture(
        "200 OK",
        json!({"data":{"devSessionDelete":{"userErrors":[{"message":"session is owned elsewhere"}]}}}),
    )
    .await;
    let client = AppDevClient::new(&store, "dev-token").unwrap();
    let error = client.delete_session("42").await.unwrap_err();
    assert!(error.message().contains("session is owned elsewhere"));
    server.await.unwrap();
}
