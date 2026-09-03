//! Rust-native import of dashboard extension registrations.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

const STATE_PATH: &str = ".shopify/extension-identifiers.json";
const MAX_HANDLE_BYTES: usize = 50;

/// An extension registration returned by the Shopify app dashboard.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RemoteExtensionRegistration {
    pub uuid: String,
    pub title: String,
    #[serde(rename = "type")]
    pub extension_type: String,
    #[serde(default)]
    pub configuration: serde_json::Value,
    #[serde(default)]
    pub context: Option<String>,
}

pub fn is_migratable_type(extension_type: &str) -> bool {
    matches!(
        extension_type.to_ascii_lowercase().as_str(),
        "payments_app"
            | "payments_app_credit_card"
            | "payments_app_custom_credit_card"
            | "payments_app_custom_onsite"
            | "payments_app_redeemable"
            | "payments_app_card_present"
            | "payments_extension"
            | "flow_action_definition"
            | "flow_trigger_definition"
            | "flow_trigger_discovery_webhook"
            | "marketing_activity_extension"
            | "subscription_link"
            | "subscription_link_extension"
            | "app_link"
            | "bulk_action"
    )
}

fn transform_configuration(
    registration: &RemoteExtensionRegistration,
    handle: &str,
) -> ImportResult<serde_json::Value> {
    let mut config = registration.configuration.clone();
    let table = config
        .as_object_mut()
        .ok_or_else(|| ExtensionImportError::Configuration {
            uuid: registration.uuid.clone(),
            message: "configuration must be a JSON object".into(),
        })?;
    match registration.extension_type.as_str() {
        "flow_trigger_discovery_webhook" => {
            let url = table.get("url").cloned().unwrap_or(serde_json::Value::Null);
            return Ok(serde_json::json!({"url": url}));
        }
        "flow_action_definition" | "flow_trigger_definition" => {
            rename_key(table, "url", "runtime_url");
            rename_key(table, "custom_configuration_page_url", "config_page_url");
            rename_key(
                table,
                "custom_configuration_page_preview_url",
                "config_page_preview_url",
            );
            if registration.extension_type == "flow_action_definition"
                && !table.contains_key("runtime_url")
            {
                table.insert(
                    "runtime_url".into(),
                    serde_json::Value::String("https://url.com/api/execute".into()),
                );
            }
            table.remove("title");
        }
        "marketing_activity_extension" => {
            if let Some(url) = table
                .remove("app_api_url")
                .and_then(|value| value.as_str().map(str::to_owned))
                && let Ok(url) = url::Url::parse(&url)
            {
                let mut path = url.path().to_owned();
                if let Some(query) = url.query() {
                    path.push('?');
                    path.push_str(query);
                }
                if let Some(fragment) = url.fragment() {
                    path.push('#');
                    path.push_str(fragment);
                }
                table.insert("api_path".into(), serde_json::Value::String(path));
            }
            if let Some(platform) = table
                .remove("platform")
                .and_then(|value| value.as_str().map(str::to_owned))
            {
                let channel = match platform.as_str() {
                    "facebook" | "instagram" | "pinterest" | "snapchat" | "tiktok" => "social",
                    "google" | "bing" => "search",
                    "email" | "flow" => "email",
                    "sms" => "sms",
                    "verizon_media" => "display",
                    "ebay" => "marketplace",
                    _ => "",
                };
                table.insert(
                    "marketing_channel".into(),
                    serde_json::Value::String(channel.into()),
                );
                let domain = match platform.as_str() {
                    "facebook" => "facebook.com",
                    "instagram" => "instagram.com",
                    "google" => "google.com",
                    "pinterest" => "pinterest.com",
                    "bing" => "bing.com",
                    "snapchat" => "snapchat.com",
                    "ebay" => "ebay.com",
                    "tiktok" => "tiktok.com",
                    _ => "",
                };
                table.insert(
                    "referring_domain".into(),
                    serde_json::Value::String(domain.into()),
                );
            }
            if let Some(fields) = table
                .get_mut("fields")
                .and_then(serde_json::Value::as_array_mut)
            {
                for field in fields {
                    if let Some(field) = field.as_object_mut() {
                        field.remove("id");
                    }
                }
            }
        }
        value if value.starts_with("payments_") => {
            rename_key(table, "start_payment_session_url", "payment_session_url");
            rename_key(table, "start_refund_session_url", "refund_session_url");
            rename_key(table, "start_capture_session_url", "capture_session_url");
            rename_key(table, "start_void_session_url", "void_session_url");
            rename_key(table, "default_buyer_label", "buyer_label");
            rename_key(table, "buyer_label_to_locale", "buyer_label_translations");
            if let Some(certificate) = table.remove("encryption_certificate")
                && let Some(fingerprint) = certificate.get("fingerprint").cloned()
            {
                table.insert("encryption_certificate_fingerprint".into(), fingerprint);
            }
            table.remove("api_version");
            let target = registration
                .context
                .clone()
                .unwrap_or_else(|| payment_target(value).into());
            table.insert("targeting".into(), serde_json::json!([{"target": target}]));
        }
        _ => {}
    }
    table.insert("handle".into(), serde_json::Value::String(handle.into()));
    Ok(config)
}

