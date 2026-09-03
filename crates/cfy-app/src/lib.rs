//! Native Shopify app-management workflows.

pub mod extension_generate;
pub mod extension_import;

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD},
};
use cfy_api::{GraphQlClient, GraphQlRequest, HttpClient};
use cfy_auth::{Secret, Session};
use cfy_core::{Error, ErrorKind, Result};
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::OnceLock};
use url::Url;

const APP_MANAGEMENT_AUDIENCE: &str =
    "7ee65a63608843c577db8b23c4d7316ea0a01bd2f7594f8a9c06ea668c1b775c";
const APP_MANAGEMENT_SCOPE: &str = "https://api.shopify.com/auth/organization.apps.manage";
const BUSINESS_PLATFORM_AUDIENCE: &str = "32ff8ee5-82b8-4d93-9f8a-c6997cefb7dc";
const BUSINESS_PLATFORM_SCOPE: &str = "https://api.shopify.com/auth/destinations.readonly";
const TOKEN_EXCHANGE_GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const DEFAULT_IDENTITY_CLIENT_ID: &str = "fbdb2649-e327-4907-8f67-908d24cfd7e3";
const APPS_QUERY: &str = r#"query listApps($query: String) {
  appsConnection(query: $query, first: 50) {
    edges { node { id key activeRelease { id version { name } } } }
    pageInfo { hasNextPage }
  }
}"#;
const EXTENSION_REGISTRATIONS_QUERY: &str = r#"query ExtensionRegistrations($apiKey: String!) {
  app: appByKey(key: $apiKey) {
    activeRelease {
      version {
        appModules {
          uuid
          handle
          config
          target
          specification { externalIdentifier }
        }
      }
    }
  }
}"#;

/// App client credentials used only for short-lived store Admin token exchange.
#[derive(Clone)]
pub struct AppClientCredentials {
    pub client_id: String,
    pub client_secret: cfy_auth::Secret,
}

