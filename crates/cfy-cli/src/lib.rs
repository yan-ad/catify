pub mod output;

use crate::output::Output;
use cfy_api::theme::{Theme, ThemeClient};
use cfy_config::project::{
    Environment, ProjectKind, ProjectOverrides, discover, resolve_environment,
};
use cfy_config::theme::{StagedFile, commit_staged_files_cancellable, safe_relative_path};
use cfy_core::{Cancellation, Error, Result};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use std::{
    env, io,
    path::{Path, PathBuf},
    time::Duration,
};

const SHOPIFY_API_VERSION: &str = "2026-07";

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Crabpify's top-level command-line interface.
#[derive(Debug, Parser)]
#[command(
    name = "cfy",
    version,
    about = "A fast, memory-efficient Shopify CLI alternative",
    long_about = None,
    propagate_version = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOptions,

    #[command(subcommand)]
    pub command: Option<Command>,
}

async fn pull_theme(
    theme: u64,
    explicit_store: Option<&str>,
    includes: &[String],
    excludes: &[String],
    destination: &Path,
    output: &Output,
) -> Result<()> {
    let store = resolve_store(explicit_store)?;
    let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| {
        Error::new(
            cfy_core::ErrorKind::Api,
            "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Crabpify login flow",
        )
    })?;
    let client = ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
    let cancellation = Cancellation::default();
    let signal = cancellation.clone();
    let _signal_task = AbortOnDrop(tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    }));
    // The complete remote set is staged in memory. Cancellation or any partial
    // HTTP failure therefore leaves the destination untouched.
    let assets = client
        .pull(theme, includes, excludes, &cancellation)
        .await
        .map_err(Error::from)?;
    let files = assets
        .into_iter()
        .map(|asset| {
            Ok(StagedFile {
                path: safe_relative_path(&asset.key).map_err(|error| {
                    Error::with_source(
                        cfy_core::ErrorKind::Config,
                        format!("Shopify returned an unsafe theme asset path: {}", asset.key),
                        error,
                    )
                })?,
                contents: asset.contents,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    commit_staged_files_cancellable(destination, &files, &cancellation).map_err(|error| {
        Error::with_source(
            cfy_core::ErrorKind::Config,
            format!("could not commit theme assets to {}", destination.display()),
            error,
        )
    })?;
    output
        .success(
            &format!("Pulled {} theme assets to {}.", files.len(), destination.display()),
            &serde_json::json!({ "theme_id": theme, "store": store, "destination": destination, "files": files.len() }),
        )
        .map_err(|error| Error::with_source(cfy_core::ErrorKind::Process, "could not write theme pull result", error))
}

async fn list_themes(explicit_store: Option<&str>, output: &Output) -> Result<()> {
    let store = resolve_store(explicit_store)?;
    let token = env::var("SHOPIFY_CLI_THEME_TOKEN").map_err(|_| {
        Error::new(
            cfy_core::ErrorKind::Api,
            "theme authentication is required; set SHOPIFY_CLI_THEME_TOKEN or complete the Crabpify login flow",
        )
    })?;
    let client = ThemeClient::new(&store, &token, SHOPIFY_API_VERSION).map_err(Error::from)?;
    let themes = client.list().await.map_err(Error::from)?;
    output
        .success(&format_themes(&themes), &themes)
        .map_err(|error| {
            Error::with_source(
                cfy_core::ErrorKind::Process,
                "could not write theme list",
                error,
            )
        })
}

fn resolve_store(explicit_store: Option<&str>) -> Result<String> {
    let environment = Environment::from_iter(
        ["CFY_STORE", "SHOPIFY_FLAG_STORE"]
            .into_iter()
            .filter_map(|name| env::var(name).ok().map(|value| (name.to_owned(), value))),
    );

    let current = env::current_dir().map_err(|error| {
        Error::with_source(
            cfy_core::ErrorKind::Config,
            "could not read current directory",
            error,
        )
    })?;
    if let Ok(project) = discover(&current, Some(ProjectKind::App)) {
        let selected =
            resolve_environment(project, &ProjectOverrides::default(), &Environment::new())?;
        return select_store(explicit_store, &environment, selected.store.as_deref());
    }

    if let Ok(project) = discover(&current, Some(ProjectKind::Theme)) {
        let selected =
            resolve_environment(project, &ProjectOverrides::default(), &Environment::new())?;
        return select_store(explicit_store, &environment, selected.store.as_deref());
    }

    select_store(explicit_store, &environment, None)
}

fn select_store(
    explicit_store: Option<&str>,
    environment: &Environment,
    configured_store: Option<&str>,
) -> Result<String> {
    if let Some(store) = explicit_store.filter(|store| !store.trim().is_empty()) {
        return Ok(store.to_owned());
    }
    for name in ["CFY_STORE", "SHOPIFY_FLAG_STORE"] {
        if let Some(store) = environment
            .get(name)
            .filter(|store| !store.trim().is_empty())
        {
            return Ok(store.clone());
        }
    }
    if let Some(store) = configured_store.filter(|store| !store.trim().is_empty()) {
        return Ok(store.to_owned());
    }

    Err(Error::invalid_input(
        "no store selected; pass --store, set CFY_STORE/SHOPIFY_FLAG_STORE, or add store to the project configuration",
    ))
}

fn format_themes(themes: &[Theme]) -> String {
    if themes.is_empty() {
        return "No themes found.".to_owned();
    }
    themes
        .iter()
        .map(|theme| format!("{}\t{}\t{}", theme.id, theme.role, theme.name))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Options shared by every Crabpify command.
#[derive(Debug, Default, Args)]
pub struct GlobalOptions {
    /// Increase diagnostic output; repeat for more detail.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Disable ANSI color output.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Emit machine-readable JSON when supported by the command.
    #[arg(long, global = true)]
    pub json: bool,

    /// Never prompt for interactive input.
    #[arg(long, global = true)]
    pub non_interactive: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage Shopify apps.
    #[command(alias = "a")]
    App {
        #[command(subcommand)]
        command: AppCommand,
    },

    /// Manage Shopify themes.
    #[command(alias = "th")]
    Theme {
        #[command(subcommand)]
        command: ThemeCommand,
    },

    /// Generate a shell completion script on standard output.
    Completion {
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Print build and runtime version information.
    #[command(alias = "v")]
    Version,

    #[command(hide = true)]
    Internal {
        #[command(subcommand)]
        command: InternalCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AppCommand {
    /// Display the currently selected app.
    #[command(alias = "show")]
    Info,
}

#[derive(Debug, Subcommand)]
pub enum ThemeCommand {
    /// List themes available on a store.
    List {
        /// Store handle or myshopify.com domain.
        #[arg(long)]
        store: Option<String>,
    },
    /// Download a theme's selected assets into a local directory.
    Pull {
        /// Numeric Shopify theme ID.
        #[arg(long)]
        theme: u64,
        /// Store handle or myshopify.com domain.
        #[arg(long)]
        store: Option<String>,
        /// Include matching asset paths (supports `*` and `?`); repeatable.
        #[arg(long)]
        include: Vec<String>,
        /// Exclude matching asset paths (supports `*` and `?`); repeatable.
        #[arg(long)]
        exclude: Vec<String>,
        /// Destination directory. Defaults to the current directory.
        #[arg(long, short = 'd', default_value = ".")]
        destination: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum InternalCommand {
    /// Hold a minimal runtime open for idle RSS benchmarks.
    Idle {
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
}

/// Execute a parsed command.
pub async fn run(cli: Cli, output: &Output) -> Result<()> {
    match cli.command {
        Some(Command::Version) => print_version(output)?,
        Some(Command::Completion { shell }) => print_completion(shell),
        Some(Command::Internal {
            command: InternalCommand::Idle { seconds },
        }) => tokio::time::sleep(Duration::from_secs(seconds)).await,
        Some(Command::App { .. }) => return Err(not_implemented("app")),
        Some(Command::Theme {
            command: ThemeCommand::List { store },
        }) => list_themes(store.as_deref(), output).await?,
        Some(Command::Theme {
            command:
                ThemeCommand::Pull {
                    theme,
                    store,
                    include,
                    exclude,
                    destination,
                },
        }) => {
            pull_theme(
                theme,
                store.as_deref(),
                &include,
                &exclude,
                &destination,
                output,
            )
            .await?
        }
        None => {
            Cli::command()
                .print_help()
                .map_err(|error| Error::process(error.to_string()))?;
            println!();
        }
    }

    Ok(())
}

fn print_version(output: &Output) -> Result<()> {
    #[derive(serde::Serialize)]
    struct Version<'a> {
        name: &'a str,
        version: &'a str,
    }

    output
        .success(
            &format!("cfy {}", env!("CARGO_PKG_VERSION")),
            &Version {
                name: "cfy",
                version: env!("CARGO_PKG_VERSION"),
            },
        )
        .map_err(|error| {
            Error::with_source(
                cfy_core::ErrorKind::Process,
                "could not write output",
                error,
            )
        })
}

fn print_completion(shell: Shell) {
    let mut command = Cli::command();
    generate(shell, &mut command, "cfy", &mut io::stdout());
}

fn not_implemented(group: &str) -> Error {
    Error::invalid_input(format!(
        "the '{group}' command group is reserved but not implemented yet"
    ))
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, ThemeCommand, format_themes, select_store};
    use cfy_api::theme::Theme;
    use cfy_config::project::Environment;
    use clap::{CommandFactory, Parser, error::ErrorKind};

    #[test]
    fn command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn root_help_matches_snapshot() {
        let help = Cli::command()
            .render_long_help()
            .to_string()
            .replace("\r\n", "\n");
        let snapshot = include_str!("../tests/snapshots/root-help.txt").replace("\r\n", "\n");
        assert_eq!(help, snapshot);
    }

    #[test]
    fn global_flags_are_accepted_after_nested_commands() {
        let cli = Cli::try_parse_from([
            "cfy",
            "app",
            "info",
            "--verbose",
            "--json",
            "--no-color",
            "--non-interactive",
        ])
        .expect("global flags should propagate");

        assert_eq!(cli.global.verbose, 1);
        assert!(cli.global.json);
        assert!(cli.global.no_color);
        assert!(cli.global.non_interactive);
    }

    #[test]
    fn command_and_nested_aliases_parse() {
        let cli = Cli::try_parse_from(["cfy", "a", "show"]).expect("aliases should parse");
        assert!(matches!(cli.command, Some(Command::App { .. })));
    }

    #[test]
    fn theme_list_parses_store_and_global_json_flag() {
        let cli =
            Cli::try_parse_from(["cfy", "theme", "list", "--store", "example", "--json"]).unwrap();

        assert!(cli.global.json);
        assert!(matches!(
            cli.command,
            Some(Command::Theme {
                command: ThemeCommand::List { store: Some(store) }
            }) if store == "example"
        ));
    }

    #[test]
    fn theme_pull_parses_filters_and_destination() {
        let cli = Cli::try_parse_from([
            "cfy",
            "theme",
            "pull",
            "--theme",
            "42",
            "--store",
            "example",
            "--include",
            "assets/*",
            "--exclude",
            "*.map",
            "--destination",
            "theme",
        ])
        .unwrap();
        let Some(Command::Theme {
            command:
                ThemeCommand::Pull {
                    theme,
                    store,
                    include,
                    exclude,
                    destination,
                },
        }) = cli.command
        else {
            panic!("expected theme pull")
        };
        assert_eq!(theme, 42);
        assert_eq!(store.as_deref(), Some("example"));
        assert_eq!(include, ["assets/*"]);
        assert_eq!(exclude, ["*.map"]);
        assert_eq!(destination, std::path::PathBuf::from("theme"));
    }

    #[test]
    fn human_theme_output_is_stable_and_complete() {
        let themes = vec![
            Theme {
                id: 10,
                name: "Dawn".to_owned(),
                role: "main".to_owned(),
                created_at: Some("2026-01-01".to_owned()),
                updated_at: Some("2026-01-02".to_owned()),
                previewable: Some(true),
                processing: Some(false),
            },
            Theme {
                id: 20,
                name: "Development".to_owned(),
                role: "development".to_owned(),
                created_at: None,
                updated_at: None,
                previewable: None,
                processing: None,
            },
        ];

        assert_eq!(
            format_themes(&themes),
            "10\tmain\tDawn\n20\tdevelopment\tDevelopment"
        );
        assert_eq!(format_themes(&[]), "No themes found.");
    }

    #[test]
    fn store_precedence_is_flag_then_environment_then_config() {
        let environment = Environment::from([
            (
                "CFY_STORE".to_owned(),
                "environment.myshopify.com".to_owned(),
            ),
            (
                "SHOPIFY_FLAG_STORE".to_owned(),
                "compatible.myshopify.com".to_owned(),
            ),
        ]);

        assert_eq!(
            select_store(
                Some("flag.myshopify.com"),
                &environment,
                Some("config.myshopify.com")
            )
            .unwrap(),
            "flag.myshopify.com"
        );
        assert_eq!(
            select_store(None, &environment, Some("config.myshopify.com")).unwrap(),
            "environment.myshopify.com"
        );
        assert_eq!(
            select_store(None, &Environment::new(), Some("config.myshopify.com")).unwrap(),
            "config.myshopify.com"
        );
        assert!(select_store(None, &Environment::new(), None).is_err());
    }

    #[test]
    fn unknown_command_suggests_a_valid_command() {
        let error = Cli::try_parse_from(["cfy", "versoin"]).expect_err("typo should fail");
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        assert!(error.to_string().contains("version"));
    }
}
