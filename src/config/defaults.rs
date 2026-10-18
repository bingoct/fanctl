//! Every default value of the two configuration files, in one place.
//!
//! serde needs a function path, so the schema references these as
//! `#[serde(default = "super::defaults::x")]`. Keeping them together means
//! "what is the default timeout?" has one answer: this file. Previously they
//! sat next to their fields in `platform.rs` and `policy.rs`, so changing a
//! default meant already knowing which file the field lived in.
//!
//! Only defaults belong here. Runtime tuning - the child process polling
//! backoff in [`crate::util::process`], the shutdown poll interval in
//! [`crate::control::engine`] - stays next to the loop it paces, because those
//! numbers are only meaningful beside the code that sleeps on them.
//!
//! [`Aggregation`] is the exception: its default is `#[default] Max` on the
//! enum itself. There is one value, the compiler keeps it in sync with the
//! variant list, and a function here would only add a second place to look.

use std::path::PathBuf;

/// `[platform].ipmitool_path`: resolved through `$PATH`.
pub(super) fn ipmitool_path() -> PathBuf {
    PathBuf::from("ipmitool")
}

/// `[global].command_timeout_seconds`.
pub(super) fn command_timeout_seconds() -> u64 {
    5
}

/// `[global].debounce_celsius`.
pub(super) fn debounce_celsius() -> f64 {
    2.0
}
