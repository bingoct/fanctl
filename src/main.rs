//! `fanctl` binary: parse the command line, set up logging, dispatch.
//!
//! Intentionally thin - everything testable lives in the library.

use std::process::ExitCode;

use clap::Parser;

use fanctl::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    fanctl::logging::init(cli.verbose);

    match fanctl::app::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err}");
            ExitCode::FAILURE
        }
    }
}
