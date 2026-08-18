use cfy_core::Cancellation;
use std::{
    collections::HashSet,
    fs, io,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// A fully downloaded theme file ready to be committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFile {
    pub path: PathBuf,
    pub contents: Vec<u8>,
}

fn remove_empty_parents(mut current: Option<&Path>, destination: &Path) {
    while let Some(directory) = current {
        if directory == destination || fs::remove_dir(directory).is_err() {
            break;
        }
        current = directory.parent();
    }
}

/// Validate an Admin API asset key as a portable, relative theme path.
pub fn safe_relative_path(key: &str) -> io::Result<PathBuf> {
    if key.is_empty() || key.contains('\\') || key.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe theme asset path",
        ));
    }
    let path = Path::new(key);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe theme asset path",
        ));
    }
    Ok(path.to_owned())
}

/// Commit a set of downloaded files as one transaction.
///
/// Existing files are backed up before replacement. If any replacement fails,
/// every prior replacement is restored and newly-created files are removed.
pub fn commit_staged_files(destination: &Path, files: &[StagedFile]) -> io::Result<()> {
    commit_staged_files_cancellable(destination, files, &Cancellation::default())
}

pub fn commit_staged_files_cancellable(
    destination: &Path,
    files: &[StagedFile],
    cancellation: &Cancellation,
) -> io::Result<()> {
    check_cancelled(cancellation)?;
    if fs::symlink_metadata(destination).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("theme destination is a symlink: {}", destination.display()),
        ));
    }
    let mut validated = Vec::with_capacity(files.len());
    let mut unique = HashSet::with_capacity(files.len());
    for file in files {
        let relative = safe_relative_path(file.path.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "theme asset path is not UTF-8")
        })?)?;
        if !unique.insert(relative.clone()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate theme asset path: {}", relative.display()),
            ));
        }
        validated.push((relative, &file.contents));
    }

    fs::create_dir_all(destination)?;
    reject_symlink_ancestors(
        destination,
        validated.iter().map(|(path, _)| path.as_path()),
    )?;

    let transaction = destination.join(format!(
        ".cfy-theme-pull-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let backup = transaction.join("backup");
    fs::create_dir_all(&backup)?;

    let mut committed: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let operation = (|| {
        for (relative, contents) in validated {
            check_cancelled(cancellation)?;
            let target = destination.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let saved = if target.exists() {
                if !target.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "theme asset destination is not a file: {}",
                            target.display()
                        ),
                    ));
                }
                let saved = backup.join(&relative);
                if let Some(parent) = saved.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&target, &saved)?;
                Some(saved)
            } else {
                None
            };
            crate::write_atomic(&target, contents)?;
            committed.push((target, saved));
        }
        Ok(())
    })();

    if let Err(error) = operation {
        let mut rollback_error = None;
        for (target, saved) in committed.into_iter().rev() {
            let was_new = saved.is_none();
            let result = if let Some(saved) = saved {
                fs::read(saved).and_then(|contents| crate::write_atomic(&target, &contents))
            } else {
                fs::remove_file(&target)
            };
            if let Err(error) = result {
                rollback_error.get_or_insert(error);
            } else if was_new {
                remove_empty_parents(target.parent(), destination);
            }
        }
        let _ = fs::remove_dir_all(&transaction);
        return if let Some(rollback) = rollback_error {
            Err(io::Error::new(
                rollback.kind(),
                format!("theme pull failed ({error}); rollback also failed ({rollback})"),
            ))
        } else {
            Err(error)
        };
    }

    fs::remove_dir_all(transaction)?;
    Ok(())
}

fn check_cancelled(cancellation: &Cancellation) -> io::Result<()> {
    if cancellation.is_cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "theme pull was cancelled",
        ))
    } else {
        Ok(())
    }
}

fn reject_symlink_ancestors<'a>(
    destination: &Path,
    paths: impl Iterator<Item = &'a Path>,
) -> io::Result<()> {
    for relative in paths {
        let mut current = destination.to_owned();
        for component in relative.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "theme asset path traverses a symlink: {}",
                            current.display()
                        ),
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cfy-theme-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn rejects_paths_that_can_escape_or_are_not_portable() {
        for key in [
            "",
            "/etc/passwd",
            "../secret",
            "assets/../secret",
            "assets\\x",
        ] {
            assert!(safe_relative_path(key).is_err(), "accepted {key:?}");
        }
        assert_eq!(
            safe_relative_path("assets/theme.js").unwrap(),
            PathBuf::from("assets/theme.js")
        );
    }

    #[test]
    fn commits_binary_files_and_changes_only_selected_paths() {
        let root = temp_directory("commit");
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("assets/keep.js"), b"keep").unwrap();
        fs::write(root.join("assets/change.bin"), b"old").unwrap();
        commit_staged_files(
            &root,
            &[StagedFile {
                path: PathBuf::from("assets/change.bin"),
                contents: vec![0, 159, 255, 10],
            }],
        )
        .unwrap();
        assert_eq!(
            fs::read(root.join("assets/change.bin")).unwrap(),
            vec![0, 159, 255, 10]
        );
        assert_eq!(fs::read(root.join("assets/keep.js")).unwrap(), b"keep");
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cfy-theme-pull")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failure_rolls_back_files_already_replaced() {
        let root = temp_directory("rollback");
        fs::create_dir_all(root.join("assets/block")).unwrap();
        fs::write(root.join("assets/first"), b"original").unwrap();
        let result = commit_staged_files(
            &root,
            &[
                StagedFile {
                    path: "assets/first".into(),
                    contents: b"changed".to_vec(),
                },
                StagedFile {
                    path: "assets/block".into(),
                    contents: b"cannot replace directory".to_vec(),
                },
            ],
        );
        assert!(result.is_err());
        assert_eq!(fs::read(root.join("assets/first")).unwrap(), b"original");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_leaves_existing_tree_untouched() {
        let root = temp_directory("cancelled");
        fs::create_dir_all(root.join("assets")).unwrap();
        fs::write(root.join("assets/app.js"), b"old").unwrap();
        let cancellation = Cancellation::default();
        cancellation.cancel();

        let error = commit_staged_files_cancellable(
            &root,
            &[StagedFile {
                path: PathBuf::from("assets/app.js"),
                contents: b"new".to_vec(),
            }],
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(fs::read(root.join("assets/app.js")).unwrap(), b"old");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_paths_are_rejected_before_any_write() {
        let root = temp_directory("duplicate");
        let error = commit_staged_files(
            &root,
            &[
                StagedFile {
                    path: PathBuf::from("assets/app.js"),
                    contents: b"first".to_vec(),
                },
                StagedFile {
                    path: PathBuf::from("assets/app.js"),
                    contents: b"second".to_vec(),
                },
            ],
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!root.exists());
    }
}
