//! Shopify application configuration graph loading.
//!
//! The graph deliberately keeps the complete TOML tables alongside the fields
//! understood by crabpify. Shopify adds configuration fields frequently, so a
//! newer field is a warning rather than a parse failure.

use crate::project::Project;
use cfy_core::{Error, ErrorKind, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use toml::{Table, Value};

const DEFAULT_EXTENSION_DIRECTORY: &str = "extensions/**";
const DEFAULT_WEB_DIRECTORY: &str = "**";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLocation {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub location: SourceLocation,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BuildConfig {
    pub automatically_update_urls_on_dev: Option<bool>,
    pub dev_store_url: Option<String>,
    pub include_config_on_deploy: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppConfig {
    pub path: PathBuf,
    pub name: Option<String>,
    pub client_id: Option<String>,
    pub application_url: Option<String>,
    pub embedded: Option<bool>,
    pub extension_directories: Vec<String>,
    pub web_directories: Vec<String>,
    pub build: BuildConfig,
    /// Complete parsed document, including fields this version does not know.
    pub raw: Table,
    /// Unknown top-level fields, retained as an easy-to-consume view.
    pub unknown: Table,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionFamily {
    Ui,
    Function,
    ThemeApp,
    WebPixel,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionConfig {
    pub path: PathBuf,
    pub directory: PathBuf,
    pub name: Option<String>,
    pub handle: Option<String>,
    pub uid: Option<String>,
    pub extension_type: Option<String>,
    pub api_version: Option<String>,
    pub family: ExtensionFamily,
    /// Complete parsed document, including extension-specific configuration.
    pub raw: Table,
    pub unknown: Table,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WebConfig {
    pub path: PathBuf,
    pub directory: PathBuf,
    pub name: Option<String>,
    pub roles: Vec<String>,
    pub web_type: Option<String>,
    pub raw: Table,
    pub unknown: Table,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppNode {
    pub config: AppConfig,
    pub extensions: Vec<ExtensionConfig>,
    pub webs: Vec<WebConfig>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppConfigGraph {
    pub root: PathBuf,
    pub apps: Vec<AppNode>,
    pub diagnostics: Vec<ConfigDiagnostic>,
}

impl AppConfigGraph {
    pub fn load(project: &Project) -> Result<Self> {
        let mut diagnostics = Vec::new();
        let mut apps = Vec::new();
        for path in project.config_files() {
            let (table, source) = parse_table(path, &mut diagnostics)?;
            let config = parse_app(path, table, &source, &mut diagnostics);
            let extensions = discover_extensions(project.root(), &config, &mut diagnostics)?;
            let webs = discover_webs(project.root(), &config, &mut diagnostics)?;
            diagnose_duplicate_handles(&extensions, &mut diagnostics);
            apps.push(AppNode {
                config,
                extensions,
                webs,
            });
        }
        Ok(Self {
            root: project.root().to_path_buf(),
            apps,
            diagnostics,
        })
    }
}

fn parse_table(path: &Path, diagnostics: &mut Vec<ConfigDiagnostic>) -> Result<(Table, String)> {
    let source = fs::read_to_string(path).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("failed to read {}", path.display()),
            error,
        )
    })?;
    match source.parse::<Table>() {
        Ok(table) => Ok((table, source)),
        Err(error) => {
            let (line, column) = error
                .span()
                .map(|span| line_column(&source, span.start))
                .unwrap_or((1, 1));
            diagnostics.push(ConfigDiagnostic {
                severity: DiagnosticSeverity::Error,
                message: format!("malformed TOML: {error}"),
                location: SourceLocation {
                    file: path.to_path_buf(),
                    line,
                    column,
                },
            });
            Ok((Table::new(), source))
        }
    }
}

fn parse_app(
    path: &Path,
    raw: Table,
    source: &str,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> AppConfig {
    const KNOWN: &[&str] = &[
        "name",
        "client_id",
        "application_url",
        "embedded",
        "extension_directories",
        "web_directories",
        "build",
        "access_scopes",
        "auth",
        "webhooks",
        "app_proxy",
        "pos",
        "organization_id",
    ];
    let unknown = unknown_fields(path, source, &raw, KNOWN, diagnostics, "app configuration");
    let build_table = raw.get("build").and_then(Value::as_table);
    AppConfig {
        path: path.to_path_buf(),
        name: string_field(path, source, &raw, "name", diagnostics),
        client_id: string_field(path, source, &raw, "client_id", diagnostics),
        application_url: string_field(path, source, &raw, "application_url", diagnostics),
        embedded: bool_field(path, source, &raw, "embedded", diagnostics),
        extension_directories: string_array_field(
            path,
            source,
            &raw,
            "extension_directories",
            diagnostics,
        )
        .unwrap_or_else(|| vec![DEFAULT_EXTENSION_DIRECTORY.into()]),
        web_directories: string_array_field(path, source, &raw, "web_directories", diagnostics)
            .unwrap_or_else(|| vec![DEFAULT_WEB_DIRECTORY.into()]),
        build: BuildConfig {
            automatically_update_urls_on_dev: build_table
                .and_then(|t| t.get("automatically_update_urls_on_dev"))
                .and_then(Value::as_bool),
            dev_store_url: build_table
                .and_then(|t| t.get("dev_store_url"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            include_config_on_deploy: build_table
                .and_then(|t| t.get("include_config_on_deploy"))
                .and_then(Value::as_bool),
        },
        raw,
        unknown,
    }
}

fn discover_extensions(
    root: &Path,
    app: &AppConfig,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> Result<Vec<ExtensionConfig>> {
    let mut paths = HashSet::new();
    for pattern in &app.extension_directories {
        for directory in expand_directory_pattern(root, pattern)? {
            collect_files(&directory, &mut |path| {
                let name = path
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or_default();
                if name == "shopify.extension.toml" || name.ends_with(".extension.toml") {
                    paths.insert(path.to_path_buf());
                }
            })?;
        }
    }
    let mut paths: Vec<_> = paths.into_iter().collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let (raw, source) = parse_table(&path, diagnostics)?;
            Ok(parse_extension(&path, raw, &source, diagnostics))
        })
        .collect()
}

fn parse_extension(
    path: &Path,
    raw: Table,
    source: &str,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> ExtensionConfig {
    const KNOWN: &[&str] = &[
        "name",
        "type",
        "handle",
        "uid",
        "description",
        "api_version",
        "extension_points",
        "targeting",
        "capabilities",
        "supported_features",
        "settings",
        "build",
        "configuration_ui",
        "ui",
        "input",
        "runtime_context",
        "merchant_label",
        "extensions",
    ];
    let unknown = unknown_fields(
        path,
        source,
        &raw,
        KNOWN,
        diagnostics,
        "extension configuration",
    );
    let extension_type = string_field(path, source, &raw, "type", diagnostics);
    let family = extension_type
        .as_deref()
        .map(classify_extension)
        .unwrap_or(ExtensionFamily::Unsupported);
    if family == ExtensionFamily::Unsupported {
        let description = extension_type.as_deref().unwrap_or("<missing>");
        warning(
            path,
            source,
            "type",
            format!(
                "unsupported extension type `{description}`; configuration was preserved but cannot be interpreted"
            ),
            diagnostics,
        );
    }
    ExtensionConfig {
        directory: path.parent().unwrap_or(Path::new("")).to_path_buf(),
        path: path.to_path_buf(),
        name: string_field(path, source, &raw, "name", diagnostics),
        handle: string_field(path, source, &raw, "handle", diagnostics),
        uid: string_field(path, source, &raw, "uid", diagnostics),
        extension_type,
        api_version: string_field(path, source, &raw, "api_version", diagnostics),
        family,
        raw,
        unknown,
    }
}

fn classify_extension(value: &str) -> ExtensionFamily {
    if value == "theme" || value == "theme_app_extension" {
        ExtensionFamily::ThemeApp
    } else if value == "function"
        || value.ends_with("_discounts")
        || [
            "cart_checkout_validation",
            "cart_transform",
            "delivery_customization",
            "payment_customization",
            "fulfillment_constraints",
            "order_routing_location_rule",
            "local_pickup_delivery_option_generator",
            "pickup_point_delivery_option_generator",
        ]
        .contains(&value)
    {
        ExtensionFamily::Function
    } else if ["web_pixel_extension", "web_pixel"].contains(&value) {
        ExtensionFamily::WebPixel
    } else if value == "ui_extension"
        || value.ends_with("_ui_extension")
        || value.contains("checkout")
        || value.starts_with("admin_")
        || value.starts_with("customer_account_")
        || value.starts_with("pos_")
    {
        ExtensionFamily::Ui
    } else {
        ExtensionFamily::Unsupported
    }
}

fn discover_webs(
    root: &Path,
    app: &AppConfig,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> Result<Vec<WebConfig>> {
    let mut paths = HashSet::new();
    for pattern in &app.web_directories {
        for directory in expand_directory_pattern(root, pattern)? {
            collect_files(&directory, &mut |path| {
                if path.file_name().and_then(|v| v.to_str()) == Some("shopify.web.toml") {
                    paths.insert(path.to_path_buf());
                }
            })?;
        }
    }
    let mut paths: Vec<_> = paths.into_iter().collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let (raw, source) = parse_table(&path, diagnostics)?;
            const KNOWN: &[&str] = &[
                "name",
                "roles",
                "type",
                "commands",
                "auth_callback_path",
                "webhooks_path",
                "port",
                "hmr_server",
            ];
            let unknown = unknown_fields(
                &path,
                &source,
                &raw,
                KNOWN,
                diagnostics,
                "web configuration",
            );
            Ok(WebConfig {
                directory: path.parent().unwrap_or(Path::new("")).to_path_buf(),
                name: string_field(&path, &source, &raw, "name", diagnostics),
                roles: string_array_field(&path, &source, &raw, "roles", diagnostics)
                    .unwrap_or_default(),
                web_type: string_field(&path, &source, &raw, "type", diagnostics),
                path,
                raw,
                unknown,
            })
        })
        .collect()
}

fn expand_directory_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let normalized = pattern.replace('\\', "/");
    let pattern_path = Path::new(&normalized);
    if pattern_path.is_absolute()
        || pattern_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!(
                "configuration directory pattern `{pattern}` must stay inside {}",
                root.display()
            ),
        ));
    }
    let wildcard = normalized.find('*');
    let prefix = wildcard
        .map(|index| normalized[..index].trim_end_matches('/'))
        .unwrap_or(&normalized);
    let start = root.join(prefix);
    if wildcard.is_none() {
        return Ok(if start.is_dir() {
            vec![start]
        } else {
            Vec::new()
        });
    }
    let recursive = normalized.contains("**");
    let mut result = Vec::new();
    if start.is_dir() {
        if recursive {
            collect_directories(&start, &mut result)?;
        } else {
            for entry in read_dir(&start)? {
                let entry = entry.map_err(|error| {
                    Error::with_source(
                        ErrorKind::Config,
                        format!("failed to inspect {}", start.display()),
                        error,
                    )
                })?;
                if entry.path().is_dir() {
                    result.push(entry.path());
                }
            }
        }
    }
    Ok(result)
}

fn collect_directories(path: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    output.push(path.to_path_buf());
    for entry in read_dir(path)? {
        let entry = entry.map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("failed to inspect {}", path.display()),
                error,
            )
        })?;
        if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            collect_directories(&entry.path(), output)?;
        }
    }
    Ok(())
}

