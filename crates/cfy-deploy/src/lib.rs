//! Shopify App Management deploy protocol.
//!
//! The protocol is deliberately modelled as four distinct operations. A deploy
//! requests a signed source URL, uploads the *complete* source with an
//! idempotent `PUT`, creates a version which references that URL, and only then
//! optionally creates a release. In particular, a version never exists before
//! its source has been uploaded successfully.

use async_trait::async_trait;
use cfy_api::{GraphQlClient, GraphQlRequest, HttpClient};
use cfy_build::BuildReport;
use cfy_core::{Cancellation, Error, ErrorKind};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    time::Duration,
};
use thiserror::Error as ThisError;
use url::Url;

const REQUEST_SOURCE_UPLOAD_URL: &str = r#"mutation CreateAssetURL($sourceExtension: SourceExtension!, $organizationId: ID!) {
  appRequestSourceUploadUrl(sourceExtension: $sourceExtension, organizationId: $organizationId) {
    sourceUploadUrl
    userErrors { field message }
  }
}"#;
const CREATE_APP_VERSION: &str = r#"mutation CreateAppVersion($appId: ID!, $version: AppVersionInput!, $metadata: VersionMetadataInput) {
  appVersionCreate(appId: $appId, version: $version, metadata: $metadata) {
    version { id metadata { versionTag message } }
    userErrors { field message category code on }
  }
}"#;
const CREATE_APP_RELEASE: &str = r#"mutation ReleaseVersion($appId: ID!, $versionId: ID!) {
  appReleaseCreate(appId: $appId, versionId: $versionId) {
    release { version { id metadata { versionTag message } } }
    userErrors { field message category code on }
  }
}"#;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeploySelection {
    /// App Management app ID (normally a Shopify GID).
    pub app: String,
    /// Organization ID. Numeric IDs are converted to Shopify organization GIDs.
    pub environment: String,
}

