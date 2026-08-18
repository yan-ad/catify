//! Configuration loading, filesystem primitives, and project path handling.

pub mod project;

use cfy_core::{Error, ErrorKind, Result};
use serde::Deserialize;
use std::{
    ffi::OsString,
    fmt,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Partial configuration loaded from one source.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub telemetry: Option<Telemetry>,
}

fn write_complete(writer: &mut impl Write, contents: &[u8]) -> io::Result<()> {
    writer.write_all(contents)
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    pub enabled: Option<bool>,
}

/// Runtime overrides have the highest precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub telemetry_enabled: Option<bool>,
}

/// Fully resolved configuration consumed by commands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub telemetry_enabled: bool,
}

/// A TOML parsing failure with a human-readable source location.
#[derive(Debug)]
pub struct ParseError {
    source_name: String,
    line: Option<usize>,
    column: Option<usize>,
    source: toml::de::Error,
}

impl ParseError {
    #[must_use]
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    #[must_use]
    pub const fn line(&self) -> Option<usize> {
        self.line
    }

    #[must_use]
    pub const fn column(&self) -> Option<usize> {
        self.column
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.column) {
            (Some(line), Some(column)) => write!(
                formatter,
                "{}:{line}:{column}: {}",
                self.source_name, self.source
            ),
            _ => write!(formatter, "{}: {}", self.source_name, self.source),
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Parse TOML while retaining the source name and one-based line/column.
pub fn parse(source_name: impl Into<String>, input: &str) -> Result<Config> {
    toml::from_str(input).map_err(|source| {
        let (line, column) = source
            .span()
            .map(|span| offset_to_line_column(input, span.start))
            .map_or((None, None), |(line, column)| (Some(line), Some(column)));
        let parse_error = ParseError {
            source_name: source_name.into(),
            line,
            column,
            source,
        };
        Error::with_source(
            ErrorKind::Config,
            format!(
                "invalid TOML configuration in {}",
                parse_error.source_name()
            ),
            parse_error,
        )
    })
}

fn offset_to_line_column(input: &str, offset: usize) -> (usize, usize) {
    let prefix = &input[..offset.min(input.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.chars().count() + 1, |(_, tail)| {
            tail.chars().count() + 1
        });
    (line, column)
}

/// Filesystem operations used by config loading and persistence.
pub trait FileSystem {
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn write_atomic(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StdFileSystem;

impl FileSystem for StdFileSystem {
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        fs::read_to_string(path)
    }

    fn write_atomic(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        write_atomic(path, contents)
    }
}

/// Write a file through a sibling temporary file, then atomically rename it.
///
/// The destination is never truncated before the replacement is ready. Temporary
/// files are removed on every reported failure.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let mut last_collision = None;
    for attempt in 0..32_u32 {
        let temporary = temporary_path(path, attempt);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };

        let operation = (|| {
            write_complete(&mut file, contents)?;
            file.sync_all()?;
            drop(file);
            replace_file(&temporary, path)?;
            sync_directory(parent)?;
            Ok(())
        })();

        if operation.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return operation;
    }

    Err(last_collision.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate an atomic-write temporary file",
        )
    }))
}

fn temporary_path(path: &Path, attempt: u32) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(
        ".{name}.cfy-{}-{nonce}-{attempt}.tmp",
        std::process::id()
    ))
}

#[cfg(unix)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    if !destination.exists() {
        return fs::rename(source, destination);
    }

    use std::{os::windows::ffi::OsStrExt, ptr};

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(
            replaced_file_name: *const u16,
            replacement_file_name: *const u16,
            backup_file_name: *const u16,
            replace_flags: u32,
            exclude: *mut std::ffi::c_void,
            reserved: *mut std::ffi::c_void,
        ) -> i32;
    }

    const REPLACEFILE_WRITE_THROUGH: u32 = 0x0000_0001;
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();

    // SAFETY: both paths are valid, null-terminated UTF-16 buffers that remain
    // alive for the call. Optional backup/exclusion/reserved pointers are null.
    let result = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Load and merge defaults, user config, project config, and runtime overrides.
pub fn load<F: FileSystem>(
    filesystem: &F,
    user_path: Option<&Path>,
    project_path: Option<&Path>,
    overrides: &Overrides,
) -> Result<ResolvedConfig> {
    let mut resolved = ResolvedConfig::default();

    if let Some(path) = user_path {
        apply_optional_file(filesystem, path, &mut resolved)?;
    }
    if let Some(path) = project_path {
        apply_optional_file(filesystem, path, &mut resolved)?;
    }
    if let Some(enabled) = overrides.telemetry_enabled {
        resolved.telemetry_enabled = enabled;
    }

    Ok(resolved)
}

fn apply_optional_file<F: FileSystem>(
    filesystem: &F,
    path: &Path,
    resolved: &mut ResolvedConfig,
) -> Result<()> {
    let input = match filesystem.read_to_string(path) {
        Ok(input) => input,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Error::with_source(
                ErrorKind::Config,
                format!("could not read configuration {}", path.display()),
                error,
            ));
        }
    };

    let config = parse(path.display().to_string(), &input)?;
    if let Some(enabled) = config.telemetry.and_then(|telemetry| telemetry.enabled) {
        resolved.telemetry_enabled = enabled;
    }
    Ok(())
}

