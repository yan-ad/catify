//! Native plugin registry and the package-manager adapter used to populate it.
//!
//! Registry inspection, precedence, linking, and removal are implemented here
//! rather than delegated to Shopify CLI. A JavaScript package manager is only
//! used for operations which intrinsically need one.

use cfy_core::{Error, ErrorKind, Result};
use cfy_process::{OutputMode, ProcessOutput, ProcessSpec, Supervisor};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
};

const REGISTRY_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Installed,
    Linked,
}

fn reject_secret_argument(value: &str) -> Result<()> {
    let lower = value.to_ascii_lowercase();
    let contains_secret_parameter = ["token=", "access_token=", "authtoken=", "_authtoken="]
        .iter()
        .any(|marker| lower.contains(marker));
    let contains_url_credentials = value.find("://").is_some_and(|scheme| {
        let authority = &value[scheme + 3..];
        let authority = authority.split(['/', '?', '#']).next().unwrap_or_default();
        authority.contains('@')
    });
    if contains_secret_parameter || contains_url_credentials {
        return Err(Error::invalid_input(
            "plugin source contains credentials; configure authentication in the package manager instead of passing secrets in argv",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginRecord {
    pub name: String,
    pub source: String,
    pub kind: PluginKind,
    pub path: PathBuf,
    pub version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RegistryFile {
    version: u32,
    plugins: Vec<PluginRecord>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        Self {
            version: REGISTRY_VERSION,
            plugins: Vec::new(),
        }
    }
}

/// Atomic JSON-backed storage. The root is entirely configurable for callers
/// and tests; no process-global environment is consulted.
#[derive(Clone, Debug)]
pub struct PluginRegistry {
    root: PathBuf,
}

impl PluginRegistry {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn registry_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }

    #[must_use]
    pub fn artifacts_path(&self) -> PathBuf {
        self.root.join("installed")
    }

    pub fn all(&self) -> Result<Vec<PluginRecord>> {
        let mut records = self.load()?.plugins;
        sort_records(&mut records);
        Ok(records)
    }

    /// Returns effective records, with links shadowing installed copies of the
    /// same package without destroying those installed records.
    pub fn resolved(&self) -> Result<Vec<PluginRecord>> {
        let mut effective = BTreeMap::<String, PluginRecord>::new();
        for record in self.load()?.plugins {
            match effective.get(&record.name) {
                Some(existing) if existing.kind == PluginKind::Linked => {}
                _ => {
                    effective.insert(record.name.clone(), record);
                }
            }
        }
        Ok(effective.into_values().collect())
    }

    pub fn find(&self, name: &str) -> Result<Option<PluginRecord>> {
        Ok(self.resolved()?.into_iter().find(|item| item.name == name))
    }

    pub fn upsert(&self, record: PluginRecord) -> Result<()> {
        validate_record(&record)?;
        let mut file = self.load()?;
        file.plugins
            .retain(|old| old.name != record.name || old.kind != record.kind);
        file.plugins.push(record);
        sort_records(&mut file.plugins);
        self.store(&file)
    }

    pub fn remove_kind(&self, name: &str, kind: PluginKind) -> Result<Option<PluginRecord>> {
        let mut file = self.load()?;
        let removed = file
            .plugins
            .iter()
            .position(|item| item.name == name && item.kind == kind)
            .map(|index| file.plugins.remove(index));
        if removed.is_some() {
            self.store(&file)?;
        }
        Ok(removed)
    }

    pub fn clear(&self) -> Result<()> {
        let path = self.registry_path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("could not remove plugin registry", error)),
        }
    }

    fn load(&self) -> Result<RegistryFile> {
        let path = self.registry_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RegistryFile::default());
            }
            Err(error) => return Err(io_error("could not read plugin registry", error)),
        };
        let file: RegistryFile = serde_json::from_slice(&bytes).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("plugin registry `{}` is corrupt JSON", path.display()),
                error,
            )
        })?;
        if file.version != REGISTRY_VERSION {
            return Err(Error::config(format!(
                "plugin registry `{}` has unsupported version {} (expected {REGISTRY_VERSION})",
                path.display(),
                file.version
            )));
        }
        for record in &file.plugins {
            validate_record(record)?;
        }
        Ok(file)
    }

    fn store(&self, file: &RegistryFile) -> Result<()> {
        fs::create_dir_all(&self.root)
            .map_err(|error| io_error("could not create plugin registry directory", error))?;
        let bytes = serde_json::to_vec_pretty(file).map_err(|error| {
            Error::with_source(ErrorKind::Config, "could not encode plugin registry", error)
        })?;
        let path = self.registry_path();
        cfy_config::write_atomic(&path, &bytes)
            .map_err(|error| io_error("could not atomically write plugin registry", error))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageManagerConfig {
    pub executable: PathBuf,
}

impl Default for PackageManagerConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("npm"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CommandResult {
    pub program: String,
    pub arguments: Vec<String>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub cancelled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MutationResult {
    pub action: String,
    pub plugin: Option<PluginRecord>,
    pub process: Option<CommandResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResetResult {
    pub removed_registry: bool,
    pub removed_artifacts: bool,
    pub reinstalled: Vec<MutationResult>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkOptions {
    pub install_dependencies: bool,
    pub verbose: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InstallOptions {
    pub force: bool,
    pub silent: bool,
    pub verbose: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UpdateOptions {
    pub verbose: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResetOptions {
    pub hard: bool,
    pub reinstall: bool,
}

#[derive(Clone)]
pub struct PluginService {
    registry: PluginRegistry,
    package_manager: PackageManagerConfig,
    supervisor: Supervisor,
}

impl PluginService {
    #[must_use]
    pub fn new(
        root: impl Into<PathBuf>,
        package_manager: PackageManagerConfig,
        supervisor: Supervisor,
    ) -> Self {
        Self {
            registry: PluginRegistry::new(root),
            package_manager,
            supervisor,
        }
    }

    #[must_use]
    pub fn registry(&self) -> &PluginRegistry {
        &self.registry
    }

    pub fn list(&self) -> Result<Vec<PluginRecord>> {
        self.registry.resolved()
    }

    /// Installs a package. `add` is an exact service-layer alias.
    pub async fn install(&self, source: &str) -> Result<MutationResult> {
        self.install_with_options(source, InstallOptions::default())
            .await
    }

    pub async fn install_with_options(
        &self,
        source: &str,
        options: InstallOptions,
    ) -> Result<MutationResult> {
        reject_secret_argument(source)?;
        let name_hint = package_name_from_source(source)?;
        let prefix = self.registry.artifacts_path().join(path_key(&name_hint));
        fs::create_dir_all(&prefix)
            .map_err(|error| io_error("could not create plugin installation directory", error))?;
        let mut args = vec![
            "install".into(),
            "--prefix".into(),
            prefix.to_string_lossy().into_owned(),
            "--no-save".into(),
        ];
        if options.force {
            args.push("--force".into());
        }
        if options.silent {
            args.push("--silent".into());
        } else if options.verbose {
            args.extend(["--loglevel".into(), "verbose".into()]);
        }
        args.push(source.into());
        let process = self.run_package_manager(args, None, options.silent).await?;
        let mut plugin = None;
        if process.exit_code == Some(0) {
            let manifest_path = prefix.join("node_modules").join(&name_hint);
            let manifest = read_manifest_if_present(&manifest_path)?;
            let name = manifest
                .as_ref()
                .map_or_else(|| name_hint.clone(), |item| item.name.clone());
            let record = PluginRecord {
                name,
                source: source.into(),
                kind: PluginKind::Installed,
                path: manifest_path,
                version: manifest.and_then(|item| item.version),
            };
            self.registry.upsert(record.clone())?;
            plugin = Some(record);
        }
        Ok(MutationResult {
            action: "install".into(),
            plugin,
            process: Some(process),
        })
    }

    pub async fn add(&self, source: &str) -> Result<MutationResult> {
        self.install(source).await
    }

    pub async fn add_with_options(
        &self,
        source: &str,
        options: InstallOptions,
    ) -> Result<MutationResult> {
        self.install_with_options(source, options).await
    }

    pub async fn link(
        &self,
        path: impl AsRef<Path>,
        options: LinkOptions,
    ) -> Result<MutationResult> {
        let path = fs::canonicalize(path.as_ref()).map_err(|error| {
            Error::with_source(
                ErrorKind::InvalidInput,
                format!(
                    "could not resolve plugin link `{}`",
                    path.as_ref().display()
                ),
                error,
            )
        })?;
        if !path.is_dir() {
            return Err(Error::invalid_input(
                "plugin link must point to a directory",
            ));
        }
        let manifest = read_manifest(&path)?;
        let process = if options.install_dependencies {
            Some(
                self.run_package_manager(
                    {
                        let mut arguments = vec![
                            "install".into(),
                            "--prefix".into(),
                            path.to_string_lossy().into_owned(),
                        ];
                        if options.verbose {
                            arguments.extend(["--loglevel".into(), "verbose".into()]);
                        }
                        arguments
                    },
                    Some(&path),
                    false,
                )
                .await?,
            )
        } else {
            None
        };
        if process.as_ref().is_some_and(|run| run.exit_code != Some(0)) {
            return Ok(MutationResult {
                action: "link".into(),
                plugin: None,
                process,
            });
        }
        let record = PluginRecord {
            name: manifest.name,
            source: path.to_string_lossy().into_owned(),
            kind: PluginKind::Linked,
            path,
            version: manifest.version,
        };
        self.registry.upsert(record.clone())?;
        Ok(MutationResult {
            action: "link".into(),
            plugin: Some(record),
            process,
        })
    }

    /// With no selectors, inspect behaves as if `.` was supplied.
    pub fn inspect(&self, selectors: &[String]) -> Result<Vec<PluginRecord>> {
        let defaults;
        let selectors = if selectors.is_empty() {
            defaults = vec![".".to_owned()];
            &defaults
        } else {
            selectors
        };
        let mut found = Vec::new();
        let mut seen = HashSet::new();
        for selector in selectors {
            let record = if selector == "." || Path::new(selector).is_dir() {
                let path = fs::canonicalize(selector).map_err(|error| {
                    Error::with_source(
                        ErrorKind::InvalidInput,
                        format!("could not inspect plugin path `{selector}`"),
                        error,
                    )
                })?;
                let manifest = read_manifest(&path)?;
                self.registry.find(&manifest.name)?.unwrap_or(PluginRecord {
                    name: manifest.name,
                    source: path.to_string_lossy().into_owned(),
                    kind: PluginKind::Linked,
                    path,
                    version: manifest.version,
                })
            } else {
                self.registry.find(selector)?.ok_or_else(|| {
                    Error::invalid_input(format!("plugin `{selector}` is not registered"))
                })?
            };
            if seen.insert(record.name.clone()) {
                found.push(record);
            }
        }
        found.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(found)
    }

    pub fn unlink(&self, name: &str) -> Result<MutationResult> {
        self.remove_kind(name, PluginKind::Linked, "unlink")
    }

    pub fn uninstall(&self, name: &str) -> Result<MutationResult> {
        let result = self.remove_kind(name, PluginKind::Installed, "uninstall")?;
        if let Some(record) = &result.plugin {
            self.remove_installed_artifact(&record.path)?;
        }
        Ok(result)
    }

    /// Removes the effective record: a link first, otherwise an installation.
    pub fn remove(&self, name: &str) -> Result<MutationResult> {
        match self.registry.find(name)? {
            Some(record) if record.kind == PluginKind::Linked => self.unlink(name),
            Some(_) => self.uninstall(name),
            None => Ok(MutationResult {
                action: "remove".into(),
                plugin: None,
                process: None,
            }),
        }
    }

    pub async fn update(&self) -> Result<Vec<MutationResult>> {
        self.update_with_options(UpdateOptions::default()).await
    }

    pub async fn update_with_options(&self, options: UpdateOptions) -> Result<Vec<MutationResult>> {
        let installed = self
            .registry
            .all()?
            .into_iter()
            .filter(|record| record.kind == PluginKind::Installed)
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(installed.len());
        for record in installed {
            let prefix = installation_prefix(&self.registry, &record)?;
            let process = self
                .run_package_manager(
                    {
                        let mut arguments = vec![
                            "update".into(),
                            "--prefix".into(),
                            prefix.to_string_lossy().into_owned(),
                            record.name.clone(),
                        ];
                        if options.verbose {
                            arguments.extend(["--loglevel".into(), "verbose".into()]);
                        }
                        arguments
                    },
                    None,
                    false,
                )
                .await?;
            results.push(MutationResult {
                action: "update".into(),
                plugin: Some(record),
                process: Some(process),
            });
        }
        Ok(results)
    }

    pub async fn reset(&self, options: ResetOptions) -> Result<ResetResult> {
        let installed = self
            .registry
            .all()?
            .into_iter()
            .filter(|record| record.kind == PluginKind::Installed)
            .collect::<Vec<_>>();
        let existed = self.registry.registry_path().exists();
        self.registry.clear()?;
        let remove_artifacts = options.hard || options.reinstall;
        if remove_artifacts {
            remove_confined_tree(self.registry.root(), &self.registry.artifacts_path())?;
        }
        let mut reinstalled = Vec::new();
        if options.reinstall {
            for record in installed {
                reinstalled.push(self.install(&record.source).await?);
            }
        }
        Ok(ResetResult {
            removed_registry: existed,
            removed_artifacts: remove_artifacts,
            reinstalled,
        })
    }

    fn remove_kind(&self, name: &str, kind: PluginKind, action: &str) -> Result<MutationResult> {
        Ok(MutationResult {
            action: action.into(),
            plugin: self.registry.remove_kind(name, kind)?,
            process: None,
        })
    }

    fn remove_installed_artifact(&self, plugin_path: &Path) -> Result<()> {
        let prefix = plugin_path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| Error::config("installed plugin path has no installation prefix"))?;
        remove_confined_tree(self.registry.root(), prefix)
    }

    async fn run_package_manager(
        &self,
        arguments: Vec<String>,
        current_dir: Option<&Path>,
        silent: bool,
    ) -> Result<CommandResult> {
        reject_shopify(&self.package_manager.executable)?;
        let mut spec = ProcessSpec::new(self.package_manager.executable.to_string_lossy())
            .args(arguments.clone())
            .output(if silent {
                OutputMode::Capture
            } else {
                OutputMode::CaptureAndStream
            });
        if let Some(directory) = current_dir {
            spec = spec.current_dir(directory);
        }
        let output = self
            .supervisor
            .spawn(spec)?
            .wait_with_signal_forwarding()
            .await?;
        Ok(command_result(
            &self.package_manager.executable,
            &arguments,
            output,
        ))
    }
}

#[derive(Deserialize)]
struct PackageManifest {
    name: String,
    #[serde(default)]
    version: Option<String>,
}

fn read_manifest(path: &Path) -> Result<PackageManifest> {
    let manifest_path = path.join("package.json");
    let bytes = fs::read(&manifest_path).map_err(|error| {
        Error::with_source(
            ErrorKind::InvalidInput,
            format!("plugin `{}` has no readable package.json", path.display()),
            error,
        )
    })?;
    let manifest: PackageManifest = serde_json::from_slice(&bytes).map_err(|error| {
        Error::with_source(
            ErrorKind::InvalidInput,
            format!(
                "plugin manifest `{}` is invalid JSON",
                manifest_path.display()
            ),
            error,
        )
    })?;
    if manifest.name.trim().is_empty() {
        return Err(Error::invalid_input(format!(
            "plugin manifest `{}` has an empty name",
            manifest_path.display()
        )));
    }
    Ok(manifest)
}

fn read_manifest_if_present(path: &Path) -> Result<Option<PackageManifest>> {
    if path.join("package.json").exists() {
        read_manifest(path).map(Some)
    } else {
        Ok(None)
    }
}

fn validate_record(record: &PluginRecord) -> Result<()> {
    if record.name.trim().is_empty() || record.source.trim().is_empty() {
        return Err(Error::config(
            "plugin registry contains an empty name or source",
        ));
    }
    if record.path.as_os_str().is_empty() {
        return Err(Error::config("plugin registry contains an empty path"));
    }
    Ok(())
}

fn sort_records(records: &mut [PluginRecord]) {
    records.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.kind.cmp(&right.kind))
            .then(left.path.cmp(&right.path))
    });
}

fn package_name_from_source(source: &str) -> Result<String> {
    let source = source.trim();
    if source.is_empty() {
        return Err(Error::invalid_input(
            "plugin package source cannot be empty",
        ));
    }
    let without_fragment = source.split(['#', '?']).next().unwrap_or(source);
    let candidate = if without_fragment.starts_with('@') {
        let slash = without_fragment.find('/').ok_or_else(|| {
            Error::invalid_input("scoped plugin package must include a package name")
        })?;
        let suffix = &without_fragment[slash + 1..];
        let end = suffix
            .find('@')
            .map_or(without_fragment.len(), |at| slash + 1 + at);
        &without_fragment[..end]
    } else if without_fragment.contains('/') || without_fragment.contains('\\') {
        Path::new(without_fragment)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(without_fragment)
            .trim_end_matches(".git")
    } else {
        without_fragment
            .split('@')
            .next()
            .unwrap_or(without_fragment)
    };
    if candidate.is_empty() || candidate == "." || candidate == ".." {
        return Err(Error::invalid_input(format!(
            "could not determine plugin package name from `{source}`"
        )));
    }
    Ok(candidate.to_owned())
}

fn path_key(name: &str) -> String {
    name.bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                vec![char::from(byte)]
            } else {
                format!("%{byte:02X}").chars().collect()
            }
        })
        .collect()
}

fn installation_prefix(registry: &PluginRegistry, record: &PluginRecord) -> Result<PathBuf> {
    let expected = registry.artifacts_path().join(path_key(&record.name));
    let actual = record
        .path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| Error::config("installed plugin path has no installation prefix"))?;
    if actual != expected {
        return Err(Error::config(format!(
            "installed plugin `{}` has an unsafe path outside its managed prefix",
            record.name
        )));
    }
    Ok(expected)
}