impl DeploySelection {
    fn organization_gid(&self) -> String {
        if self.environment.starts_with("gid://") {
            self.environment.clone()
        } else {
            format!("gid://shopify/Organization/{}", self.environment)
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_control_url: Option<String>,
}

/// Contract for uploading to Shopify's signed object-storage URL.
///
/// A complete source larger than `max_bytes` is rejected before any network
/// operation. The initial request plus `max_retries` attempts may be made. A
/// retry is allowed only for connection/timeout failures and HTTP 408, 429, or
/// 5xx responses. Since the body is sent with `PUT`, replay is idempotent.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceUploadPolicy {
    pub max_bytes: u64,
    pub max_retries: u32,
    pub base_delay_millis: u64,
    pub max_delay_millis: u64,
}

impl Default for SourceUploadPolicy {
    fn default() -> Self {
        Self {
            max_bytes: 100 * 1024 * 1024,
            max_retries: 3,
            base_delay_millis: 200,
            max_delay_millis: 5_000,
        }
    }
}

impl SourceUploadPolicy {
    fn delay(&self, attempt: u32) -> Duration {
        Duration::from_millis(
            self.base_delay_millis
                .saturating_mul(2_u64.saturating_pow(attempt))
                .min(self.max_delay_millis),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeployOptions {
    pub selection: Option<DeploySelection>,
    pub non_interactive: bool,
    pub dry_run: bool,
    pub release: bool,
    #[serde(default)]
    pub metadata: VersionMetadata,
    #[serde(default)]
    pub upload_policy: SourceUploadPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub digest: String,
}

/// The one complete source archive accepted by App Management.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompleteSource {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub digest: String,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SignedSourceUpload {
    /// This URL is secret-bearing and is intentionally omitted from reports and
    /// from `Debug` output.
    url: Url,
}

impl SignedSourceUpload {
    pub fn new(url: Url) -> Result<Self, BackendError> {
        if url.scheme() != "https" {
            return Err(BackendError::Protocol(
                "signed source upload URL must use HTTPS".into(),
            ));
        }
        Ok(Self { url })
    }

    #[must_use]
    pub fn source_url(&self) -> &str {
        self.url.as_str()
    }
}

impl std::fmt::Debug for SignedSourceUpload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SignedSourceUpload")
            .field("url", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UserError {
    #[serde(default)]
    pub field: Vec<String>,
    pub message: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub on: Option<serde_json::Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DeployOperation {
    RequestSourceUpload,
    CreateVersion,
    CreateRelease,
}

#[derive(Debug, ThisError)]
pub enum BackendError {
    #[error("Shopify rejected the operation")]
    UserErrors(Vec<UserError>),
    #[error("transport failed: {0}")]
    Transport(String),
    #[error("invalid backend response: {0}")]
    Protocol(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateVersionRequest<'a> {
    pub app_id: &'a str,
    pub source_url: &'a str,
    pub metadata: &'a VersionMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CreatedVersion {
    pub id: String,
    pub version_tag: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UploadProgress {
    pub path: String,
    pub uploaded_bytes: usize,
    pub total_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeployReport {
    pub selection: DeploySelection,
    pub dry_run: bool,
    pub version_id: Option<String>,
    pub uploaded: Vec<String>,
    pub released: bool,
    pub progress: Vec<UploadProgress>,
    pub warnings: Vec<String>,
}

#[derive(Debug, ThisError)]
pub enum DeployError {
    #[error("app/organization selection is required")]
    MissingSelection,
    #[error("deploy validation failed: {0}")]
    Validation(String),
    #[error("Shopify rejected {operation:?}: {errors:?}")]
    UserErrors {
        operation: DeployOperation,
        errors: Vec<UserError>,
    },
    #[error("{operation:?} failed: {message}")]
    Backend {
        operation: DeployOperation,
        message: String,
    },
    #[error("source upload failed for {path}: {message}; no app version was created")]
    Upload { path: String, message: String },
    #[error("release failed for version {}: {message}; the created version remains available", version.id)]
    Release {
        version: CreatedVersion,
        message: String,
        user_errors: Vec<UserError>,
    },
    #[error("deploy was cancelled")]
    Cancelled,
}

impl From<DeployError> for Error {
    fn from(error: DeployError) -> Self {
        let kind = match error {
            DeployError::MissingSelection | DeployError::Validation(_) => ErrorKind::Config,
            DeployError::Cancelled => ErrorKind::Process,
            DeployError::UserErrors { .. }
            | DeployError::Backend { .. }
            | DeployError::Upload { .. }
            | DeployError::Release { .. } => ErrorKind::Api,
        };
        Error::new(kind, error.to_string())
    }
}

/// Typed boundary for the Shopify App Management deploy protocol.
#[async_trait]
pub trait DeployBackend: Send + Sync {
    async fn request_source_upload(
        &self,
        selection: &DeploySelection,
    ) -> Result<SignedSourceUpload, BackendError>;

    async fn put_complete_source(
        &self,
        upload: &SignedSourceUpload,
        source: &CompleteSource,
        policy: &SourceUploadPolicy,
        progress: &mut (dyn FnMut(UploadProgress) + Send),
        cancellation: &Cancellation,
    ) -> Result<(), BackendError>;

    async fn create_version(
        &self,
        request: CreateVersionRequest<'_>,
    ) -> Result<CreatedVersion, BackendError>;

    async fn create_release(&self, app_id: &str, version_id: &str) -> Result<(), BackendError>;
}

/// Concrete App Management GraphQL backend plus signed-URL uploader.
#[derive(Clone)]
pub struct AppManagementBackend {
    graphql: GraphQlClient,
    upload_client: reqwest::Client,
}

/// Compatibility alias for the former untyped wrapper.
pub type AppAdminBackend = AppManagementBackend;

impl AppManagementBackend {
    pub fn new(endpoint: &str, token: &str) -> Result<Self, DeployError> {
        let endpoint = Url::parse(endpoint).map_err(|error| {
            DeployError::Validation(format!("invalid App Management endpoint: {error}"))
        })?;
        if endpoint.scheme() != "https" {
            return Err(DeployError::Validation(
                "App Management endpoint must use HTTPS".into(),
            ));
        }
        let path = match endpoint.query() {
            Some(query) => format!("{}?{query}", endpoint.path()),
            None => endpoint.path().to_owned(),
        };
        let mut origin = endpoint.clone();
        origin.set_path("/");
        origin.set_query(None);
        origin.set_fragment(None);
        let http = HttpClient::new(origin.as_str())
            .map_err(|error| DeployError::Validation(error.to_string()))?
            .with_sensitive_header(
                header::AUTHORIZATION,
                header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|error| DeployError::Validation(format!("invalid token: {error}")))?,
            );
        let upload_client = reqwest::Client::builder().build().map_err(|error| {
            DeployError::Validation(format!("could not create upload client: {error}"))
        })?;
        Ok(Self {
            graphql: GraphQlClient::new(http, path),
            upload_client,
        })
    }

    async fn execute<V, D>(&self, query: &str, variables: V) -> Result<D, BackendError>
    where
        V: Serialize + Send + Sync,
        D: serde::de::DeserializeOwned,
    {
        self.graphql
            .execute::<_, D>(&GraphQlRequest::mutation(query, variables))
            .await
            .map(|response| response.data)
            .map_err(|error| BackendError::Transport(error.to_string()))
    }
}

#[derive(Deserialize)]
struct GraphUserError {
    #[serde(default)]
    field: Vec<String>,
    message: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    on: Option<serde_json::Value>,
}

impl From<GraphUserError> for UserError {
    fn from(value: GraphUserError) -> Self {
        Self {
            field: value.field,
            message: value.message,
            category: value.category,
            code: value.code,
            on: value.on,
        }
    }
}

#[async_trait]
impl DeployBackend for AppManagementBackend {
    async fn request_source_upload(
        &self,
        selection: &DeploySelection,
    ) -> Result<SignedSourceUpload, BackendError> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "appRequestSourceUploadUrl")]
            result: ResultData,
        }
        #[derive(Deserialize)]
        struct ResultData {
            #[serde(rename = "sourceUploadUrl")]
            source_upload_url: Option<String>,
            #[serde(rename = "userErrors", default)]
            user_errors: Vec<GraphUserError>,
        }
        let data: Data = self
            .execute(
                REQUEST_SOURCE_UPLOAD_URL,
                serde_json::json!({
                    "sourceExtension": "BR",
                    "organizationId": selection.organization_gid(),
                }),
            )
            .await?;
        reject_user_errors(data.result.user_errors)?;
        let raw = data.result.source_upload_url.ok_or_else(|| {
            BackendError::Protocol("source upload response omitted sourceUploadUrl".into())
        })?;
        SignedSourceUpload::new(
            Url::parse(&raw)
                .map_err(|error| BackendError::Protocol(format!("invalid signed URL: {error}")))?,
        )
    }

    async fn put_complete_source(
        &self,
        upload: &SignedSourceUpload,
        source: &CompleteSource,
        policy: &SourceUploadPolicy,
        progress: &mut (dyn FnMut(UploadProgress) + Send),
        cancellation: &Cancellation,
    ) -> Result<(), BackendError> {
        let size = u64::try_from(source.bytes.len()).unwrap_or(u64::MAX);
        if size > policy.max_bytes {
            return Err(BackendError::Protocol(format!(
                "complete source is {size} bytes; maximum is {} bytes",
                policy.max_bytes
            )));
        }
        let mut attempt = 0;
        loop {
            if cancellation.is_cancelled() {
                return Err(BackendError::Protocol("upload cancelled".into()));
            }
            let result = self
                .upload_client
                .put(upload.url.clone())
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::CONTENT_LENGTH, size)
                .body(source.bytes.clone())
                .send()
                .await;
            match result {
                Ok(response) if response.status().is_success() => {
                    progress(UploadProgress {
                        path: source.path.display().to_string(),
                        uploaded_bytes: source.bytes.len(),
                        total_bytes: source.bytes.len(),
                    });
                    return Ok(());
                }
                Ok(response)
                    if retryable_status(response.status()) && attempt < policy.max_retries => {}
                Ok(response) => {
                    return Err(BackendError::Transport(format!(
                        "signed URL returned HTTP {}",
                        response.status()
                    )));
                }
                Err(error)
                    if (error.is_connect() || error.is_timeout())
                        && attempt < policy.max_retries => {}
                Err(error) => return Err(BackendError::Transport(error.to_string())),
            }
            tokio::time::sleep(policy.delay(attempt)).await;
            attempt += 1;
        }
    }

    async fn create_version(
        &self,
        request: CreateVersionRequest<'_>,
    ) -> Result<CreatedVersion, BackendError> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "appVersionCreate")]
            result: ResultData,
        }
        #[derive(Deserialize)]
        struct ResultData {
            version: Option<Version>,
            #[serde(rename = "userErrors", default)]
            user_errors: Vec<GraphUserError>,
        }
        #[derive(Deserialize)]
        struct Version {
            id: String,
            metadata: Option<ResponseMetadata>,
        }
        #[derive(Deserialize)]
        struct ResponseMetadata {
            #[serde(rename = "versionTag")]
            version_tag: Option<String>,
            message: Option<String>,
        }
        let data: Data = self
            .execute(
                CREATE_APP_VERSION,
                serde_json::json!({
                    "appId": request.app_id,
                    "version": {"sourceUrl": request.source_url},
                    "metadata": request.metadata,
                }),
            )
            .await?;
        reject_user_errors(data.result.user_errors)?;
        let version = data.result.version.ok_or_else(|| {
            BackendError::Protocol("appVersionCreate response omitted version".into())
        })?;
        Ok(CreatedVersion {
            id: version.id,
            version_tag: version
                .metadata
                .as_ref()
                .and_then(|value| value.version_tag.clone()),
            message: version.metadata.and_then(|value| value.message),
        })
    }

    async fn create_release(&self, app_id: &str, version_id: &str) -> Result<(), BackendError> {
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "appReleaseCreate")]
            result: ResultData,
        }
        #[derive(Deserialize)]
        struct ResultData {
            release: Option<serde_json::Value>,
            #[serde(rename = "userErrors", default)]
            user_errors: Vec<GraphUserError>,
        }
        let data: Data = self
            .execute(
                CREATE_APP_RELEASE,
                serde_json::json!({"appId": app_id, "versionId": version_id}),
            )
            .await?;
        reject_user_errors(data.result.user_errors)?;
        if data.result.release.is_none() {
            return Err(BackendError::Protocol(
                "appReleaseCreate response omitted release".into(),
            ));
        }
        Ok(())
    }
}

