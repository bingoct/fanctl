//! Hardware description (`platform.toml`).
//!
//! Nothing here knows about zones, curves, or hysteresis: this layer only maps
//! semantic names to the raw addresses `ipmitool` expects, which is what keeps
//! the control code free of board specific constants.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

/// Contents of `platform.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformConfig {
    /// The `[platform]` table.
    #[serde(rename = "platform")]
    pub info: PlatformInfo,
    /// Logical pin name -> hexadecimal fan id, e.g. `CPU_FAN = "0x00"`.
    pub pins: BTreeMap<String, String>,
    /// Semantic sensor name -> the name printed by `ipmitool sdr`.
    pub sensors: BTreeMap<String, String>,
}

/// Static board description.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformInfo {
    /// Board name, used in logs and reports only.
    pub name: String,
    /// `ipmitool` argument template containing `{id}` and `{pwm}`.
    pub cmd_template: String,
    /// Path to the `ipmitool` executable.
    #[serde(default = "default_ipmitool_path")]
    pub ipmitool_path: PathBuf,
}

fn default_ipmitool_path() -> PathBuf {
    PathBuf::from("ipmitool")
}

impl PlatformConfig {
    /// Returns every problem found in the file; an empty vector means "valid".
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();

        if self.info.name.trim().is_empty() {
            problems.push("[platform].name must not be empty".to_string());
        }
        if self.info.cmd_template.trim().is_empty() {
            problems.push("[platform].cmd_template must not be empty".to_string());
        }
        for placeholder in ["{id}", "{pwm}"] {
            if !self.info.cmd_template.contains(placeholder) {
                problems.push(format!(
                    "[platform].cmd_template must contain the `{placeholder}` placeholder"
                ));
            }
        }
        if self.info.ipmitool_path.as_os_str().is_empty() {
            problems.push("[platform].ipmitool_path must not be empty".to_string());
        }
        if self.pins.is_empty() {
            problems.push("[pins] must define at least one fan".to_string());
        }
        if self.sensors.is_empty() {
            problems.push("[sensors] must define at least one sensor".to_string());
        }

        for (name, raw_id) in &self.pins {
            if name.trim().is_empty() {
                problems.push("[pins] contains an empty pin name".to_string());
            }
            if let Err(err) = parse_hex_byte(raw_id) {
                problems.push(format!("[pins].{name}: {err}"));
            }
        }
        for (name, sdr_name) in &self.sensors {
            if name.trim().is_empty() {
                problems.push("[sensors] contains an empty sensor name".to_string());
            }
            if sdr_name.trim().is_empty() {
                problems.push(format!("[sensors].{name}: SDR name must not be empty"));
            }
        }

        problems
    }

    /// Hexadecimal id configured for `pin`, `None` for an unknown pin.
    pub fn pin_id(&self, pin: &str) -> Option<&str> {
        self.pins.get(pin).map(String::as_str)
    }
}

/// Parses `0x1f` or `1f` into a byte.
pub fn parse_hex_byte(raw: &str) -> Result<u8, String> {
    let trimmed = raw.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);

    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("`{raw}` is not a hexadecimal byte such as `0x1f`"));
    }

    u8::from_str_radix(digits, 16)
        .map_err(|_| format!("`{raw}` does not fit into a single byte (0x00-0xFF)"))
}

#[cfg(test)]
mod tests {
    use super::parse_hex_byte;

    #[test]
    fn accepts_prefixed_and_bare_hex() {
        assert_eq!(parse_hex_byte("0x00"), Ok(0));
        assert_eq!(parse_hex_byte("0x1F"), Ok(31));
        assert_eq!(parse_hex_byte("ff"), Ok(255));
        assert_eq!(parse_hex_byte(" 0x0a "), Ok(10));
    }

    #[test]
    fn rejects_invalid_bytes() {
        assert!(parse_hex_byte("").is_err());
        assert!(parse_hex_byte("0x").is_err());
        assert!(parse_hex_byte("0x100").is_err());
        assert!(parse_hex_byte("0xzz").is_err());
        // Bare digits are still hexadecimal, not decimal.
        assert_eq!(parse_hex_byte("12"), Ok(0x12));
    }
}
