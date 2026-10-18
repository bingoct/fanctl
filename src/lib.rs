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
//!
//! Each module is `foo.rs` next to `foo/`, the layout `cargo new` produces and
//! the one the 2018 edition made possible; `mod.rs` is not used anywhere.
//!
//! Four files sit at the root because they are spoken by every layer or by
//! `main` directly: [`error`] (the shared vocabulary), [`actuator`] (the only
//! writer of the BMC), [`logging`] (process-wide subscriber setup) and the
//! usual `main` / `lib` pair. Everything else lives in one of five directories
//! grouped by concern: [`app`], [`config`], [`sensors`], [`control`], [`util`].

#![forbid(unsafe_code)]

pub mod actuator;
pub mod app;
pub mod config;
pub mod control;
pub mod error;
pub mod logging;
pub mod sensors;
pub mod util;

pub use error::Error;

/// Crate version, taken from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