impl std::fmt::Debug for AppClientCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppClientCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .finish()
    }
}
const APP_CREDENTIALS_QUERY: &str = r#"query AppClientCredentials($apiKey: String!) {
  app: appByKey(key: $apiKey) {
    key
    activeRoot { clientCredentials { secrets { key } } }
  }
}"#;
const ORGANIZATIONS_QUERY: &str = r#"query ListOrganizations {
  currentUserAccount {
    organizationsWithAccessToDestination(destination: APPS_CLI) {
      nodes { id name }
    }
  }
}"#;
const APP_VERSION_BY_TAG_QUERY: &str = r#"query AppVersionByTag($versionTag: String!) {
  versionByTag(tag: $versionTag) { id metadata { message versionTag } }
}"#;
const RELEASE_VERSION_MUTATION: &str = r#"mutation ReleaseVersion($appId: ID!, $versionId: ID!) {
  appReleaseCreate(appId: $appId, versionId: $versionId) {
    release { version { id metadata { message versionTag } } }
    userErrors { message }
  }
}"#;
const APP_VERSIONS_QUERY: &str = r#"query AppVersions($appId: ID!) {
  app(id: $appId) {
    activeRelease { version { id } }
    versions(first: 20) {
      edges { node { id createdAt createdBy metadata { message versionTag } } }
    }
    versionsCount
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteOrganization {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemoteApp {
    pub id: String,
    pub client_id: String,
    pub name: String,
    pub handle: Option<String>,
    pub organization_id: String,
    pub application_url: Option<String>,
    pub embedded: Option<bool>,
    pub scopes: Vec<String>,
    /// Top-level Shopify app configuration reconstructed from remote app modules.
    pub remote_configuration: toml::Table,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemoteAppVersion {
    pub id: String,
    pub version: Option<String>,
    pub status: String,
    pub message: String,
    pub created_at: String,
    pub created_by: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppVersionsReport {
    pub versions: Vec<RemoteAppVersion>,
    pub total: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReleaseReport {
    pub app_id: String,
    pub version_id: String,
    pub version: String,
    pub message: String,
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

pub struct BusinessPlatformClient {
    graphql: GraphQlClient,
}

impl BusinessPlatformClient {
    pub async fn from_session(session: &Session) -> Result<Self> {
        let token = exchange_business_platform_token(session).await?;
        Self::new(
            &std::env::var("CFY_BUSINESS_PLATFORM_URL").unwrap_or_else(|_| {
                "https://destinations.shopifysvc.com/destinations/api/2020-07/graphql".into()
            }),
            token.expose(),
        )
    }

    pub fn new(endpoint: &str, token: &str) -> Result<Self> {
        let url = Url::parse(endpoint).map_err(|error| {
            Error::new(
                ErrorKind::Config,
                format!("invalid business platform endpoint: {error}"),
            )
        })?;
        if url.scheme() != "https"
            && url.host_str() != Some("127.0.0.1")
            && url.host_str() != Some("localhost")
        {
            return Err(Error::new(
                ErrorKind::Config,
                "business platform endpoint must use HTTPS",
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
                format!("invalid business platform token: {error}"),
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

    pub async fn list_organizations(&self) -> Result<Vec<RemoteOrganization>> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "currentUserAccount")]
            account: Option<Account>,
        }
        #[derive(Deserialize)]
        struct Account {
            #[serde(rename = "organizationsWithAccessToDestination")]
            organizations: Nodes,
        }
        #[derive(Deserialize)]
        struct Nodes {
            nodes: Vec<Node>,
        }
        #[derive(Deserialize)]
        struct Node {
            id: String,
            name: String,
        }
        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::query(
                ORGANIZATIONS_QUERY,
                serde_json::json!({}),
            ))
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::Api,
                    format!("could not list Shopify organizations: {error}"),
                )
            })?;
        let account = response.data.account.ok_or_else(|| {
            Error::new(
                ErrorKind::Api,
                "Shopify account could not be resolved; run `cfy auth login` again",
            )
        })?;
        let mut organizations = account
            .organizations
            .nodes
            .into_iter()
            .map(|organization| {
                Ok(RemoteOrganization {
                    id: decode_organization_id(&organization.id)?,
                    name: organization.name,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        organizations
            .sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
        Ok(organizations)
    }
}

pub struct AppManagementClient {
    graphql: GraphQlClient,
}

#[async_trait::async_trait]
impl extension_import::ExtensionRegistrationProvider for AppManagementClient {
    async fn fetch_extension_registrations(
        &self,
        client_id: &str,
        organization_id: &str,
    ) -> Result<Vec<extension_import::RemoteExtensionRegistration>> {
        self.extension_registrations(client_id, organization_id)
            .await
    }
}

impl AppManagementClient {
    /// Fetch extension registrations from the dashboard without invoking Shopify CLI.
    pub async fn extension_registrations(
        &self,
        client_id: &str,
        organization_id: &str,
    ) -> Result<Vec<extension_import::RemoteExtensionRegistration>> {
        #[derive(Deserialize)]
        struct Data {
            app: Option<App>,
        }
        #[derive(Deserialize)]
        struct App {
            #[serde(rename = "activeRelease")]
            release: Option<Release>,
        }
        #[derive(Deserialize)]
        struct Release {
            version: Version,
        }
        #[derive(Deserialize)]
        struct Version {
            #[serde(rename = "appModules", default)]
            modules: Vec<Module>,
        }
        #[derive(Deserialize)]
        struct Module {
            uuid: Option<String>,
            handle: Option<String>,
            config: Option<serde_json::Value>,
            target: Option<String>,
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
                EXTENSION_REGISTRATIONS_QUERY,
                serde_json::json!({"apiKey": client_id, "organizationId": organization_id}),
            ))
            .await
            .map_err(|error| {
                Error::api(format!(
                    "could not fetch extension registrations for `{client_id}`: {error}"
                ))
            })?;
        let app = response.data.app.ok_or_else(|| {
            Error::api(format!("no Shopify app found for client ID `{client_id}`"))
        })?;
        let mut registrations = app
            .release
            .map(|release| release.version.modules)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|module| {
                let uuid = module.uuid?;
                Some(extension_import::RemoteExtensionRegistration {
                    title: module.handle.clone().unwrap_or_else(|| uuid.clone()),
                    uuid,
                    extension_type: module.specification.external_identifier,
                    configuration: module.config.unwrap_or(serde_json::Value::Null),
                    context: module.target,
                })
            })
            .filter(|registration| {
                extension_import::is_migratable_type(&registration.extension_type)
            })
            .collect::<Vec<_>>();
        registrations.sort_by(|left, right| left.uuid.cmp(&right.uuid));
        Ok(registrations)
    }

    pub async fn app_client_credentials(&self, client_id: &str) -> Result<AppClientCredentials> {
        #[derive(Deserialize)]
        struct Data {
            app: Option<App>,
        }
        #[derive(Deserialize)]
        struct App {
            key: String,
            #[serde(rename = "activeRoot")]
            root: Root,
        }
        #[derive(Deserialize)]
        struct Root {
            #[serde(rename = "clientCredentials")]
            credentials: Credentials,
        }
        #[derive(Deserialize)]
        struct Credentials {
            #[serde(default)]
            secrets: Vec<ClientSecret>,
        }
        #[derive(Deserialize)]
        struct ClientSecret {
            key: String,
        }
        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::query(
                APP_CREDENTIALS_QUERY,
                serde_json::json!({"apiKey": client_id}),
            ))
            .await
            .map_err(|error| Error::api(format!("could not fetch app credentials: {error}")))?;
        let app = response.data.app.ok_or_else(|| {
            Error::api(format!("no Shopify app found for client ID `{client_id}`"))
        })?;
        let secret = app
            .root
            .credentials
            .secrets
            .into_iter()
            .next()
            .ok_or_else(|| {
                Error::api("Shopify app has no client secret available for Admin authentication")
            })?;
        Ok(AppClientCredentials {
            client_id: app.key,
            client_secret: cfy_auth::Secret::new(secret.key),
        })
    }

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

    pub async fn list_apps(&self, organization_id: &str) -> Result<Vec<RemoteAppSummary>> {
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
                serde_json::json!({"query": null, "organizationId": organization_id}),
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
        self.app_by_client_id_with_variables(client_id, serde_json::json!({"apiKey": client_id}))
            .await
    }

    pub async fn app_by_client_id_in_organization(
        &self,
        organization_id: &str,
        client_id: &str,
    ) -> Result<RemoteApp> {
        self.app_by_client_id_with_variables(
            client_id,
            serde_json::json!({"apiKey": client_id, "organizationId": organization_id}),
        )
        .await
    }

    async fn app_by_client_id_with_variables(
        &self,
        client_id: &str,
        variables: serde_json::Value,
    ) -> Result<RemoteApp> {
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
            .execute::<_, Data>(&GraphQlRequest::query(APP_QUERY, variables))
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
        let modules = app.release.version.modules;
        let home = modules
            .iter()
            .find(|module| module.specification.external_identifier == "app_home")
            .and_then(|module| module.config.as_ref());
        let mut remote_configuration = toml::Table::new();
        for module in &modules {
            let Some(config) = module.config.as_ref() else {
                continue;
            };
            let transformed =
                transform_remote_module(&module.specification.external_identifier, config)?;
            merge_remote_configuration(&mut remote_configuration, transformed)?;
        }
        Ok(RemoteApp {
            id: app.id,
            client_id: app.key,
            name: app.release.version.name,
            // The App Management `App` type queried by API key does not expose
            // a top-level handle. Preserve an existing local handle during the
            // TOML merge instead of making the whole link operation fail by
            // requesting an unsupported GraphQL field.
            handle: None,
            organization_id: app.organization_id,
            application_url: home
                .and_then(|value| value.get("app_url"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            embedded: home
                .and_then(|value| value.get("embedded"))
                .and_then(serde_json::Value::as_bool),
            scopes: app.root.scopes,
            remote_configuration,
        })
    }

    pub async fn list_versions(&self, app_id: &str) -> Result<AppVersionsReport> {
        #[derive(Deserialize)]
        struct Data {
            app: Option<App>,
        }
        #[derive(Deserialize)]
        struct App {
            #[serde(rename = "activeRelease")]
            active_release: Option<ActiveRelease>,
            versions: Connection,
            #[serde(rename = "versionsCount")]
            versions_count: u64,
        }
        #[derive(Deserialize)]
        struct ActiveRelease {
            version: VersionId,
        }
        #[derive(Deserialize)]
        struct VersionId {
            id: String,
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
            #[serde(rename = "createdAt")]
            created_at: String,
            #[serde(rename = "createdBy")]
            created_by: Option<String>,
            metadata: Option<Metadata>,
        }
        #[derive(Deserialize)]
        struct Metadata {
            message: Option<String>,
            #[serde(rename = "versionTag")]
            version_tag: Option<String>,
        }
        let response = self
            .graphql
            .execute::<_, Data>(&GraphQlRequest::query(
                APP_VERSIONS_QUERY,
                serde_json::json!({"appId": app_id}),
            ))
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::Api,
                    format!("could not list app versions: {error}"),
                )
            })?;
        let app = response
            .data
            .app
            .ok_or_else(|| Error::new(ErrorKind::Api, "Shopify app was not found"))?;
        let active_id = app.active_release.map(|release| release.version.id);
        let versions = app
            .versions
            .edges
            .into_iter()
            .map(|edge| {
                let metadata = edge.node.metadata;
                RemoteAppVersion {
                    status: if active_id.as_deref() == Some(edge.node.id.as_str()) {
                        "active".into()
                    } else {
                        "inactive".into()
                    },
                    id: edge.node.id,
                    version: metadata
                        .as_ref()
                        .and_then(|value| value.version_tag.clone()),
                    message: metadata
                        .as_ref()
                        .and_then(|value| value.message.clone())
                        .unwrap_or_default(),
                    created_at: edge.node.created_at,
                    created_by: edge.node.created_by.unwrap_or_default(),
                }
            })
            .collect();
        Ok(AppVersionsReport {
            versions,
            total: app.versions_count,
        })
    }

    pub async fn release_version(&self, app_id: &str, version_tag: &str) -> Result<ReleaseReport> {
        #[derive(Deserialize)]
        struct VersionData {
            #[serde(rename = "versionByTag")]
            version: Option<Version>,
        }
        #[derive(Deserialize)]
        struct Version {
            id: String,
            metadata: Option<Metadata>,
        }
        #[derive(Deserialize)]
        struct Metadata {
            message: Option<String>,
            #[serde(rename = "versionTag")]
            version_tag: Option<String>,
        }
        let version = self
            .graphql
            .execute::<_, VersionData>(&GraphQlRequest::query(
                APP_VERSION_BY_TAG_QUERY,
                serde_json::json!({"versionTag": version_tag}),
            ))
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::Api,
                    format!("could not find app version `{version_tag}`: {error}"),
                )
            })?
            .data
            .version
            .ok_or_else(|| {
                Error::invalid_input(format!("app version `{version_tag}` was not found"))
            })?;

        #[derive(Deserialize)]
        struct ReleaseData {
            #[serde(rename = "appReleaseCreate")]
            result: ReleaseResult,
        }
        #[derive(Deserialize)]
        struct ReleaseResult {
            release: Option<Release>,
            #[serde(rename = "userErrors", default)]
            user_errors: Vec<UserError>,
        }
        #[derive(Deserialize)]
        struct Release {
            version: Version,
        }
        #[derive(Deserialize)]
        struct UserError {
            message: String,
        }
        let response = self
            .graphql
            .execute::<_, ReleaseData>(&GraphQlRequest::mutation(
                RELEASE_VERSION_MUTATION,
                serde_json::json!({"appId": app_id, "versionId": version.id}),
            ))
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::Api,
                    format!("could not release app version `{version_tag}`: {error}"),
                )
            })?;
        if !response.data.result.user_errors.is_empty() {
            return Err(Error::new(
                ErrorKind::Api,
                response
                    .data
                    .result
                    .user_errors
                    .into_iter()
                    .map(|error| error.message)
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        let released = response
            .data
            .result
            .release
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Api,
                    "release response omitted the released version",
                )
            })?
            .version;
        let metadata = released.metadata.or(version.metadata);
        Ok(ReleaseReport {
            app_id: app_id.to_owned(),
            version_id: released.id,
            version: metadata
                .as_ref()
                .and_then(|value| value.version_tag.clone())
                .unwrap_or_else(|| version_tag.to_owned()),
            message: metadata.and_then(|value| value.message).unwrap_or_default(),
        })
    }
}

