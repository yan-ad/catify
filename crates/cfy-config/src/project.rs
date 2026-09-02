//! Shopify app/theme project discovery and deterministic environment selection.

use cfy_core::{Error, ErrorKind, Result};
use std::{
    collections::BTreeMap,
    fmt, fs, io,
    path::{Path, PathBuf},
};

const THEME_CONFIG_FILE: &str = "shopify.theme.toml";

/// A supported Shopify project type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    App,
    Theme,
}

impl fmt::Display for ProjectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::App => "app",
            Self::Theme => "theme",
        })
    }
}

/// A discovered project and its available configuration variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    root: PathBuf,
    kind: ProjectKind,
    config_files: Vec<PathBuf>,
}

impl Project {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn kind(&self) -> ProjectKind {
        self.kind
    }

    #[must_use]
    pub fn config_files(&self) -> &[PathBuf] {
        &self.config_files
    }
}

/// Values supplied directly by command flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectOverrides {
    pub config: Option<String>,
    pub store: Option<String>,
    pub organization: Option<String>,
}

/// A selected project configuration and its effective store/organization.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectEnvironment {
    pub project: Project,
    pub config_path: PathBuf,
    pub config_name: String,
    pub store: Option<String>,
    pub organization: Option<String>,
    pub document: toml::Value,
}

/// Environment values are passed explicitly to keep command resolution testable.
pub type Environment = BTreeMap<String, String>;