fn collect_files(path: &Path, callback: &mut impl FnMut(&Path)) -> Result<()> {
    if !path.is_dir() {
        return Ok(());
    }
    for entry in read_dir(path)? {
        let entry = entry.map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("failed to inspect {}", path.display()),
                error,
            )
        })?;
        let path = entry.path();
        if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            collect_files(&path, callback)?;
        } else if entry.file_type().is_ok_and(|file_type| file_type.is_file()) {
            callback(&path);
        }
    }
    Ok(())
}

fn read_dir(path: &Path) -> Result<fs::ReadDir> {
    fs::read_dir(path).map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            format!("failed to inspect {}", path.display()),
            error,
        )
    })
}

fn diagnose_duplicate_handles(
    extensions: &[ExtensionConfig],
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let mut seen: HashMap<&str, &Path> = HashMap::new();
    for extension in extensions {
        if let Some(handle) = extension.handle.as_deref()
            && let Some(first) = seen.insert(handle, &extension.path)
        {
            diagnostics.push(ConfigDiagnostic {
                severity: DiagnosticSeverity::Error,
                message: format!(
                    "duplicate extension handle `{handle}`; first declared in {}",
                    first.display()
                ),
                location: SourceLocation {
                    file: extension.path.clone(),
                    line: key_location_from_file(&extension.path, "handle").0,
                    column: key_location_from_file(&extension.path, "handle").1,
                },
            });
        }
    }
}

