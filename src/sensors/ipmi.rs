//! `ipmitool sdr` reader.
//!
//! The output of `ipmitool sdr` is line oriented:
//!
//! ```text
//! CPU_Tctl_Value   | 45 degrees C      | ok
//! FAN1             | 3200 RPM          | ok
//! ```
//!
//! Only the first two fields matter here: the sensor name and the reading. A
//! reading that is not a temperature (`RPM`, `Volts`, `no reading`, ...) yields
//! `None` so the caller can tell "sensor exists but is not a temperature" apart
//! from "sensor does not exist".

use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;

use crate::error::Error;
use crate::process::output_with_timeout;

/// One line of `ipmitool sdr`.
#[derive(Debug, Clone, PartialEq)]
pub struct SdrEntry {
    /// Sensor name exactly as printed by the BMC.
    pub name: String,
    /// Temperature in degrees Celsius, `None` for non-temperature readings.
    pub celsius: Option<f64>,
}

/// Runs `ipmitool sdr` and parses the result.
pub fn read_sdr(ipmitool: &Path, timeout: Duration) -> Result<Vec<SdrEntry>, Error> {
    let mut command = Command::new(ipmitool);
    command.arg("sdr");

    let output = output_with_timeout(command, timeout).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            Error::Ipmi(format!(
                "`{}` not found - install ipmitool or fix [platform].ipmitool_path",
                ipmitool.display()
            ))
        } else {
            Error::Ipmi(format!(
                "could not run `{} sdr`: {source}",
                ipmitool.display()
            ))
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Ipmi(format!(
            "`ipmitool sdr` failed with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_sdr(&stdout))
}

/// Parses raw `ipmitool sdr` output.
///
/// Kept as a free function so it can be unit tested without a BMC.
pub fn parse_sdr(text: &str) -> Vec<SdrEntry> {
    let mut entries = Vec::new();

    for line in text.lines() {
        if !line.contains('|') {
            // Header noise, warnings printed on stdout, blank lines.
            continue;
        }
        let mut fields = line.split('|');
        let name = match fields.next() {
            Some(field) => field.trim(),
            None => continue,
        };
        if name.is_empty() {
            continue;
        }
        let value = fields.next().map(str::trim).unwrap_or_default();
        entries.push(SdrEntry {
            name: name.to_string(),
            celsius: parse_celsius(value),
        });
    }

    entries
}

/// Extracts `45` from `45 degrees C`; returns `None` for anything else.
fn parse_celsius(value: &str) -> Option<f64> {
    let captures = celsius_pattern().captures(value)?;
    let number = captures.get(1)?.as_str();
    number.parse::<f64>().ok()
}

fn celsius_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?i)^(-?\d+(?:\.\d+)?)\s*degrees?\s+C").expect("static regex is valid")
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_celsius, parse_sdr};

    const SAMPLE: &str = "\
CPU_Tctl_Value   | 45 degrees C      | ok
CPU_MOSFET       | 38.50 degrees C   | ok
FAN1             | 3200 RPM          | ok
PSU_Status       | 0x01              | ok
Broken_Sensor    | no reading        | ns
";

    #[test]
    fn parses_temperatures_only() {
        let entries = parse_sdr(SAMPLE);
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0].name, "CPU_Tctl_Value");
        assert_eq!(entries[0].celsius, Some(45.0));
        assert_eq!(entries[1].celsius, Some(38.5));
        assert_eq!(entries[2].celsius, None);
        assert_eq!(entries[3].celsius, None);
        assert_eq!(entries[4].celsius, None);
    }

    #[test]
    fn ignores_lines_without_separator() {
        let entries = parse_sdr("Loading SDR...\nCPU_Tctl_Value | 41 degrees C | ok\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].celsius, Some(41.0));
    }

    #[test]
    fn parsing_is_case_and_space_tolerant() {
        assert_eq!(parse_celsius("41 Degrees C"), Some(41.0));
        assert_eq!(parse_celsius("-5 degrees c"), Some(-5.0));
        assert_eq!(parse_celsius("41 C"), None);
        assert_eq!(parse_celsius("41 degrees F"), None);
    }
}
