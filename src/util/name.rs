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

/// Parses a reserved GPU source name in any spelling.
///
/// `GPU` selects every device NVML reports and folds them with the zone
/// aggregation (the historical behaviour); `GPU0`, `GPU1`, ... select a single
/// device by its NVML index. Which index belongs to which card is the
/// operator's business - we deliberately do not guess from the model name,
/// because two identical cards cannot be told apart that way.
///
/// Returns `None` when `raw` is not a GPU source at all, so the caller can fall
/// back to treating it as a `[sensors]` name.
///
/// The outer `Option` says "is this a GPU source", the inner one says "all
/// devices" (`None`) or "this index" (`Some`).
pub fn parse_gpu_source(raw: &str) -> Option<Option<u32>> {
    let normalized = normalize(raw);
    let rest = normalized.strip_prefix(&normalize(GPU_SOURCE))?;
    if rest.is_empty() {
        return Some(None);
    }
    // `GPU00` is accepted as index 0; a suffix that is not a plain number
    // (`GPU-X`, `GPU_ALL`, ...) is not a GPU source and falls through to the
    // `[sensors]` lookup, where validation reports it as unknown. The trim
    // accepts `GPU 1`, which is how someone reads "GPU number one".
    rest.trim().parse().ok().map(Some)
}

#[cfg(test)]
mod tests {
    use super::{normalize, parse_gpu_source, sdr_key};

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
        assert_eq!(parse_gpu_source("GPU"), Some(None));
        assert_eq!(parse_gpu_source("gpu"), Some(None));
        assert_eq!(parse_gpu_source(" Gpu "), Some(None));
    }

    #[test]
    fn gpu_source_accepts_an_optional_index() {
        assert_eq!(parse_gpu_source("GPU0"), Some(Some(0)));
        assert_eq!(parse_gpu_source("gpu1"), Some(Some(1)));
        assert_eq!(parse_gpu_source(" Gpu 10 "), Some(Some(10)));
        assert_eq!(parse_gpu_source("GPU00"), Some(Some(0)));
    }

    #[test]
    fn gpu_source_rejects_a_non_numeric_suffix() {
        // These must fall through to the `[sensors]` lookup so validation can
        // report them as unknown names instead of silently reading every GPU.
        assert_eq!(parse_gpu_source("GPUX"), None);
        assert_eq!(parse_gpu_source("GPU-1"), None);
        assert_eq!(parse_gpu_source("GPU_0"), None);
        assert_eq!(parse_gpu_source("CPU_Tctl"), None);
    }
}
