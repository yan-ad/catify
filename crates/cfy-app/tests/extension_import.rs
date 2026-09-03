use async_trait::async_trait;
use cfy_app::extension_import::{
    ExistingDirectoryPolicy, ExtensionRegistrationProvider, ImportExtensionsOptions, ImportOutcome,
    ImportSelection, RemoteExtensionRegistration, import_extension_registrations,
    import_extensions,
};
use cfy_core::{Error, Result};
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);
impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cfy-extension-import-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn decodes_dashboard_registration_version_configuration() {
    let registration: RemoteExtensionRegistration = serde_json::from_value(json!({
        "uuid": "dashboard-uuid",
        "title": "Dashboard extension",
        "type": "flow_action",
        "draftVersion": {
            "config": "{\"description\":\"from dashboard\",\"settings\":{\"enabled\":true}}"
        },
        "activeVersion": {
            "config": "{\"description\":\"old\"}"
        }
    }))
    .unwrap();
    assert_eq!(registration.configuration["description"], "from dashboard");
    assert_eq!(registration.configuration["settings"]["enabled"], true);
}

fn registration(uuid: &str, title: &str) -> RemoteExtensionRegistration {
    RemoteExtensionRegistration {
        uuid: uuid.into(),
        title: title.into(),
        extension_type: "theme".into(),
        configuration: json!({"description": "fixture"}),
    }
}

fn options(root: &Path) -> ImportExtensionsOptions {
    ImportExtensionsOptions {
        app_directory: root.into(),
        client_id: "client-id".into(),
        organization_id: "organization-id".into(),
        selection: ImportSelection::All,
        existing_directory_policy: ExistingDirectoryPolicy::Skip,
    }
}

#[test]
fn imports_all_and_selected_registrations_deterministically() {
    let all = TempDir::new("all");
    let report = import_extension_registrations(
        vec![registration("b", "Second"), registration("a", "First")],
        &options(all.path()),
    )
    .unwrap();
    assert_eq!(
        report
            .items
            .iter()
            .map(|item| item.uuid.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(
        all.path()
            .join("extensions/first/shopify.extension.toml")
            .is_file()
    );
    let state = fs::read_to_string(all.path().join(".shopify/extension-identifiers.json")).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&state).unwrap()["extensions"]["a"],
        "first"
    );

    let selected = TempDir::new("selected");
    let mut uuids = BTreeSet::new();
    uuids.insert("b".into());
    let mut selected_options = options(selected.path());
    selected_options.selection = ImportSelection::Uuids(uuids);
    let report = import_extension_registrations(
        vec![registration("a", "First"), registration("b", "Second")],
        &selected_options,
    )
    .unwrap();
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.items[0].uuid, "b");
    assert!(!selected.path().join("extensions/first").exists());
}

#[test]
fn reports_already_imported_without_rewriting() {
    let root = TempDir::new("already");
    let initial =
        import_extension_registrations(vec![registration("a", "Original")], &options(root.path()))
            .unwrap();
    let config = root
        .path()
        .join("extensions/original/shopify.extension.toml");
    fs::write(&config, "local = true\n").unwrap();
    let report =
        import_extension_registrations(vec![registration("a", "Changed")], &options(root.path()))
            .unwrap();
    assert_eq!(report.items[0].outcome, ImportOutcome::AlreadyImported);
    assert_eq!(report.items[0].handle, initial.items[0].handle);
    assert_eq!(fs::read_to_string(config).unwrap(), "local = true\n");
}

