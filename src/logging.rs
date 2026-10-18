//! `tracing` setup: every record goes to stdout so systemd can put it in the
//! journal.
//!
//! No ANSI colours on purpose: the daemon runs detached, its output is read by
//! `journalctl`, and `tracing-subscriber` is built without the `ansi` feature.
//!
//! The filter is a plain [`LevelFilter`]. `env-filter` would pull
//! `regex-automata` into the binary to support directives such as
//! `fanctl::control=trace`; for a crate with a handful of modules, `-v` and
//! `-vv` cover the same need at no cost.

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt;

/// Installs the global subscriber.
///
/// `RUST_LOG` wins when it holds a level (`RUST_LOG=debug`); the `-v` flags are
/// the fallback.
pub fn init(verbosity: u8) {
    let level = env_level().unwrap_or(match verbosity {
        0 => LevelFilter::INFO,
        1 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    });

    fmt()
        .with_max_level(level)
        .with_writer(std::io::stdout)
        .with_target(false)
        .init();
}

/// Reads `RUST_LOG`, accepting only a global level.
///
/// Per-target directives (`fanctl::control=trace`) are reported rather than
/// silently ignored: a debugging session that believes it set TRACE and gets
/// INFO is worse than one that is told the directive is unsupported.
fn env_level() -> Option<LevelFilter> {
    let raw = std::env::var("RUST_LOG").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    match raw.parse() {
        Ok(level) => Some(level),
        Err(_) => {
            eprintln!(
                "fanctl: ignoring RUST_LOG={raw:?}; only a global level \
                 (error|warn|info|debug|trace|off) is supported"
            );
            None
        }
    }
}
