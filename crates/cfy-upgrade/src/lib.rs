//! Safe, provenance-aware upgrade planning for Catify.
//!
//! This crate deliberately separates detection, planning, and execution. Merely
//! detecting an installation or creating a plan never changes the machine.

use cfy_core::{Error, ErrorKind, Result};
use cfy_process::{OutputMode, ProcessOutput, ProcessSpec, Supervisor};
use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

pub const HOMEBREW_FORMULA: &str = "catify";
pub const CARGO_PACKAGE: &str = "cfy-cli";
pub const NPM_PACKAGE: &str = "catify-cli";
pub const EXECUTABLE_NAME: &str = "cfy";
pub const DEFAULT_RELEASE_API_URL: &str =
    "https://api.github.com/repos/yan-ad/catify/releases/latest";
pub const DEFAULT_RELEASES_API_URL: &str =
    "https://api.github.com/repos/yan-ad/catify/releases?per_page=30";
pub const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// The mechanism which placed the running executable on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallProvenance {
    Homebrew {
        executable: PathBuf,
        formula: String,
    },
    Cargo {
        executable: PathBuf,
        package: String,
    },
    Npm {
        executable: PathBuf,
        package: String,
    },
    /// A binary unpacked from a Catify release archive. `version_file` is the
    /// archive's adjacent `VERSION` marker.
    Standalone {
        executable: PathBuf,
        version_file: PathBuf,
    },
    /// An executable inside this repository's Cargo target directory.
    Source {
        executable: PathBuf,
        workspace_root: PathBuf,
    },
    Unknown {
        executable: PathBuf,
        reason: String,
    },
}

fn release_target() -> std::result::Result<&'static str, UpgradeError> {
    match (env::consts::OS, env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        (os, arch) => Err(UpgradeError::ReleaseDiscovery {
            message: format!("unsupported release platform {os}/{arch}"),
        }),
    }
}

fn checksum_for(manifest: &str, asset: &str) -> Option<String> {
    manifest.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        (name == asset && digest.len() == 64).then(|| digest.to_ascii_lowercase())
    })
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn secure_url(url: &str) -> std::result::Result<reqwest::Url, UpgradeError> {
    let parsed = reqwest::Url::parse(url).map_err(|error| UpgradeError::ReleaseDiscovery {
        message: error.to_string(),
    })?;
    let loopback_http = parsed.scheme() == "http"
        && parsed.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if parsed.scheme() != "https" && !loopback_http {
        return Err(UpgradeError::ReleaseDiscovery {
            message: "release URLs must use HTTPS (HTTP is allowed only for loopback tests)".into(),
        });
    }
    Ok(parsed)
}

fn extract_binary(
    archive_name: &str,
    bytes: Vec<u8>,
) -> std::result::Result<Vec<u8>, UpgradeError> {
    let binary_name = if cfg!(windows) { "cfy.exe" } else { "cfy" };
    if archive_name.ends_with(".zip") {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| UpgradeError::Archive {
                message: error.to_string(),
            })?;
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|error| UpgradeError::Archive {
                    message: error.to_string(),
                })?;
            if entry.is_file()
                && Path::new(entry.name()).file_name() == Some(OsStr::new(binary_name))
            {
                let mut binary = Vec::new();
                entry
                    .read_to_end(&mut binary)
                    .map_err(|error| UpgradeError::Archive {
                        message: error.to_string(),
                    })?;
                return Ok(binary);
            }
        }
    } else {
        let decoder = GzDecoder::new(Cursor::new(bytes));
        let mut archive = tar::Archive::new(decoder);
        let entries = archive.entries().map_err(|error| UpgradeError::Archive {
            message: error.to_string(),
        })?;
        for entry in entries {
            let mut entry = entry.map_err(|error| UpgradeError::Archive {
                message: error.to_string(),
            })?;
            let path = entry.path().map_err(|error| UpgradeError::Archive {
                message: error.to_string(),
            })?;
            if entry.header().entry_type().is_file()
                && path.file_name() == Some(OsStr::new(binary_name))
            {
                let mut binary = Vec::new();
                entry
                    .read_to_end(&mut binary)
                    .map_err(|error| UpgradeError::Archive {
                        message: error.to_string(),
                    })?;
                return Ok(binary);
            }
        }
    }
    Err(UpgradeError::Archive {
        message: format!("archive does not contain {binary_name}"),
    })
}

