use cfy_core::Error;
use serde::Serialize;
use serde_json::Value;
use std::{env, error::Error as _, io::Write};

const SECRET_ENVIRONMENT_VARIABLES: &[&str] = &[
    "SHOPIFY_ACCESS_TOKEN",
    "SHOPIFY_CLI_PARTNERS_TOKEN",
    "SHOPIFY_CLI_THEME_TOKEN",
    "SHOPIFY_CLI_ADMIN_AUTH_TOKEN",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
}

#[derive(Debug)]
pub struct Output {
    mode: OutputMode,
    verbosity: u8,
    redactor: Redactor,
}

impl Output {
    #[must_use]
    pub fn new(json: bool, verbosity: u8) -> Self {
        let secrets = SECRET_ENVIRONMENT_VARIABLES
            .iter()
            .filter_map(|name| env::var(name).ok());

        Self {
            mode: if json {
                OutputMode::Json
            } else {
                OutputMode::Human
            },
            verbosity,
            redactor: Redactor::new(secrets),
        }
    }

    /// Emit an always-visible lifecycle message without corrupting JSON stdout.
    pub fn lifecycle(&self, message: &str) -> std::io::Result<()> {
        let stderr = std::io::stderr();
        writeln!(stderr.lock(), "{}", self.redactor.redact(message))
    }

    #[must_use]
    pub const fn mode(&self) -> OutputMode {
        self.mode
    }

    #[must_use]
    pub fn with_json(&self, enabled: bool) -> Self {
        Self::new(enabled || self.mode == OutputMode::Json, self.verbosity)
    }

    pub fn success<T: Serialize>(&self, human: &str, value: &T) -> std::io::Result<()> {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        match self.mode {
            OutputMode::Human => writeln!(stdout, "{}", self.redactor.redact(human)),
            OutputMode::Json => {
                let mut value = serde_json::to_value(value)?;
                self.redactor.redact_json(&mut value);
                serde_json::to_writer(&mut stdout, &value)?;
                writeln!(stdout)
            }
        }
    }

    pub fn diagnostic(&self, message: &str) -> std::io::Result<()> {
        if self.verbosity == 0 {
            return Ok(());
        }
        writeln!(
            std::io::stderr(),
            "debug: {}",
            self.redactor.redact(message)
        )
    }

    pub fn error(&self, error: &Error) -> std::io::Result<()> {
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        self.write_error(&mut stderr, error)
    }

    fn write_error(&self, writer: &mut impl Write, error: &Error) -> std::io::Result<()> {
        match self.mode {
            OutputMode::Human => {
                writeln!(
                    writer,
                    "error: {}",
                    self.redactor.redact(&error.to_string())
                )?;
                if self.verbosity > 0 {
                    let mut source = error.source();
                    while let Some(cause) = source {
                        writeln!(
                            writer,
                            "  caused by: {}",
                            self.redactor.redact(&cause.to_string())
                        )?;
                        source = cause.source();
                    }
                }
                Ok(())
            }
            OutputMode::Json => {
                let causes = if self.verbosity > 0 {
                    causes(error)
                        .into_iter()
                        .map(|cause| self.redactor.redact(&cause))
                        .collect()
                } else {
                    Vec::new()
                };
                let diagnostic = JsonError {
                    error: JsonErrorBody {
                        code: error.kind().code(),
                        message: self.redactor.redact(error.message()),
                        causes,
                    },
                };
                serde_json::to_writer(&mut *writer, &diagnostic)?;
                writeln!(writer)
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonError<'a> {
    error: JsonErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct JsonErrorBody<'a> {
    code: &'a str,
    message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    causes: Vec<String>,
}

fn causes(error: &Error) -> Vec<String> {
    let mut values = Vec::new();
    let mut source = error.source();
    while let Some(cause) = source {
        values.push(cause.to_string());
        source = cause.source();
    }
    values
}

#[derive(Debug)]
struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<_> = secrets
            .into_iter()
            .filter(|secret| !secret.is_empty())
            .collect();
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();
        Self { secrets }
    }

    fn redact(&self, value: &str) -> String {
        self.secrets
            .iter()
            .fold(value.to_owned(), |redacted, secret| {
                redacted.replace(secret, "[REDACTED]")
            })
    }

    fn redact_json(&self, value: &mut Value) {
        match value {
            Value::String(string) => *string = self.redact(string),
            Value::Array(values) => {
                for value in values {
                    self.redact_json(value);
                }
            }
            Value::Object(values) => {
                for value in values.values_mut() {
                    self.redact_json(value);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Output, OutputMode, Redactor};
    use cfy_core::{Error, ErrorKind};
    use serde_json::json;
    use std::io;

    #[test]
    fn redacts_all_registered_secret_occurrences() {
        let redactor = Redactor::new(["shpat_secret".to_owned(), "short".to_owned()]);
        assert_eq!(
            redactor.redact("Bearer shpat_secret and short"),
            "Bearer [REDACTED] and [REDACTED]"
        );
    }

    #[test]
    fn ignores_empty_secrets() {
        let redactor = Redactor::new([String::new()]);
        assert_eq!(redactor.redact("unchanged"), "unchanged");
    }

    #[test]
    fn redacts_nested_json_values() {
        let redactor = Redactor::new(["secret".to_owned()]);
        let mut value = json!({"nested": ["Bearer secret"]});
        redactor.redact_json(&mut value);
        assert_eq!(value, json!({"nested": ["Bearer [REDACTED]"]}));
    }

    #[test]
    fn json_errors_redact_messages_and_debug_causes() {
        let output = Output {
            mode: OutputMode::Json,
            verbosity: 1,
            redactor: Redactor::new(["shpat_secret".to_owned()]),
        };
        let error = Error::with_source(
            ErrorKind::Api,
            "request with shpat_secret failed",
            io::Error::other("server echoed shpat_secret"),
        );
        let mut rendered = Vec::new();

        output.write_error(&mut rendered, &error).unwrap();

        let rendered = String::from_utf8(rendered).unwrap();
        assert!(!rendered.contains("shpat_secret"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&rendered).unwrap(),
            json!({
                "error": {
                    "code": "api",
                    "message": "request with [REDACTED] failed",
                    "causes": ["server echoed [REDACTED]"]
                }
            })
        );
    }
}
