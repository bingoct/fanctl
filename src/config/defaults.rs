//! Every default value of the two configuration files, in one place.
//!
//! These are functions, not constants, because serde only accepts a *function
//! path* in `#[serde(default = "...")]`; it generates a call, so a `const`
//! cannot be named there. Each one is a single line so there is still exactly
//! one copy of the value.
//!
//! What is absent from this file is deliberate. Only four fields may be left
//! out and still need a value:
//!
//! | field | default |
//! |---|---|
//! | `[platform].ipmitool_path` | `ipmitool` (through `$PATH`) |
//! | `[global].command_timeout_seconds` | 10 |
//! | `[global].debounce_celsius` | 2.0 |
//! | `[global].aggregation` | `max` |
//!
//! Everything else is either required (`interval_seconds`, the duty cycle
//! bounds, `cmd_template`, a sensor's `sdr` and `id`, ...), or inherits instead
//! of defaulting (`[[zones]].aggregation`, `.default_duty_cycle`,
//! `.debounce_celsius` read them from `[global]`). The bare `#[serde(default)]`
//! uses in `policy.rs` are not defaults either: `Vec::new()` means "no rules",
//! and `None` means "inherit".
//!
//! Only defaults belong here. Values that are not part of the configuration
//! stay next to the code that uses them, and this table is the index:
//!
//! | value | where | why there |
//! |---|---|---|
//! | shutdown poll interval | [`crate::control::engine`] | paces the loop that sleeps on it |
//! | child process backoff | [`crate::util::process`] | same |
//! | plausible temperature range | [`crate::sensors`] | it decides when to shout, not what to do |
//! | duty cycle bounds | [`crate::config`] | a contract, not a default |

use std::path::PathBuf;

use super::policy::Aggregation;

/// `[platform].ipmitool_path`: resolved through `$PATH`.
pub(super) fn ipmitool_path() -> PathBuf {
    PathBuf::from("ipmitool")
}

/// `[global].command_timeout_seconds`.
///
/// 10 s, not something tighter. `ipmitool sdr` measured 2.9 s on a TYAN S8030
/// with a healthy BMC, and a BMC that is still coming up - which is exactly when
/// the daemon starts under systemd - is slower than that. A timeout that is too
/// short throws away a whole cycle of readings and logs a warning every cycle
/// instead of sampling.
pub(super) fn command_timeout_seconds() -> u64 {
    10
}

/// `[global].debounce_celsius`.
pub(super) fn debounce_celsius() -> f64 {
    2.0
}

/// `[global].aggregation`: the highest reading wins.
pub(super) fn aggregation() -> Aggregation {
    Aggregation::Max
}
