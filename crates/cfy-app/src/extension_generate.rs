//! Native app-extension template generation.

use cfy_process::{OutputMode, ProcessSpec, Supervisor};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

const DEFAULT_UI_REPOSITORY: &str = "https://github.com/Shopify/extensions-templates";
const DEFAULT_FUNCTION_REPOSITORY: &str = "https://github.com/Shopify/function-examples";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerateExtensionOptions {
    pub app_directory: PathBuf,
    pub name: String,
    pub template: String,
    pub flavor: Option<String>,
    pub repository: Option<String>,
}

fn ensure_no_symlink_ancestors(root: &Path, target: &Path) -> GenerateResult<()> {
    if !target.starts_with(root) {
        return Err(GenerateExtensionError::PathEscape(target.to_owned()));
    }
    let mut current = root.to_owned();
    for component in target
        .strip_prefix(root)
        .expect("prefix checked")
        .components()
    {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(GenerateExtensionError::PathEscape(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(source) => return Err(io_at(&current, source)),
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GenerateExtensionReport {
    pub directory: PathBuf,
    pub name: String,
    pub handle: String,
    pub template: String,
    pub flavor: Option<String>,
    pub repository: String,
}

#[derive(Debug, Error)]
pub enum GenerateExtensionError {
    #[error("extension name cannot be empty")]
    EmptyName,
    #[error("extension template cannot be empty")]
    EmptyTemplate,
    #[error("extension directory already exists: {0}")]
    ExistingDirectory(PathBuf),
    #[error("extension path escapes the app directory: {0}")]
    PathEscape(PathBuf),
    #[error("could not create extension at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("could not clone extension template repository `{repository}`: {message}")]
    Clone { repository: String, message: String },
    #[error("template `{template}`{flavor} was not found in `{repository}`")]
    MissingTemplate {
        template: String,
        flavor: String,
        repository: String,
    },
    #[error("generated extension does not contain shopify.extension.toml")]
    MissingConfiguration,
    #[error("could not render Liquid template {path}: {message}")]
    Render { path: PathBuf, message: String },
}

pub type GenerateResult<T> = std::result::Result<T, GenerateExtensionError>;

/// Clone and render an official Shopify extension template without invoking Shopify CLI.
pub async fn generate_extension(
    supervisor: &Supervisor,
    options: &GenerateExtensionOptions,
) -> GenerateResult<GenerateExtensionReport> {
    let name = options.name.trim();
    if name.is_empty() {
        return Err(GenerateExtensionError::EmptyName);
    }
    let template = options.template.trim();
    if template.is_empty() {
        return Err(GenerateExtensionError::EmptyTemplate);
    }
    let handle = slug(name);
    let root = absolute_lexical(&options.app_directory)?;
    let destination = confined_join(&root, Path::new("extensions").join(&handle).as_path())?;
    ensure_no_symlink_ancestors(&root, &destination)?;
    if destination.exists() {
        return Err(GenerateExtensionError::ExistingDirectory(destination));
    }
    let repository = options.repository.clone().unwrap_or_else(|| {
        if template.contains("function") {
            DEFAULT_FUNCTION_REPOSITORY.to_owned()
        } else {
            DEFAULT_UI_REPOSITORY.to_owned()
        }
    });
    let temp = root.join(".catify").join(format!(
        "generate-extension-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    if temp.exists() {
        remove_path(&temp).map_err(|source| io_at(&temp, source))?;
    }
    fs::create_dir_all(temp.parent().expect("temporary path has parent"))
        .map_err(|source| io_at(&temp, source))?;

    let operation = async {
        let mut spec = ProcessSpec::new("git");
        spec.args = vec![
            "clone".into(),
            "--depth".into(),
            "1".into(),
            "--".into(),
            repository.clone(),
            temp.to_string_lossy().into_owned(),
        ];
        spec.output = OutputMode::Capture;
        let result = supervisor
            .spawn(spec)
            .map_err(|error| GenerateExtensionError::Clone {
                repository: repository.clone(),
                message: error.to_string(),
            })?
            .wait()
            .await
            .map_err(|error| GenerateExtensionError::Clone {
                repository: repository.clone(),
                message: error.to_string(),
            })?;
        if result.exit_code() != Some(0) {
            return Err(GenerateExtensionError::Clone {
                repository: repository.clone(),
                message: String::from_utf8_lossy(&result.stderr).trim().to_owned(),
            });
        }
        let template_dir = find_template_directory(&temp, template, options.flavor.as_deref())
            .ok_or_else(|| GenerateExtensionError::MissingTemplate {
                template: template.to_owned(),
                flavor: options
                    .flavor
                    .as_ref()
                    .map(|value| format!(" with flavor `{value}`"))
                    .unwrap_or_default(),
                repository: repository.clone(),
            })?;
        fs::create_dir_all(&destination).map_err(|source| io_at(&destination, source))?;
        let mut values = BTreeMap::new();
        values.insert("name", name.to_owned());
        values.insert("handle", handle.clone());
        values.insert("type", template.to_owned());
        values.insert("flavor", options.flavor.clone().unwrap_or_default());
        values.insert("uid", deterministic_uid(&handle));
        values.insert(
            "srcFileExtension",
            source_extension(options.flavor.as_deref()).to_owned(),
        );
        copy_rendered(&template_dir, &destination, &values)?;
        if !destination.join("shopify.extension.toml").is_file() {
            return Err(GenerateExtensionError::MissingConfiguration);
        }
        Ok::<_, GenerateExtensionError>(())
    }
    .await;

    let _ = remove_path(&temp);
    if let Err(error) = operation {
        let _ = remove_path(&destination);
        return Err(error);
    }
    Ok(GenerateExtensionReport {
        directory: destination,
        name: name.to_owned(),
        handle,
        template: template.to_owned(),
        flavor: options.flavor.clone(),
        repository,
    })
}

fn find_template_directory(root: &Path, template: &str, flavor: Option<&str>) -> Option<PathBuf> {
    let normalized = template.replace('_', "-");
    let aliases = [
        normalized.clone(),
        normalized.replace("-ui-extension", "-extension"),
        normalized.replace("-app-extension", "-extension"),
    ];
    let flavor_suffix = match flavor {
        Some("vanilla-js" | "react" | "typescript" | "typescript-react") => Some("js"),
        Some("rust") => Some("rs"),
        Some("wasm") => Some("wasm"),
        _ => None,
    };
    let mut candidates = Vec::new();
    for alias in aliases {
        candidates.push(root.join(&alias));
        if let Some(suffix) = flavor_suffix {
            candidates.push(root.join(format!("{alias}-{suffix}")));
        }
        if let Some(flavor) = flavor {
            candidates.push(root.join(&alias).join(flavor));
        }
    }
    candidates.push(root.to_owned());
    candidates.into_iter().find(|path| {
        path.join("shopify.extension.toml.liquid").is_file()
            || path.join("shopify.extension.toml").is_file()
    })
}

fn copy_rendered(
    source: &Path,
    destination: &Path,
    values: &BTreeMap<&str, String>,
) -> GenerateResult<()> {
    for entry in fs::read_dir(source).map_err(|source_error| io_at(source, source_error))? {
        let entry = entry.map_err(|source_error| io_at(source, source_error))?;
        let source_path = entry.path();
        if entry
            .file_type()
            .map_err(|source_error| io_at(&source_path, source_error))?
            .is_symlink()
        {
            return Err(GenerateExtensionError::PathEscape(source_path));
        }
        if entry.file_name() == ".git" {
            continue;
        }
        let is_liquid = source_path
            .extension()
            .is_some_and(|value| value == "liquid");
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if let Some(stripped) = name.strip_suffix(".liquid") {
            name = stripped.to_owned();
        }
        if is_liquid
            && source_path
                .components()
                .any(|component| component.as_os_str() == "src")
            && Path::new(&name).extension().is_none()
            && let Some(extension) = values.get("srcFileExtension")
            && !extension.is_empty()
        {
            name.push('.');
            name.push_str(extension);
        }
        let target = destination.join(name);
        if source_path.is_dir() {
            fs::create_dir_all(&target).map_err(|source_error| io_at(&target, source_error))?;
            copy_rendered(&source_path, &target, values)?;
        } else {
            let bytes =
                fs::read(&source_path).map_err(|source_error| io_at(&source_path, source_error))?;
            let rendered = match String::from_utf8(bytes.clone()) {
                Ok(text) if is_liquid => {
                    let parser = liquid::ParserBuilder::with_stdlib()
                        .build()
                        .map_err(|error| GenerateExtensionError::Render {
                            path: source_path.clone(),
                            message: error.to_string(),
                        })?;
                    let template =
                        parser
                            .parse(&text)
                            .map_err(|error| GenerateExtensionError::Render {
                                path: source_path.clone(),
                                message: error.to_string(),
                            })?;
                    let globals = liquid::Object::from_iter(values.iter().map(|(key, value)| {
                        (
                            (*key).to_owned().into(),
                            liquid::model::Value::scalar(value.clone()),
                        )
                    }));
                    template
                        .render(&globals)
                        .map_err(|error| GenerateExtensionError::Render {
                            path: source_path.clone(),
                            message: error.to_string(),
                        })?
                        .into_bytes()
                }
                Ok(text) => text.into_bytes(),
                Err(_) => bytes,
            };
            cfy_config::write_atomic(&target, &rendered)
                .map_err(|source_error| io_at(&target, source_error))?;
        }
    }
    Ok(())
}

fn source_extension(flavor: Option<&str>) -> &'static str {
    match flavor {
        Some("react") => "jsx",
        Some("typescript") => "ts",
        Some("typescript-react") => "tsx",
        Some("rust") => "rs",
        Some("wasm") => "wasm",
        _ => "js",
    }
}

fn deterministic_uid(handle: &str) -> String {
    use sha1::{Digest, Sha1};
    let digest = Sha1::digest(handle.as_bytes());
    let hex = format!("{digest:x}");
    format!(
        "{}-{}-5{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut dash = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            dash = false;
        } else if !output.is_empty() && !dash {
            output.push('-');
            dash = true;
        }
    }
    output.trim_matches('-').to_owned()
}

fn absolute_lexical(path: &Path) -> GenerateResult<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|source| io_at(path, source))?
            .join(path)
    };
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}

fn confined_join(root: &Path, relative: &Path) -> GenerateResult<PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|value| matches!(value, Component::ParentDir))
    {
        return Err(GenerateExtensionError::PathEscape(relative.to_owned()));
    }
    let path = root.join(relative);
    if !path.starts_with(root) {
        return Err(GenerateExtensionError::PathEscape(path));
    }
    Ok(path)
}

fn remove_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn io_at(path: &Path, source: io::Error) -> GenerateExtensionError {
    GenerateExtensionError::Io {
        path: path.to_owned(),
        source,
    }
}
