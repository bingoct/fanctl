//! `tracing` setup: every record goes to stdout so systemd can put it in the
//! journal.

use std::io::IsTerminal;

use tracing_subscriber::fmt;
use tracing_subscriber::EnvFilter;

/// Installs the global subscriber.
///
/// `RUST_LOG` wins when it is set (handy for `RUST_LOG=fanctl::control=trace`
/// while debugging a single layer); the `-v` flags are the fallback.
pub fn init(verbosity: u8) {
    let directive = match verbosity {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(directive));
    let ansi = std::io::stdout().is_terminal();

    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stdout)
        .with_ansi(ansi)
        .with_target(false)
        .init();
}
