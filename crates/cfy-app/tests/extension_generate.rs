use cfy_app::extension_generate::{GenerateExtensionOptions, generate_extension};
use cfy_process::Supervisor;
use std::{fs, path::Path, process::Command, time::Duration};
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlinked_extensions_directory() {
    use std::os::unix::fs::symlink;

    let repo = fixture_repository();
    let app = app_root();
    let outside = TempDir::new().unwrap();
    symlink(outside.path(), app.path().join("extensions")).unwrap();
    let error = generate_extension(
        &Supervisor::new(Duration::from_secs(2)),
        &GenerateExtensionOptions {
            app_directory: app.path().to_owned(),
            name: "Unsafe".into(),
            template: "checkout_ui_extension".into(),
            flavor: Some("typescript".into()),
            repository: Some(repo.path().to_string_lossy().into_owned()),
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("escapes"));
    assert!(!outside.path().join("unsafe").exists());
}

fn fixture_repository() -> TempDir {
    let repo = TempDir::new().unwrap();
    fs::create_dir_all(repo.path().join("checkout-extension/src")).unwrap();
    fs::write(
        repo.path().join("checkout-extension/shopify.extension.toml.liquid"),
        "name = \"{{ name }}\"\nhandle = \"{{ handle }}\"\ntype = \"{{ type }}\"\nuid = \"{{ uid }}\"\n{% if flavor == 'typescript' %}typescript = true\n{% endif %}",
    ).unwrap();
    fs::write(
        repo.path().join("checkout-extension/src/index.liquid"),
        "export const name = '{{name}}';\n",
    )
    .unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["add", "."]);
    git(
        repo.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.com",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    repo
}

fn app_root() -> TempDir {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("shopify.app.toml"),
        "client_id = \"fixture\"\n",
    )
    .unwrap();
    root
}

#[tokio::test]
async fn clones_and_renders_official_style_template() {
    let repo = fixture_repository();
    let app = app_root();
    let report = generate_extension(
        &Supervisor::new(Duration::from_secs(2)),
        &GenerateExtensionOptions {
            app_directory: app.path().to_owned(),
            name: "Checkout Helper".into(),
            template: "checkout_ui_extension".into(),
            flavor: Some("typescript".into()),
            repository: Some(repo.path().to_string_lossy().into_owned()),
        },
    )
    .await
    .unwrap();
    assert_eq!(report.handle, "checkout-helper");
    let config = fs::read_to_string(report.directory.join("shopify.extension.toml")).unwrap();
    assert!(config.contains("name = \"Checkout Helper\""));
    assert!(config.contains("handle = \"checkout-helper\""));
    assert!(config.contains("type = \"checkout_ui_extension\""));
    assert!(config.contains("typescript = true"));
    assert!(!config.contains("{{"));
    assert_eq!(
        fs::read_to_string(report.directory.join("src/index.ts"))
            .unwrap()
            .replace("\r\n", "\n"),
        "export const name = 'Checkout Helper';\n"
    );
    assert!(!report.directory.join(".git").exists());
}

#[tokio::test]
async fn rejects_existing_destination_without_modifying_it() {
    let repo = fixture_repository();
    let app = app_root();
    let destination = app.path().join("extensions/existing");
    fs::create_dir_all(&destination).unwrap();
    fs::write(destination.join("keep"), "keep").unwrap();
    let error = generate_extension(
        &Supervisor::new(Duration::from_secs(2)),
        &GenerateExtensionOptions {
            app_directory: app.path().to_owned(),
            name: "Existing".into(),
            template: "checkout_ui_extension".into(),
            flavor: Some("typescript".into()),
            repository: Some(repo.path().to_string_lossy().into_owned()),
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("already exists"));
    assert_eq!(
        fs::read_to_string(destination.join("keep")).unwrap(),
        "keep"
    );
}