async fn release_assets(
    releases_url: &str,
    current: &Version,
) -> std::result::Result<Option<(Version, String, String, String)>, UpgradeError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .user_agent(concat!("catify/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| UpgradeError::ReleaseDiscovery {
            message: error.to_string(),
        })?;
    let releases = client
        .get(secure_url(releases_url)?)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| UpgradeError::ReleaseDiscovery {
            message: error.to_string(),
        })?
        .json::<Vec<Release>>()
        .await
        .map_err(|error| UpgradeError::ReleaseDiscovery {
            message: error.to_string(),
        })?;
    let target = release_target()?;
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    let archive_name = format!("cfy-v{{version}}-{target}.{extension}");
    let release = releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            Version::parse(release.tag_name.trim_start_matches('v'))
                .ok()
                .map(|version| (version, release.assets))
        })
        .filter(|(version, _)| version > current)
        .filter(|(version, assets)| {
            let expected = archive_name.replace("{version}", &version.to_string());
            assets.iter().any(|asset| asset.name == expected)
                && assets.iter().any(|asset| asset.name == "SHA256SUMS")
        })
        .max_by(|left, right| left.0.cmp(&right.0));
    let Some((version, assets)) = release else {
        return Ok(None);
    };
    let expected_archive = archive_name.replace("{version}", &version.to_string());
    let archive_url = assets
        .iter()
        .find(|asset| asset.name == expected_archive)
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| UpgradeError::ReleaseAssetMissing {
            version: version.clone(),
            asset: expected_archive.clone(),
        })?;
    let checksums_url = assets
        .iter()
        .find(|asset| asset.name == "SHA256SUMS")
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| UpgradeError::ReleaseAssetMissing {
            version: version.clone(),
            asset: "SHA256SUMS".into(),
        })?;
    Ok(Some((
        version,
        expected_archive,
        archive_url,
        checksums_url,
    )))
}

async fn download(
    client: &reqwest::Client,
    url: &str,
) -> std::result::Result<Vec<u8>, UpgradeError> {
    let parsed = secure_url(url)?;
    client
        .get(parsed)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| UpgradeError::ReleaseDiscovery {
            message: error.to_string(),
        })?
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| UpgradeError::ReleaseDiscovery {
            message: error.to_string(),
        })
}

#[cfg(unix)]
fn replace_standalone(
    executable: &Path,
    version_file: &Path,
    binary: &[u8],
    version: &Version,
) -> std::result::Result<bool, UpgradeError> {
    cfy_config::write_atomic(executable, binary).map_err(|error| UpgradeError::Replacement {
        message: error.to_string(),
    })?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).map_err(|error| {
        UpgradeError::Replacement {
            message: error.to_string(),
        }
    })?;
    cfy_config::write_atomic(version_file, format!("{version}\n").as_bytes()).map_err(|error| {
        UpgradeError::Replacement {
            message: error.to_string(),
        }
    })?;
    Ok(false)
}

#[cfg(windows)]
fn replace_standalone(
    executable: &Path,
    version_file: &Path,
    binary: &[u8],
    version: &Version,
) -> std::result::Result<bool, UpgradeError> {
    use std::process::Command;
    let staging = executable.with_extension(format!("update-{}.exe", std::process::id()));
    let script = executable.with_extension(format!("update-{}.cmd", std::process::id()));
    fs::write(&staging, binary).map_err(|error| UpgradeError::Replacement {
        message: error.to_string(),
    })?;
    let body = format!(
        "@echo off\r\n:retry\r\nmove /Y \"{}\" \"{}\" >nul 2>&1\r\nif errorlevel 1 (timeout /t 1 /nobreak >nul & goto retry)\r\n>\"{}\" echo {}\r\ndel \"%~f0\"\r\n",
        staging.display(),
        executable.display(),
        version_file.display(),
        version
    );
    fs::write(&script, body).map_err(|error| UpgradeError::Replacement {
        message: error.to_string(),
    })?;
    Command::new("cmd")
        .args(["/C", "start", "", "/B", "cmd", "/C"])
        .arg(&script)
        .spawn()
        .map_err(|error| UpgradeError::Replacement {
            message: error.to_string(),
        })?;
    Ok(true)
}

