//! Command line surface.
//!
//! The binary never daemonises itself: it logs to stdout and blocks in the
//! foreground so that systemd (or a terminal, or `tmux`) owns the process
//! lifecycle and the log stream.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Top level command line.
#[derive(Debug, Parser)]
#[command(
    name = "fanctl",
    version,
    about = "Hardware-agnostic IPMI fan control daemon",
    long_about = "Reads temperatures through `ipmitool sdr` and NVML, evaluates the \
                  configured fan curves, and writes PWM duty cycles back through \
                  `ipmitool raw`."
)]
pub struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Command,

    /// Business logic configuration (zones, curves, follow rules).
    #[arg(
        short = 'c',
        long,
        global = true,
        value_name = "PATH",
        default_value = "/etc/fanctl/config.toml"
    )]
    pub config: PathBuf,

    /// Hardware description (pin ids, sensor names, command template).
    #[arg(
        short = 'p',
        long,
        global = true,
        value_name = "PATH",
        default_value = "/etc/fanctl/platform.toml"
    )]
    pub platform: PathBuf,

    /// Increase log verbosity: `-v` = DEBUG, `-vv` = TRACE.
    #[arg(short = 'v', long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// Available subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Validate the configuration files and probe the current hardware.
    Check,

    /// Run the control loop in the foreground until SIGINT/SIGTERM.
    Run {
        /// Read sensors and log the decisions, but never send a PWM command.
        #[arg(long)]
        dry_run: bool,
    },
}
