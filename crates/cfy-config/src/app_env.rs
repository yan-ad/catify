//! Native Shopify app environment extraction and dotenv rendering.

use crate::project::ProjectEnvironment;
use std::collections::BTreeMap;

/// Environment variables derived from a selected Shopify app configuration.
pub type AppEnvironment = BTreeMap<String, String>;

/// Extracts the app environment values that are available from local config.
#[must_use]
pub fn from_project(environment: &ProjectEnvironment) -> AppEnvironment {
    let mut values = AppEnvironment::new();
    insert_string(
        &mut values,
        "SHOPIFY_API_KEY",
        environment.document.get("client_id"),
    );
    insert_string(
        &mut values,
        "SHOPIFY_APP_URL",
        environment.document.get("application_url"),
    );
    insert_string(
        &mut values,
        "SCOPES",
        environment
            .document
            .get("access_scopes")
            .and_then(|value| value.get("scopes")),
    );
    values.insert(
        "SHOPIFY_FLAG_APP_CONFIG".to_owned(),
        environment.config_name.clone(),
    );
    values
}

/// Merges managed app variables into existing dotenv content while preserving
/// comments, blank lines, and variables that Catify does not own.
#[must_use]
pub fn merge_dotenv(existing: &str, values: &AppEnvironment) -> String {
    let mut pending = values.clone();
    let mut output = String::new();
    for line in existing.lines() {
        let trimmed = line.trim_start();
        let name = trimmed
            .split_once('=')
            .map(|(name, _)| name.trim())
            .filter(|name| !name.is_empty() && !name.starts_with('#'));
        if let Some(name) = name
            && let Some(value) = pending.remove(name)
        {
            output.push_str(name);
            output.push('=');
            output.push_str(&quote_dotenv(&value));
            output.push('\n');
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }
    if !output.is_empty() && !output.ends_with("\n\n") && !pending.is_empty() {
        output.push('\n');
    }
    output.push_str(&render_dotenv(&pending));
    output
}

fn insert_string(values: &mut AppEnvironment, name: &str, value: Option<&toml::Value>) {
    if let Some(value) = value.and_then(toml::Value::as_str)
        && !value.trim().is_empty()
    {
        values.insert(name.to_owned(), value.to_owned());
    }
}

/// Returns a copy suitable for terminal output without exposing sensitive values.
#[must_use]
pub fn redacted(values: &AppEnvironment) -> AppEnvironment {
    values
        .iter()
        .map(|(name, value)| {
            let value = if is_sensitive(name) {
                "[REDACTED]".to_owned()
            } else {
                value.clone()
            };
            (name.clone(), value)
        })
        .collect()
}

fn is_sensitive(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.contains("KEY")
        || name.contains("TOKEN")
        || name.contains("SECRET")
        || name.contains("PASSWORD")
}

/// Renders values as deterministic dotenv content.
#[must_use]
pub fn render_dotenv(values: &AppEnvironment) -> String {
    let mut output = String::new();
    for (name, value) in values {
        output.push_str(name);
        output.push('=');
        output.push_str(&quote_dotenv(value));
        output.push('\n');
    }
    output
}

fn quote_dotenv(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b',' | b'/' | b':')
        })
    {
        return value.to_owned();
    }

    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{
        Environment, ProjectKind, ProjectOverrides, discover, resolve_environment,
    };
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("cfy-app-env-{nonce}"));
            fs::create_dir_all(&root).unwrap();
            fs::write(
                root.join("shopify.app.toml"),
                r#"client_id = "client-key"
application_url = "https://example.test/path with spaces"
[access_scopes]
scopes = "read_products,write_products"
"#,
            )
            .unwrap();
            Self(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn extracts_known_values_and_redacts_api_key() {
        let fixture = Fixture::new();
        let project = discover(&fixture.0, Some(ProjectKind::App)).unwrap();
        let selected =
            resolve_environment(project, &ProjectOverrides::default(), &Environment::new())
                .unwrap();
        let values = from_project(&selected);

        assert_eq!(values["SHOPIFY_API_KEY"], "client-key");
        assert_eq!(values["SCOPES"], "read_products,write_products");
        assert_eq!(redacted(&values)["SHOPIFY_API_KEY"], "[REDACTED]");
        assert_eq!(
            redacted(&values)["SHOPIFY_APP_URL"],
            values["SHOPIFY_APP_URL"]
        );
    }

    #[test]
    fn renders_deterministic_and_escaped_dotenv() {
        let values = AppEnvironment::from([
            ("A".to_owned(), "plain-value".to_owned()),
            ("B".to_owned(), "has spaces\nand newline".to_owned()),
        ]);
        assert_eq!(
            render_dotenv(&values),
            "A=plain-value\nB=\"has spaces\\nand newline\"\n"
        );
    }

    #[test]
    fn merges_managed_values_without_destroying_comments_or_custom_values() {
        let values = AppEnvironment::from([
            ("SHOPIFY_API_KEY".to_owned(), "new-key".to_owned()),
            ("SCOPES".to_owned(), "read_products".to_owned()),
        ]);
        let existing = "# keep this\nCUSTOM=value\nSHOPIFY_API_KEY=old-key\n# SCOPES=commented\n";

        assert_eq!(
            merge_dotenv(existing, &values),
            "# keep this\nCUSTOM=value\nSHOPIFY_API_KEY=new-key\n# SCOPES=commented\n\nSCOPES=read_products\n"
        );
    }
}