pub async fn execute_standalone(
    plan: &UpgradePlan,
    current_version: &Version,
    releases_url: &str,
) -> std::result::Result<StandaloneUpgradeOutcome, UpgradeError> {
    let UpgradePlan::Standalone {
        executable,
        version_file,
    } = plan
    else {
        return Err(UpgradeError::ExecutionFailed {
            message: "standalone updater received a non-standalone plan".into(),
        });
    };
    let Some((version, archive_name, archive_url, checksums_url)) =
        release_assets(releases_url, current_version).await?
    else {
        return Ok(StandaloneUpgradeOutcome {
            previous_version: current_version.to_string(),
            installed_version: current_version.to_string(),
            changed: false,
            replacement_scheduled: false,
        });
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .user_agent(concat!("catify/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| UpgradeError::ReleaseDiscovery {
            message: error.to_string(),
        })?;
    let checksums =
        String::from_utf8(download(&client, &checksums_url).await?).map_err(|error| {
            UpgradeError::Checksum {
                message: error.to_string(),
            }
        })?;
    let archive = download(&client, &archive_url).await?;
    let expected =
        checksum_for(&checksums, &archive_name).ok_or_else(|| UpgradeError::Checksum {
            message: format!("{archive_name} is missing from SHA256SUMS"),
        })?;
    let actual = sha256(&archive);
    if actual != expected {
        return Err(UpgradeError::Checksum {
            message: format!("expected {expected}, got {actual}"),
        });
    }
    let binary = extract_binary(&archive_name, archive)?;
    let replacement_scheduled = replace_standalone(executable, version_file, &binary, &version)?;
    Ok(StandaloneUpgradeOutcome {
        previous_version: current_version.to_string(),
        installed_version: version.to_string(),
        changed: true,
        replacement_scheduled,
    })
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StandaloneUpgradeOutcome {
    pub previous_version: String,
    pub installed_version: String,
    pub changed: bool,
    pub replacement_scheduled: bool,
}

fn is_legacy_shell_install(executable: &Path, context: &DetectionContext) -> bool {
    let Some(directory) = executable.parent() else {
        return false;
    };
    let trusted_directory = env::var_os("XDG_BIN_HOME")
        .map(PathBuf::from)
        .or_else(|| context.home.as_ref().map(|home| home.join(".local/bin")));
    if trusted_directory.is_none_or(|trusted| canonical_or_original(&trusted) != directory) {
        return false;
    }
    if executable.file_name() != Some(OsStr::new(EXECUTABLE_NAME)) {
        return false;
    }
    let alias = directory.join("catify");
    fs::read_link(alias).is_ok_and(|target| target == Path::new(EXECUTABLE_NAME))
}

/// A cached result from the release update checker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateCache {
    pub checked_at: u64,
    pub latest_version: Option<String>,
}

impl UpdateCache {
    #[must_use]
    pub fn is_fresh_at(&self, now: u64) -> bool {
        now.saturating_sub(self.checked_at) < UPDATE_CHECK_INTERVAL.as_secs()
    }

    #[must_use]
    pub fn available_version(&self, current: &str) -> Option<&str> {
        let current = Version::parse(current).ok()?;
        let latest = Version::parse(self.latest_version.as_deref()?).ok()?;
        (latest > current)
            .then_some(self.latest_version.as_deref())
            .flatten()
    }
}

#[must_use]
pub fn update_cache_path() -> PathBuf {
    env::var_os("CFY_UPDATE_CACHE_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME").map(|root| PathBuf::from(root).join("catify/update.json"))
        })
        .or_else(|| {
            env::var_os("HOME").map(|root| PathBuf::from(root).join(".cache/catify/update.json"))
        })
        .unwrap_or_else(|| PathBuf::from(".catify-cache/update.json"))
}

#[must_use]
pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn read_update_cache(path: &Path) -> std::io::Result<Option<UpdateCache>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(std::io::Error::other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn write_update_cache(path: &Path, cache: &UpdateCache) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_vec(cache).map_err(std::io::Error::other)?,
    )?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// Fetch the latest stable Catify release version. This never downloads an executable.
