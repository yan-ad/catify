use cfy_config::graph::{AppConfigGraph, ExtensionConfig};
use cfy_core::{Cancellation, Error, ErrorKind, Result};
use cfy_extension_adapter::{
    Adapter, BuildJob, BuildRequest, BuildResponse, Parallelism, build_all,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

const CACHE_FILE: &str = ".catify/build-cache.json";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BuildMode {
    #[default]
    Incremental,
    Clean,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuildOptions {
    pub mode: BuildMode,
    pub parallelism: Parallelism,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            mode: BuildMode::Incremental,
            parallelism: Parallelism {
                max_jobs: 1,
                max_memory_mb: 512,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildInput {
    pub extension: ExtensionConfig,
    pub output_dir: PathBuf,
    pub memory_mb: u32,
    pub configuration: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub extension: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub extension: String,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildReport {
    pub mode: String,
    pub skipped: Vec<String>,
    pub artifacts: Vec<Artifact>,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Cache {
    fingerprints: BTreeMap<String, String>,
}

pub struct BuildPipeline<'a> {
    adapter: Option<&'a Adapter>,
    supervisor: &'a cfy_process::Supervisor,
    cancel: Cancellation,
}

impl<'a> BuildPipeline<'a> {
    pub fn new(adapter: Option<&'a Adapter>, supervisor: &'a cfy_process::Supervisor) -> Self {
        Self {
            adapter,
            supervisor,
            cancel: Cancellation::default(),
        }
    }
    pub fn cancellation_token(&self) -> Cancellation {
        self.cancel.clone()
    }

    pub async fn run(
        &self,
        graph: &AppConfigGraph,
        inputs: Vec<BuildInput>,
        options: BuildOptions,
    ) -> Result<BuildReport> {
        options.parallelism.validate()?;
        let cache_path = graph.root.join(CACHE_FILE);
        if options.mode == BuildMode::Clean {
            clean_outputs(&inputs)?;
        }
        let mut cache = load_cache(&cache_path)?;
        let mut jobs = Vec::new();
        let mut skipped = Vec::new();
        for input in inputs {
            if self.cancel.is_cancelled() {
                return Err(Error::new(
                    ErrorKind::Process,
                    "app build cancelled before scheduling extensions",
                ));
            }
            let key = extension_key(&input.extension);
            let fingerprint = fingerprint(&input.extension)?;
            if options.mode == BuildMode::Incremental
                && cache.fingerprints.get(&key) == Some(&fingerprint)
                && input.output_dir.exists()
            {
                skipped.push(key);
                continue;
            }
            let extension_type = input.extension.extension_type.clone().ok_or_else(|| {
                Error::new(
                    ErrorKind::Config,
                    format!(
                        "extension `{key}` has no type; add `type` to {}",
                        input.extension.path.display()
                    ),
                )
            })?;
            let mut request = BuildRequest::new(
                extension_type,
                &input.extension.directory,
                &input.output_dir,
            );
            request.configuration = input.configuration;
            jobs.push((
                key,
                fingerprint,
                BuildJob {
                    request,
                    memory_mb: input.memory_mb,
                },
            ));
        }
        let responses = if jobs.is_empty() {
            Vec::new()
        } else {
            let adapter = self.adapter.ok_or_else(|| {
                Error::config(
                    "this app contains extensions; set CFY_EXTENSION_ADAPTER to a compatible build adapter executable",
                )
            })?;
            build_all(
                self.supervisor,
                adapter,
                jobs.iter().map(|(_, _, job)| job.clone()).collect(),
                options.parallelism,
            )
            .await
            .map_err(|error| {
                Error::with_source(
                    ErrorKind::Process,
                    "app extension build pipeline failed",
                    error,
                )
            })?
        };
        let mut report = BuildReport {
            mode: if options.mode == BuildMode::Clean {
                "clean".into()
            } else {
                "incremental".into()
            },
            skipped,
            artifacts: Vec::new(),
            diagnostics: Vec::new(),
        };
        for ((key, fingerprint, _), response) in jobs.into_iter().zip(responses) {
            validate_response(&key, &response, &graph.root)?;
            for artifact in response.artifacts {
                report.artifacts.push(Artifact {
                    extension: key.clone(),
                    path: artifact,
                });
            }
            for diagnostic in response.diagnostics {
                report.diagnostics.push(Diagnostic {
                    extension: key.clone(),
                    level: format!("{:?}", diagnostic.level).to_lowercase(),
                    message: diagnostic.message,
                });
            }
            cache.fingerprints.insert(key, fingerprint);
        }
        report.artifacts.sort_by(|a, b| a.path.cmp(&b.path));
        report.diagnostics.sort_by(|a, b| {
            a.extension
                .cmp(&b.extension)
                .then(a.message.cmp(&b.message))
        });
        save_cache(&cache_path, &cache)?;
        Ok(report)
    }
}

fn extension_key(extension: &ExtensionConfig) -> String {
    extension
        .handle
        .clone()
        .or_else(|| extension.name.clone())
        .unwrap_or_else(|| extension.path.display().to_string())
}
fn fingerprint(extension: &ExtensionConfig) -> Result<String> {
    let metadata = fs::metadata(&extension.path).map_err(|e| {
        Error::with_source(
            ErrorKind::Config,
            format!(
                "failed to fingerprint extension {}",
                extension.path.display()
            ),
            e,
        )
    })?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    Ok(format!(
        "{}:{}:{}",
        extension.path.display(),
        metadata.len(),
        modified
    ))
}
fn clean_outputs(inputs: &[BuildInput]) -> Result<()> {
    for input in inputs {
        if input.output_dir.exists() {
            fs::remove_dir_all(&input.output_dir).map_err(|e| {
                Error::with_source(
                    ErrorKind::Config,
                    format!(
                        "failed to clean build output {}",
                        input.output_dir.display()
                    ),
                    e,
                )
            })?;
        }
    }
    Ok(())
}
fn validate_response(extension: &str, response: &BuildResponse, root: &Path) -> Result<()> {
    for artifact in &response.artifacts {
        let absolute = if artifact.is_absolute() {
            artifact.clone()
        } else {
            root.join(artifact)
        };
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let canonical = absolute.canonicalize().map_err(|e| {
            Error::with_source(
                ErrorKind::Config,
                format!(
                    "extension `{extension}` reported missing artifact {}",
                    artifact.display()
                ),
                e,
            )
        })?;
        if !canonical.starts_with(&canonical_root) {
            return Err(Error::new(
                ErrorKind::Config,
                format!(
                    "extension `{extension}` reported artifact outside project output root: {}",
                    artifact.display()
                ),
            ));
        }
    }
    Ok(())
}
fn load_cache(path: &Path) -> Result<Cache> {
    if !path.exists() {
        return Ok(Cache::default());
    }
    let text = fs::read_to_string(path).map_err(|e| {
        Error::with_source(
            ErrorKind::Config,
            format!("failed to read build cache {}", path.display()),
            e,
        )
    })?;
    serde_json::from_str(&text).map_err(|e| {
        Error::with_source(
            ErrorKind::Config,
            format!(
                "build cache {} is invalid; delete it and rebuild",
                path.display()
            ),
            e,
        )
    })
}
fn save_cache(path: &Path, cache: &Cache) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            Error::with_source(
                ErrorKind::Config,
                "failed to create build cache directory",
                e,
            )
        })?;
    }
    let text = serde_json::to_vec_pretty(cache)
        .map_err(|e| Error::with_source(ErrorKind::Config, "failed to encode build cache", e))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text)
        .map_err(|e| Error::with_source(ErrorKind::Config, "failed to write build cache", e))?;
    fs::rename(&temporary, path)
        .map_err(|e| Error::with_source(ErrorKind::Config, "failed to replace build cache", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    #[test]
    fn cache_roundtrip_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE);
        let cache = Cache {
            fingerprints: BTreeMap::from([("a".into(), "b".into())]),
        };
        save_cache(&path, &cache).unwrap();
        assert_eq!(load_cache(&path).unwrap().fingerprints, cache.fingerprints);
    }
    #[test]
    fn report_json_is_stable() {
        let report = BuildReport {
            mode: "incremental".into(),
            skipped: vec!["a".into()],
            artifacts: vec![Artifact {
                extension: "a".into(),
                path: PathBuf::from("dist/a.js"),
            }],
            diagnostics: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("dist/a.js"));
    }
    #[test]
    fn build_options_reject_zero_resource_limits() {
        let error = Parallelism {
            max_jobs: 0,
            max_memory_mb: 0,
        }
        .validate()
        .unwrap_err();
        assert!(error.to_string().contains("max_jobs"));
    }
    #[test]
    fn build_report_keeps_extension_attribution() {
        let report = BuildReport {
            mode: "clean".into(),
            skipped: vec![],
            artifacts: vec![],
            diagnostics: vec![Diagnostic {
                extension: "checkout-ui".into(),
                level: "error".into(),
                message: "adapter failed".into(),
            }],
        };
        assert_eq!(report.diagnostics[0].extension, "checkout-ui");
    }
    #[test]
    fn fingerprints_change_when_config_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shopify.extension.toml");
        fs::write(&path, "type='ui_extension'").unwrap();
        let mut extension = ExtensionConfig {
            path: path.clone(),
            directory: dir.path().into(),
            name: None,
            handle: None,
            uid: None,
            extension_type: Some("ui_extension".into()),
            api_version: None,
            family: cfy_config::graph::ExtensionFamily::Ui,
            raw: toml::Table::new(),
            unknown: toml::Table::new(),
        };
        let before = fingerprint(&extension).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(&path, "type='function'").unwrap();
        extension.extension_type = Some("function".into());
        let after = fingerprint(&extension).unwrap();
        assert_ne!(before, after);
        let _ = SystemTime::now();
    }

    #[tokio::test]
    async fn app_without_extensions_builds_without_an_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("shopify.app.toml");
        fs::write(&config, "client_id='client'\nname='app'\n").unwrap();
        let project =
            cfy_config::project::discover(dir.path(), Some(cfy_config::project::ProjectKind::App))
                .unwrap();
        let graph = AppConfigGraph::load_selected(&project, &config).unwrap();
        let supervisor = cfy_process::Supervisor::default();
        let report = BuildPipeline::new(None, &supervisor)
            .run(&graph, Vec::new(), BuildOptions::default())
            .await
            .unwrap();
        assert!(report.artifacts.is_empty());
        assert!(report.diagnostics.is_empty());
    }
}