/// Normalize a host path lexically without requiring it to exist.
#[must_use]
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut prefix = None;
    let mut root = false;
    let mut parts: Vec<OsString> = Vec::new();

    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_owned()),
            Component::RootDir => root = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.last().is_some_and(|part| part != "..") {
                    parts.pop();
                } else if !root {
                    parts.push(OsString::from(".."));
                }
            }
            Component::Normal(value) => parts.push(value.to_owned()),
        }
    }

    let mut normalized = PathBuf::new();
    if let Some(value) = prefix {
        normalized.push(value);
    }
    if root {
        normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR));
    }
    normalized.extend(parts);
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, error::Error as _};

    #[derive(Default)]
    struct PartialWriter {
        contents: Vec<u8>,
        maximum_chunk: usize,
    }

    impl Write for PartialWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let length = buffer.len().min(self.maximum_chunk);
            self.contents.extend_from_slice(&buffer[..length]);
            Ok(length)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemoryFileSystem {
        files: HashMap<PathBuf, io::Result<String>>,
    }

    impl MemoryFileSystem {
        fn with_file(mut self, path: &str, contents: &str) -> Self {
            self.files
                .insert(PathBuf::from(path), Ok(contents.to_owned()));
            self
        }

        fn with_error(mut self, path: &str, kind: io::ErrorKind) -> Self {
            self.files.insert(
                PathBuf::from(path),
                Err(io::Error::new(kind, "filesystem failure")),
            );
            self
        }
    }

    impl FileSystem for MemoryFileSystem {
        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.files.get(path).map_or_else(
                || Err(io::Error::from(io::ErrorKind::NotFound)),
                |result| match result {
                    Ok(contents) => Ok(contents.clone()),
                    Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
                },
            )
        }

        fn write_atomic(&self, _path: &Path, _contents: &[u8]) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn parse_error_reports_source_line_and_column() {
        let input = "[telemetry]\nenabled = maybe\n";
        let error = parse("project/cfy.toml", input).unwrap_err();
        let source = error.source().unwrap().to_string();

        assert!(source.contains("project/cfy.toml:2:11"), "{source}");
        assert!(source.contains("string values must be quoted"), "{source}");
    }

    #[test]
    fn precedence_is_defaults_then_user_then_project_then_overrides() {
        let filesystem = MemoryFileSystem::default()
            .with_file("user.toml", "[telemetry]\nenabled = true")
            .with_file("project.toml", "[telemetry]\nenabled = false");

        let project = load(
            &filesystem,
            Some(Path::new("user.toml")),
            Some(Path::new("project.toml")),
            &Overrides::default(),
        )
        .unwrap();
        assert!(!project.telemetry_enabled);

        let runtime = load(
            &filesystem,
            Some(Path::new("user.toml")),
            Some(Path::new("project.toml")),
            &Overrides {
                telemetry_enabled: Some(true),
            },
        )
        .unwrap();
        assert!(runtime.telemetry_enabled);
    }

    #[test]
    fn missing_optional_files_keep_defaults() {
        let resolved = load(
            &MemoryFileSystem::default(),
            Some(Path::new("missing-user.toml")),
            Some(Path::new("missing-project.toml")),
            &Overrides::default(),
        )
        .unwrap();

        assert_eq!(resolved, ResolvedConfig::default());
    }

    #[test]
    fn permission_errors_are_not_treated_as_missing_files() {
        let filesystem =
            MemoryFileSystem::default().with_error("user.toml", io::ErrorKind::PermissionDenied);
        let error = load(
            &filesystem,
            Some(Path::new("user.toml")),
            None,
            &Overrides::default(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), ErrorKind::Config);
        assert_eq!(
            error
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn atomic_write_replaces_complete_contents_and_cleans_temporary_files() {
        let directory = std::env::temp_dir().join(format!(
            "cfy-config-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("config.toml");
        fs::write(&target, "old").unwrap();

        write_atomic(&target, b"new complete contents").unwrap();

        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "new complete contents"
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn complete_write_retries_partial_writes_until_all_bytes_are_written() {
        let mut writer = PartialWriter {
            maximum_chunk: 3,
            ..PartialWriter::default()
        };

        write_complete(&mut writer, b"complete configuration").unwrap();

        assert_eq!(writer.contents, b"complete configuration");
    }

    #[test]
    fn failed_atomic_replacement_preserves_destination() {
        let directory = std::env::temp_dir().join(format!(
            "cfy-config-failure-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = directory.join("destination");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep"), "safe").unwrap();

        assert!(write_atomic(&target, b"replacement").is_err());
        assert_eq!(fs::read_to_string(target.join("keep")).unwrap(), "safe");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn normalizes_host_relative_and_rooted_paths() {
        assert_eq!(
            normalize_path(Path::new("a/./b/../c")),
            PathBuf::from("a/c")
        );
        assert_eq!(
            normalize_path(Path::new("../../a/../b")),
            PathBuf::from("../../b")
        );

        #[cfg(unix)]
        assert_eq!(normalize_path(Path::new("/a/../../b")), PathBuf::from("/b"));

        #[cfg(windows)]
        {
            assert_eq!(
                normalize_path(Path::new(r"C:\a\.\b\..\c")),
                PathBuf::from(r"C:\a\c")
            );
            assert_eq!(
                normalize_path(Path::new(r"\\server\share\a\..\b")),
                PathBuf::from(r"\\server\share\b")
            );
        }
    }
}
