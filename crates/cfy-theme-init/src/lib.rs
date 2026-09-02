//! Native orchestration for `theme init`.
//!
//! Git is the intrinsic external engine. It is invoked directly with an argv
//! vector through `cfy-process`; this crate never delegates to Shopify CLI.

use cfy_process::{OutputMode, ProcessOutput, ProcessSpec, Supervisor};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

/// Shopify's default starting point for new themes.
pub const SKELETON_THEME_URL: &str = "https://github.com/Shopify/skeleton-theme.git";

/// Inputs to a theme initialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInitRequest {
    /// Existing directory in which the theme directory will be created.
    pub parent: PathBuf,
    /// A single directory name, not a relative or absolute path.
    pub name: String,
    /// Git URL, optionally suffixed with `#branch`.
    pub clone_url: String,
    /// Check out the most recent reachable tag after a non-shallow clone.
    pub latest: bool,
    /// Whether Git is allowed to ask for credentials interactively.
    pub interactive: bool,
    /// Git executable. This is primarily useful to embedders and tests.
    pub git_executable: PathBuf,
}

impl ThemeInitRequest {
    #[must_use]
    pub fn new(parent: impl Into<PathBuf>, name: impl Into<String>) -> Self {
        Self {
            parent: parent.into(),
            name: name.into(),
            clone_url: SKELETON_THEME_URL.to_owned(),
            latest: false,
            interactive: true,
            git_executable: PathBuf::from("git"),
        }
    }
}

/// Cleanup applied only when cloning Shopify's default Skeleton theme.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkeletonCleanupReport {
    pub removed_git_directory: bool,
    pub removed_instruction_directories: Vec<PathBuf>,
}

/// A typed account of the completed initialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInitReport {
    pub destination: PathBuf,
    pub repository: String,
    pub branch: Option<String>,
    pub checked_out_tag: Option<String>,
    pub shallow: bool,
    pub origin_removed: bool,
    pub skeleton_cleanup: Option<SkeletonCleanupReport>,
}