pub async fn exchange_app_management_token(session: &Session) -> Result<Secret> {
    exchange_application_token(
        session,
        APP_MANAGEMENT_AUDIENCE,
        APP_MANAGEMENT_SCOPE,
        "app-management",
    )
    .await
}

pub async fn exchange_business_platform_token(session: &Session) -> Result<Secret> {
    exchange_application_token(
        session,
        BUSINESS_PLATFORM_AUDIENCE,
        BUSINESS_PLATFORM_SCOPE,
        "business-platform",
    )
    .await
}

async fn exchange_application_token(
    session: &Session,
    audience: &str,
    scope: &str,
    label: &str,
) -> Result<Secret> {
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
            ("audience", audience),
            ("scope", scope),
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
                format!("{label} token exchange failed"),
                error,
            )
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::new(
            ErrorKind::Api,
            format!("{label} token exchange returned HTTP {status}; run `cfy auth login` again"),
        ));
    }
    let token = response.json::<Response>().await.map_err(|error| {
        Error::with_source(
            ErrorKind::Api,
            format!("invalid {label} token response"),
            error,
        )
    })?;
    Ok(Secret::new(token.access_token))
}

fn decode_organization_id(encoded: &str) -> Result<String> {
    let decoded = STANDARD
        .decode(encoded)
        .or_else(|_| STANDARD_NO_PAD.decode(encoded))
        .map_err(|error| {
            Error::with_source(
                ErrorKind::Api,
                format!("invalid Shopify organization ID `{encoded}`"),
                error,
            )
        })?;
    let gid = String::from_utf8(decoded).map_err(|error| {
        Error::with_source(ErrorKind::Api, "invalid Shopify organization GID", error)
    })?;
    gid.rsplit('/')
        .next()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::new(
                ErrorKind::Api,
                format!("invalid Shopify organization GID `{gid}`"),
            )
        })
}