#[test]
fn explicitly_skips_or_overwrites_existing_directories() {
    let skipped = TempDir::new("skip");
    let target = skipped.path().join("extensions/collision");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("keep.txt"), "keep").unwrap();
    let report = import_extension_registrations(
        vec![registration("a", "Collision")],
        &options(skipped.path()),
    )
    .unwrap();
    assert_eq!(
        report.items[0].outcome,
        ImportOutcome::SkippedExistingDirectory
    );
    assert!(target.join("keep.txt").exists());
    assert!(
        !skipped
            .path()
            .join(".shopify/extension-identifiers.json")
            .exists()
    );

    let overwritten = TempDir::new("overwrite");
    let target = overwritten.path().join("extensions/collision");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("old.txt"), "old").unwrap();
    let mut overwrite = options(overwritten.path());
    overwrite.existing_directory_policy = ExistingDirectoryPolicy::Overwrite;
    let report =
        import_extension_registrations(vec![registration("a", "Collision")], &overwrite).unwrap();
    assert_eq!(report.items[0].outcome, ImportOutcome::Imported);
    assert!(!target.join("old.txt").exists());
    assert!(target.join("shopify.extension.toml").exists());
}

#[test]
fn confines_generated_paths_and_truncates_handles() {
    let root = TempDir::new("confine");
    let outside = root.path().parent().unwrap().join("escaped-extension");
    let _ = fs::remove_dir_all(&outside);
    let title = format!("../../escaped-extension {}", "A".repeat(100));
    let report =
        import_extension_registrations(vec![registration("a", &title)], &options(root.path()))
            .unwrap();
    let handle = &report.items[0].handle;
    assert!(handle.len() <= 50);
    assert!(!handle.contains('/') && !handle.contains(".."));
    assert!(
        root.path()
            .join("extensions")
            .join(handle)
            .starts_with(root.path())
    );
    assert!(!outside.exists());
}

#[test]
fn rolls_back_staging_and_existing_content_when_a_later_item_fails() {
    let root = TempDir::new("rollback");
    let existing = root.path().join("extensions/first");
    fs::create_dir_all(&existing).unwrap();
    fs::write(existing.join("original.txt"), "original").unwrap();
    let mut invalid = registration("b", "Second");
    invalid.configuration = json!(["not", "an", "object"]);
    let mut overwrite = options(root.path());
    overwrite.existing_directory_policy = ExistingDirectoryPolicy::Overwrite;
    let error =
        import_extension_registrations(vec![registration("a", "First"), invalid], &overwrite)
            .unwrap_err();
    assert!(error.to_string().contains("configuration"));
    assert_eq!(
        fs::read_to_string(existing.join("original.txt")).unwrap(),
        "original"
    );
    assert!(!root.path().join("extensions/second").exists());
    let leftovers = fs::read_dir(root.path().join("extensions"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".cfy-import-")
        })
        .count();
    assert_eq!(leftovers, 0);
    assert!(
        !root
            .path()
            .join(".shopify/extension-identifiers.json")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn rejects_symlinked_extension_root() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new("symlink");
    let outside = TempDir::new("outside");
    symlink(outside.path(), root.path().join("extensions")).unwrap();
    let error =
        import_extension_registrations(vec![registration("a", "Unsafe")], &options(root.path()))
            .unwrap_err();
    assert!(error.to_string().contains("symbolic link"));
    assert!(!outside.path().join("unsafe").exists());
}

struct FixtureProvider(Vec<RemoteExtensionRegistration>);
#[async_trait]
impl ExtensionRegistrationProvider for FixtureProvider {
    async fn fetch_extension_registrations(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Vec<RemoteExtensionRegistration>> {
        Ok(self.0.clone())
    }
}

struct FailingProvider;
#[async_trait]
impl ExtensionRegistrationProvider for FailingProvider {
    async fn fetch_extension_registrations(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Vec<RemoteExtensionRegistration>> {
        Err(Error::api("fixture failure"))
    }
}

#[tokio::test]
async fn imports_through_provider_fixture_and_returns_provider_failure() {
    let root = TempDir::new("provider");
    let report = import_extensions(
        &FixtureProvider(vec![registration("a", "Fixture")]),
        &options(root.path()),
    )
    .await
    .unwrap();
    assert_eq!(report.items[0].handle, "fixture");
    let error = import_extensions(&FailingProvider, &options(root.path()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("fixture failure"));
}