pub async fn fetch_latest_version(
    url: &str,
) -> std::result::Result<Option<Version>, reqwest::Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let response = reqwest::Client::builder()
        .user_agent(concat!("catify/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(4))
        .build()?
        .get(url)
        .send()
        .await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let release = response.error_for_status()?.json::<LatestRelease>().await?;
    Ok(Version::parse(release.tag_name.trim_start_matches('v')).ok())
}

impl InstallProvenance {
    #[must_use]
    pub const fn kind(&self) -> ProvenanceKind {
        match self {
            Self::Homebrew { .. } => ProvenanceKind::Homebrew,
            Self::Cargo { .. } => ProvenanceKind::Cargo,
            Self::Npm { .. } => ProvenanceKind::Npm,
            Self::Standalone { .. } => ProvenanceKind::Standalone,
            Self::Source { .. } => ProvenanceKind::Source,
            Self::Unknown { .. } => ProvenanceKind::Unknown,
        }
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        match self {
            Self::Homebrew { executable, .. }
            | Self::Cargo { executable, .. }
            | Self::Npm { executable, .. }
            | Self::Standalone { executable, .. }
            | Self::Source { executable, .. }
            | Self::Unknown { executable, .. } => executable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceKind {
    Homebrew,
    Cargo,
    Npm,
    Standalone,
    Source,
    Unknown,
}

impl fmt::Display for ProvenanceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Homebrew => "homebrew",
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Standalone => "standalone",
            Self::Source => "source",
            Self::Unknown => "unknown",
        })
    }
}

/// Inputs to deterministic provenance detection.
#[derive(Debug, Clone)]
pub struct DetectionContext {
    pub executable: PathBuf,
    pub cargo_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub homebrew_prefix: Option<PathBuf>,
    /// Explicit packaging metadata. This is a hint, not permission to mutate.
    pub install_channel: Option<String>,
}

impl DetectionContext {
    pub fn from_environment() -> std::io::Result<Self> {
        Ok(Self {
            executable: env::current_exe()?,
            cargo_home: env::var_os("CARGO_HOME").map(PathBuf::from),
            home: home_dir(),
            homebrew_prefix: env::var_os("HOMEBREW_PREFIX").map(PathBuf::from),
            install_channel: env::var("CFY_INSTALL_CHANNEL").ok(),
        })
    }
}

/// Detects the running installation without executing another program or
/// writing to disk.
pub fn detect() -> Result<InstallProvenance> {
    let context = DetectionContext::from_environment().map_err(|error| {
        Error::with_source(
            ErrorKind::Config,
            "could not locate the running cfy executable",
            error,
        )
    })?;
    Ok(detect_with(&context))
}

