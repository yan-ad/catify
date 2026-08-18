//! Deterministic coalescing for noisy filesystem events emitted during theme development.

use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    Upsert(PathBuf),
    Remove(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    Upload(PathBuf),
    Delete(PathBuf),
}

/// Coalesce a debounce window into deterministic, last-write-wins sync actions.
/// Renames intentionally become delete-old plus upload-new.
#[must_use]
pub fn coalesce(events: impl IntoIterator<Item = FileEvent>) -> Vec<SyncAction> {
    let mut actions = BTreeMap::new();
    for event in events {
        match event {
            FileEvent::Upsert(path) => {
                actions.insert(path, true);
            }
            FileEvent::Remove(path) => {
                actions.insert(path, false);
            }
            FileEvent::Rename { from, to } => {
                actions.insert(from, false);
                actions.insert(to, true);
            }
        }
    }
    actions
        .into_iter()
        .map(|(path, upload)| {
            if upload {
                SyncAction::Upload(path)
            } else {
                SyncAction::Delete(path)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_last_event_and_expands_rename() {
        assert_eq!(
            coalesce([
                FileEvent::Upsert("assets/a.css".into()),
                FileEvent::Remove("assets/a.css".into()),
                FileEvent::Rename {
                    from: "templates/a.json".into(),
                    to: "templates/b.json".into()
                },
            ]),
            vec![
                SyncAction::Delete("assets/a.css".into()),
                SyncAction::Delete("templates/a.json".into()),
                SyncAction::Upload("templates/b.json".into()),
            ]
        );
    }

    #[test]
    fn recreation_becomes_upload() {
        assert_eq!(
            coalesce([
                FileEvent::Remove("assets/a.css".into()),
                FileEvent::Upsert("assets/a.css".into()),
            ]),
            vec![SyncAction::Upload("assets/a.css".into())]
        );
    }
}
