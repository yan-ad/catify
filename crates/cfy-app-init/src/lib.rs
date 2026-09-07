//! Rust-native Shopify app project scaffolding.
//!
//! Filesystem validation, template rendering, manifest updates, and rollback are
//! native Rust. Git and the selected JavaScript package manager are intrinsic
//! external engines and are run directly through `cfy-process`.

use cfy_process::{OutputMode, ProcessOutput, ProcessSpec, Supervisor};
use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;
use url::Url;

pub const REACT_ROUTER_REPOSITORY: &str =
    "https://github.com/Shopify/shopify-app-template-react-router.git";
pub const EXTENSION_ONLY_REPOSITORY: &str =
    "https://github.com/Shopify/shopify-app-template-extension-only.git";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactRouterFlavor {
    TypeScript,
    JavaScript,
}

#[must_use]
pub fn slugify(name: &str) -> String {
    let mut output = String::new();
    let mut pending_separator = false;
    for character in name.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !output.is_empty() {
                output.push('-');
            }
            pending_separator = false;
            output.push(character.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }
    output.trim_matches('-').to_owned()
}

impl ReactRouterFlavor {
    #[must_use]
    pub const fn branch(self) -> &'static str {
        match self {
            Self::TypeScript => "main-cli",
            Self::JavaScript => "javascript-cli",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTemplate {
    pub repository: String,
    pub branch: Option<String>,
    pub subpath: Option<PathBuf>,
}

impl GitTemplate {
    #[must_use]
    pub fn new(repository: impl Into<String>) -> Self {
        Self {
            repository: repository.into(),
            branch: None,
            subpath: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppTemplate {
    ReactRouter(ReactRouterFlavor),
    None,
    Custom(GitTemplate),
}

impl AppTemplate {
    #[must_use]
    pub fn git_template(&self) -> GitTemplate {
        match self {
            Self::ReactRouter(flavor) => GitTemplate {
                repository: REACT_ROUTER_REPOSITORY.to_owned(),
                branch: Some(flavor.branch().to_owned()),
                subpath: None,
            },
            Self::None => GitTemplate {
                repository: EXTENSION_ONLY_REPOSITORY.to_owned(),
                branch: None,
                subpath: None,
            },
            Self::Custom(template) => template.clone(),
        }
    }
}

/// Parses `https://github.com/owner/repo[/subpath][#branch]`.
pub fn parse_github_template_url(input: &str) -> Result<GitTemplate, AppInitError> {
    // `url` follows the URL standard and normalizes dot segments, so reject
    // traversal in the caller-provided spelling before parsing can erase it.
    let path_and_fragment = input
        .split_once("github.com/")
        .map(|(_, tail)| tail)
        .unwrap_or(input);
    let raw_path = path_and_fragment
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    if raw_path.split('/').any(|segment| {
        matches!(
            segment.to_ascii_lowercase().as_str(),
            "." | ".." | "%2e" | "%2e%2e" | ".%2e" | "%2e."
        )
    }) {
        return Err(AppInitError::InvalidTemplateUrl(input.to_owned()));
    }
    let url = Url::parse(input).map_err(|_| AppInitError::InvalidTemplateUrl(input.to_owned()))?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") || url.query().is_some() {
        return Err(AppInitError::InvalidTemplateUrl(input.to_owned()));
    }
    let segments = url
        .path_segments()
        .map(|parts| parts.filter(|part| !part.is_empty()).collect::<Vec<_>>())
        .unwrap_or_default();
    if segments.len() < 2 {
        return Err(AppInitError::InvalidTemplateUrl(input.to_owned()));
    }
    let owner = segments[0];
    let repository_name = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    if owner.is_empty() || repository_name.is_empty() {
        return Err(AppInitError::InvalidTemplateUrl(input.to_owned()));
    }
    let subpath = if segments.len() > 2 {
        let path = segments[2..].iter().collect::<PathBuf>();
        validate_relative_path(&path)
            .map_err(|_| AppInitError::InvalidTemplateSubpath(path.clone()))?;
        Some(path)
    } else {
        None
    };
    let branch = url.fragment().map(str::to_owned);
    if branch
        .as_deref()
        .is_some_and(|value| value.is_empty() || value.starts_with('-'))
    {
        return Err(AppInitError::InvalidBranch(branch.unwrap_or_default()));
    }
    Ok(GitTemplate {
        repository: format!("https://github.com/{owner}/{repository_name}.git"),
        branch,
        subpath,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Npm,
    Yarn,
    Pnpm,
    Bun,
}

impl PackageManager {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Yarn => "yarn",
            Self::Pnpm => "pnpm",
            Self::Bun => "bun",
        }
    }

    fn install_args(self) -> &'static [&'static str] {
        match self {
            Self::Npm => &["install"],
            Self::Yarn => &["install"],
            Self::Pnpm => &["install"],
            Self::Bun => &["install"],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInitRequest {
    pub parent: PathBuf,
    pub name: String,
    pub directory_name: String,
    pub template: AppTemplate,
    pub package_manager: PackageManager,
    pub install_dependencies: bool,
    pub initialize_git: bool,
    pub interactive: bool,
    pub git_executable: PathBuf,
    pub package_manager_executable: Option<PathBuf>,
}

impl AppInitRequest {
    #[must_use]
    pub fn new(parent: impl Into<PathBuf>, name: impl Into<String>, template: AppTemplate) -> Self {
        let name = name.into();
        Self {
            parent: parent.into(),
            directory_name: slugify(&name),
            name,
            template,
            package_manager: PackageManager::Npm,
            install_dependencies: true,
            initialize_git: true,
            interactive: true,
            git_executable: PathBuf::from("git"),
            package_manager_executable: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppInitReport {
    pub destination: PathBuf,
    pub repository: String,
    pub branch: Option<String>,
    pub subpath: Option<PathBuf>,
    pub package_manager: PackageManager,
    pub rendered_files: usize,
    pub copied_files: usize,
    pub package_json_updated: bool,
    pub pnpm_workspace_updated: bool,
    pub dependencies_installed: bool,
    pub git_initialized: bool,
}

#[derive(Debug, Error)]
pub enum AppInitError {
    #[error("app parent path does not exist: {0}")]
    ParentMissing(PathBuf),
    #[error("app parent path is not a directory: {0}")]
    ParentNotDirectory(PathBuf),
    #[error("app name must be one non-empty directory name, got {0:?}")]
    InvalidName(String),
    #[error("app destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("invalid GitHub template URL: {0}")]
    InvalidTemplateUrl(String),
    #[error("template repository must not be empty")]
    EmptyRepository,
    #[error("invalid template branch: {0:?}")]
    InvalidBranch(String),
    #[error("template subpath is unsafe: {0}")]
    InvalidTemplateSubpath(PathBuf),
    #[error("template subpath does not exist or is not a directory: {0}")]
    TemplateSubpathMissing(PathBuf),
    #[error("template contains an unsafe symbolic link: {0}")]
    UnsafeSymlink(PathBuf),
    #[error("path cannot be represented as UTF-8 for the process engine: {0}")]
    NonUtf8Path(PathBuf),
    #[error("filesystem operation failed at {path}: {source}")]
    FileSystem {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not render Liquid template {path}: {message}")]
    Render { path: PathBuf, message: String },
    #[error("package.json is invalid at {path}: {message}")]
    InvalidPackageJson { path: PathBuf, message: String },
    #[error("could not run {engine} while {operation}: {source}")]
    ProcessEngine {
        engine: &'static str,
        operation: &'static str,
        #[source]
        source: cfy_core::Error,
    },
    #[error("{engine} failed while {operation} (exit {exit_code:?}): {stderr}")]
    ProcessFailed {
        engine: &'static str,
        operation: &'static str,
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error(
        "initialization failed and destination rollback also failed at {path}: {source}; original error: {original}"
    )]
    Rollback {
        path: PathBuf,
        #[source]
        source: io::Error,
        original: String,
    },
}

#[derive(Default)]
struct CopyReport {
    copied: usize,
    rendered: usize,
}

pub async fn initialize(request: AppInitRequest) -> Result<AppInitReport, AppInitError> {
    initialize_with_supervisor(request, &Supervisor::default()).await
}

pub async fn initialize_with_supervisor(
    request: AppInitRequest,
    supervisor: &Supervisor,
) -> Result<AppInitReport, AppInitError> {
    let (parent, destination, template) = validate(&request)?;
    let staging = parent.join(format!(
        ".cfy-app-init-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let result = initialize_inner(&request, supervisor, &destination, &staging, template).await;
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    match result {
        Ok(report) => Ok(report),
        Err(error) => {
            if destination.exists()
                && let Err(source) = fs::remove_dir_all(&destination)
            {
                return Err(AppInitError::Rollback {
                    path: destination,
                    source,
                    original: error.to_string(),
                });
            }
            Err(error)
        }
    }
}

async fn initialize_inner(
    request: &AppInitRequest,
    supervisor: &Supervisor,
    destination: &Path,
    staging: &Path,
    template: GitTemplate,
) -> Result<AppInitReport, AppInitError> {
    let git = path_arg(&request.git_executable)?;
    let mut clone_args = vec![
        "clone".to_owned(),
        "--depth".to_owned(),
        "1".to_owned(),
        "--recurse-submodules".to_owned(),
    ];
    if let Some(branch) = &template.branch {
        clone_args.extend(["--branch".to_owned(), branch.clone()]);
    }
    if !request.interactive {
        clone_args.extend([
            "-c".to_owned(),
            "core.askPass=true".to_owned(),
            "-c".to_owned(),
            "credential.interactive=false".to_owned(),
        ]);
    }
    clone_args.extend([template.repository.clone(), path_arg(staging)?]);
    run(
        supervisor,
        &git,
        clone_args,
        None,
        request.interactive,
        "Git",
        "cloning the app template",
    )
    .await?;

    let source = template
        .subpath
        .as_ref()
        .map_or_else(|| staging.to_owned(), |path| staging.join(path));
    let metadata = fs::symlink_metadata(&source).map_err(|source_error| {
        if source_error.kind() == io::ErrorKind::NotFound {
            AppInitError::TemplateSubpathMissing(source.clone())
        } else {
            io_at(&source, source_error)
        }
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(AppInitError::TemplateSubpathMissing(source));
    }
    fs::create_dir(destination).map_err(|error| io_at(destination, error))?;
    let copy = copy_template(&source, destination, &request.name, request.package_manager)?;
    let package_json_updated = update_package_json(destination, &request.name)?;
    let pnpm_workspace_updated = if request.package_manager == PackageManager::Pnpm {
        update_pnpm_workspace(destination)?
    } else {
        false
    };

    let dependencies_installed = if request.install_dependencies {
        let executable = request
            .package_manager_executable
            .as_deref()
            .map(path_arg)
            .transpose()?
            .unwrap_or_else(|| request.package_manager.name().to_owned());
        run(
            supervisor,
            &executable,
            request.package_manager.install_args(),
            Some(destination),
            request.interactive,
            "package manager",
            "installing app dependencies",
        )
        .await?;
        true
    } else {
        false
    };

    let git_initialized = if request.initialize_git {
        run(
            supervisor,
            &git,
            ["init"],
            Some(destination),
            request.interactive,
            "Git",
            "initializing the app repository",
        )
        .await?;
        true
    } else {
        false
    };

    Ok(AppInitReport {
        destination: destination.to_owned(),
        repository: template.repository,
        branch: template.branch,
        subpath: template.subpath,
        package_manager: request.package_manager,
        rendered_files: copy.rendered,
        copied_files: copy.copied,
        package_json_updated,
        pnpm_workspace_updated,
        dependencies_installed,
        git_initialized,
    })
}

fn validate(request: &AppInitRequest) -> Result<(PathBuf, PathBuf, GitTemplate), AppInitError> {
    if !request.parent.exists() {
        return Err(AppInitError::ParentMissing(request.parent.clone()));
    }
    if !request.parent.is_dir() {
        return Err(AppInitError::ParentNotDirectory(request.parent.clone()));
    }
    validate_name(&request.directory_name)?;
    let parent =
        fs::canonicalize(&request.parent).map_err(|error| io_at(&request.parent, error))?;
    let destination = parent.join(&request.directory_name);
    if destination.exists() {
        return Err(AppInitError::DestinationExists(destination));
    }
    let template = request.template.git_template();
    if template.repository.trim().is_empty() {
        return Err(AppInitError::EmptyRepository);
    }
    if let Some(branch) = &template.branch
        && (branch.is_empty()
            || branch.starts_with('-')
            || branch.contains('\n')
            || branch.contains('\r'))
    {
        return Err(AppInitError::InvalidBranch(branch.clone()));
    }
    if let Some(subpath) = &template.subpath {
        validate_relative_path(subpath)
            .map_err(|_| AppInitError::InvalidTemplateSubpath(subpath.clone()))?;
    }
    Ok((parent, destination, template))
}

fn validate_name(name: &str) -> Result<(), AppInitError> {
    let mut components = Path::new(name).components();
    if name.is_empty()
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(AppInitError::InvalidName(name.to_owned()));
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), ()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(());
    }
    if path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Ok(())
    } else {
        Err(())
    }
}

fn copy_template(
    source: &Path,
    destination: &Path,
    app_name: &str,
    manager: PackageManager,
) -> Result<CopyReport, AppInitError> {
    let mut report = CopyReport::default();
    copy_directory(source, destination, app_name, manager, &mut report)?;
    Ok(report)
}

fn copy_directory(
    source: &Path,
    destination: &Path,
    app_name: &str,
    manager: PackageManager,
    report: &mut CopyReport,
) -> Result<(), AppInitError> {
    let mut entries = fs::read_dir(source)
        .map_err(|error| io_at(source, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io_at(source, error))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let source_path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        let metadata =
            fs::symlink_metadata(&source_path).map_err(|error| io_at(&source_path, error))?;
        if metadata.file_type().is_symlink() {
            return Err(AppInitError::UnsafeSymlink(source_path));
        }
        let original_name = entry.file_name();
        let mut target_name = original_name.clone();
        let is_liquid = source_path
            .extension()
            .is_some_and(|extension| extension == "liquid");
        if is_liquid {
            let name = original_name.to_string_lossy();
            target_name = name.strip_suffix(".liquid").unwrap_or(&name).into();
        }
        let target = destination.join(target_name);
        if metadata.is_dir() {
            fs::create_dir(&target).map_err(|error| io_at(&target, error))?;
            copy_directory(&source_path, &target, app_name, manager, report)?;
        } else if metadata.is_file() {
            let bytes = fs::read(&source_path).map_err(|error| io_at(&source_path, error))?;
            let output = if is_liquid {
                let text = String::from_utf8(bytes).map_err(|error| AppInitError::Render {
                    path: source_path.clone(),
                    message: error.to_string(),
                })?;
                render_liquid(&source_path, &text, app_name, manager)?.into_bytes()
            } else {
                bytes
            };
            fs::write(&target, output).map_err(|error| io_at(&target, error))?;
            fs::set_permissions(&target, metadata.permissions())
                .map_err(|error| io_at(&target, error))?;
            report.copied += 1;
            if is_liquid {
                report.rendered += 1;
            }
        }
    }
    Ok(())
}

fn render_liquid(
    path: &Path,
    text: &str,
    app_name: &str,
    manager: PackageManager,
) -> Result<String, AppInitError> {
    let parser = liquid::ParserBuilder::with_stdlib()
        .build()
        .map_err(|error| AppInitError::Render {
            path: path.to_owned(),
            message: error.to_string(),
        })?;
    let template = parser.parse(text).map_err(|error| AppInitError::Render {
        path: path.to_owned(),
        message: error.to_string(),
    })?;
    let globals = liquid::object!({
        "dependency_manager": manager.name(),
        "app_name": app_name,
    });
    template
        .render(&globals)
        .map_err(|error| AppInitError::Render {
            path: path.to_owned(),
            message: error.to_string(),
        })
}

fn update_package_json(destination: &Path, app_name: &str) -> Result<bool, AppInitError> {
    let path = destination.join("package.json");
    if !path.exists() {
        return Ok(false);
    }
    let bytes = fs::read(&path).map_err(|error| io_at(&path, error))?;
    let mut value: Value =
        serde_json::from_slice(&bytes).map_err(|error| AppInitError::InvalidPackageJson {
            path: path.clone(),
            message: error.to_string(),
        })?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| AppInitError::InvalidPackageJson {
            path: path.clone(),
            message: "root must be an object".to_owned(),
        })?;
    object.insert("name".to_owned(), Value::String(package_name(app_name)));
    object.insert("private".to_owned(), Value::Bool(true));
    ensure_workspaces(object, &path)?;
    let mut output =
        serde_json::to_vec_pretty(&value).map_err(|error| AppInitError::InvalidPackageJson {
            path: path.clone(),
            message: error.to_string(),
        })?;
    output.push(b'\n');
    fs::write(&path, output).map_err(|error| io_at(&path, error))?;
    Ok(true)
}

fn ensure_workspaces(object: &mut Map<String, Value>, path: &Path) -> Result<(), AppInitError> {
    let workspaces = object
        .entry("workspaces")
        .or_insert_with(|| Value::Array(Vec::new()));
    let array = if let Some(array) = workspaces.as_array_mut() {
        array
    } else if let Some(packages) = workspaces
        .as_object_mut()
        .and_then(|value| value.get_mut("packages"))
        .and_then(Value::as_array_mut)
    {
        packages
    } else {
        return Err(AppInitError::InvalidPackageJson {
            path: path.to_owned(),
            message: "workspaces must be an array or an object with a packages array".to_owned(),
        });
    };
    if !array
        .iter()
        .any(|value| value.as_str() == Some("extensions/*"))
    {
        array.push(Value::String("extensions/*".to_owned()));
    }
    Ok(())
}

fn package_name(name: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            if separator && !result.is_empty() {
                result.push('-');
            }
            separator = false;
            result.push(character);
        } else {
            separator = true;
        }
    }
    let result = result.trim_matches(['-', '.']);
    if result.is_empty() {
        "shopify-app".to_owned()
    } else {
        result.to_owned()
    }
}

fn update_pnpm_workspace(destination: &Path) -> Result<bool, AppInitError> {
    let path = destination.join("pnpm-workspace.yaml");
    let text = if path.exists() {
        fs::read_to_string(&path).map_err(|error| io_at(&path, error))?
    } else {
        String::new()
    };
    if text.lines().any(|line| {
        line.trim()
            .strip_prefix("- ")
            .is_some_and(|item| item.trim_matches(['\'', '"']) == "extensions/*")
    }) {
        return Ok(true);
    }
    let output = if let Some(index) = text.lines().position(|line| line.trim() == "packages:") {
        let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
        lines.insert(index + 1, "  - 'extensions/*'".to_owned());
        format!("{}\n", lines.join("\n"))
    } else if text.is_empty() {
        "packages:\n  - 'extensions/*'\n".to_owned()
    } else {
        format!("packages:\n  - 'extensions/*'\n{text}")
    };
    fs::write(&path, output).map_err(|error| io_at(&path, error))?;
    Ok(true)
}

async fn run<I, S>(
    supervisor: &Supervisor,
    program: &str,
    args: I,
    current_dir: Option<&Path>,
    interactive: bool,
    engine: &'static str,
    operation: &'static str,
) -> Result<ProcessOutput, AppInitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut spec = ProcessSpec::new(program)
        .args(args.into_iter().map(|arg| arg.as_ref().to_owned()))
        .output(OutputMode::Capture);
    if let Some(directory) = current_dir {
        spec = spec.current_dir(directory);
    }
    if !interactive && engine == "Git" {
        spec = spec
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .env("SSH_ASKPASS", "true");
    }
    let output = supervisor
        .spawn(spec)
        .map_err(|source| AppInitError::ProcessEngine {
            engine,
            operation,
            source,
        })?
        .wait()
        .await
        .map_err(|source| AppInitError::ProcessEngine {
            engine,
            operation,
            source,
        })?;
    if !output.status.success() {
        return Err(AppInitError::ProcessFailed {
            engine,
            operation,
            exit_code: output.exit_code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output)
}

fn path_arg(path: &Path) -> Result<String, AppInitError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| AppInitError::NonUtf8Path(path.to_owned()))
}
fn io_at(path: &Path, source: io::Error) -> AppInitError {
    AppInitError::FileSystem {
        path: path.to_owned(),
        source,
    }
}
fn unique_suffix() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(directory: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(directory)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn fixture_repo() -> TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test"]);
        fs::create_dir(repo.path().join("nested")).unwrap();
        fs::write(repo.path().join("nested/package.json.liquid"), r#"{"name":"old","private":false,"message":"{{ app_name }} via {{ dependency_manager }}","workspaces":["web/*"]}"#).unwrap();
        fs::write(
            repo.path().join("nested/README.md.liquid"),
            "# {{ app_name }} ({{ dependency_manager }})\n",
        )
        .unwrap();
        fs::create_dir(repo.path().join("nested/extensions")).unwrap();
        fs::write(repo.path().join("nested/extensions/.keep"), "").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "fixture"]);
        git(repo.path(), &["branch", "-M", "fixture-branch"]);
        repo
    }

    #[test]
    fn predefined_mapping_is_exact() {
        assert_eq!(
            AppTemplate::ReactRouter(ReactRouterFlavor::TypeScript).git_template(),
            GitTemplate {
                repository: REACT_ROUTER_REPOSITORY.to_owned(),
                branch: Some("main-cli".to_owned()),
                subpath: None
            }
        );
        assert_eq!(
            AppTemplate::ReactRouter(ReactRouterFlavor::JavaScript)
                .git_template()
                .branch
                .as_deref(),
            Some("javascript-cli")
        );
        assert_eq!(
            AppTemplate::None.git_template().repository,
            EXTENSION_ONLY_REPOSITORY
        );
    }

    #[test]
    fn parses_github_url_branch_and_subpath() {
        assert_eq!(
            parse_github_template_url("https://github.com/acme/template/examples/app#next")
                .unwrap(),
            GitTemplate {
                repository: "https://github.com/acme/template.git".to_owned(),
                branch: Some("next".to_owned()),
                subpath: Some(PathBuf::from("examples/app"))
            }
        );
        assert!(parse_github_template_url("https://example.com/acme/repo").is_err());
        assert!(parse_github_template_url("https://github.com/acme/repo/../secret").is_err());
        assert!(parse_github_template_url("https://github.com/acme/repo/%2e%2e/secret").is_err());
    }

    #[tokio::test]
    async fn scaffolds_local_git_subpath_renders_and_uses_fake_pnpm() {
        let repository = fixture_repo();
        let parent = tempfile::tempdir().unwrap();
        let fake = parent.path().join("fake-pnpm.sh");
        fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > package-manager.args\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut request = AppInitRequest::new(
            parent.path(),
            "My Cool App",
            AppTemplate::Custom(GitTemplate {
                repository: repository.path().to_string_lossy().into_owned(),
                branch: Some("fixture-branch".to_owned()),
                subpath: Some(PathBuf::from("nested")),
            }),
        );
        request.package_manager = PackageManager::Pnpm;
        request.package_manager_executable = Some(fake);
        request.initialize_git = true;
        request.interactive = false;
        let report = initialize(request).await.unwrap();
        let destination = parent.path().join("my-cool-app");
        assert_eq!(report.rendered_files, 2);
        assert!(
            report.dependencies_installed
                && report.git_initialized
                && report.pnpm_workspace_updated
        );
        assert_eq!(
            fs::read_to_string(destination.join("README.md")).unwrap(),
            "# My Cool App (pnpm)\n"
        );
        let package: Value =
            serde_json::from_slice(&fs::read(destination.join("package.json")).unwrap()).unwrap();
        assert_eq!(package["name"], "my-cool-app");
        assert_eq!(package["private"], true);
        assert_eq!(package["message"], "My Cool App via pnpm");
        assert!(
            package["workspaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "extensions/*")
        );
        assert_eq!(
            fs::read_to_string(destination.join("package-manager.args")).unwrap(),
            "install\n"
        );
        assert!(destination.join(".git").is_dir());
        assert!(
            fs::read_to_string(destination.join("pnpm-workspace.yaml"))
                .unwrap()
                .contains("extensions/*")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlinks_and_rolls_back_destination() {
        use std::os::unix::fs::symlink;
        let repository = fixture_repo();
        symlink("/etc/passwd", repository.path().join("nested/escape")).unwrap();
        git(repository.path(), &["add", "."]);
        git(repository.path(), &["commit", "-m", "symlink"]);
        let parent = tempfile::tempdir().unwrap();
        let mut request = AppInitRequest::new(
            parent.path(),
            "app",
            AppTemplate::Custom(GitTemplate {
                repository: repository.path().to_string_lossy().into_owned(),
                branch: Some("fixture-branch".to_owned()),
                subpath: Some(PathBuf::from("nested")),
            }),
        );
        request.install_dependencies = false;
        request.initialize_git = false;
        let error = initialize(request).await.unwrap_err();
        assert!(matches!(error, AppInitError::UnsafeSymlink(_)));
        assert!(!parent.path().join("app").exists());
    }

    #[tokio::test]
    async fn package_manager_failure_rolls_back() {
        let repository = fixture_repo();
        let parent = tempfile::tempdir().unwrap();
        let fake = parent.path().join("false.sh");
        fs::write(&fake, "#!/bin/sh\nexit 17\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut request = AppInitRequest::new(
            parent.path(),
            "app",
            AppTemplate::Custom(GitTemplate {
                repository: repository.path().to_string_lossy().into_owned(),
                branch: Some("fixture-branch".to_owned()),
                subpath: Some(PathBuf::from("nested")),
            }),
        );
        request.package_manager_executable = Some(fake);
        request.initialize_git = false;
        let error = initialize(request).await.unwrap_err();
        assert!(matches!(
            error,
            AppInitError::ProcessFailed {
                exit_code: Some(17),
                ..
            }
        ));
        assert!(!parent.path().join("app").exists());
    }
}
