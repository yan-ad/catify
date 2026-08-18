#![cfg(unix)]

use cfy_extension_adapter::{
    Adapter, AdapterCommand, BuildJob, BuildRequest, Parallelism, build_all,
};
use cfy_process::Supervisor;
use semver::VersionReq;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Instant};
use tempfile::TempDir;

fn fixture(name: &str) -> (TempDir, PathBuf) {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(name);
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
        &path,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    (directory, path)
}

fn fake_adapter() -> (TempDir, PathBuf) {
    fixture("fake-adapter.sh")
}

#[tokio::test]
async fn discovers_and_builds_using_machine_readable_protocol() {
    let (_directory, path) = fake_adapter();
    let supervisor = Supervisor::default();
    let adapter = Adapter::discover(
        &supervisor,
        AdapterCommand::new(path),
        Some(&VersionReq::parse("^1.0").unwrap()),
    )
    .await
    .unwrap();

    assert_eq!(adapter.info().name, "fake");
    let response = adapter
        .build(
            &supervisor,
            &BuildRequest::new("ui_extension", "extension", "output"),
        )
        .await
        .unwrap();
    assert_eq!(response.artifacts, vec![PathBuf::from("dist/main.js")]);
}

#[tokio::test]
async fn missing_and_incompatible_adapters_have_actionable_errors() {
    let supervisor = Supervisor::default();
    let missing = Adapter::discover(
        &supervisor,
        AdapterCommand::new("definitely-not-a-real-cfy-adapter"),
        None,
    )
    .await
    .unwrap_err();
    assert!(missing.to_string().contains("install it"));

    let (_directory, path) = fake_adapter();
    let incompatible = Adapter::discover(
        &supervisor,
        AdapterCommand::new(path),
        Some(&VersionReq::parse(">=2").unwrap()),
    )
    .await
    .unwrap_err();
    assert!(
        incompatible
            .to_string()
            .contains("install a compatible adapter")
    );
}

#[tokio::test]
async fn rejects_unsupported_protocol_versions() {
    let (_directory, path) = fixture("incompatible-adapter.sh");
    let supervisor = Supervisor::default();
    let error = Adapter::discover(&supervisor, AdapterCommand::new(path), None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("install a compatible adapter"));
}

#[tokio::test]
async fn memory_budget_constrains_configured_parallelism() {
    let (_directory, path) = fake_adapter();
    let supervisor = Supervisor::default();
    let adapter = Adapter::discover(&supervisor, AdapterCommand::new(path), None)
        .await
        .unwrap();
    let jobs = (0..2)
        .map(|_| BuildJob {
            request: BuildRequest::new("ui_extension", "extension", "output"),
            memory_mb: 150,
        })
        .collect();

    // A single 200 MiB budget cannot admit both 150 MiB jobs. The fake's sleep is
    // inherited through Supervisor, making elapsed time a focused scheduler assertion.
    unsafe { std::env::set_var("CFY_FAKE_SLEEP", "0.15") };
    let started = Instant::now();
    let responses = build_all(
        &supervisor,
        &adapter,
        jobs,
        Parallelism {
            max_jobs: 2,
            max_memory_mb: 200,
        },
    )
    .await
    .unwrap();
    unsafe { std::env::remove_var("CFY_FAKE_SLEEP") };

    assert_eq!(responses.len(), 2);
    assert!(started.elapsed().as_millis() >= 250);
}

#[tokio::test]
async fn rejects_jobs_larger_than_memory_budget() {
    let (_directory, path) = fake_adapter();
    let supervisor = Supervisor::default();
    let adapter = Adapter::discover(&supervisor, AdapterCommand::new(path), None)
        .await
        .unwrap();
    let error = build_all(
        &supervisor,
        &adapter,
        vec![BuildJob {
            request: BuildRequest::new("ui_extension", "extension", "output"),
            memory_mb: 300,
        }],
        Parallelism {
            max_jobs: 4,
            max_memory_mb: 256,
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("increase max_memory_mb"));
}
