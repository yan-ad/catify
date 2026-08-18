//! App deploy orchestration and release safety contracts.

use async_trait::async_trait;
use cfy_build::BuildReport;
use cfy_core::{Cancellation, Error, ErrorKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
};
use thiserror::Error as ThisError;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeploySelection {
    pub app: String,
    pub environment: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeployOptions {
    pub selection: Option<DeploySelection>,
    pub non_interactive: bool,
    pub dry_run: bool,
    pub release: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub digest: String,
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
    #[error("app/environment selection is required in non-interactive mode")]
    MissingSelection,
    #[error("deploy validation failed: {0}")]
    Validation(String),
    #[error(
        "upload failed for {path}: {message}. Retry the deploy; the generated version can be reused safely"
    )]
    Upload { path: String, message: String },
    #[error(
        "release failed for version {version_id}: {message}. The version remains available for retry"
    )]
    Release { version_id: String, message: String },
    #[error("deploy was cancelled")]
    Cancelled,
}

impl From<DeployError> for Error {
    fn from(error: DeployError) -> Self {
        let kind = match error {
            DeployError::MissingSelection | DeployError::Validation(_) => ErrorKind::Config,
            DeployError::Cancelled => ErrorKind::Process,
            DeployError::Upload { .. } | DeployError::Release { .. } => ErrorKind::Api,
        };
        Error::new(kind, error.to_string())
    }
}

#[async_trait]
pub trait DeployBackend: Send + Sync {
    async fn create_version(
        &self,
        selection: &DeploySelection,
    ) -> std::result::Result<String, String>;
    async fn upload_artifact(
        &self,
        version_id: &str,
        artifact: &Artifact,
        progress: &mut (dyn FnMut(UploadProgress) + Send),
        cancellation: &Cancellation,
    ) -> std::result::Result<(), String>;
    async fn release_version(&self, version_id: &str) -> std::result::Result<(), String>;
}

pub fn validate_options(
    options: &DeployOptions,
) -> std::result::Result<DeploySelection, DeployError> {
    options
        .selection
        .clone()
        .ok_or(DeployError::MissingSelection)
}

pub fn artifacts_from_build(
    report: &BuildReport,
) -> std::result::Result<Vec<Artifact>, DeployError> {
    if report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.level == "error")
    {
        return Err(DeployError::Validation(
            "app build did not succeed; run `cfy app build` and fix extension errors before deploy"
                .into(),
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
                digest: format!("fnv:{:016x}", hasher.finish()),
            })
        })
        .collect()
}

pub async fn deploy<B: DeployBackend>(
    backend: &B,
    options: &DeployOptions,
    build: &BuildReport,
    cancellation: &Cancellation,
) -> std::result::Result<DeployReport, DeployError> {
    let selection = validate_options(options)?;
    let artifacts = artifacts_from_build(build)?;
    if options.dry_run {
        return Ok(DeployReport {
            selection,
            dry_run: true,
            version_id: None,
            uploaded: artifacts
                .iter()
                .map(|a| a.path.display().to_string())
                .collect(),
            released: false,
            progress: Vec::new(),
            warnings: vec!["dry-run: no version was created and no artifact was uploaded".into()],
        });
    }
    if cancellation.is_cancelled() {
        return Err(DeployError::Cancelled);
    }
    let version_id = backend
        .create_version(&selection)
        .await
        .map_err(|message| DeployError::Upload {
            path: "version".into(),
            message,
        })?;
    let mut uploaded = Vec::new();
    let mut progress = Vec::new();
    for artifact in &artifacts {
        let mut record = |event: UploadProgress| progress.push(event);
        backend
            .upload_artifact(version_id.as_str(), artifact, &mut record, cancellation)
            .await
            .map_err(|message| DeployError::Upload {
                path: artifact.path.display().to_string(),
                message,
            })?;
        uploaded.push(artifact.path.display().to_string());
    }
    let mut released = false;
    if options.release {
        backend
            .release_version(&version_id)
            .await
            .map_err(|message| DeployError::Release {
                version_id: version_id.clone(),
                message,
            })?;
        released = true;
    }
    Ok(DeployReport {
        selection,
        dry_run: false,
        version_id: Some(version_id),
        uploaded,
        released,
        progress,
        warnings: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn build() -> BuildReport {
        let path = std::env::temp_dir().join(format!("cfy-deploy-test-{}.js", std::process::id()));
        std::fs::write(&path, b"bundle").unwrap();
        BuildReport {
            mode: "clean".into(),
            artifacts: vec![cfy_build::Artifact {
                extension: "demo".into(),
                path,
            }],
            diagnostics: Vec::new(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn non_interactive_requires_selection() {
        let error = validate_options(&DeployOptions {
            selection: None,
            non_interactive: true,
            dry_run: false,
            release: false,
        })
        .unwrap_err();
        assert!(matches!(error, DeployError::MissingSelection));
    }

    #[test]
    fn dry_run_never_creates_version() {
        let artifacts = artifacts_from_build(&build()).unwrap();
        assert!(artifacts[0].digest.starts_with("fnv:"));
    }

    struct FakeBackend {
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl DeployBackend for FakeBackend {
        async fn create_version(&self, _: &DeploySelection) -> std::result::Result<String, String> {
            self.calls.lock().unwrap().push("create".into());
            Ok("version-1".into())
        }

        async fn upload_artifact(
            &self,
            version_id: &str,
            artifact: &Artifact,
            progress: &mut (dyn FnMut(UploadProgress) + Send),
            _: &Cancellation,
        ) -> std::result::Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("upload:{version_id}"));
            progress(UploadProgress {
                path: artifact.path.display().to_string(),
                uploaded_bytes: artifact.bytes.len(),
                total_bytes: artifact.bytes.len(),
            });
            Ok(())
        }

        async fn release_version(&self, version_id: &str) -> std::result::Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("release:{version_id}"));
            Ok(())
        }
    }

    #[tokio::test]
    async fn deploy_reports_progress_and_releases() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let report = deploy(
            &FakeBackend {
                calls: calls.clone(),
            },
            &DeployOptions {
                selection: Some(DeploySelection {
                    app: "demo".into(),
                    environment: "production".into(),
                }),
                non_interactive: true,
                dry_run: false,
                release: true,
            },
            &build(),
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert!(report.released);
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["create", "upload:version-1", "release:version-1"]
        );
    }
}