fn rename_key(table: &mut serde_json::Map<String, serde_json::Value>, from: &str, to: &str) {
    if let Some(value) = table.remove(from) {
        table.insert(to.into(), value);
    }
}

fn local_extension_type(extension_type: &str) -> &str {
    match extension_type {
        "flow_action_definition" => "flow_action",
        "flow_trigger_definition" => "flow_trigger",
        "flow_trigger_discovery_webhook" => "flow_trigger_lifecycle_callback",
        "marketing_activity_extension" => "marketing_activity",
        "subscription_link" => "subscription_link_extension",
        "app_link" | "bulk_action" => "admin_link",
        value if value.starts_with("payments_") => "payments_extension",
        value => value,
    }
}

fn payment_target(extension_type: &str) -> &str {
    match extension_type {
        "payments_app_credit_card" => "payments.credit-card.render",
        "payments_app_custom_credit_card" => "payments.custom-credit-card.render",
        "payments_app_custom_onsite" => "payments.custom-onsite.render",
        "payments_app_redeemable" => "payments.redeemable.render",
        "payments_app_card_present" => "payments.card-present.render",
        _ => "payments.offsite.render",
    }
}

impl<'de> Deserialize<'de> for RemoteExtensionRegistration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Version {
            config: serde_json::Value,
        }
        #[derive(Deserialize)]
        struct Wire {
            uuid: String,
            title: String,
            #[serde(rename = "type")]
            extension_type: String,
            #[serde(default)]
            configuration: Option<serde_json::Value>,
            #[serde(default)]
            context: Option<String>,
            #[serde(rename = "draftVersion", default)]
            draft_version: Option<Version>,
            #[serde(rename = "activeVersion", default)]
            active_version: Option<Version>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let configuration = wire
            .configuration
            .or_else(|| wire.draft_version.map(|version| version.config))
            .or_else(|| wire.active_version.map(|version| version.config))
            .unwrap_or(serde_json::Value::Null);
        let configuration = match configuration {
            serde_json::Value::String(value) => {
                serde_json::from_str(&value).map_err(serde::de::Error::custom)?
            }
            value => value,
        };
        Ok(Self {
            uuid: wire.uuid,
            title: wire.title,
            extension_type: wire.extension_type,
            configuration,
            context: wire.context,
        })
    }
}

