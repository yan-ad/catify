use cfy_app::AppDevClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn deletes_numeric_app_session_without_exposing_token() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0_u8; 8192];
        let read = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.contains("POST /app_dev/unstable/graphql.json"));
        assert!(request.contains("authorization: Bearer dev-token"));
        assert!(request.contains("mutation DevSessionDelete"));
        assert!(request.contains("\"appId\":\"42\""));
        let body = r#"{"data":{"devSessionDelete":{"userErrors":[]}}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let client = AppDevClient::new(&format!("http://{address}"), "dev-token").unwrap();
    client.delete_session("gid://shopify/App/42").await.unwrap();
    server.await.unwrap();
}
