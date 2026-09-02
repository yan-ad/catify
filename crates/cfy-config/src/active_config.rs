use crate::write_atomic;
use cfy_core::{Error, ErrorKind, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// Non-secret local preferences keyed by normalized app project directory.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ActiveConfigState {
    projects: BTreeMap<String, ProjectPreference>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct ProjectPreference {
    config_file: String,
}

impl ActiveConfigState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(path).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("could not read app configuration state {}", path.display()),
                error,
            )
        })?;
        serde_json::from_str(&contents).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("could not parse app configuration state {}", path.display()),
                error,
            )
        })
    }

    pub fn selected(&self, project_root: &Path) -> Option<&str> {
        self.projects
            .get(&project_key(project_root))
            .map(|entry| entry.config_file.as_str())
    }

    pub fn set(&mut self, project_root: &Path, config_file: String) {
        self.projects
            .insert(project_key(project_root), ProjectPreference { config_file });
    }

    pub fn clear(&mut self, project_root: &Path) -> bool {
        self.projects.remove(&project_key(project_root)).is_some()
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let contents = serde_json::to_vec_pretty(self).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                "could not serialize app configuration state",
                error,
            )
        })?;
        write_atomic(path, &contents).map_err(|error| {
            Error::with_source(
                ErrorKind::Config,
                format!("could not write app configuration state {}", path.display()),
                error,
            )
        })
    }
}

fn project_key(root: &Path) -> String {
    normalize_path(root).to_string_lossy().into_owned()
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::ActiveConfigState;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "cfy-active-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn persists_project_selection_and_clears_it() {
        let root = temp_dir();
        let path = root.join("state.json");
        let project = root.join("app");
        fs::create_dir_all(&project).unwrap();

        let mut state = ActiveConfigState::default();
        state.set(&project, "shopify.app.staging.toml".to_owned());
        state.write(&path).unwrap();

        let mut loaded = ActiveConfigState::load(&path).unwrap();
        assert_eq!(loaded.selected(&project), Some("shopify.app.staging.toml"));
        assert!(loaded.clear(&project));
        loaded.write(&path).unwrap();
        assert_eq!(
            ActiveConfigState::load(&path).unwrap().selected(&project),
            None
        );
    }
}
