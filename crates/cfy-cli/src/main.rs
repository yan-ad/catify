use clap::{Parser, Subcommand};
use std::{process::ExitCode, time::Duration};

#[derive(Debug, Parser)]
#[command(
    name = "cfy",
    version,
    about = "A fast, memory-efficient Shopify CLI alternative"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print build and runtime version information.
    Version,
    #[command(hide = true)]
    Internal {
        #[command(subcommand)]
        command: InternalCommand,
    },
}

#[derive(Debug, Subcommand)]
enum InternalCommand {
    /// Hold a minimal runtime open for idle RSS benchmarks.
    Idle {
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Some(Command::Version) => println!("cfy {}", env!("CARGO_PKG_VERSION")),
        Some(Command::Internal {
            command: InternalCommand::Idle { seconds },
        }) => tokio::time::sleep(Duration::from_secs(seconds)).await,
        None => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            println!();
        }
    }
    ExitCode::SUCCESS
}
