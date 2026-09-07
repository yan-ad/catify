use cfy_app::AppManagementClient;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn fetches_active_modules_for_deploy_reconciliation() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0_u8; 16 * 1024];
        let read = stream.read(&mut request).await.unwrap();
        let request = String::from_utf8_lossy(&request[..read]);
        assert!(request.contains("query ActiveAppModules"));
        assert!(request.contains("gid://shopify/App/1"));
        assert!(request.contains("authorization: Bearer token"));
        let body = r#"{"data":{"app":{"activeRelease":{"version":{"appModules":[{"uuid":"uuid-1","userIdentifier":"checkout-ui","handle":"checkout-ui","config":{"targeting":[{"target":"purchase.checkout.block.render"}]},"target":"purchase.checkout.block.render","specification":{"identifier":"checkout_ui_extension","externalIdentifier":"checkout_ui_extension","experience":"extension"}},{"uuid":null,"userIdentifier":"app_home","handle":"app_home","config":{"app_url":"https://example.test","embedded":true},"target":null,"specification":{"identifier":"app_home","externalIdentifier":"app_home","experience":"configuration"}}]}}}}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });

    let client = AppManagementClient::new(
        &format!("http://{address}/app_management/unstable/graphql.json"),
        "token",
    )
    .unwrap();
    let modules = client
        .active_app_modules("gid://shopify/App/1")
        .await
        .unwrap();
    assert_eq!(modules.len(), 2);
    assert_eq!(modules[0].external_identifier, "app_home");
    assert_eq!(modules[1].user_identifier.as_deref(), Some("checkout-ui"));
    assert!(!format!("{modules:?}").contains("token"));
    server.await.unwrap();
}
