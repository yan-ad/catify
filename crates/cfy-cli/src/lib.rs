pub mod output;

use crate::output::Output;
use cfy_core::{Error, Result};
use clap::{ArgAction, Args, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use std::{io, time::Duration};

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
    /// Display the currently selected theme environment.
    #[command(alias = "show")]
    Info,
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
        Some(Command::Theme { .. }) => return Err(not_implemented("theme")),
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
    use super::{Cli, Command};
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
        assert_eq!(help, include_str!("../tests/snapshots/root-help.txt"));
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
    fn unknown_command_suggests_a_valid_command() {
        let error = Cli::try_parse_from(["cfy", "versoin"]).expect_err("typo should fail");
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        assert!(error.to_string().contains("version"));
    }
}
