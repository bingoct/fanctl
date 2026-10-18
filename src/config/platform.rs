//! Hardware description (`platform.toml`).
//!
//! Nothing here knows about zones, curves, or hysteresis: this layer only maps
//! semantic names to the raw addresses `ipmitool` expects, which is what keeps
//! the control code free of board specific constants.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::Deserialize;

use crate::util::name::{normalize, GPU_SOURCE};

/// Contents of `platform.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformConfig {
    /// The `[platform]` table.
    #[serde(rename = "platform")]
    pub info: PlatformInfo,
    /// Logical pin name -> hexadecimal fan id, e.g. `CPU_FAN = "0x00"`.
    pub pins: BTreeMap<String, String>,
    /// Semantic sensor name -> how to read it.
    pub sensors: BTreeMap<String, SensorSpec>,
}

/// One temperature sensor: its SDR name and its IPMI sensor number.
///
/// Both are needed. The number drives the read (`raw 0x04 0x2d <n>`, which is
/// 25 times faster than `ipmitool sdr` on a TYAN S8030), and the name is what
/// `check` prints, matches against the SDR table, and uses to catch a sensor
/// that was renumbered by a firmware update.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SensorSpec {
    /// Name exactly as `ipmitool sdr` prints it.
    pub sdr: String,
    /// IPMI sensor number, written like a pin id (`"0x01"`).
    #[serde(deserialize_with = "deserialize_hex_byte")]
    pub id: u8,
}

/// Deserializes a hex byte eagerly, so an invalid number is a load error
/// pointing at the file rather than a failure in the control loop.
fn deserialize_hex_byte<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse_hex_byte(&raw).map_err(serde::de::Error::custom)
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
    #[serde(default = "super::defaults::ipmitool_path")]
    pub ipmitool_path: PathBuf,
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
        let mut sensor_ids: HashMap<u8, &str> = HashMap::new();
        for (name, spec) in &self.sensors {
            if name.trim().is_empty() {
                problems.push("[sensors] contains an empty sensor name".to_string());
            }
            if spec.sdr.trim().is_empty() {
                problems.push(format!("[sensors].{name}: sdr must not be empty"));
            }
            // Two names for one number means one of them reads the other's
            // temperature; there is no reading in which that is intended.
            if let Some(first) = sensor_ids.insert(spec.id, name.as_str()) {
                problems.push(format!(
                    "[sensors]: `{first}` and `{name}` both use sensor number 0x{:02x}",
                    spec.id
                ));
            }
        }

        // Names are compared case-insensitively everywhere, so two keys that
        // only differ in case would make one of them unreachable.
        check_unique_names("[pins]", self.pins.keys(), &mut problems);
        check_unique_names("[sensors]", self.sensors.keys(), &mut problems);

        // "GPU" is taken by the NVML provider; a sensor of that name would be
        // shadowed in every `monitor_sources` entry, with no diagnostic.
        if self.find_sensor(GPU_SOURCE).is_some() {
            problems.push(format!(
                "[sensors] must not define `{GPU_SOURCE}`: the name is reserved for the NVIDIA \
                 readings and would shadow the SDR sensor"
            ));
        }

        problems
    }

    /// Key of `pin` as declared in `[pins]`, matched case-insensitively.
    ///
    /// Returning the declared spelling lets the policy layer rewrite its own
    /// references to it, so the rest of the program can compare exactly.
    pub fn find_pin(&self, pin: &str) -> Option<&str> {
        find_key(&self.pins, pin)
    }

    /// Key of `semantic` as declared in `[sensors]`, matched case-insensitively.
    pub fn find_sensor(&self, semantic: &str) -> Option<&str> {
        find_key(&self.sensors, semantic)
    }

    /// Hexadecimal id configured for `pin`, `None` for an unknown pin.
    pub fn pin_id(&self, pin: &str) -> Option<&str> {
        self.pins.get(pin).map(String::as_str)
    }
}

/// Declared key matching `wanted` case-insensitively.
fn find_key<'a, V>(map: &'a BTreeMap<String, V>, wanted: &str) -> Option<&'a str> {
    let wanted = normalize(wanted);
    map.keys()
        .map(String::as_str)
        .find(|name| normalize(name) == wanted)
}

/// Reports keys that collapse to the same name once case is ignored.
fn check_unique_names<'a>(
    section: &str,
    names: impl Iterator<Item = &'a String>,
    problems: &mut Vec<String>,
) {
    let mut seen: HashMap<String, &str> = HashMap::new();
    for name in names {
        if let Some(first) = seen.insert(normalize(name), name.as_str()) {
            problems.push(format!(
                "{section}: `{first}` and `{name}` are the same name (case is ignored)"
            ));
        }
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