#[derive(Debug, Error)]
pub enum ThemeInitError {
    #[error("theme parent path does not exist: {0}")]
    ParentMissing(PathBuf),
    #[error("theme parent path is not a directory: {0}")]
    ParentNotDirectory(PathBuf),
    #[error("theme name must be one non-empty directory name, got {0:?}")]
    InvalidName(String),
    #[error("theme destination escapes its parent: {destination}")]
    DestinationEscapesParent { destination: PathBuf },
    #[error("theme destination exists but is not a directory: {0}")]
    DestinationNotDirectory(PathBuf),
    #[error("theme destination already exists and is not empty: {0}")]
    DestinationNotEmpty(PathBuf),
    #[error("clone URL must not be empty")]
    EmptyCloneUrl,
    #[error("clone URL branch suffix must not be empty or start with '-'")]
    InvalidBranch,
    #[error("the latest release cannot be selected together with a #branch suffix")]
    LatestWithBranch,
    #[error("path cannot be represented as UTF-8 for the process engine: {0}")]
    NonUtf8Path(PathBuf),
    #[error("could not inspect {path}: {source}")]
    FileSystem {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not run Git while {operation}: {source}")]
    ProcessEngine {
        operation: &'static str,
        #[source]
        source: cfy_core::Error,
    },
    #[error("Git failed while {operation} (exit {exit_code:?}): {stderr}")]
    GitFailed {
        operation: &'static str,
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error("couldn't obtain the most recent tag of repository {repository}")]
    NoTags { repository: String },
    #[error("Git returned a tag that was not valid UTF-8")]
    InvalidTagOutput,
}

#[derive(Debug)]
struct ValidatedRequest {
    destination: PathBuf,
    repository: String,
    branch: Option<String>,
}

/// Initialize a theme using a fresh process supervisor.
pub async fn initialize(request: ThemeInitRequest) -> Result<ThemeInitReport, ThemeInitError> {
    let supervisor = Supervisor::default();
    initialize_with_supervisor(request, &supervisor).await
}

/// Initialize a theme using the caller's process supervisor.
pub async fn initialize_with_supervisor(
    request: ThemeInitRequest,
    supervisor: &Supervisor,
) -> Result<ThemeInitReport, ThemeInitError> {
    let validated = validate(&request)?;
    let destination_arg = path_arg(&validated.destination)?;
    let git_program = path_arg(&request.git_executable)?;

    let shallow = !request.latest;
    let mut clone_args = vec!["clone".to_owned(), "--recurse-submodules".to_owned()];
    if let Some(branch) = &validated.branch {
        clone_args.extend(["--branch".to_owned(), branch.clone()]);
    }
    if shallow {
        clone_args.extend(["--depth".to_owned(), "1".to_owned()]);
    }
    if !request.interactive {
        // `git clone -c` applies this before fetching. The environment settings
        // cover credential and SSH transports without ever putting a secret in argv.
        clone_args.extend([
            "-c".to_owned(),
            "core.askPass=true".to_owned(),
            "-c".to_owned(),
            "credential.interactive=false".to_owned(),
        ]);
    }
    clone_args.extend([validated.repository.clone(), destination_arg]);

    run_git(
        supervisor,
        &git_program,
        clone_args,
        None,
        request.interactive,
        "cloning the theme repository",
    )
    .await?;

    let checked_out_tag = if request.latest {
        let output = run_git(
            supervisor,
            &git_program,
            ["describe", "--tags", "--abbrev=0"],
            Some(&validated.destination),
            request.interactive,
            "finding the latest tag",
        )
        .await
        .map_err(|error| match error {
            ThemeInitError::GitFailed { .. } => ThemeInitError::NoTags {
                repository: validated.repository.clone(),
            },
            other => other,
        })?;
        let tag = std::str::from_utf8(&output.stdout)
            .map_err(|_| ThemeInitError::InvalidTagOutput)?
            .trim()
            .to_owned();
        if tag.is_empty() {
            return Err(ThemeInitError::NoTags {
                repository: validated.repository.clone(),
            });
        }
        run_git(
            supervisor,
            &git_program,
            ["checkout", tag.as_str()],
            Some(&validated.destination),
            request.interactive,
            "checking out the latest tag",
        )
        .await?;
        Some(tag)
    } else {
        None
    };

    let remotes = run_git(
        supervisor,
        &git_program,
        ["remote"],
        Some(&validated.destination),
        request.interactive,
        "listing Git remotes",
    )
    .await?;
    let has_origin = String::from_utf8_lossy(&remotes.stdout)
        .lines()
        .any(|remote| remote.trim() == "origin");
    if has_origin {
        run_git(
            supervisor,
            &git_program,
            ["remote", "remove", "origin"],
            Some(&validated.destination),
            request.interactive,
            "removing the origin remote",
        )
        .await?;
    }

    let skeleton_cleanup = if validated.repository == SKELETON_THEME_URL {
        Some(clean_skeleton(&validated.destination)?)
    } else {
        None
    };

    Ok(ThemeInitReport {
        destination: validated.destination,
        repository: validated.repository,
        branch: validated.branch,
        checked_out_tag,
        shallow,
        origin_removed: has_origin,
        skeleton_cleanup,
    })
}

fn validate(request: &ThemeInitRequest) -> Result<ValidatedRequest, ThemeInitError> {
    if !request.parent.exists() {
        return Err(ThemeInitError::ParentMissing(request.parent.clone()));
    }
    if !request.parent.is_dir() {
        return Err(ThemeInitError::ParentNotDirectory(request.parent.clone()));
    }
    validate_name(&request.name)?;

    let parent =
        fs::canonicalize(&request.parent).map_err(|source| ThemeInitError::FileSystem {
            path: request.parent.clone(),
            source,
        })?;
    let destination = parent.join(&request.name);
    if destination.parent() != Some(parent.as_path()) {
        return Err(ThemeInitError::DestinationEscapesParent { destination });
    }
    if destination.exists() {
        if !destination.is_dir() {
            return Err(ThemeInitError::DestinationNotDirectory(destination));
        }
        let canonical_destination =
            fs::canonicalize(&destination).map_err(|source| ThemeInitError::FileSystem {
                path: destination.clone(),
                source,
            })?;
        if !canonical_destination.starts_with(&parent) {
            return Err(ThemeInitError::DestinationEscapesParent {
                destination: canonical_destination,
            });
        }
        let mut entries =
            fs::read_dir(&destination).map_err(|source| ThemeInitError::FileSystem {
                path: destination.clone(),
                source,
            })?;
        if let Some(entry) = entries.next() {
            entry.map_err(|source| ThemeInitError::FileSystem {
                path: destination.clone(),
                source,
            })?;
            return Err(ThemeInitError::DestinationNotEmpty(destination));
        }
    }

    let clone_url = request.clone_url.trim();
    if clone_url.is_empty() {
        return Err(ThemeInitError::EmptyCloneUrl);
    }
    let (repository, branch) = match clone_url.split_once('#') {
        Some((repository, branch)) => {
            if repository.is_empty() {
                return Err(ThemeInitError::EmptyCloneUrl);
            }
            if branch.is_empty() || branch.starts_with('-') {
                return Err(ThemeInitError::InvalidBranch);
            }
            (repository.to_owned(), Some(branch.to_owned()))
        }
        None => (clone_url.to_owned(), None),
    };
    if request.latest && branch.is_some() {
        return Err(ThemeInitError::LatestWithBranch);
    }

    Ok(ValidatedRequest {
        destination,
        repository,
        branch,
    })
}

fn validate_name(name: &str) -> Result<(), ThemeInitError> {
    if name.is_empty() || name == "." || name == ".." || Path::new(name).is_absolute() {
        return Err(ThemeInitError::InvalidName(name.to_owned()));
    }
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(ThemeInitError::InvalidName(name.to_owned()));
    }
    Ok(())
}

