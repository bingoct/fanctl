//! Identifier canonicalisation shared by every layer.
//!
//! Pin, sensor and zone names come from TOML, where `CPU_FAN`, `cpu_fan` and
//! `Cpu_Fan` are three distinct keys but obviously mean the same fan. Treating
//! them as distinct buys nothing and turns a typo into a silently missing rule,
//! so every comparison goes through this module instead of comparing raw
//! strings. Keeping the folding in one place is also what stops `check` and the
//! daemon from disagreeing about whether a name matches.

/// The one `monitor_sources` entry that is not an IPMI sensor.
///
/// Declared here because both the configuration layer (it must reject a sensor
/// with this name) and the sensor layer (it must route it to NVML) need it.
pub const GPU_SOURCE: &str = "GPU";

/// Canonical form of a configured identifier: trimmed and ASCII lowercase.
///
/// Idempotent, so it can be applied at the boundary and again at lookup time.
pub fn normalize(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Canonical form used to match names printed by `ipmitool sdr`.
///
/// BMC firmware prints the same sensor as `CPU_Tctl_Value`, `CPU Tctl Value`
/// or `cpu-tctl-value`, so separators are dropped on top of [`normalize`].
pub fn sdr_key(raw: &str) -> String {
    normalize(raw)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

/// `true` when `raw` is the reserved GPU source name, in any spelling.
pub fn is_gpu_source(raw: &str) -> bool {
    normalize(raw) == normalize(GPU_SOURCE)
}

#[cfg(test)]
mod tests {
    use super::{is_gpu_source, normalize, sdr_key};

    #[test]
    fn normalization_ignores_case_and_padding() {
        assert_eq!(normalize("  CPU_FAN "), "cpu_fan");
        assert_eq!(normalize("cpu_fan"), "cpu_fan");
        assert_eq!(normalize("Cpu_Fan"), normalize("cPu_fAn"));
    }

    #[test]
    fn sdr_keys_ignore_separators() {
        assert_eq!(sdr_key("CPU_Tctl_Value"), sdr_key("CPU Tctl Value"));
        assert_eq!(sdr_key("CPU-TCTL-VALUE"), "cputctlvalue");
    }

    #[test]
    fn gpu_source_matches_any_spelling() {
        assert!(is_gpu_source("GPU"));
        assert!(is_gpu_source("gpu"));
        assert!(is_gpu_source(" Gpu "));
        assert!(!is_gpu_source("gpu2"));
        assert!(!is_gpu_source("CPU_Tctl"));
    }
}