/// Backend boundary for the unstable dashboard registration contract.
#[async_trait]
pub trait ExtensionRegistrationProvider: Send + Sync {
    async fn fetch_extension_registrations(
        &self,
        client_id: &str,
        organization_id: &str,
    ) -> crate::Result<Vec<RemoteExtensionRegistration>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportSelection {
    All,
    Uuids(BTreeSet<String>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExistingDirectoryPolicy {
    Overwrite,
    Skip,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportExtensionsOptions {
    pub app_directory: PathBuf,
    pub client_id: String,
    pub organization_id: String,
    pub selection: ImportSelection,
    pub existing_directory_policy: ExistingDirectoryPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportOutcome {
    Imported,
    AlreadyImported,
    SkippedExistingDirectory,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExtensionImportItem {
    pub uuid: String,
    pub title: String,
    pub extension_type: String,
    pub handle: String,
    pub outcome: ImportOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ImportExtensionsReport {
    pub items: Vec<ExtensionImportItem>,
    pub state_path: PathBuf,
}

/// Typed failures for planning and committing an extension import transaction.
#[derive(Debug, Error)]
pub enum ExtensionImportError {
    #[error("could not fetch extension registrations: {source}")]
    Provider {
        #[source]
        source: crate::Error,
    },
    #[error("selected extension UUID(s) were not returned by Shopify: {0}")]
    UnknownSelection(String),
    #[error("extension registration has an empty UUID")]
    EmptyUuid,
    #[error("extension `{uuid}` has an empty type")]
    EmptyType { uuid: String },
    #[error("import path escapes the app directory: {0}")]
    PathEscape(PathBuf),
    #[error("import path contains a symbolic link and is not safe to replace: {0}")]
    Symlink(PathBuf),
    #[error("could not read extension import state at {path}: {source}")]
    ReadState { path: PathBuf, source: io::Error },
    #[error("invalid extension import state at {path}: {source}")]
    InvalidState {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("could not serialize extension configuration `{uuid}`: {message}")]
    Configuration { uuid: String, message: String },
    #[error("extension import filesystem operation failed at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("extension import failed and rollback also failed: {operation}; rollback: {rollback}")]
    Rollback { operation: String, rollback: String },
}

pub type ImportResult<T> = std::result::Result<T, ExtensionImportError>;

#[derive(Default, Deserialize, Serialize)]
struct IdentifierState {
    /// Dashboard UUID to local, non-secret extension handle.
    extensions: BTreeMap<String, String>,
}

struct PlannedImport {
    registration: RemoteExtensionRegistration,
    handle: String,
    target: PathBuf,
    outcome: ImportOutcome,
}

/// Fetch registrations through a native provider, plan deterministically, then commit atomically.
pub async fn import_extensions<P: ExtensionRegistrationProvider>(
    provider: &P,
    options: &ImportExtensionsOptions,
) -> ImportResult<ImportExtensionsReport> {
    let registrations = provider
        .fetch_extension_registrations(&options.client_id, &options.organization_id)
        .await
        .map_err(|source| ExtensionImportError::Provider { source })?;
    import_extension_registrations(registrations, options)
}

/// Plan and commit already-fetched registrations. Useful for fixtures and non-HTTP callers.
pub fn import_extension_registrations(
    mut registrations: Vec<RemoteExtensionRegistration>,
    options: &ImportExtensionsOptions,
) -> ImportResult<ImportExtensionsReport> {
    let root = absolute_lexical(&options.app_directory)?;
    ensure_no_symlink_ancestors(&root, &root)?;
    let extensions_root = confined_join(&root, Path::new("extensions"))?;
    let state_path = confined_join(&root, Path::new(STATE_PATH))?;
    let mut state = load_state(&state_path)?;

    registrations.sort_by(|a, b| a.uuid.cmp(&b.uuid).then_with(|| a.title.cmp(&b.title)));
    let available: BTreeSet<_> = registrations.iter().map(|r| r.uuid.clone()).collect();
    if let ImportSelection::Uuids(selected) = &options.selection {
        let missing: Vec<_> = selected.difference(&available).cloned().collect();
        if !missing.is_empty() {
            return Err(ExtensionImportError::UnknownSelection(missing.join(", ")));
        }
        registrations.retain(|registration| selected.contains(&registration.uuid));
    }

    let mut reserved: BTreeSet<String> = state.extensions.values().cloned().collect();
    let mut planned = Vec::new();
    for registration in registrations {
        if registration.uuid.trim().is_empty() {
            return Err(ExtensionImportError::EmptyUuid);
        }
        if registration.extension_type.trim().is_empty() {
            return Err(ExtensionImportError::EmptyType {
                uuid: registration.uuid,
            });
        }
        if let Some(handle) = state.extensions.get(&registration.uuid).cloned() {
            planned.push(PlannedImport {
                target: confined_join(&extensions_root, Path::new(&handle))?,
                registration,
                handle,
                outcome: ImportOutcome::AlreadyImported,
            });
            continue;
        }
        let handle = unique_handle(&registration.title, &registration.uuid, &mut reserved);
        let target = confined_join(&extensions_root, Path::new(&handle))?;
        ensure_no_symlink_ancestors(&root, &target)?;
        let outcome = if target.exists() {
            match options.existing_directory_policy {
                ExistingDirectoryPolicy::Overwrite => ImportOutcome::Imported,
                ExistingDirectoryPolicy::Skip => ImportOutcome::SkippedExistingDirectory,
            }
        } else {
            ImportOutcome::Imported
        };
        planned.push(PlannedImport {
            registration,
            handle,
            target,
            outcome,
        });
    }

    commit(&root, &extensions_root, &state_path, &mut state, &planned)?;
    Ok(ImportExtensionsReport {
        items: planned
            .into_iter()
            .map(|item| ExtensionImportItem {
                uuid: item.registration.uuid,
                title: item.registration.title,
                extension_type: item.registration.extension_type,
                handle: item.handle,
                outcome: item.outcome,
            })
            .collect(),
        state_path,
    })
}

fn commit(
    root: &Path,
    extensions_root: &Path,
    state_path: &Path,
    state: &mut IdentifierState,
    planned: &[PlannedImport],
) -> ImportResult<()> {
    let actionable: Vec<_> = planned
        .iter()
        .filter(|item| item.outcome == ImportOutcome::Imported)
        .collect();
    if actionable.is_empty() {
        return Ok(());
    }
    create_dir_all(extensions_root)?;
    let transaction = extensions_root.join(format!(".cfy-import-{}", std::process::id()));
    if transaction.exists() {
        remove_path(&transaction).map_err(|source| io_at(&transaction, source))?;
    }
    create_dir_all(&transaction)?;
    let staged_root = transaction.join("staged");
    let backup_root = transaction.join("backup");
    create_dir_all(&staged_root)?;
    create_dir_all(&backup_root)?;

    let operation = (|| -> ImportResult<()> {
        for item in &actionable {
            let staged = staged_root.join(&item.handle);
            create_dir_all(&staged)?;
            let config = render_configuration(&item.registration, &item.handle)?;
            let config_path = staged.join("shopify.extension.toml");
            cfy_config::write_atomic(&config_path, config.as_bytes())
                .map_err(|source| io_at(&config_path, source))?;
        }
        for item in &actionable {
            if item.target.exists() {
                ensure_no_symlink_ancestors(root, &item.target)?;
                fs::rename(&item.target, backup_root.join(&item.handle))
                    .map_err(|source| io_at(&item.target, source))?;
            }
            fs::rename(staged_root.join(&item.handle), &item.target)
                .map_err(|source| io_at(&item.target, source))?;
            state
                .extensions
                .insert(item.registration.uuid.clone(), item.handle.clone());
        }
        let bytes = serde_json::to_vec_pretty(state).expect("identifier state is serializable");
        cfy_config::write_atomic(state_path, &bytes).map_err(|source| io_at(state_path, source))?;
        Ok(())
    })();

    if let Err(error) = operation {
        let rollback = rollback(&actionable, &staged_root, &backup_root, &transaction);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback) => Err(ExtensionImportError::Rollback {
                operation: error.to_string(),
                rollback: rollback.to_string(),
            }),
        };
    }
    remove_path(&transaction).map_err(|source| io_at(&transaction, source))?;
    Ok(())
}

fn rollback(
    items: &[&PlannedImport],
    staged_root: &Path,
    backup_root: &Path,
    transaction: &Path,
) -> ImportResult<()> {
    for item in items.iter().rev() {
        let backup = backup_root.join(&item.handle);
        if backup.exists() {
            if item.target.exists() {
                remove_path(&item.target).map_err(|source| io_at(&item.target, source))?;
            }
            fs::rename(&backup, &item.target).map_err(|source| io_at(&item.target, source))?;
        } else if !staged_root.join(&item.handle).exists() && item.target.exists() {
            // A missing staged directory proves this newly-created target was committed.
            remove_path(&item.target).map_err(|source| io_at(&item.target, source))?;
        }
    }
    if transaction.exists() {
        remove_path(transaction).map_err(|source| io_at(transaction, source))?;
    }
    Ok(())
}

fn render_configuration(
    registration: &RemoteExtensionRegistration,
    handle: &str,
) -> ImportResult<String> {
    let transformed = transform_configuration(registration, handle)?;
    let mut table = match &transformed {
        serde_json::Value::Null => toml::Table::new(),
        serde_json::Value::Object(_) => toml::Value::try_from(&transformed)
            .map_err(|error| ExtensionImportError::Configuration {
                uuid: registration.uuid.clone(),
                message: error.to_string(),
            })?
            .as_table()
            .cloned()
            .ok_or_else(|| ExtensionImportError::Configuration {
                uuid: registration.uuid.clone(),
                message: "configuration must serialize as a TOML table".into(),
            })?,
        _ => {
            return Err(ExtensionImportError::Configuration {
                uuid: registration.uuid.clone(),
                message: "configuration must be a JSON object".into(),
            });
        }
    };
    table.insert(
        "name".into(),
        toml::Value::String(registration.title.clone()),
    );
    table.insert("handle".into(), toml::Value::String(handle.into()));
    table.insert(
        "type".into(),
        toml::Value::String(local_extension_type(&registration.extension_type).into()),
    );
    let mut document = toml::Table::new();
    document.insert(
        "extensions".into(),
        toml::Value::Array(vec![toml::Value::Table(table)]),
    );
    toml::to_string_pretty(&document).map_err(|error| ExtensionImportError::Configuration {
        uuid: registration.uuid.clone(),
        message: error.to_string(),
    })
}

fn load_state(path: &Path) -> ImportResult<IdentifierState> {
    match fs::read(path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|source| ExtensionImportError::InvalidState {
                path: path.to_owned(),
                source,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(IdentifierState::default()),
        Err(source) => Err(ExtensionImportError::ReadState {
            path: path.to_owned(),
            source,
        }),
    }
}

fn unique_handle(title: &str, uuid: &str, reserved: &mut BTreeSet<String>) -> String {
    let mut base = slug(title);
    if base.is_empty() {
        base = "extension".into();
    }
    base = truncate_handle(&base, MAX_HANDLE_BYTES);
    let mut candidate = base.clone();
    if reserved.contains(&candidate) {
        let suffix = slug(uuid);
        let suffix = truncate_handle(if suffix.is_empty() { "remote" } else { &suffix }, 10);
        let prefix_len = MAX_HANDLE_BYTES.saturating_sub(suffix.len() + 1);
        candidate = format!("{}-{suffix}", truncate_handle(&base, prefix_len));
        let mut index = 2;
        while reserved.contains(&candidate) {
            let numbered = format!("-{index}");
            candidate = format!(
                "{}-{suffix}{numbered}",
                truncate_handle(&base, MAX_HANDLE_BYTES - suffix.len() - numbered.len() - 1)
            );
            index += 1;
        }
    }
    reserved.insert(candidate.clone());
    candidate
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !output.is_empty() {
                output.push('-');
            }
            output.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
    }
    output.trim_matches('-').to_owned()
}

fn truncate_handle(value: &str, max: usize) -> String {
    value
        .get(..value.len().min(max))
        .unwrap_or(value)
        .trim_matches('-')
        .to_owned()
}

fn absolute_lexical(path: &Path) -> ImportResult<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|source| io_at(path, source))?
            .join(path)
    };
    normalize(&path).ok_or(ExtensionImportError::PathEscape(path))
}

fn confined_join(root: &Path, child: &Path) -> ImportResult<PathBuf> {
    if child.is_absolute() {
        return Err(ExtensionImportError::PathEscape(child.to_owned()));
    }
    let joined = normalize(&root.join(child))
        .ok_or_else(|| ExtensionImportError::PathEscape(root.join(child)))?;
    if !joined.starts_with(root) {
        return Err(ExtensionImportError::PathEscape(joined));
    }
    Ok(joined)
}

fn normalize(path: &Path) -> Option<PathBuf> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => output.push(prefix.as_os_str()),
            Component::RootDir => output.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    return None;
                }
            }
            Component::Normal(part) => output.push(part),
        }
    }
    Some(output)
}

fn ensure_no_symlink_ancestors(root: &Path, target: &Path) -> ImportResult<()> {
    if !target.starts_with(root) {
        return Err(ExtensionImportError::PathEscape(target.to_owned()));
    }
    let mut current = root.to_owned();
    let relative = target.strip_prefix(root).expect("prefix checked");
    for part in relative.components() {
        current.push(part.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ExtensionImportError::Symlink(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(source) => return Err(io_at(&current, source)),
        }
    }
    Ok(())
}

fn create_dir_all(path: &Path) -> ImportResult<()> {
    fs::create_dir_all(path).map_err(|source| io_at(path, source))
}

fn remove_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn io_at(path: &Path, source: io::Error) -> ExtensionImportError {
    ExtensionImportError::Io {
        path: path.to_owned(),
        source,
    }
}
