use std::{net::SocketAddr, path::PathBuf};

use cfy_dev::{TlsProxy, TlsProxyError};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

async fn backend() -> (SocketAddr, oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (request_tx, request_rx) = oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let count = stream.read(&mut request).await.unwrap();
        request.truncate(count);
        let _ = request_tx.send(String::from_utf8(request).unwrap());
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\nconnection: close\r\n\r\nproxied")
            .await
            .unwrap();
    });
    (address, request_rx)
}

#[tokio::test]
async fn forwards_an_actual_https_request_and_stops_cleanly() {
    let (backend, request) = backend().await;
    let mut proxy = TlsProxy::start(
        "127.0.0.1:0".parse().unwrap(),
        backend,
        fixture("localhost-cert.pem"),
        fixture("localhost-key.pem"),
    )
    .await
    .unwrap();
    let address = proxy.local_addr();

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .build()
        .unwrap();
    let response = client
        .get(format!(
            "https://localhost:{}/hello?via=tls",
            address.port()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "proxied");
    assert!(
        request
            .await
            .unwrap()
            .starts_with("GET /hello?via=tls HTTP/1.1\r\n")
    );

    proxy.stop().await.unwrap();
    proxy.stop().await.unwrap();
    assert!(TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn serves_multiple_connections() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_address = backend.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = backend.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = [0; 1024];
                let count = stream.read(&mut request).await.unwrap();
                assert!(count > 0);
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                    .await
                    .unwrap();
            });
        }
    });
    let mut proxy = TlsProxy::start(
        "127.0.0.1:0".parse().unwrap(),
        backend_address,
        fixture("localhost-cert.pem"),
        fixture("localhost-key.pem"),
    )
    .await
    .unwrap();
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .no_proxy()
        .build()
        .unwrap();
    let url = format!("https://localhost:{}/", proxy.local_addr().port());
    let (first, second) = tokio::join!(client.get(&url).send(), client.get(&url).send());
    assert_eq!(first.unwrap().status(), 204);
    assert_eq!(second.unwrap().status(), 204);
    proxy.stop().await.unwrap();
}

#[tokio::test]
async fn rejects_non_loopback_addresses_and_reports_pem_errors() {
    let cert = fixture("localhost-cert.pem");
    let key = fixture("localhost-key.pem");
    let error = TlsProxy::start(
        "0.0.0.0:0".parse().unwrap(),
        "127.0.0.1:80".parse().unwrap(),
        &cert,
        &key,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, TlsProxyError::NonLoopbackListen(_)));

    let error = TlsProxy::start(
        "127.0.0.1:0".parse().unwrap(),
        "192.0.2.1:80".parse().unwrap(),
        &cert,
        &key,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, TlsProxyError::NonLoopbackBackend(_)));

    let invalid = fixture("invalid.pem");
    std::fs::write(&invalid, "not a PEM file").unwrap();
    let error = TlsProxy::start(
        "127.0.0.1:0".parse().unwrap(),
        "127.0.0.1:80".parse().unwrap(),
        &invalid,
        &key,
    )
    .await
    .unwrap_err();
    std::fs::remove_file(invalid).unwrap();
    assert!(matches!(error, TlsProxyError::MissingCertificate));
}

#[tokio::test]
async fn stop_closes_listener_before_returning() {
    let (backend, _request) = backend().await;
    let mut proxy = TlsProxy::start(
        "127.0.0.1:0".parse().unwrap(),
        backend,
        fixture("localhost-cert.pem"),
        fixture("localhost-key.pem"),
    )
    .await
    .unwrap();
    let address = proxy.local_addr();
    proxy.stop().await.unwrap();
    assert!(TcpStream::connect(address).await.is_err());
}

#[test]
fn debug_does_not_contain_identity_paths() {
    let source = include_str!("../src/tls_proxy.rs");
    assert!(!source.contains("field(\"certificate_path\""));
    assert!(!source.contains("field(\"private_key_path\""));
}