/// Project discovery and selection failures with actionable context.
#[derive(Debug)]
pub enum ProjectError {
    InvalidStart(PathBuf),
    NotFound(PathBuf),
    AmbiguousKinds {
        root: PathBuf,
    },
    AmbiguousConfigs {
        root: PathBuf,
        choices: Vec<String>,
    },
    UnknownConfig {
        requested: String,
        choices: Vec<String>,
    },
    ReadDirectory {
        path: PathBuf,
        source: io::Error,
    },
    ReadConfig {
        path: PathBuf,
        source: io::Error,
    },
    ParseConfig {
        path: PathBuf,
        source: toml::de::Error,
    },
    InvalidValue {
        path: PathBuf,
        key: &'static str,
    },
}

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStart(path) => write!(
                formatter,
                "project discovery start path does not exist: {}",
                path.display()
            ),
            Self::NotFound(path) => write!(
                formatter,
                "no Shopify app or theme project found from {}; run inside a project or pass an explicit project directory",
                path.display()
            ),
            Self::AmbiguousKinds { root } => write!(
                formatter,
                "{} contains both app and theme project markers; select the command's project type explicitly",
                root.display()
            ),
            Self::AmbiguousConfigs { root, choices } => write!(
                formatter,
                "{} contains multiple app configurations ({}); select one with --config",
                root.display(),
                choices.join(", ")
            ),
            Self::UnknownConfig { requested, choices } => write!(
                formatter,
                "unknown app configuration {requested:?}; available configurations: {}",
                choices.join(", ")
            ),
            Self::ReadDirectory { path, source } => {
                write!(formatter, "could not inspect {}: {source}", path.display())
            }
            Self::ReadConfig { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::ParseConfig { path, source } => {
                write!(formatter, "invalid TOML in {}: {source}", path.display())
            }
            Self::InvalidValue { path, key } => write!(
                formatter,
                "{key} in {} must be a non-empty string",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ProjectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadDirectory { source, .. } | Self::ReadConfig { source, .. } => Some(source),
            Self::ParseConfig { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<ProjectError> for Error {
    fn from(source: ProjectError) -> Self {
        let kind = match source {
            ProjectError::InvalidStart(_)
            | ProjectError::NotFound(_)
            | ProjectError::AmbiguousKinds { .. }
            | ProjectError::AmbiguousConfigs { .. }
            | ProjectError::UnknownConfig { .. }
            | ProjectError::InvalidValue { .. } => ErrorKind::InvalidInput,
            ProjectError::ReadDirectory { .. }
            | ProjectError::ReadConfig { .. }
            | ProjectError::ParseConfig { .. } => ErrorKind::Config,
        };
        Error::with_source(kind, source.to_string(), source)
    }
}

/// Finds the nearest ancestor containing the requested Shopify project marker.
///
/// If `kind` is omitted, a directory containing both app and theme markers is
/// rejected rather than selected implicitly.
pub fn discover(start: impl AsRef<Path>, kind: Option<ProjectKind>) -> Result<Project> {
    discover_inner(start.as_ref(), kind).map_err(Into::into)
}

fn discover_inner(
    start: &Path,
    kind: Option<ProjectKind>,
) -> std::result::Result<Project, ProjectError> {
    if !start.exists() {
        return Err(ProjectError::InvalidStart(start.to_path_buf()));
    }

    let start = if start.is_file() {
        start.parent().unwrap_or(start)
    } else {
        start
    };

    for directory in start.ancestors() {
        let markers = inspect_directory(directory)?;
        let selected = match kind {
            Some(ProjectKind::App) if !markers.app.is_empty() => Some(Project {
                root: directory.to_path_buf(),
                kind: ProjectKind::App,
                config_files: markers.app,
            }),
            Some(ProjectKind::Theme) if markers.theme.is_some() => Some(Project {
                root: directory.to_path_buf(),
                kind: ProjectKind::Theme,
                config_files: vec![markers.theme.expect("theme marker was checked")],
            }),
            None if !markers.app.is_empty() && markers.theme.is_some() => {
                return Err(ProjectError::AmbiguousKinds {
                    root: directory.to_path_buf(),
                });
            }
            None if !markers.app.is_empty() => Some(Project {
                root: directory.to_path_buf(),
                kind: ProjectKind::App,
                config_files: markers.app,
            }),
            None if markers.theme.is_some() => Some(Project {
                root: directory.to_path_buf(),
                kind: ProjectKind::Theme,
                config_files: vec![markers.theme.expect("theme marker was checked")],
            }),
            _ => None,
        };

        if let Some(project) = selected {
            return Ok(project);
        }
    }

    Err(ProjectError::NotFound(start.to_path_buf()))
}

#[derive(Default)]
struct Markers {
    app: Vec<PathBuf>,
    theme: Option<PathBuf>,
}

fn inspect_directory(directory: &Path) -> std::result::Result<Markers, ProjectError> {
    let entries = fs::read_dir(directory).map_err(|source| ProjectError::ReadDirectory {
        path: directory.to_path_buf(),
        source,
    })?;
    let mut markers = Markers::default();

    for entry in entries {
        let entry = entry.map_err(|source| ProjectError::ReadDirectory {
            path: directory.to_path_buf(),
            source,
        })?;
        let file_type = entry
            .file_type()
            .map_err(|source| ProjectError::ReadDirectory {
                path: entry.path(),
                source,
            })?;
        if !file_type.is_file() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_app_config(&name) {
            markers.app.push(entry.path());
        } else if name == THEME_CONFIG_FILE {
            markers.theme = Some(entry.path());
        }
    }

    markers.app.sort();
    Ok(markers)
}

fn is_app_config(name: &str) -> bool {
    name == "shopify.app.toml"
        || name
            .strip_prefix("shopify.app.")
            .and_then(|suffix| suffix.strip_suffix(".toml"))
            .is_some_and(|environment| !environment.is_empty())
}

/// Selects a config variant and resolves store/organization values.
///
/// Precedence is command flags, then environment variables, then values in the
/// selected TOML document. Supported environment names are `SHOPIFY_FLAG_*`
/// for compatibility and `CFY_*` for Catify-native automation.
pub fn resolve_environment(
    project: Project,
    overrides: &ProjectOverrides,
    environment: &Environment,
) -> Result<ProjectEnvironment> {
    resolve_environment_inner(project, overrides, environment).map_err(Into::into)
}

fn resolve_environment_inner(
    project: Project,
    overrides: &ProjectOverrides,
    environment: &Environment,
) -> std::result::Result<ProjectEnvironment, ProjectError> {
    let requested_config = overrides
        .config
        .as_deref()
        .or_else(|| environment_value(environment, &["CFY_CONFIG", "SHOPIFY_FLAG_APP_CONFIG"]));
    let config_path = select_config(&project, requested_config)?;
    let config_name = config_name(&config_path, project.kind);
    let contents = fs::read_to_string(&config_path).map_err(|source| ProjectError::ReadConfig {
        path: config_path.clone(),
        source,
    })?;
    let document =
        toml::from_str::<toml::Value>(&contents).map_err(|source| ProjectError::ParseConfig {
            path: config_path.clone(),
            source,
        })?;

    let store = resolve_string(
        overrides.store.as_deref(),
        environment_value(environment, &["CFY_STORE", "SHOPIFY_FLAG_STORE"]),
        document.get("store"),
        &config_path,
        "store",
    )?;
    let organization = resolve_string(
        overrides.organization.as_deref(),
        environment_value(
            environment,
            &["CFY_ORGANIZATION", "SHOPIFY_FLAG_ORGANIZATION"],
        ),
        document
            .get("organization")
            .or_else(|| document.get("organization_id")),
        &config_path,
        "organization",
    )?;

    Ok(ProjectEnvironment {
        project,
        config_path,
        config_name,
        store,
        organization,
        document,
    })
}

fn select_config(
    project: &Project,
    requested: Option<&str>,
) -> std::result::Result<PathBuf, ProjectError> {
    if project.kind == ProjectKind::Theme {
        return Ok(project.config_files[0].clone());
    }

    let choices = project
        .config_files
        .iter()
        .map(|path| config_name(path, ProjectKind::App))
        .collect::<Vec<_>>();

    if let Some(requested) = requested {
        let requested_path = Path::new(requested);
        let selected = project.config_files.iter().find(|path| {
            path == &requested_path
                || path.file_name() == requested_path.file_name()
                || config_name(path, ProjectKind::App) == requested
        });
        return selected
            .cloned()
            .ok_or_else(|| ProjectError::UnknownConfig {
                requested: requested.to_owned(),
                choices,
            });
    }

    if let Some(default) = project.config_files.iter().find(|path| {
        path.file_name()
            .is_some_and(|name| name == "shopify.app.toml")
    }) {
        return Ok(default.clone());
    }

    match project.config_files.as_slice() {
        [only] => Ok(only.clone()),
        _ => Err(ProjectError::AmbiguousConfigs {
            root: project.root.clone(),
            choices,
        }),
    }
}

fn config_name(path: &Path, kind: ProjectKind) -> String {
    if kind == ProjectKind::Theme {
        return "theme".to_owned();
    }

    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if name == "shopify.app.toml" {
        "default".to_owned()
    } else {
        name.strip_prefix("shopify.app.")
            .and_then(|suffix| suffix.strip_suffix(".toml"))
            .unwrap_or(name)
            .to_owned()
    }
}

fn environment_value<'a>(environment: &'a Environment, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| environment.get(*name))
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn resolve_string(
    explicit: Option<&str>,
    environment: Option<&str>,
    configured: Option<&toml::Value>,
    path: &Path,
    key: &'static str,
) -> std::result::Result<Option<String>, ProjectError> {
    if let Some(value) = explicit.or(environment) {
        return non_empty(value, path, key).map(Some);
    }

    match configured {
        None => Ok(None),
        Some(toml::Value::String(value)) => non_empty(value, path, key).map(Some),
        Some(_) => Err(ProjectError::InvalidValue {
            path: path.to_path_buf(),
            key,
        }),
    }
}

fn non_empty(
    value: &str,
    path: &Path,
    key: &'static str,
) -> std::result::Result<String, ProjectError> {
    let value = value.trim();
    if value.is_empty() {
        Err(ProjectError::InvalidValue {
            path: path.to_path_buf(),
            key,
        })
    } else {
        Ok(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        fs::{self, File},
        sync::atomic::{AtomicU64, Ordering},
    };

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("cfy-project-{name}-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }

        fn directory(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nested_directory_resolves_nearest_app_project() {
        let outer = Fixture::new("nearest-app");
        outer.write("shopify.app.toml", "name = 'outer'");
        outer.write("packages/inner/shopify.app.toml", "name = 'inner'");
        let nested = outer.directory("packages/inner/web/src");

        let project = discover(&nested, None).unwrap();

        assert_eq!(project.kind(), ProjectKind::App);
        assert_eq!(project.root(), outer.path().join("packages/inner"));
    }

    #[test]
    fn discovers_theme_from_nested_directory() {
        let fixture = Fixture::new("theme");
        fixture.write("shopify.theme.toml", "name = 'theme'");
        let nested = fixture.directory("sections/generated");

        let project = discover(nested, None).unwrap();

        assert_eq!(project.kind(), ProjectKind::Theme);
        assert_eq!(project.root(), fixture.path());
    }

    #[test]
    fn non_project_error_is_actionable() {
        let fixture = Fixture::new("none");
        let error = discover(fixture.path(), None).unwrap_err();

        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.message().contains("no Shopify app or theme project"));
        assert!(error.message().contains("explicit project directory"));
    }

    #[test]
    fn mixed_project_requires_explicit_kind() {
        let fixture = Fixture::new("mixed");
        fixture.write("shopify.app.toml", "name = 'app'");
        fixture.write("shopify.theme.toml", "name = 'theme'");

        let error = discover(fixture.path(), None).unwrap_err();
        assert!(error.message().contains("both app and theme"));

        let app = discover(fixture.path(), Some(ProjectKind::App)).unwrap();
        let theme = discover(fixture.path(), Some(ProjectKind::Theme)).unwrap();
        assert_eq!(app.kind(), ProjectKind::App);
        assert_eq!(theme.kind(), ProjectKind::Theme);
    }

    #[test]
    fn default_config_wins_without_explicit_selection() {
        let fixture = Fixture::new("default-config");
        let default = fixture.write(
            "shopify.app.toml",
            "store = 'config.myshopify.com'\norganization = '100'",
        );
        fixture.write(
            "shopify.app.staging.toml",
            "store = 'staging.myshopify.com'",
        );
        let project = discover(fixture.path(), Some(ProjectKind::App)).unwrap();

        let selected =
            resolve_environment(project, &ProjectOverrides::default(), &Environment::new())
                .unwrap();

        assert_eq!(selected.config_path, default);
        assert_eq!(selected.config_name, "default");
        assert_eq!(selected.store.as_deref(), Some("config.myshopify.com"));
        assert_eq!(selected.organization.as_deref(), Some("100"));
    }

    #[test]
    fn explicit_values_override_environment_and_config() {
        let fixture = Fixture::new("precedence");
        fixture.write(
            "shopify.app.toml",
            "store = 'config.myshopify.com'\norganization = '100'",
        );
        fixture.write(
            "shopify.app.staging.toml",
            "store = 'staging.myshopify.com'\norganization = '200'",
        );
        let project = discover(fixture.path(), Some(ProjectKind::App)).unwrap();
        let environment = Environment::from([
            ("CFY_CONFIG".to_owned(), "staging".to_owned()),
            (
                "CFY_STORE".to_owned(),
                "environment.myshopify.com".to_owned(),
            ),
            ("CFY_ORGANIZATION".to_owned(), "300".to_owned()),
        ]);
        let overrides = ProjectOverrides {
            config: None,
            store: Some("flag.myshopify.com".to_owned()),
            organization: Some("400".to_owned()),
        };

        let selected = resolve_environment(project, &overrides, &environment).unwrap();

        assert_eq!(selected.config_name, "staging");
        assert_eq!(selected.store.as_deref(), Some("flag.myshopify.com"));
        assert_eq!(selected.organization.as_deref(), Some("400"));
    }

    #[test]
    fn environment_overrides_selected_config_values() {
        let fixture = Fixture::new("environment-precedence");
        fixture.write(
            "shopify.app.toml",
            "store = 'config.myshopify.com'\norganization_id = '100'",
        );
        let project = discover(fixture.path(), Some(ProjectKind::App)).unwrap();
        let environment = Environment::from([
            (
                "SHOPIFY_FLAG_STORE".to_owned(),
                "env.myshopify.com".to_owned(),
            ),
            ("SHOPIFY_FLAG_ORGANIZATION".to_owned(), "200".to_owned()),
        ]);

        let selected =
            resolve_environment(project, &ProjectOverrides::default(), &environment).unwrap();

        assert_eq!(selected.store.as_deref(), Some("env.myshopify.com"));
        assert_eq!(selected.organization.as_deref(), Some("200"));
    }

    #[test]
    fn multiple_named_configs_require_selection() {
        let fixture = Fixture::new("ambiguous-configs");
        fixture.write("shopify.app.development.toml", "name = 'dev'");
        fixture.write("shopify.app.staging.toml", "name = 'staging'");
        let project = discover(fixture.path(), Some(ProjectKind::App)).unwrap();

        let error = resolve_environment(project, &ProjectOverrides::default(), &Environment::new())
            .unwrap_err();

        assert!(error.message().contains("multiple app configurations"));
        assert!(error.message().contains("development"));
        assert!(error.message().contains("staging"));
        assert!(error.message().contains("--config"));
    }

    #[test]
    fn unknown_config_lists_available_choices() {
        let fixture = Fixture::new("unknown-config");
        fixture.write("shopify.app.toml", "name = 'default'");
        fixture.write("shopify.app.staging.toml", "name = 'staging'");
        let project = discover(fixture.path(), Some(ProjectKind::App)).unwrap();
        let overrides = ProjectOverrides {
            config: Some("production".to_owned()),
            ..ProjectOverrides::default()
        };

        let error = resolve_environment(project, &overrides, &Environment::new()).unwrap_err();

        assert!(error.message().contains("production"));
        assert!(error.message().contains("default"));
        assert!(error.message().contains("staging"));
    }

    #[test]
    fn discovery_accepts_file_start_path() {
        let fixture = Fixture::new("file-start");
        fixture.write("shopify.app.toml", "name = 'app'");
        let source = fixture.write("web/src/main.rs", "fn main() {};");

        let project = discover(source, None).unwrap();

        assert_eq!(project.root(), fixture.path());
    }

    #[test]
    fn directories_named_like_configs_are_ignored() {
        let fixture = Fixture::new("directory-marker");
        fixture.directory("shopify.app.toml");
        File::create(fixture.path().join("unrelated.txt")).unwrap();

        assert!(discover(fixture.path(), None).is_err());
    }
}