fn remove_confined_tree(root: &Path, target: &Path) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let root = fs::canonicalize(root)
        .map_err(|error| io_error("could not resolve plugin root for safe removal", error))?;
    let target = fs::canonicalize(target)
        .map_err(|error| io_error("could not resolve plugin artifact for safe removal", error))?;
    if target == root || !target.starts_with(&root) {
        return Err(Error::config(format!(
            "refusing to remove plugin artifact `{}` outside registry root `{}`",
            target.display(),
            root.display()
        )));
    }
    fs::remove_dir_all(&target)
        .map_err(|error| io_error("could not remove plugin artifacts", error))
}

fn reject_shopify(executable: &Path) -> Result<()> {
    let name = executable
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if name.eq_ignore_ascii_case("shopify") {
        return Err(Error::config(
            "Shopify CLI cannot be configured as the plugin package manager",
        ));
    }
    Ok(())
}

fn command_result(program: &Path, arguments: &[String], output: ProcessOutput) -> CommandResult {
    CommandResult {
        program: redact(program.to_string_lossy().as_ref()),
        arguments: arguments.iter().map(|arg| redact(arg)).collect(),
        exit_code: output.exit_code(),
        stdout: redact(&String::from_utf8_lossy(&output.stdout)),
        stderr: redact(&String::from_utf8_lossy(&output.stderr)),
        cancelled: output.cancelled,
    }
}