fn transform_remote_module(identifier: &str, config: &serde_json::Value) -> Result<toml::Table> {
    let mut output = toml::Table::new();
    match identifier {
        "app_home" => {
            copy_json_field(config, "app_url", &mut output, "application_url")?;
            copy_json_field(config, "embedded", &mut output, "embedded")?;
            if let Some(value) = config
                .get("preferences_url")
                .filter(|value| !value.is_null())
            {
                insert_nested_json(&mut output, &["app_preferences", "url"], value)?;
            }
        }
        "app_access" => {
            copy_json_object_field(config, "access", &mut output, "access")?;
            for (remote, local) in [
                ("scopes", "scopes"),
                ("required_scopes", "required_scopes"),
                ("optional_scopes", "optional_scopes"),
                ("use_legacy_install_flow", "use_legacy_install_flow"),
            ] {
                if let Some(value) = config.get(remote).filter(|value| !value.is_null()) {
                    insert_nested_json(&mut output, &["access_scopes", local], value)?;
                }
            }
            if let Some(value) = config
                .get("redirect_url_allowlist")
                .filter(|value| !value.is_null())
            {
                insert_nested_json(&mut output, &["auth", "redirect_urls"], value)?;
            }
        }
        "app_proxy" => {
            let mut config = config.clone();
            if let Some(url) = config
                .get("url")
                .and_then(serde_json::Value::as_str)
                .map(|url| url.trim_end_matches('/').to_owned())
            {
                config["url"] = serde_json::Value::String(url);
            }
            let value = json_to_toml(&config)?;
            if let toml::Value::Table(table) = value {
                output.insert("app_proxy".into(), toml::Value::Table(table));
            }
        }
        "point_of_sale" => {
            if let Some(value) = config.get("embedded").filter(|value| !value.is_null()) {
                insert_nested_json(&mut output, &["pos", "embedded"], value)?;
            }
        }
        "webhooks" => {
            let value = json_to_toml(config)?;
            if let toml::Value::Table(table) = value {
                let webhooks = output
                    .entry("webhooks")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                if let toml::Value::Table(target) = webhooks {
                    deep_merge_table(target, table);
                }
            }
        }
        "webhook_subscription" => {
            let mut subscription = toml::Table::new();
            for key in [
                "uri",
                "actions",
                "include_fields",
                "filter",
                "payload_query",
                "name",
            ] {
                if let Some(value) = config.get(key).filter(|value| !value.is_null()) {
                    subscription.insert(key.into(), json_to_toml(value)?);
                }
            }
            if let Some(topic) = config.get("topic").filter(|value| !value.is_null()) {
                subscription.insert(
                    "topics".into(),
                    toml::Value::Array(vec![json_to_toml(topic)?]),
                );
            }
            append_webhook_subscription(&mut output, subscription)?;
        }
        "privacy_compliance_webhooks" => {
            for (key, topic) in [
                ("customers_data_request_url", "customers/data_request"),
                ("customers_redact_url", "customers/redact"),
                ("shop_redact_url", "shop/redact"),
            ] {
                let Some(uri) = config.get(key).and_then(serde_json::Value::as_str) else {
                    continue;
                };
                append_or_merge_compliance_subscription(&mut output, uri, topic)?;
            }
            if let Some(version) = config.get("api_version").filter(|value| !value.is_null()) {
                insert_nested_json(&mut output, &["webhooks", "api_version"], version)?;
            }
        }
        _ => {}
    }
    Ok(output)
}