fn unknown_fields(
    path: &Path,
    source: &str,
    raw: &Table,
    known: &[&str],
    diagnostics: &mut Vec<ConfigDiagnostic>,
    context: &str,
) -> Table {
    let known: HashSet<_> = known.iter().copied().collect();
    let unknown: Table = raw
        .iter()
        .filter(|(key, _)| !known.contains(key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for key in unknown.keys() {
        warning(
            path,
            source,
            key,
            format!("unknown {context} key `{key}`; it was preserved for forward compatibility"),
            diagnostics,
        );
    }
    unknown
}

fn string_field(
    path: &Path,
    source: &str,
    table: &Table,
    key: &str,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> Option<String> {
    typed_field(
        path,
        source,
        table,
        key,
        "a string",
        Value::as_str,
        diagnostics,
    )
    .map(str::to_owned)
}

fn bool_field(
    path: &Path,
    source: &str,
    table: &Table,
    key: &str,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> Option<bool> {
    typed_field(
        path,
        source,
        table,
        key,
        "a boolean",
        Value::as_bool,
        diagnostics,
    )
}

fn string_array_field(
    path: &Path,
    source: &str,
    table: &Table,
    key: &str,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> Option<Vec<String>> {
    let value = table.get(key)?;
    let Some(array) = value.as_array() else {
        warning(
            path,
            source,
            key,
            format!("`{key}` must be an array of strings"),
            diagnostics,
        );
        return None;
    };
    let mut output = Vec::new();
    for item in array {
        let Some(item) = item.as_str() else {
            warning(
                path,
                source,
                key,
                format!("`{key}` must contain only strings"),
                diagnostics,
            );
            return None;
        };
        output.push(item.to_owned());
    }
    Some(output)
}

fn typed_field<'a, T>(
    path: &Path,
    source: &str,
    table: &'a Table,
    key: &str,
    expected: &str,
    extract: impl Fn(&'a Value) -> Option<T>,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> Option<T> {
    let value = table.get(key)?;
    match extract(value) {
        Some(value) => Some(value),
        None => {
            warning(
                path,
                source,
                key,
                format!("`{key}` must be {expected}"),
                diagnostics,
            );
            None
        }
    }
}

fn warning(
    path: &Path,
    source: &str,
    key: &str,
    message: String,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) {
    let (line, column) = key_location(source, key);
    diagnostics.push(ConfigDiagnostic {
        severity: DiagnosticSeverity::Warning,
        message,
        location: SourceLocation {
            file: path.to_path_buf(),
            line,
            column,
        },
    });
}

fn key_location(source: &str, key: &str) -> (usize, usize) {
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with(key) && trimmed[key.len()..].trim_start().starts_with('=') {
            return (index + 1, line.len() - trimmed.len() + 1);
        }
    }
    (1, 1)
}

fn key_location_from_file(path: &Path, key: &str) -> (usize, usize) {
    fs::read_to_string(path)
        .map(|source| key_location(&source, key))
        .unwrap_or((1, 1))
}

fn line_column(source: &str, offset: usize) -> (usize, usize) {
    let prefix = &source[..offset.min(source.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.len() + 1, |(_, tail)| tail.len() + 1);
    (line, column)
}

/// Stable map useful to consumers that need lookup by extension handle.
pub fn extensions_by_handle(node: &AppNode) -> BTreeMap<&str, &ExtensionConfig> {
    node.extensions
        .iter()
        .filter_map(|extension| {
            extension
                .handle
                .as_deref()
                .map(|handle| (handle, extension))
        })
        .collect()
}