fn path_arg(path: &Path) -> Result<String, ThemeInitError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| ThemeInitError::NonUtf8Path(path.to_owned()))
}

async fn run_git<I, S>(
    supervisor: &Supervisor,
    git_program: &str,
    args: I,
    current_dir: Option<&Path>,
    interactive: bool,
    operation: &'static str,
) -> Result<ProcessOutput, ThemeInitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut spec = ProcessSpec::new(git_program)
        .args(args.into_iter().map(|arg| arg.as_ref().to_owned()))
        .output(OutputMode::Capture);
    if let Some(directory) = current_dir {
        spec = spec.current_dir(directory);
    }
    if !interactive {
        spec = spec
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "true")
            .env("SSH_ASKPASS", "true");
    }
    let output = supervisor
        .spawn(spec)
        .map_err(|source| ThemeInitError::ProcessEngine { operation, source })?
        .wait()
        .await
        .map_err(|source| ThemeInitError::ProcessEngine { operation, source })?;
    if !output.status.success() {
        return Err(ThemeInitError::GitFailed {
            operation,
            exit_code: output.exit_code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(output)
}

fn clean_skeleton(destination: &Path) -> Result<SkeletonCleanupReport, ThemeInitError> {
    let mut report = SkeletonCleanupReport::default();
    for directory in [".all", ".github", ".cursor", ".claude"] {
        let path = destination.join(directory);
        if path.exists() {
            remove_dir_all(&path)?;
            report
                .removed_instruction_directories
                .push(PathBuf::from(directory));
        }
    }
    let git = destination.join(".git");
    if git.exists() {
        remove_dir_all(&git)?;
        report.removed_git_directory = true;
    }
    Ok(report)
}

fn remove_dir_all(path: &Path) -> Result<(), ThemeInitError> {
    fs::remove_dir_all(path).map_err(|source| ThemeInitError::FileSystem {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsStr, process::Command};
    use tempfile::TempDir;

    fn git(directory: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(directory)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", directory)
            .output()
            .expect("git should be installed for fixture tests");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn fixture() -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.name", "Fixture"]);
        git(&source, &["config", "user.email", "fixture@example.test"]);
        fs::write(source.join("theme.txt"), "one").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "one"]);
        git(&source, &["tag", "v1.0.0"]);
        fs::write(source.join("theme.txt"), "two").unwrap();
        git(&source, &["commit", "-am", "two"]);
        git(&source, &["tag", "v2.0.0"]);
        (temp, source)
    }

    #[test]
    fn rejects_names_that_can_escape_the_parent() {
        let temp = TempDir::new().unwrap();
        for name in ["", ".", "..", "../theme", "nested/theme"] {
            let error = validate(&ThemeInitRequest::new(temp.path(), name)).unwrap_err();
            assert!(matches!(error, ThemeInitError::InvalidName(_)));
        }
    }

    #[test]
    fn rejects_non_empty_destinations_including_hidden_files() {
        let temp = TempDir::new().unwrap();
        let destination = temp.path().join("theme");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join(".hidden"), "occupied").unwrap();
        let error = validate(&ThemeInitRequest::new(temp.path(), "theme")).unwrap_err();
        assert!(matches!(error, ThemeInitError::DestinationNotEmpty(_)));
    }

    #[test]
    fn parses_branch_suffix_and_rejects_latest_combination() {
        let temp = TempDir::new().unwrap();
        let mut request = ThemeInitRequest::new(temp.path(), "theme");
        request.clone_url = "https://example.test/theme.git#develop".into();
        let parsed = validate(&request).unwrap();
        assert_eq!(parsed.repository, "https://example.test/theme.git");
        assert_eq!(parsed.branch.as_deref(), Some("develop"));
        request.latest = true;
        assert!(matches!(
            validate(&request),
            Err(ThemeInitError::LatestWithBranch)
        ));
    }

    #[tokio::test]
    async fn shallow_clone_removes_origin_and_keeps_custom_git_metadata() {
        let (temp, source) = fixture();
        let parent = temp.path().join("output");
        fs::create_dir(&parent).unwrap();
        let mut request = ThemeInitRequest::new(&parent, "my-theme");
        request.clone_url = source.to_string_lossy().into_owned();
        request.interactive = false;

        let report = initialize(request).await.unwrap();
        assert!(report.shallow);
        assert!(report.origin_removed);
        assert!(report.skeleton_cleanup.is_none());
        assert!(report.destination.join(".git").is_dir());
        assert_eq!(git(&report.destination, &["remote"]), "");
    }

    #[tokio::test]
    async fn latest_uses_non_shallow_clone_and_checks_out_latest_reachable_tag() {
        let (temp, source) = fixture();
        let parent = temp.path().join("output");
        fs::create_dir(&parent).unwrap();
        let mut request = ThemeInitRequest::new(&parent, "latest-theme");
        request.clone_url = source.to_string_lossy().into_owned();
        request.latest = true;
        request.interactive = false;

        let report = initialize(request).await.unwrap();
        assert!(!report.shallow);
        assert_eq!(report.checked_out_tag.as_deref(), Some("v2.0.0"));
        assert_eq!(
            git(
                &report.destination,
                &["describe", "--tags", "--exact-match"]
            ),
            "v2.0.0"
        );
        assert!(!report.destination.join(".git/shallow").exists());
    }

    #[tokio::test]
    async fn latest_reports_a_typed_error_when_the_repository_has_no_tags() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        git(&source, &["config", "user.name", "Fixture"]);
        git(&source, &["config", "user.email", "fixture@example.test"]);
        fs::write(source.join("theme.txt"), "untagged").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-m", "untagged"]);
        let parent = temp.path().join("output");
        fs::create_dir(&parent).unwrap();
        let mut request = ThemeInitRequest::new(&parent, "latest-theme");
        request.clone_url = source.to_string_lossy().into_owned();
        request.latest = true;
        request.interactive = false;

        let error = initialize(request).await.unwrap_err();
        assert!(matches!(error, ThemeInitError::NoTags { .. }));
    }

    #[test]
    fn skeleton_cleanup_removes_upstream_metadata_and_instruction_directories() {
        let temp = TempDir::new().unwrap();
        for directory in [".git", ".all", ".github", ".cursor", ".claude"] {
            fs::create_dir(temp.path().join(directory)).unwrap();
        }
        fs::write(temp.path().join("theme.liquid"), "theme").unwrap();
        let report = clean_skeleton(temp.path()).unwrap();
        assert!(report.removed_git_directory);
        assert_eq!(report.removed_instruction_directories.len(), 4);
        assert!(temp.path().join("theme.liquid").exists());
        assert!(!temp.path().join(".github").exists());
    }

    #[test]
    fn name_is_one_normal_component() {
        assert!(validate_name("my-theme").is_ok());
        if OsStr::new("a\\b") != OsStr::new("a/b") && cfg!(windows) {
            assert!(validate_name("a\\b").is_err());
        }
    }
}
