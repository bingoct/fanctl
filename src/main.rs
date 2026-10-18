//! `fanctl` binary: parse the command line, set up logging, dispatch.
//!
//! Intentionally thin - everything testable lives in the library.

use std::process::ExitCode;

use clap::Parser;

use fanctl::app::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    fanctl::logging::init(cli.verbose);
    fanctl::app::run(cli)
}