fn reject_user_errors(errors: Vec<GraphUserError>) -> Result<(), BackendError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(BackendError::UserErrors(
            errors.into_iter().map(Into::into).collect(),
        ))
    }
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

pub fn validate_options(options: &DeployOptions) -> Result<DeploySelection, DeployError> {
    options
        .selection
        .clone()
        .ok_or(DeployError::MissingSelection)
}

pub fn artifacts_from_build(report: &BuildReport) -> Result<Vec<Artifact>, DeployError> {
    if report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.level == "error")
    {
        return Err(DeployError::Validation(
            "app build did not succeed; fix extension errors before deploy".into(),
        ));
    }
    report
        .artifacts
        .iter()
        .map(|artifact| {
            let bytes = fs::read(&artifact.path).map_err(|error| {
                DeployError::Validation(format!(
                    "artifact {} is unavailable: {error}",
                    artifact.path.display()
                ))
            })?;
            let mut hasher = DefaultHasher::new();
            bytes.hash(&mut hasher);
            Ok(Artifact {
                path: artifact.path.clone(),
                bytes,
                digest: format!("hash:{:016x}", hasher.finish()),
            })
        })
        .collect()
}

/// Requires the build layer to provide one already-complete Shopify source
/// archive. Combining independently built extension files here would produce an
/// invalid archive and was the source of the old abstraction's ambiguity.
pub fn complete_source_from_build(report: &BuildReport) -> Result<CompleteSource, DeployError> {
    let mut artifacts = artifacts_from_build(report)?;
    if artifacts.len() != 1 {
        return Err(DeployError::Validation(format!(
            "deploy requires exactly one complete source archive, but the build produced {} artifacts",
            artifacts.len()
        )));
    }
    let artifact = artifacts.remove(0);
    Ok(CompleteSource {
        path: artifact.path,
        bytes: artifact.bytes,
        digest: artifact.digest,
    })
}

