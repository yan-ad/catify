//! Native Shopify app-management workflows.

use cfy_api::{GraphQlClient, GraphQlRequest, HttpClient};
use cfy_auth::{Secret, Session};
use cfy_core::{Error, ErrorKind, Result};
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, sync::OnceLock};
use url::Url;

const APP_MANAGEMENT_AUDIENCE: &str =
    "7ee65a63608843c577db8b23c4d7316ea0a01bd2f7594f8a9c06ea668c1b775c";
const APP_MANAGEMENT_SCOPE: &str = "https://api.shopify.com/auth/organization.apps.manage";
const TOKEN_EXCHANGE_GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const DEFAULT_IDENTITY_CLIENT_ID: &str = "fbdb2649-e327-4907-8f67-908d24cfd7e3";
const APPS_QUERY: &str = r#"query listApps($query: String) {
  appsConnection(query: $query, first: 50) {
    edges { node { id key activeRelease { id version { name } } } }
    pageInfo { hasNextPage }
  }
}"#;
const APP_QUERY: &str = r#"query ActiveAppReleaseFromApiKey($apiKey: String!) {
  app: appByKey(key: $apiKey) {
    id key organizationId
    activeRoot { grantedShopifyApprovalScopes }
    activeRelease { version { name appModules { config specification { externalIdentifier } } } }
  }
}"#;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteAppSummary {
    pub id: String,
    pub client_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteApp {
    pub id: String,
    pub client_id: String,
    pub name: String,
    pub organization_id: String,
    pub application_url: Option<String>,
    pub embedded: Option<bool>,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkOptions {
    pub directory: PathBuf,
    pub client_id: Option<String>,
    pub file_name: Option<String>,
    pub force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LinkReport {
    pub path: PathBuf,
    pub app_name: String,
    pub client_id: String,
}

pub struct AppManagementClient {
    graphql: GraphQlClient,
}

impl AppManagementClient {
    pub async fn from_session(session: &Session) -> Result<Self> {
        let token = exchange_app_management_token(session).await?;
        Self::new(
            &std::env::var("CFY_APP_MANAGEMENT_URL").unwrap_or_else(|_| {
                "https://app.shopify.com/app_management/unstable/graphql.json".into()
            }),
            token.expose(),
        )
    }

    pub fn new(endpoint: &str, token: &str) -> Result<Self> {
        let url = Url::parse(endpoint).map_err(|error| {
            Error::new(
                ErrorKind::Config,
                format!("invalid app management endpoint: {error}"),
            )
        })?;
        if url.scheme() != "https"
            && url.host_str() != Some("127.0.0.1")
            && url.host_str() != Some("localhost")
        {
            return Err(Error::new(
                ErrorKind::Config,
                "app management endpoint must use HTTPS",
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
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|error| {
            Error::new(
                ErrorKind::Config,
                format!("invalid app management token: {error}"),
            )
        })?;
        auth.set_sensitive(true);
        let http = HttpClient::new(&base)
            .map_err(|error| Error::new(ErrorKind::Api, error.to_string()))?
            .with_sensitive_header(HeaderName::from_static("authorization"), auth);
        Ok(Self {
            graphql: GraphQlClient::new(http, url.path()),
        })
    }

    pub async fn list_apps(&self) -> Result<Vec<RemoteAppSummary>> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "appsConnection")]
            apps: Connection,
        }
        #[derive(Deserialize)]
        struct Connection {
            edges: Vec<Edge>,
        }
        #[derive(Deserialize)]
        struct Edge {
            node: Node,
        }
        #[derive(Deserialize)]
        struct Node {
            id: String,
            key: String,
            #[serde(rename = "activeRelease")]
            release: Option<Release>,
        }
        #[derive(Deserialize)]
        struct Release {
            version: Version,
        }
        #[derive(Deserialize)]
        struct Version {
            name: String,
        }
        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::query(
                APPS_QUERY,
                serde_json::json!({"query": null}),
            ))
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::Api,
                    format!("could not list Shopify apps: {error}"),
                )
            })?;
        let mut apps: Vec<_> = response
            .data
            .apps
            .edges
            .into_iter()
            .map(|edge| RemoteAppSummary {
                id: edge.node.id,
                client_id: edge.node.key,
                name: edge
                    .node
                    .release
                    .map(|release| release.version.name)
                    .unwrap_or_else(|| "Untitled app".into()),
            })
            .collect();
        apps.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
        Ok(apps)
    }

    pub async fn app_by_client_id(&self, client_id: &str) -> Result<RemoteApp> {
        #[derive(Deserialize)]
        struct Data {
            app: Option<App>,
        }
        #[derive(Deserialize)]
        struct App {
            id: String,
            key: String,
            #[serde(rename = "organizationId")]
            organization_id: String,
            #[serde(rename = "activeRoot")]
            root: Root,
            #[serde(rename = "activeRelease")]
            release: Release,
        }
        #[derive(Deserialize)]
        struct Root {
            #[serde(rename = "grantedShopifyApprovalScopes", default)]
            scopes: Vec<String>,
        }
        #[derive(Deserialize)]
        struct Release {
            version: Version,
        }
        #[derive(Deserialize)]
        struct Version {
            name: String,
            #[serde(rename = "appModules", default)]
            modules: Vec<Module>,
        }
        #[derive(Deserialize)]
        struct Module {
            config: Option<serde_json::Value>,
            specification: Specification,
        }
        #[derive(Deserialize)]
        struct Specification {
            #[serde(rename = "externalIdentifier")]
            external_identifier: String,
        }
        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::query(
                APP_QUERY,
                serde_json::json!({"apiKey": client_id}),
            ))
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::Api,
                    format!("could not fetch Shopify app `{client_id}`: {error}"),
                )
            })?;
        let app = response.data.app.ok_or_else(|| {
            Error::new(
                ErrorKind::Api,
                format!("no Shopify app found for client ID `{client_id}`"),
            )
        })?;
        let home = app
            .release
            .version
            .modules
            .iter()
            .find(|module| module.specification.external_identifier == "app_home")
            .and_then(|module| module.config.as_ref());
        Ok(RemoteApp {
            id: app.id,
            client_id: app.key,
            name: app.release.version.name,
            organization_id: app.organization_id,
            application_url: home
                .and_then(|value| value.get("app_url"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            embedded: home
                .and_then(|value| value.get("embedded"))
                .and_then(serde_json::Value::as_bool),
            scopes: app.root.scopes,
        })
    }
}

