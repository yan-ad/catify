use cfy_plugins::{
    LinkOptions, PackageManagerConfig, PluginKind, PluginRecord, PluginRegistry, PluginService,
    ResetOptions,
};
use cfy_process::Supervisor;
use std::{fs, path::Path};
use tempfile::TempDir;

fn record(name: &str, kind: PluginKind, path: &Path) -> PluginRecord {
    PluginRecord {
        name: name.into(),
        source: format!("source-{name}"),
        kind,
        path: path.into(),
        version: Some("1.2.3".into()),
    }
}

#[test]
fn registry_roundtrip_is_deterministic() {
    let temp = TempDir::new().unwrap();
    let registry = PluginRegistry::new(temp.path());
    registry
        .upsert(record("zeta", PluginKind::Installed, Path::new("z")))
        .unwrap();
    registry
        .upsert(record("alpha", PluginKind::Linked, Path::new("a")))
        .unwrap();
    assert_eq!(registry.all().unwrap()[0].name, "alpha");
    let first = fs::read(registry.registry_path()).unwrap();
    registry
        .upsert(record("alpha", PluginKind::Linked, Path::new("a")))
        .unwrap();
    assert_eq!(first, fs::read(registry.registry_path()).unwrap());
}

#[test]
fn linked_record_overrides_installed_record() {
    let temp = TempDir::new().unwrap();
    let registry = PluginRegistry::new(temp.path());
    registry
        .upsert(record("plugin", PluginKind::Linked, Path::new("link")))
        .unwrap();
    registry
        .upsert(record(
            "plugin",
            PluginKind::Installed,
            Path::new("installed"),
        ))
        .unwrap();
    assert_eq!(
        registry.find("plugin").unwrap().unwrap().kind,
        PluginKind::Linked
    );
    registry.remove_kind("plugin", PluginKind::Linked).unwrap();
    assert_eq!(
        registry.find("plugin").unwrap().unwrap().kind,
        PluginKind::Installed
    );
}

#[test]
fn corrupt_registry_is_an_actionable_configuration_error() {
    let temp = TempDir::new().unwrap();
    fs::write(temp.path().join("registry.json"), b"not-json").unwrap();
    let error = PluginRegistry::new(temp.path()).all().unwrap_err();
    assert_eq!(error.kind(), cfy_core::ErrorKind::Config);
    assert!(error.message().contains("corrupt JSON"));
}

#[tokio::test]
async fn link_validates_manifest_and_can_run_install() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("root");
    let plugin = temp.path().join("plugin");
    fs::create_dir(&plugin).unwrap();
    let service = service(&root, fake_program());
    assert!(service.link(&plugin, LinkOptions::default()).await.is_err());
    fs::write(
        plugin.join("package.json"),
        r#"{"name":"linked-plugin","version":"1.0.0"}"#,
    )
    .unwrap();
    let result = service
        .link(
            &plugin,
            LinkOptions {
                install_dependencies: true,
                verbose: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(result.plugin.unwrap().name, "linked-plugin");
    assert!(
        result
            .process
            .unwrap()
            .arguments
            .contains(&"install".into())
    );
}

#[tokio::test]
async fn package_manager_argv_is_portably_observable() {
    let temp = TempDir::new().unwrap();
    let service = service(temp.path(), fake_program());
    let result = service.install("example-plugin@1.2.3").await.unwrap();
    let process = result.process.unwrap();
    assert_eq!(process.exit_code, Some(0));
    assert_eq!(process.arguments[0], "install");
    assert_eq!(process.arguments[3], "--no-save");
    assert_eq!(process.arguments[4], "example-plugin@1.2.3");
}

#[tokio::test]
async fn reset_only_removes_managed_artifacts() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("root");
    let outside = temp.path().join("keep");
    fs::create_dir_all(root.join("installed/item")).unwrap();
    fs::write(&outside, "safe").unwrap();
    let service = service(&root, fake_program());
    service
        .reset(ResetOptions {
            hard: true,
            reinstall: false,
        })
        .await
        .unwrap();
    assert!(!root.join("installed").exists());
    assert_eq!(fs::read_to_string(outside).unwrap(), "safe");
}

#[tokio::test]
async fn process_results_redact_secrets() {
    let temp = TempDir::new().unwrap();
    let service = service(temp.path(), fake_program());
    let result = service.install("example-plugin").await.unwrap();
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("hunter2"));
    assert!(json.contains("REDACTED"));

    let error = service
        .install("https://secret-token@example.test/plugin.git?token=hunter2")
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("hunter2"));
}

fn service(root: &Path, executable: std::path::PathBuf) -> PluginService {
    PluginService::new(
        root,
        PackageManagerConfig { executable },
        Supervisor::default(),
    )
}

fn fake_program() -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!("cfy-plugin-fake-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    #[cfg(windows)]
    let path = directory.join("fake-pm.cmd");
    #[cfg(not(windows))]
    let path = directory.join("fake-pm");

    #[cfg(windows)]
    fs::write(&path, "@echo off\r\necho token=hunter2\r\nexit /b 0\r\n").unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::write(&path, "#!/bin/sh\nprintf 'token=hunter2\\n'\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    path
}