pub async fn deploy<B: DeployBackend>(
    backend: &B,
    options: &DeployOptions,
    build: &BuildReport,
    cancellation: &Cancellation,
) -> Result<DeployReport, DeployError> {
    let selection = validate_options(options)?;
    let source = complete_source_from_build(build)?;
    let source_path = source.path.display().to_string();
    let source_size = u64::try_from(source.bytes.len()).unwrap_or(u64::MAX);
    if source_size > options.upload_policy.max_bytes {
        return Err(DeployError::Validation(format!(
            "complete source is {source_size} bytes; maximum upload size is {} bytes",
            options.upload_policy.max_bytes
        )));
    }
    if options.dry_run {
        return Ok(DeployReport {
            selection,
            dry_run: true,
            version_id: None,
            uploaded: vec![source_path],
            released: false,
            progress: Vec::new(),
            warnings: vec![
                "dry-run: no upload URL was requested and no version was created".into(),
            ],
        });
    }
    cancelled(cancellation)?;

    // Shopify CLI 4.6.1 ordering: URL -> complete PUT -> version -> release.
    let upload = backend
        .request_source_upload(&selection)
        .await
        .map_err(|error| map_backend(DeployOperation::RequestSourceUpload, error))?;
    cancelled(cancellation)?;

    let mut progress_events = Vec::new();
    let mut record = |event| progress_events.push(event);
    backend
        .put_complete_source(
            &upload,
            &source,
            &options.upload_policy,
            &mut record,
            cancellation,
        )
        .await
        .map_err(|error| {
            if cancellation.is_cancelled() {
                DeployError::Cancelled
            } else {
                DeployError::Upload {
                    path: source_path.clone(),
                    message: backend_message(error),
                }
            }
        })?;
    cancelled(cancellation)?;

    let version = backend
        .create_version(CreateVersionRequest {
            app_id: &selection.app,
            source_url: upload.source_url(),
            metadata: &options.metadata,
        })
        .await
        .map_err(|error| map_backend(DeployOperation::CreateVersion, error))?;

    let mut released = false;
    if options.release {
        if let Err(error) = backend.create_release(&selection.app, &version.id).await {
            let (message, user_errors) = match error {
                BackendError::UserErrors(errors) => (
                    errors
                        .iter()
                        .map(|error| error.message.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    errors,
                ),
                other => (backend_message(other), Vec::new()),
            };
            return Err(DeployError::Release {
                version,
                message,
                user_errors,
            });
        }
        released = true;
    }

    Ok(DeployReport {
        selection,
        dry_run: false,
        version_id: Some(version.id),
        uploaded: vec![source_path],
        released,
        progress: progress_events,
        warnings: Vec::new(),
    })
}

fn cancelled(cancellation: &Cancellation) -> Result<(), DeployError> {
    if cancellation.is_cancelled() {
        Err(DeployError::Cancelled)
    } else {
        Ok(())
    }
}

fn map_backend(operation: DeployOperation, error: BackendError) -> DeployError {
    match error {
        BackendError::UserErrors(errors) => DeployError::UserErrors { operation, errors },
        other => DeployError::Backend {
            operation,
            message: backend_message(other),
        },
    }
}

fn backend_message(error: BackendError) -> String {
    match error {
        BackendError::UserErrors(errors) => errors
            .into_iter()
            .map(|error| error.message)
            .collect::<Vec<_>>()
            .join(", "),
        BackendError::Transport(message) | BackendError::Protocol(message) => message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };

    static BUILD_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn build_with(contents: &[&[u8]]) -> BuildReport {
        let sequence = BUILD_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let artifacts = contents
            .iter()
            .enumerate()
            .map(|(index, bytes)| {
                let path = std::env::temp_dir().join(format!(
                    "cfy-deploy-test-{}-{sequence}-{index}.br",
                    std::process::id()
                ));
                std::fs::write(&path, bytes).unwrap();
                cfy_build::Artifact {
                    extension: format!("source-{index}"),
                    path,
                }
            })
            .collect();
        BuildReport {
            mode: "clean".into(),
            artifacts,
            diagnostics: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn options(release: bool) -> DeployOptions {
        DeployOptions {
            selection: Some(DeploySelection {
                app: "gid://shopify/App/1".into(),
                environment: "42".into(),
            }),
            non_interactive: true,
            dry_run: false,
            release,
            metadata: VersionMetadata {
                version_tag: Some("1.2.3".into()),
                message: Some("ship it".into()),
                source_control_url: Some("https://example.test/commit/abc".into()),
            },
            upload_policy: SourceUploadPolicy::default(),
        }
    }

    struct FakeBackend {
        calls: Arc<Mutex<Vec<String>>>,
        fail: Option<DeployOperation>,
        user_failure: bool,
        fail_upload: bool,
    }

    impl FakeBackend {
        fn result(&self, operation: DeployOperation) -> Result<(), BackendError> {
            if self.fail == Some(operation) {
                if self.user_failure {
                    Err(BackendError::UserErrors(vec![UserError {
                        field: vec!["versionTag".into()],
                        message: "has already been taken".into(),
                        category: Some("INVALID".into()),
                        code: None,
                        on: None,
                    }]))
                } else {
                    Err(BackendError::Transport("offline".into()))
                }
            } else {
                Ok(())
            }
        }
    }

    #[async_trait]
    impl DeployBackend for FakeBackend {
        async fn request_source_upload(
            &self,
            _: &DeploySelection,
        ) -> Result<SignedSourceUpload, BackendError> {
            self.calls.lock().unwrap().push("request-url".into());
            self.result(DeployOperation::RequestSourceUpload)?;
            SignedSourceUpload::new(
                Url::parse("https://storage.test/source?signature=secret").unwrap(),
            )
        }

        async fn put_complete_source(
            &self,
            _: &SignedSourceUpload,
            source: &CompleteSource,
            policy: &SourceUploadPolicy,
            progress: &mut (dyn FnMut(UploadProgress) + Send),
            _: &Cancellation,
        ) -> Result<(), BackendError> {
            self.calls
                .lock()
                .unwrap()
                .push("put-complete-source".into());
            if self.fail_upload {
                return Err(BackendError::Transport("object storage unavailable".into()));
            }
            self.result(DeployOperation::RequestSourceUpload)?;
            if source.bytes.len() as u64 > policy.max_bytes {
                return Err(BackendError::Protocol("too large".into()));
            }
            progress(UploadProgress {
                path: source.path.display().to_string(),
                uploaded_bytes: source.bytes.len(),
                total_bytes: source.bytes.len(),
            });
            Ok(())
        }

        async fn create_version(
            &self,
            request: CreateVersionRequest<'_>,
        ) -> Result<CreatedVersion, BackendError> {
            self.calls.lock().unwrap().push(format!(
                "create-version:{}:{}",
                request.source_url,
                request.metadata.version_tag.as_deref().unwrap_or_default()
            ));
            self.result(DeployOperation::CreateVersion)?;
            Ok(CreatedVersion {
                id: "version-1".into(),
                version_tag: request.metadata.version_tag.clone(),
                message: request.metadata.message.clone(),
            })
        }

        async fn create_release(&self, _: &str, version_id: &str) -> Result<(), BackendError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("create-release:{version_id}"));
            self.result(DeployOperation::CreateRelease)
        }
    }

    fn fake(
        fail: Option<DeployOperation>,
        user_failure: bool,
    ) -> (FakeBackend, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            FakeBackend {
                calls: calls.clone(),
                fail,
                user_failure,
                fail_upload: false,
            },
            calls,
        )
    }

    #[tokio::test]
    async fn follows_shopify_protocol_order_and_passes_metadata() {
        let (backend, calls) = fake(None, false);
        let report = deploy(
            &backend,
            &options(true),
            &build_with(&[b"complete bundle"]),
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert!(report.released);
        assert_eq!(report.version_id.as_deref(), Some("version-1"));
        assert_eq!(report.progress[0].uploaded_bytes, 15);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                "request-url",
                "put-complete-source",
                "create-version:https://storage.test/source?signature=secret:1.2.3",
                "create-release:version-1",
            ]
        );
    }

    #[tokio::test]
    async fn upload_failure_never_creates_a_version() {
        let (mut backend, calls) = fake(None, false);
        backend.fail_upload = true;
        let error = deploy(
            &backend,
            &options(true),
            &build_with(&[b"bundle"]),
            &Cancellation::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, DeployError::Upload { .. }));
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["request-url", "put-complete-source"]
        );
    }

    #[tokio::test]
    async fn source_url_user_errors_stop_before_upload() {
        let (backend, calls) = fake(Some(DeployOperation::RequestSourceUpload), true);
        let error = deploy(
            &backend,
            &options(true),
            &build_with(&[b"bundle"]),
            &Cancellation::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            DeployError::UserErrors {
                operation: DeployOperation::RequestSourceUpload,
                ..
            }
        ));
        assert_eq!(*calls.lock().unwrap(), vec!["request-url"]);
    }

    #[tokio::test]
    async fn create_user_errors_are_distinct_from_transport_errors() {
        let (backend, _) = fake(Some(DeployOperation::CreateVersion), true);
        let error = deploy(
            &backend,
            &options(false),
            &build_with(&[b"bundle"]),
            &Cancellation::default(),
        )
        .await
        .unwrap_err();
        match error {
            DeployError::UserErrors { operation, errors } => {
                assert_eq!(operation, DeployOperation::CreateVersion);
                assert_eq!(errors[0].field, ["versionTag"]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn release_failure_carries_the_created_version() {
        let (backend, calls) = fake(Some(DeployOperation::CreateRelease), true);
        let error = deploy(
            &backend,
            &options(true),
            &build_with(&[b"bundle"]),
            &Cancellation::default(),
        )
        .await
        .unwrap_err();
        match error {
            DeployError::Release {
                version,
                user_errors,
                ..
            } => {
                assert_eq!(version.id, "version-1");
                assert_eq!(version.version_tag.as_deref(), Some("1.2.3"));
                assert_eq!(user_errors.len(), 1);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            calls.lock().unwrap().last().unwrap(),
            "create-release:version-1"
        );
    }

    #[tokio::test]
    async fn dry_run_performs_no_remote_operations() {
        let (backend, calls) = fake(None, false);
        let mut opts = options(true);
        opts.dry_run = true;
        let report = deploy(
            &backend,
            &opts,
            &build_with(&[b"bundle"]),
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert!(report.dry_run);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn oversized_source_is_rejected_before_requesting_a_url() {
        let (backend, calls) = fake(None, false);
        let mut opts = options(false);
        opts.upload_policy.max_bytes = 3;
        let error = deploy(
            &backend,
            &opts,
            &build_with(&[b"bundle"]),
            &Cancellation::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, DeployError::Validation(_)));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_multiple_partial_artifacts() {
        let error = complete_source_from_build(&build_with(&[b"one", b"two"])).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exactly one complete source archive")
        );
    }

    #[test]
    fn signed_urls_require_https_and_are_redacted() {
        assert!(SignedSourceUpload::new(Url::parse("http://storage.test/x").unwrap()).is_err());
        let upload = SignedSourceUpload::new(
            Url::parse("https://storage.test/x?signature=do-not-log").unwrap(),
        )
        .unwrap();
        let debug = format!("{upload:?}");
        assert!(!debug.contains("do-not-log"));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn upload_policy_has_bounded_exponential_delay() {
        let policy = SourceUploadPolicy {
            max_bytes: 1,
            max_retries: 2,
            base_delay_millis: 100,
            max_delay_millis: 150,
        };
        assert_eq!(policy.delay(0), Duration::from_millis(100));
        assert_eq!(policy.delay(1), Duration::from_millis(150));
        assert_eq!(policy.delay(99), Duration::from_millis(150));
        assert!(retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_status(StatusCode::BAD_GATEWAY));
        assert!(!retryable_status(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn numeric_organization_id_is_encoded_as_gid() {
        assert_eq!(
            options(false).selection.unwrap().organization_gid(),
            "gid://shopify/Organization/42"
        );
    }
}