/// Removes common credentials from diagnostics and JSON-ready process output.
/// Raw arguments are only retained until the child has been spawned.
fn redact(value: &str) -> String {
    let mut result = value.to_owned();
    for marker in ["token=", "access_token=", "authToken=", "_authToken="] {
        let mut offset = 0;
        while let Some(index) = result[offset..].find(marker) {
            let start = offset + index + marker.len();
            let end = result[start..]
                .find(|character: char| character.is_whitespace() || matches!(character, '&' | ';'))
                .map_or(result.len(), |length| start + length);
            result.replace_range(start..end, "[REDACTED]");
            offset = start + "[REDACTED]".len();
        }
    }
    // Redact URL userinfo (including npm tokens represented as a username).
    if let Some(scheme) = result.find("://") {
        let authority = scheme + 3;
        let authority_end = result[authority..]
            .find(['/', ' ', '\n', '\r'])
            .map_or(result.len(), |length| authority + length);
        if let Some(at) = result[authority..authority_end].rfind('@') {
            result.replace_range(authority..authority + at, "[REDACTED]");
        }
    }
    result
}

fn io_error(message: &str, error: std::io::Error) -> Error {
    Error::with_source(ErrorKind::Config, message, error)
}

/// Returns true when a lexical relative path contains no parent/root escape.
#[must_use]
pub fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}