/// Deterministic detector used by callers and tests.
#[must_use]
pub fn detect_with(context: &DetectionContext) -> InstallProvenance {
    let executable = canonical_or_original(&context.executable);
    let channel = context
        .install_channel
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if let Some(root) = source_workspace(&executable) {
        return InstallProvenance::Source {
            executable,
            workspace_root: root,
        };
    }

    if channel == Some("homebrew")
        || is_homebrew_path(&executable, context.homebrew_prefix.as_deref())
    {
        return InstallProvenance::Homebrew {
            executable,
            formula: HOMEBREW_FORMULA.into(),
        };
    }

    if channel == Some("cargo") || is_cargo_path(&executable, context) {
        return InstallProvenance::Cargo {
            executable,
            package: CARGO_PACKAGE.into(),
        };
    }

    if channel == Some("npm") {
        return InstallProvenance::Npm {
            executable,
            package: NPM_PACKAGE.into(),
        };
    }

    let install_directory = executable.parent().unwrap_or(Path::new("."));
    let version_file = [
        install_directory.join(".catify-version"),
        install_directory.join("VERSION"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .unwrap_or_else(|| install_directory.join(".catify-version"));
    if channel == Some("standalone")
        || version_file.is_file()
        || is_legacy_shell_install(&executable, context)
    {
        return InstallProvenance::Standalone {
            executable,
            version_file,
        };
    }

    if channel == Some("source") {
        return InstallProvenance::Source {
            workspace_root: executable.parent().unwrap_or(Path::new(".")).to_path_buf(),
            executable,
        };
    }

    let reason = match channel {
        Some(other) => format!("unrecognized CFY_INSTALL_CHANNEL value `{other}`"),
        None => "no trusted package-manager path or standalone VERSION marker was found".into(),
    };
    InstallProvenance::Unknown { executable, reason }
}

fn canonical_or_original(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_cargo_path(executable: &Path, context: &DetectionContext) -> bool {
    let cargo_home = context
        .cargo_home
        .clone()
        .or_else(|| context.home.as_ref().map(|home| home.join(".cargo")));
    cargo_home.is_some_and(|home| {
        executable_names()
            .into_iter()
            .any(|name| executable == canonical_or_original(&home.join("bin").join(name)))
    })
}

fn is_homebrew_path(executable: &Path, configured_prefix: Option<&Path>) -> bool {
    let mut prefixes = Vec::new();
    if let Some(prefix) = configured_prefix {
        prefixes.push(prefix.to_path_buf());
    }
    prefixes.extend([
        PathBuf::from("/opt/homebrew"),
        PathBuf::from("/usr/local"),
        PathBuf::from("/home/linuxbrew/.linuxbrew"),
    ]);

    prefixes.into_iter().any(|prefix| {
        let prefix = canonical_or_original(&prefix);
        executable.starts_with(prefix.join("Cellar").join(HOMEBREW_FORMULA))
            || executable_names().into_iter().any(|name| {
                executable
                    == canonical_or_original(
                        &prefix
                            .join("opt")
                            .join(HOMEBREW_FORMULA)
                            .join("bin")
                            .join(name),
                    )
            })
    })
}

fn source_workspace(executable: &Path) -> Option<PathBuf> {
    let mut directory = executable.parent()?;
    while let Some(parent) = directory.parent() {
        if directory.file_name() == Some(OsStr::new("target")) {
            let manifest = parent.join("Cargo.toml");
            if manifest.is_file()
                && fs::read_to_string(&manifest).is_ok_and(|text| text.contains("[workspace]"))
            {
                return Some(parent.to_path_buf());
            }
        }
        directory = parent;
    }
    None
}

#[cfg(windows)]
fn executable_names() -> [OsString; 4] {
    [
        OsString::from(EXECUTABLE_NAME),
        OsString::from("cfy.exe"),
        OsString::from("catify"),
        OsString::from("catify.exe"),
    ]
}
#[cfg(not(windows))]
fn executable_names() -> [OsString; 2] {
    [OsString::from(EXECUTABLE_NAME), OsString::from("catify")]
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        env::var_os("HOME").map(PathBuf::from)
    }
}

/// An exact command-line, represented without shell parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl UpgradeCommand {
    #[must_use]
    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(self.args.iter())
            .map(|part| shell_display(part))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A safe plan. Planning has no side effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradePlan {
    /// Homebrew owns the binary; only Homebrew may replace it.
    Homebrew {
        formula: String,
        command: UpgradeCommand,
    },
    /// Cargo owns the binary; reinstall the exact package and preserve Cargo's
    /// lockfile policy.
    Cargo {
        package: String,
        command: UpgradeCommand,
    },
    /// npm owns the launcher and downloaded native binary.
    Npm {
        package: String,
        command: UpgradeCommand,
    },
    /// Standalone replacement requires verified release metadata. The library
    /// identifies it but refuses to invent an unsafe curl/extract pipeline.
    Standalone {
        executable: PathBuf,
        version_file: PathBuf,
    },
}

impl UpgradePlan {
    #[must_use]
    pub fn command(&self) -> Option<&UpgradeCommand> {
        match self {
            Self::Homebrew { command, .. }
            | Self::Cargo { command, .. }
            | Self::Npm { command, .. } => Some(command),
            Self::Standalone { .. } => None,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UpgradeError {
    #[error(
        "refusing to upgrade an executable built from source at {workspace_root}; rebuild it from that source checkout"
    )]
    SourceInstall { workspace_root: PathBuf },
    #[error(
        "cannot safely upgrade installation at {executable}: {reason}; reinstall cfy through npm, Homebrew, Cargo, or a standalone release archive"
    )]
    UnknownInstall { executable: PathBuf, reason: String },
    #[error("could not discover Catify releases: {message}")]
    ReleaseDiscovery { message: String },
    #[error("release v{version} has no asset `{asset}`")]
    ReleaseAssetMissing { version: Version, asset: String },
    #[error("release checksum verification failed: {message}")]
    Checksum { message: String },
    #[error("could not extract the Catify release archive: {message}")]
    Archive { message: String },
    #[error("could not replace the Catify executable: {message}")]
    Replacement { message: String },
    #[error(
        "refusing to mutate the installation in non-interactive mode without explicit approval"
    )]
    NonInteractiveApprovalRequired,
    #[error("could not execute upgrade command: {message}")]
    ExecutionFailed { message: String },
}

