//! Error type shared by every layer.

use std::path::PathBuf;

use thiserror::Error;

/// Every failure that can abort a `fanctl` operation.
///
/// The daemon deliberately has a single error enum: a control loop only ever
/// reports and retries, so error *categories* matter more than precise types.
#[derive(Debug, Error)]
pub enum Error {
    /// A configuration file could not be read.
    #[error("cannot read {path}: {source}")]
    ReadFile {
        /// File that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A configuration file is not valid TOML or does not match the schema.
    ///
    /// The TOML error is boxed: it carries a lot of context and must not bloat
    /// every `Result` in the daemon.
    #[error("cannot parse {path}: {source}")]
    Parse {
        /// File that failed to parse.
        path: PathBuf,
        /// TOML deserialisation error.
        #[source]
        source: Box<toml::de::Error>,
    },

    /// The file parses but the combination of settings is unsafe or meaningless.
    #[error("invalid configuration:\n{0}")]
    Validation(String),

    /// `ipmitool` is missing, failed, or timed out.
    #[error("ipmitool: {0}")]
    Ipmi(String),

    /// The signal handlers could not be installed.
    #[error("signal handling: {0}")]
    Signal(String),

    /// Generic I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