fn webhook_subscriptions_mut(output: &mut toml::Table) -> Result<&mut Vec<toml::Value>> {
    let webhooks = output
        .entry("webhooks")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| Error::new(ErrorKind::Config, "webhooks configuration must be a table"))?;
    webhooks
        .entry("subscriptions")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| Error::new(ErrorKind::Config, "webhook subscriptions must be an array"))
}

fn append_webhook_subscription(output: &mut toml::Table, subscription: toml::Table) -> Result<()> {
    if !subscription.is_empty() {
        webhook_subscriptions_mut(output)?.push(toml::Value::Table(subscription));
    }
    Ok(())
}

fn append_or_merge_compliance_subscription(
    output: &mut toml::Table,
    uri: &str,
    topic: &str,
) -> Result<()> {
    let subscriptions = webhook_subscriptions_mut(output)?;
    if let Some(existing) = subscriptions.iter_mut().find_map(|value| {
        let table = value.as_table_mut()?;
        (table.get("uri").and_then(toml::Value::as_str) == Some(uri)).then_some(table)
    }) {
        let topics = existing
            .entry("compliance_topics")
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| Error::new(ErrorKind::Config, "compliance_topics must be an array"))?;
        topics.push(toml::Value::String(topic.into()));
    } else {
        let mut subscription = toml::Table::new();
        subscription.insert("uri".into(), toml::Value::String(uri.into()));
        subscription.insert(
            "compliance_topics".into(),
            toml::Value::Array(vec![toml::Value::String(topic.into())]),
        );
        subscriptions.push(toml::Value::Table(subscription));
    }
    Ok(())
}

fn copy_json_field(
    source: &serde_json::Value,
    source_key: &str,
    target: &mut toml::Table,
    target_key: &str,
) -> Result<()> {
    if let Some(value) = source.get(source_key).filter(|value| !value.is_null()) {
        target.insert(target_key.into(), json_to_toml(value)?);
    }
    Ok(())
}