impl From<UpgradeError> for Error {
    fn from(error: UpgradeError) -> Self {
        let kind = match error {
            UpgradeError::ExecutionFailed { .. } => ErrorKind::Process,
            _ => ErrorKind::InvalidInput,
        };
        Error::with_source(kind, "could not upgrade Catify", error)
    }
}

pub fn plan(provenance: &InstallProvenance) -> std::result::Result<UpgradePlan, UpgradeError> {
    match provenance {
        InstallProvenance::Homebrew { formula, .. } => Ok(UpgradePlan::Homebrew {
            formula: formula.clone(),
            command: UpgradeCommand {
                program: "brew".into(),
                args: vec!["upgrade".into(), formula.into()],
            },
        }),
        InstallProvenance::Npm { package, .. } => Ok(UpgradePlan::Npm {
            package: package.clone(),
            command: UpgradeCommand {
                program: "npm".into(),
                args: vec![
                    "install".into(),
                    "--global".into(),
                    format!("{package}@latest").into(),
                ],
            },
        }),
        InstallProvenance::Cargo { package, .. } => Ok(UpgradePlan::Cargo {
            package: package.clone(),
            command: UpgradeCommand {
                program: "cargo".into(),
                args: vec!["install".into(), package.into(), "--locked".into()],
            },
        }),
        InstallProvenance::Standalone {
            executable,
            version_file,
        } => Ok(UpgradePlan::Standalone {
            executable: executable.clone(),
            version_file: version_file.clone(),
        }),
        InstallProvenance::Source { workspace_root, .. } => Err(UpgradeError::SourceInstall {
            workspace_root: workspace_root.clone(),
        }),
        InstallProvenance::Unknown { executable, reason } => Err(UpgradeError::UnknownInstall {
            executable: executable.clone(),
            reason: reason.clone(),
        }),
    }
}

/// Explicit execution policy. There is intentionally no environment-variable
/// backdoor: a non-interactive caller must pass `approved = true` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionPolicy {
    pub interactive: bool,
    pub approved: bool,
}

impl ExecutionPolicy {
    pub const INTERACTIVE: Self = Self {
        interactive: true,
        approved: true,
    };
    pub const NON_INTERACTIVE_REFUSE: Self = Self {
        interactive: false,
        approved: false,
    };
}

/// Executes a package-manager plan with inherited stdio/TTY, signal forwarding,
/// and the child's exact exit status. It never executes a shell.
pub async fn execute(
    plan: &UpgradePlan,
    policy: ExecutionPolicy,
    supervisor: &Supervisor,
) -> std::result::Result<ProcessOutput, UpgradeError> {
    if !policy.interactive && !policy.approved {
        return Err(UpgradeError::NonInteractiveApprovalRequired);
    }
    let Some(command) = plan.command() else {
        return Err(UpgradeError::ExecutionFailed {
            message: "standalone plans must be executed through execute_standalone".into(),
        });
    };
    let spec = ProcessSpec::new(command.program.to_string_lossy())
        .args(
            command
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned()),
        )
        .output(OutputMode::Inherit);
    supervisor
        .spawn(spec)
        .map_err(|error| UpgradeError::ExecutionFailed {
            message: error.to_string(),
        })?
        .wait_with_signal_forwarding()
        .await
        .map_err(|error| UpgradeError::ExecutionFailed {
            message: error.to_string(),
        })
}

/// The public `cfy upgrade` no-flag operation: detect and plan only. Mutation is
/// always a separate, explicit call to [`execute`].
pub fn upgrade() -> Result<UpgradePlan> {
    let provenance = detect()?;
    plan(&provenance).map_err(Into::into)
}

fn shell_display(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-._/:@".contains(character))
    {
        value.into_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
