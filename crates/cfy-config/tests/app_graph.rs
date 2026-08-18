use cfy_config::project::{self, ProjectKind};
use cfy_config::{AppConfigGraph, DiagnosticSeverity, ExtensionFamily};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn directory_patterns_cannot_escape_the_project_root() {
    let root = temp_dir("escape");
    fs::write(
        root.join("shopify.app.toml"),
        "name = \"escape\"\nextension_directories = [\"../outside/**\"]\n",
    )
    .unwrap();
    let project = project::discover(&root, Some(ProjectKind::App)).unwrap();
    let error = AppConfigGraph::load(&project).unwrap_err();
    assert!(error.to_string().contains("must stay inside"));
}

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "cfy-config-graph-{name}-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn parses_all_supported_extension_families_and_preserves_unknown_app_fields() {
    let project = project::discover(fixture("app-graph"), Some(ProjectKind::App)).unwrap();
    let graph = AppConfigGraph::load(&project).unwrap();
    let app = &graph.apps[0];
    assert_eq!(app.extensions.len(), 4);
    assert!(
        app.extensions
            .iter()
            .any(|e| e.family == ExtensionFamily::Ui)
    );
    assert!(
        app.extensions
            .iter()
            .any(|e| e.family == ExtensionFamily::Function)
    );
    assert!(
        app.extensions
            .iter()
            .any(|e| e.family == ExtensionFamily::ThemeApp)
    );
    assert!(
        app.extensions
            .iter()
            .any(|e| e.family == ExtensionFamily::WebPixel)
    );
    assert_eq!(app.webs.len(), 1);
    assert_eq!(app.webs[0].roles, ["frontend"]);
    assert_eq!(
        app.config.unknown["mystery_app_key"].as_str(),
        Some("preserved")
    );
    let warning = graph
        .diagnostics
        .iter()
        .find(|d| d.message.contains("mystery_app_key"))
        .unwrap();
    assert_eq!(warning.location.line, 5);
    assert!(warning.location.column > 0);
}

#[test]
fn missing_default_extension_and_web_directories_are_empty_not_errors() {
    let root = temp_dir("missing");
    fs::write(root.join("shopify.app.toml"), "name = \"empty\"\n").unwrap();
    let project = project::discover(&root, Some(ProjectKind::App)).unwrap();
    let graph = AppConfigGraph::load(&project).unwrap();
    assert!(graph.apps[0].extensions.is_empty());
    assert!(graph.apps[0].webs.is_empty());
}

#[test]
fn malformed_toml_has_file_line_and_column_diagnostic() {
    let root = temp_dir("malformed");
    fs::write(root.join("shopify.app.toml"), "name = [\n").unwrap();
    let project = project::discover(&root, Some(ProjectKind::App)).unwrap();
    let graph = AppConfigGraph::load(&project).unwrap();
    let diagnostic = graph
        .diagnostics
        .iter()
        .find(|d| d.severity == DiagnosticSeverity::Error)
        .unwrap();
    assert_eq!(diagnostic.location.file, root.join("shopify.app.toml"));
    assert!(diagnostic.location.line >= 1 && diagnostic.location.column >= 1);
    assert!(diagnostic.message.contains("malformed TOML"));
}

#[test]
fn duplicate_handles_and_unsupported_types_are_actionable() {
    let root = temp_dir("diagnostics");
    fs::write(root.join("shopify.app.toml"), "name = \"diagnostics\"\n").unwrap();
    fs::create_dir_all(root.join("extensions/a")).unwrap();
    fs::create_dir_all(root.join("extensions/b")).unwrap();
    fs::write(
        root.join("extensions/a/shopify.extension.toml"),
        "name = \"a\"\ntype = \"future_extension\"\nhandle = \"same\"\nfuture_key = true\n",
    )
    .unwrap();
    fs::write(
        root.join("extensions/b/shopify.extension.toml"),
        "name = \"b\"\ntype = \"ui_extension\"\nhandle = \"same\"\n",
    )
    .unwrap();
    let project = project::discover(&root, Some(ProjectKind::App)).unwrap();
    let graph = AppConfigGraph::load(&project).unwrap();
    assert!(graph.diagnostics.iter().any(|d| {
        d.message
            .contains("unsupported extension type `future_extension`")
    }));
    assert!(graph.diagnostics.iter().any(|d| {
        d.message
            .contains("unknown extension configuration key `future_key`")
    }));
    let duplicate = graph
        .diagnostics
        .iter()
        .find(|d| d.message.contains("duplicate extension handle `same`"))
        .unwrap();
    assert_eq!(duplicate.severity, DiagnosticSeverity::Error);
    assert_eq!(duplicate.location.line, 3);
}