pub async fn exchange_app_management_token(session: &Session) -> Result<Secret> {
    static TLS: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    TLS.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(())
    })
    .clone()
    .map_err(|error| Error::new(ErrorKind::Config, error))?;
    let base = std::env::var("CFY_IDENTITY_BASE_URL")
        .unwrap_or_else(|_| "https://accounts.shopify.com".into());
    let endpoint = Url::parse(&base)
        .and_then(|url| url.join("oauth/token"))
        .map_err(|error| {
            Error::new(
                ErrorKind::Config,
                format!("invalid identity endpoint: {error}"),
            )
        })?;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("grant_type", TOKEN_EXCHANGE_GRANT),
            ("requested_token_type", ACCESS_TOKEN_TYPE),
            ("subject_token_type", ACCESS_TOKEN_TYPE),
            ("client_id", DEFAULT_IDENTITY_CLIENT_ID),
            ("audience", APP_MANAGEMENT_AUDIENCE),
            ("scope", APP_MANAGEMENT_SCOPE),
            ("subject_token", session.access_token.expose()),
        ])
        .finish();
    #[derive(Deserialize)]
    struct Response {
        access_token: String,
    }
    let response = reqwest::Client::new()
        .post(endpoint)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|error| {
            Error::with_source(
                ErrorKind::Api,
                "app-management token exchange failed",
                error,
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::new(
            ErrorKind::Api,
            format!(
                "app-management token exchange returned HTTP {status}; run `cfy auth login` again"
            ),
        ));
    }
    let token = response.json::<Response>().await.map_err(|error| {
        Error::with_source(
            ErrorKind::Api,
            "invalid app-management token response",
            error,
        )
    })?;
    Ok(Secret::new(token.access_token))
}