fn copy_json_object_field(
    source: &serde_json::Value,
    source_key: &str,
    target: &mut toml::Table,
    target_key: &str,
) -> Result<()> {
    copy_json_field(source, source_key, target, target_key)
}

fn insert_nested_json(
    target: &mut toml::Table,
    path: &[&str],
    value: &serde_json::Value,
) -> Result<()> {
    let (last, parents) = path
        .split_last()
        .ok_or_else(|| Error::invalid_input("configuration path cannot be empty"))?;
    let mut table = target;
    for key in parents {
        let entry = table
            .entry((*key).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        table = entry.as_table_mut().ok_or_else(|| {
            Error::new(
                ErrorKind::Config,
                format!("remote configuration `{key}` is not a table"),
            )
        })?;
    }
    table.insert((*last).to_owned(), json_to_toml(value)?);
    Ok(())
}

fn json_to_toml(value: &serde_json::Value) -> Result<toml::Value> {
    match value {
        serde_json::Value::Null => Err(Error::new(
            ErrorKind::Config,
            "remote configuration contains an unsupported null value",
        )),
        serde_json::Value::Bool(value) => Ok(toml::Value::Boolean(*value)),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| value.as_f64().map(toml::Value::Float))
            .ok_or_else(|| Error::new(ErrorKind::Config, "invalid remote numeric value")),
        serde_json::Value::String(value) => Ok(toml::Value::String(value.clone())),
        serde_json::Value::Array(values) => values
            .iter()
            .map(json_to_toml)
            .collect::<Result<Vec<_>>>()
            .map(toml::Value::Array),
        serde_json::Value::Object(values) => values
            .iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(key, value)| Ok((key.clone(), json_to_toml(value)?)))
            .collect::<Result<toml::Table>>()
            .map(toml::Value::Table),
    }
}

fn deep_merge_table(target: &mut toml::Table, source: toml::Table) {
    for (key, value) in source {
        match (target.get_mut(&key), value) {
            (Some(toml::Value::Table(target)), toml::Value::Table(source)) => {
                deep_merge_table(target, source);
            }
            (_, value) => {
                target.insert(key, value);
            }
        }
    }
}

