use cfy_upgrade::{
    CARGO_PACKAGE, DetectionContext, ExecutionPolicy, HOMEBREW_FORMULA, InstallProvenance,
    NPM_PACKAGE, UpdateCache, UpgradeError, UpgradePlan, detect_with, execute_standalone,
    fetch_latest_version, plan, read_update_cache, write_update_cache,
};
use std::{fs, path::PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn context(executable: impl Into<PathBuf>) -> DetectionContext {
    DetectionContext {
        executable: executable.into(),
        cargo_home: None,
        home: None,
        homebrew_prefix: None,
        install_channel: None,
    }
}

#[cfg(unix)]
#[test]
fn detects_legacy_shell_installer_layout_without_a_marker() {
    use std::os::unix::fs::symlink;
    let home = std::env::temp_dir().join(format!(
        "cfy-upgrade-legacy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let bin = home.join(".local/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("cfy"), b"legacy").unwrap();
    symlink("cfy", bin.join("catify")).unwrap();
    let mut detection = context(bin.join("cfy"));
    detection.home = Some(home.clone());
    assert!(matches!(
        detect_with(&detection),
        InstallProvenance::Standalone { .. }
    ));
    fs::remove_dir_all(home).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn standalone_upgrade_verifies_extracts_and_replaces_the_binary() {
    use flate2::{Compression, write::GzEncoder};
    use sha2::{Digest, Sha256};

    let root = std::env::temp_dir().join(format!(
        "cfy-upgrade-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let executable = root.join("cfy");
    let version_file = root.join(".catify-version");
    fs::write(&executable, b"old-binary").unwrap();
    fs::write(&version_file, b"0.0.1-pre.2\n").unwrap();

    let target = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    let archive_name = format!("cfy-v0.0.1-pre.3-{target}.tar.gz");
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(encoder);
    let binary = b"new-binary";
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    header.set_size(binary.len() as u64);
    header.set_cksum();
    tar.append_data(
        &mut header,
        format!("cfy-v0.0.1-pre.3-{target}/cfy"),
        &binary[..],
    )
    .unwrap();
    let encoder = tar.into_inner().unwrap();
    let archive = encoder.finish().unwrap();
    let digest = format!("{:x}", Sha256::digest(&archive));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let releases = format!(
        r#"[{{"tag_name":"v0.0.1-pre.3","draft":false,"assets":[{{"name":"{archive_name}","browser_download_url":"http://{address}/archive"}},{{"name":"SHA256SUMS","browser_download_url":"http://{address}/sums"}}]}}]"#
    );
    let sums = format!("{digest}  {archive_name}\n");
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let read = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            let body = if request.starts_with("GET /releases ") {
                releases.as_bytes()
            } else if request.starts_with("GET /archive ") {
                archive.as_slice()
            } else {
                sums.as_bytes()
            };
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
        }
    });
    let plan = UpgradePlan::Standalone {
        executable: executable.clone(),
        version_file: version_file.clone(),
    };
    let outcome = execute_standalone(
        &plan,
        &semver::Version::parse("0.0.1-pre.2").unwrap(),
        &format!("http://{address}/releases"),
    )
    .await
    .unwrap();
    assert!(outcome.changed);
    assert_eq!(fs::read(&executable).unwrap(), b"new-binary");
    assert_eq!(fs::read_to_string(&version_file).unwrap(), "0.0.1-pre.3\n");
    server.await.unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[test]
fn detects_windows_executable_suffix_in_cargo_home() {
    let mut context = context(r"C:\Users\me\.cargo\bin\cfy.exe");
    context.cargo_home = Some(r"C:\Users\me\.cargo".into());
    assert!(matches!(
        detect_with(&context),
        InstallProvenance::Cargo { .. }
    ));
}

#[test]
fn detects_long_catify_command_in_cargo_home() {
    let mut context = context("/users/me/.cargo/bin/catify");
    context.cargo_home = Some("/users/me/.cargo".into());
    assert!(matches!(
        detect_with(&context),
        InstallProvenance::Cargo { .. }
    ));
}

#[test]
fn detects_homebrew_cellar_and_builds_exact_plan() {
    let provenance = detect_with(&context("/opt/homebrew/Cellar/catify/1.2.3/bin/cfy"));
    assert_eq!(provenance.kind().to_string(), "homebrew");
    let plan = plan(&provenance).unwrap();
    assert_eq!(plan.command().unwrap().display(), "brew upgrade catify");
    assert!(
        matches!(plan, UpgradePlan::Homebrew { ref formula, .. } if formula == HOMEBREW_FORMULA)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn execution_returns_the_childs_exact_nonzero_exit_status() {
    let plan = UpgradePlan::Cargo {
        package: CARGO_PACKAGE.into(),
        command: cfy_upgrade::UpgradeCommand {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 23".into()],
        },
    };
    let output = cfy_upgrade::execute(
        &plan,
        ExecutionPolicy::INTERACTIVE,
        &cfy_process::Supervisor::default(),
    )
    .await
    .unwrap();
    assert_eq!(output.exit_code(), Some(23));
}

#[test]
fn detects_configured_homebrew_prefix() {
    let mut context = context("/brew/Cellar/catify/1.0/bin/cfy");
    context.homebrew_prefix = Some("/brew".into());
    assert!(matches!(
        detect_with(&context),
        InstallProvenance::Homebrew { .. }
    ));
}

#[test]
fn detects_cargo_home_and_builds_exact_locked_plan() {
    let mut context = context("/users/me/.cargo/bin/cfy");
    context.cargo_home = Some("/users/me/.cargo".into());
    let provenance = detect_with(&context);
    assert!(
        matches!(provenance, InstallProvenance::Cargo { ref package, .. } if package == CARGO_PACKAGE)
    );
    assert_eq!(
        plan(&provenance).unwrap().command().unwrap().display(),
        "cargo install cfy-cli --locked"
    );
}

#[test]
fn standalone_requires_archive_version_marker() {
    let root = std::env::temp_dir().join(format!("cfy-upgrade-standalone-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("cfy"), b"binary").unwrap();
    fs::write(root.join("VERSION"), b"1.2.3\n").unwrap();
    let provenance = detect_with(&context(root.join("cfy")));
    assert!(matches!(provenance, InstallProvenance::Standalone { .. }));
    assert!(matches!(
        plan(&provenance).unwrap(),
        UpgradePlan::Standalone { .. }
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn source_checkout_wins_over_channel_hint_and_is_refused() {
    let root = std::env::temp_dir().join(format!("cfy-upgrade-source-{}", std::process::id()));
    let executable = root.join("target/debug/cfy");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
    fs::write(&executable, b"binary").unwrap();
    let mut context = context(&executable);
    context.install_channel = Some("cargo".into());
    let provenance = detect_with(&context);
    assert!(matches!(provenance, InstallProvenance::Source { .. }));
    assert!(matches!(
        plan(&provenance),
        Err(UpgradeError::SourceInstall { .. })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unknown_install_is_typed_and_refused() {
    let provenance = detect_with(&context("/some/custom/bin/cfy"));
    assert!(matches!(provenance, InstallProvenance::Unknown { .. }));
    assert!(matches!(
        plan(&provenance),
        Err(UpgradeError::UnknownInstall { .. })
    ));
}

#[test]
fn npm_channel_builds_global_package_upgrade_plan() {
    let mut context = context("/some/custom/bin/cfy");
    context.install_channel = Some("npm".into());
    let provenance = detect_with(&context);
    assert!(matches!(
        provenance,
        InstallProvenance::Npm { ref package, .. } if package == NPM_PACKAGE
    ));
    assert_eq!(
        plan(&provenance).unwrap().command().unwrap().display(),
        "npm install --global catify-cli@latest"
    );
}

#[test]
fn update_cache_is_fresh_and_only_reports_newer_semver() {
    let cache = UpdateCache {
        checked_at: 1_000,
        latest_version: Some("1.3.0".into()),
    };
    assert!(cache.is_fresh_at(1_100));
    assert!(!cache.is_fresh_at(1_000 + 24 * 60 * 60));
    assert_eq!(cache.available_version("1.2.9"), Some("1.3.0"));
    assert_eq!(cache.available_version("1.3.0"), None);
    assert_eq!(cache.available_version("2.0.0"), None);
}

#[test]
fn update_cache_round_trips_atomically() {
    let root = std::env::temp_dir().join(format!(
        "cfy-update-cache-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = root.join("nested/update.json");
    let cache = UpdateCache {
        checked_at: 42,
        latest_version: Some("9.8.7".into()),
    };
    write_update_cache(&path, &cache).unwrap();
    assert_eq!(read_update_cache(&path).unwrap(), Some(cache));
    let replacement = UpdateCache {
        checked_at: 43,
        latest_version: None,
    };
    write_update_cache(&path, &replacement).unwrap();
    assert_eq!(read_update_cache(&path).unwrap(), Some(replacement));
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn fetches_latest_github_release_tag_as_semver() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        let body = r#"{"tag_name":"v2.4.1"}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let latest = fetch_latest_version(&format!("http://{address}/latest"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.to_string(), "2.4.1");
    server.await.unwrap();
}

#[tokio::test]
async fn noninteractive_execution_requires_explicit_approval_before_spawning() {
    let plan = UpgradePlan::Cargo {
        package: CARGO_PACKAGE.into(),
        command: cfy_upgrade::UpgradeCommand {
            program: "this-program-must-not-run".into(),
            args: vec![],
        },
    };
    let result = cfy_upgrade::execute(
        &plan,
        ExecutionPolicy::NON_INTERACTIVE_REFUSE,
        &cfy_process::Supervisor::default(),
    )
    .await;
    assert_eq!(
        result.unwrap_err(),
        UpgradeError::NonInteractiveApprovalRequired
    );
}