pub fn write_linked_config(options: &LinkOptions, app: &RemoteApp) -> Result<LinkReport> {
    let name = options
        .file_name
        .clone()
        .unwrap_or_else(|| "shopify.app.toml".into());
    if !name.starts_with("shopify.app")
        || !name.ends_with(".toml")
        || name.contains('/')
        || name.contains('\\')
    {
        return Err(Error::invalid_input(
            "--file-name must be a shopify.app*.toml file name",
        ));
    }
    let path = options.directory.join(name);
    if path.exists() && !options.force {
        return Err(Error::invalid_input(format!(
            "{} already exists; pass --force to replace it",
            path.display()
        )));
    }
    let mut document: toml::Table = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|source| toml::from_str(&source).ok())
            .unwrap_or_default()
    } else {
        toml::Table::new()
    };
    document.insert(
        "client_id".into(),
        toml::Value::String(app.client_id.clone()),
    );
    document.insert("name".into(), toml::Value::String(app.name.clone()));
    if let Some(url) = &app.application_url {
        document.insert("application_url".into(), toml::Value::String(url.clone()));
    }
    if let Some(embedded) = app.embedded {
        document.insert("embedded".into(), toml::Value::Boolean(embedded));
    }
    if !app.scopes.is_empty() {
        let mut scopes = BTreeMap::new();
        scopes.insert(
            "scopes".to_owned(),
            toml::Value::String(app.scopes.join(",")),
        );
        document.insert(
            "access_scopes".into(),
            toml::Value::Table(scopes.into_iter().collect()),
        );
    }
    let content = toml::to_string_pretty(&document).map_err(|error| {
        Error::new(
            ErrorKind::Config,
            format!("could not encode linked configuration: {error}"),
        )
    })?;
    cfy_config::write_atomic(&path, content.as_bytes()).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("could not write {}", path.display()),
            error,
        )
    })?;
    Ok(LinkReport {
        path,
        app_name: app.name.clone(),
        client_id: app.client_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    fn temp() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cfy-app-link-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn writes_native_linked_configuration_atomically() {
        let directory = temp();
        let report = write_linked_config(
            &LinkOptions {
                directory: directory.clone(),
                client_id: None,
                file_name: None,
                force: false,
            },
            &RemoteApp {
                id: "gid://shopify/App/1".into(),
                client_id: "client-key".into(),
                name: "Native app".into(),
                organization_id: "1".into(),
                application_url: Some("https://example.test".into()),
                embedded: Some(true),
                scopes: vec!["read_products".into(), "write_products".into()],
            },
        )
        .unwrap();
        let source = std::fs::read_to_string(report.path).unwrap();
        assert!(source.contains("client_id = \"client-key\""));
        assert!(source.contains("application_url = \"https://example.test\""));
        assert!(source.contains("scopes = \"read_products,write_products\""));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refuses_to_replace_without_force() {
        let directory = temp();
        std::fs::write(directory.join("shopify.app.toml"), "client_id='old'").unwrap();
        let error = write_linked_config(
            &LinkOptions {
                directory: directory.clone(),
                client_id: None,
                file_name: None,
                force: false,
            },
            &RemoteApp {
                id: "1".into(),
                client_id: "new".into(),
                name: "App".into(),
                organization_id: "1".into(),
                application_url: None,
                embedded: None,
                scopes: vec![],
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("--force"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn app_management_backend_lists_and_fetches_remote_apps() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for body in [
                r#"{"data":{"appsConnection":{"edges":[{"node":{"id":"app-1","key":"client-1","activeRelease":{"version":{"name":"Example"}}}}],"pageInfo":{"hasNextPage":false}}}}"#,
                r#"{"data":{"app":{"id":"app-1","key":"client-1","organizationId":"gid://shopify/Organization/7","activeRoot":{"grantedShopifyApprovalScopes":["read_products"]},"activeRelease":{"version":{"name":"Example","appModules":[{"config":{"app_url":"https://example.test","embedded":true},"specification":{"externalIdentifier":"app_home"}}]}}}}}"#,
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0_u8; 8192];
                let read = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.contains("authorization: Bearer token"));
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let client = AppManagementClient::new(
            &format!("http://{address}/app_management/unstable/graphql.json"),
            "token",
        )
        .unwrap();
        let apps = client.list_apps().await.unwrap();
        assert_eq!(apps[0].name, "Example");
        let app = client.app_by_client_id("client-1").await.unwrap();
        assert_eq!(app.application_url.as_deref(), Some("https://example.test"));
        assert_eq!(app.scopes, ["read_products"]);
        server.await.unwrap();
    }
}
