//! Error types shared by every layer.
//!
//! There are two enums, split by what the caller can do about the failure:
//!
//! * [`Error`] aborts the current operation. It is either returned from
//!   `main` (startup, `check`) or logged and dropped by the control loop.
//! * [`CommandError`] describes why an external process failed. The
//!   distinction matters because the three cases need different reactions:
//!   a missing binary never recovers by itself, a timeout means "the BMC did
//!   not answer, retry next cycle", and a non-zero exit is the BMC answering
//!   with an error.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use thiserror::Error;

/// Every failure that can abort a `fanctl` operation.
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

    /// The configured `ipmitool` binary does not exist.
    ///
    /// Separate from [`CommandError::Spawn`] because nothing will change on the
    /// next cycle: this one is worth a single loud message, not a warning every
    /// ten seconds.
    #[error("`{path}` not found - install ipmitool or fix [platform].ipmitool_path")]
    IpmitoolMissing {
        /// Path taken from `[platform].ipmitool_path`.
        path: PathBuf,
    },

    /// An external command could not be run to completion.
    #[error(transparent)]
    Command(#[from] CommandError),

    /// A pin used at runtime is not declared in `platform.toml` `[pins]`.
    ///
    /// Validation rejects this before the daemon starts, so reaching it means
    /// a bug in the caller - not a bad configuration file.
    #[error("pin `{0}` is not defined in platform.toml [pins]")]
    UnknownPin(String),

    /// The signal handlers could not be installed.
    #[error("signal handling: {0}")]
    Signal(String),

    /// Generic I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Why an external command did not produce a usable result.
#[derive(Debug, Error)]
pub enum CommandError {
    /// The process could not be started: missing binary, permission denied, ...
    #[error("cannot start `{program}`: {source}")]
    Spawn {
        /// Program that was requested.
        program: String,
        /// Reason the spawn failed.
        #[source]
        source: io::Error,
    },

    /// The process did not finish in time and was killed.
    #[error("`{command}` did not finish within {timeout:?} and was killed")]
    Timeout {
        /// The command that hung, for the log.
        command: String,
        /// Deadline that expired.
        timeout: Duration,
    },

    /// The process exited with a non-zero status.
    #[error("`{command}` exited with {status}: {details}")]
    Failed {
        /// The command that ran, for the log.
        command: String,
        /// Exit status reported by the process.
        status: ExitStatus,
        /// Trimmed stderr, or the first stdout line when stderr was empty.
        details: String,
    },

    /// Waiting for the process or reading its output failed.
    #[error("`{command}`: {source}")]
    Wait {
        /// The command that ran, for the log.
        command: String,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}

impl CommandError {
    /// `true` when the binary itself is missing.
    pub fn is_missing_program(&self) -> bool {
        matches!(
            self,
            CommandError::Spawn { source, .. } if source.kind() == io::ErrorKind::NotFound
        )
    }
}