fn merge_remote_configuration(target: &mut toml::Table, mut source: toml::Table) -> Result<()> {
    let subscriptions = source
        .get_mut("webhooks")
        .and_then(toml::Value::as_table_mut)
        .and_then(|webhooks| webhooks.remove("subscriptions"))
        .and_then(|value| value.as_array().cloned());
    deep_merge_table(target, source);
    if let Some(subscriptions) = subscriptions {
        webhook_subscriptions_mut(target)?.extend(subscriptions);
    }
    Ok(())
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
    deep_merge_table(&mut document, app.remote_configuration.clone());
    document.insert(
        "client_id".into(),
        toml::Value::String(app.client_id.clone()),
    );
    document.insert("name".into(), toml::Value::String(app.name.clone()));
    if let Some(handle) = &app.handle {
        document.insert("handle".into(), toml::Value::String(handle.clone()));
    }
    if let Some(url) = &app.application_url {
        document.insert("application_url".into(), toml::Value::String(url.clone()));
    }
    if let Some(embedded) = app.embedded {
        document.insert("embedded".into(), toml::Value::Boolean(embedded));
    }
    if !app.scopes.is_empty() {
        let access_scopes = document
            .entry("access_scopes")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Config,
                    "linked configuration access_scopes must be a table",
                )
            })?;
        access_scopes
            .entry("scopes")
            .or_insert_with(|| toml::Value::String(app.scopes.join(",")));
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
        let remote_configuration: toml::Table = toml::from_str(
            r#"[auth]
redirect_urls = ["https://example.test/auth/callback"]

[access_scopes]
optional_scopes = ["write_products"]

[app_preferences]
url = "https://example.test/settings"

[app_proxy]
url = "https://example.test/apps/proxy"
subpath = "proxy"
prefix = "apps"

[pos]
embedded = true

[webhooks]
api_version = "2026-07"

[[webhooks.subscriptions]]
topics = ["orders/create"]
uri = "pubsub://project:topic"

[[webhooks.subscriptions]]
compliance_topics = ["customers/data_request", "customers/redact", "shop/redact"]
uri = "/webhooks"
"#,
        )
        .unwrap();
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
                handle: Some("native-app".into()),
                organization_id: "1".into(),
                application_url: Some("https://example.test".into()),
                embedded: Some(true),
                scopes: vec!["read_products".into(), "write_products".into()],
                remote_configuration,
            },
        )
        .unwrap();
        let source = std::fs::read_to_string(report.path).unwrap();
        assert!(source.contains("client_id = \"client-key\""));
        assert!(source.contains("handle = \"native-app\""));
        assert!(source.contains("application_url = \"https://example.test\""));
        assert!(source.contains("scopes = \"read_products,write_products\""));
        assert!(source.contains("redirect_urls = [\"https://example.test/auth/callback\"]"));
        assert!(source.contains("optional_scopes = [\"write_products\"]"));
        assert!(source.contains("[app_preferences]"));
        assert!(source.contains("[app_proxy]"));
        assert!(source.contains("[pos]"));
        assert!(source.contains("[webhooks]"));
        assert!(source.contains("[[webhooks.subscriptions]]"));
        assert!(source.contains("topics = [\"orders/create\"]"));
        let linked: toml::Value = toml::from_str(&source).unwrap();
        assert_eq!(
            linked["webhooks"]["subscriptions"][1]["compliance_topics"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refreshing_linked_config_preserves_local_sections() {
        let directory = temp();
        let path = directory.join("shopify.app.staging.toml");
        std::fs::write(
            &path,
            r#"client_id = "old"
[build]
automatically_update_urls_on_dev = true

[[webhooks.subscriptions]]
topics = ["products/create"]
uri = "/webhooks"
"#,
        )
        .unwrap();
        write_linked_config(
            &LinkOptions {
                directory: directory.clone(),
                client_id: Some("new".into()),
                file_name: Some("shopify.app.staging.toml".into()),
                force: true,
            },
            &RemoteApp {
                id: "1".into(),
                client_id: "new".into(),
                name: "Updated app".into(),
                handle: None,
                organization_id: "1".into(),
                application_url: Some("https://updated.example".into()),
                embedded: Some(true),
                scopes: vec!["read_products".into()],
                remote_configuration: toml::Table::new(),
            },
        )
        .unwrap();
        let source = std::fs::read_to_string(path).unwrap();
        assert!(source.contains("client_id = \"new\""));
        assert!(source.contains("automatically_update_urls_on_dev = true"));
        assert!(source.contains("topics = [\"products/create\"]"));
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
                handle: None,
                organization_id: "1".into(),
                application_url: None,
                embedded: None,
                scopes: vec![],
                remote_configuration: toml::Table::new(),
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
            for (index, body) in [
                r#"{"data":{"appsConnection":{"edges":[{"node":{"id":"app-1","key":"client-1","activeRelease":{"version":{"name":"Example"}}}}],"pageInfo":{"hasNextPage":false}}}}"#,
                r#"{"data":{"app":{"id":"app-1","key":"client-1","organizationId":"gid://shopify/Organization/7","activeRoot":{"grantedShopifyApprovalScopes":["read_products"]},"activeRelease":{"version":{"name":"Example","appModules":[{"config":{"app_url":"https://example.test","embedded":true,"preferences_url":"https://example.test/settings"},"specification":{"externalIdentifier":"app_home"}},{"config":{"redirect_url_allowlist":["https://example.test/auth/callback"],"scopes":"read_products,write_orders","optional_scopes":["write_products"],"access":{"admin":{"direct_api_mode":"online"}}},"specification":{"externalIdentifier":"app_access"}},{"config":{"url":"https://example.test/apps/proxy/","subpath":"proxy","prefix":"apps"},"specification":{"externalIdentifier":"app_proxy"}},{"config":{"embedded":false},"specification":{"externalIdentifier":"point_of_sale"}},{"config":{"topic":"orders/create","uri":"pubsub://project:topic"},"specification":{"externalIdentifier":"webhook_subscription"}},{"config":{"topic":"orders/updated","uri":"pubsub://project:topic"},"specification":{"externalIdentifier":"webhook_subscription"}},{"config":{"api_version":"2025-07","customers_data_request_url":"/webhooks","customers_redact_url":"/webhooks","shop_redact_url":"/webhooks"},"specification":{"externalIdentifier":"privacy_compliance_webhooks"}}]}}}}}"#,
                r#"{"data":{"app":{"activeRelease":{"version":{"id":"version-2"}},"versions":{"edges":[{"node":{"id":"version-2","createdAt":"2026-09-02T10:00:00Z","createdBy":"Yanuar","metadata":{"message":"Current","versionTag":"2"}}},{"node":{"id":"version-1","createdAt":"2026-09-01T10:00:00Z","createdBy":null,"metadata":{"message":null,"versionTag":"1"}}}]},"versionsCount":2}}}"#,
                r#"{"data":{"app":{"key":"client-1","activeRoot":{"clientCredentials":{"secrets":[{"key":"super-secret"}]}}}}}"#,
            ]
            .into_iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0_u8; 8192];
                let read = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                assert!(request.contains("authorization: Bearer token"));
                if index == 0 {
                    assert!(request.contains("organizationId"));
                    assert!(request.contains("\"7\""));
                }
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
        let apps = client.list_apps("7").await.unwrap();
        assert_eq!(apps[0].name, "Example");
        let app = client.app_by_client_id("client-1").await.unwrap();
        assert_eq!(app.application_url.as_deref(), Some("https://example.test"));
        assert_eq!(app.handle, None);
        assert_eq!(app.scopes, ["read_products"]);
        assert_eq!(
            app.remote_configuration["auth"]["redirect_urls"][0].as_str(),
            Some("https://example.test/auth/callback")
        );
        assert_eq!(
            app.remote_configuration["app_preferences"]["url"].as_str(),
            Some("https://example.test/settings")
        );
        assert_eq!(
            app.remote_configuration["app_proxy"]["subpath"].as_str(),
            Some("proxy")
        );
        assert_eq!(
            app.remote_configuration["pos"]["embedded"].as_bool(),
            Some(false)
        );
        assert_eq!(
            app.remote_configuration["webhooks"]["api_version"].as_str(),
            Some("2025-07")
        );
        let subscriptions = app.remote_configuration["webhooks"]["subscriptions"]
            .as_array()
            .unwrap();
        assert_eq!(subscriptions.len(), 3);
        assert_eq!(
            subscriptions[0]["topics"][0].as_str(),
            Some("orders/create")
        );
        assert_eq!(
            subscriptions[1]["topics"][0].as_str(),
            Some("orders/updated")
        );
        assert_eq!(
            subscriptions[2]["compliance_topics"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let versions = client.list_versions("app-1").await.unwrap();
        assert_eq!(versions.total, 2);
        assert_eq!(versions.versions[0].status, "active");
        assert_eq!(versions.versions[1].status, "inactive");
        let credentials = client.app_client_credentials("client-1").await.unwrap();
        assert_eq!(credentials.client_id, "client-1");
        assert_eq!(credentials.client_secret.expose(), "super-secret");
        assert!(!format!("{credentials:?}").contains("super-secret"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn app_management_backend_decodes_all_extension_registration_families() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 8192];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("organizationId"));
            assert!(request.contains("organization-7"));
            assert!(request.contains("appModules"));
            assert!(request.contains("externalIdentifier"));
            let body = r#"{"data":{"app":{"activeRelease":{"version":{"appModules":[{"uuid":"a","handle":"Payments","config":{"start_payment_session_url":"https://example.test/pay"},"target":"payments.offsite.render","specification":{"externalIdentifier":"payments_app"}},{"uuid":"b","handle":"Flow","config":{"title":"Flow","description":"active","url":"https://example.test/flow"},"specification":{"externalIdentifier":"flow_action_definition"}},{"uuid":"c","handle":"Admin link","config":{"text":"Open","url":"https://example.test/open"},"target":"products#show","specification":{"externalIdentifier":"app_link"}},{"uuid":"ignored","handle":"Pixel","config":{"runtime_context":"strict"},"specification":{"externalIdentifier":"web_pixel"}}]}}}}}"#;
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
        let registrations = client
            .extension_registrations("client-1", "organization-7")
            .await
            .unwrap();
        assert_eq!(
            registrations
                .iter()
                .map(|item| item.uuid.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(
            registrations[0].configuration["start_payment_session_url"],
            "https://example.test/pay"
        );
        assert_eq!(registrations[1].configuration["description"], "active");
        assert_eq!(registrations[2].configuration["text"], "Open");
        assert_eq!(registrations[2].context.as_deref(), Some("products#show"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn business_platform_backend_lists_and_decodes_organizations() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 8192];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("authorization: Bearer token"));
            assert!(request.contains("organizationsWithAccessToDestination"));
            let body = r#"{"data":{"currentUserAccount":{"organizationsWithAccessToDestination":{"nodes":[{"id":"Z2lkOi8vb3JnYW5pemF0aW9uL09yZ2FuaXphdGlvbi83","name":"Example Org"}]}}}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = BusinessPlatformClient::new(
            &format!("http://{address}/destinations/api/2020-07/graphql"),
            "token",
        )
        .unwrap();
        let organizations = client.list_organizations().await.unwrap();
        assert_eq!(
            organizations,
            [RemoteOrganization {
                id: "7".into(),
                name: "Example Org".into(),
            }]
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn releases_an_existing_version_and_reports_user_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for body in [
                r#"{"data":{"versionByTag":{"id":"version-2","metadata":{"message":"Ship it","versionTag":"2"}}}}"#,
                r#"{"data":{"appReleaseCreate":{"release":{"version":{"id":"version-2","metadata":{"message":"Ship it","versionTag":"2"}}},"userErrors":[]}}}"#,
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
        let report = client.release_version("app-1", "2").await.unwrap();
        assert_eq!(report.version_id, "version-2");
        assert_eq!(report.version, "2");
        assert_eq!(report.message, "Ship it");
        server.await.unwrap();
    }
}
