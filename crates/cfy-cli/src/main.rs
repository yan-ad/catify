use cfy_cli::{Cli, output::Output, run};
use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let output = Output::new(cli.global.json, cli.global.verbose);
    let _ = output.diagnostic("debug diagnostics enabled");

    match run(cli, &output).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            let _ = output.error(&error);
            error.exit_code()
        }
    }
}
