//! `fanctl` - a hardware-agnostic IPMI fan control engine.
//!
//! The crate is split into layers so that board specific details can never leak
//! into the control logic:
//!
//! | layer | module | responsibility |
//! |-------|--------|----------------|
//! | hardware | [`config::platform`] | pin ids, SDR sensor names, `ipmitool` template |
//! | policy | [`config::policy`] | zones, curves, follow rules |
//! | acquisition | [`sensors`] | `ipmitool sdr` and NVML readings |
//! | decision | [`control`] | aggregation, hysteresis, conflict merging |
//! | acting | [`actuator`] | rendered `ipmitool raw` invocations |
//!
//! Everything the daemon needs is decided in [`control`]; [`actuator`] only
//! executes. That split is what makes the core testable without any hardware.

#![forbid(unsafe_code)]

pub mod actuator;
pub mod app;
pub mod cli;
pub mod config;
pub mod control;
pub mod error;
pub mod logging;
pub mod process;
pub mod sensors;

pub use error::Error;

/// Crate version, taken from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
