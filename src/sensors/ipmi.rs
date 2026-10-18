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

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::error::Error;
use crate::util::process::run_checked;

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

    let output = run_checked(command, timeout).map_err(|err| {
        if err.is_missing_program() {
            // A missing dependency is not a transient hardware fault.
            Error::IpmitoolMissing {
                path: ipmitool.to_path_buf(),
            }
        } else {
            Error::Command(err)
        }
    })?;

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
///
/// Hand written rather than a regex. For a temperature SDR, `ipmitool` prints
/// exactly three whitespace separated tokens: `<number> degrees C` (the
/// `sdr` implementation formats it, it is not free form text). Three tokens
/// and a case insensitive comparison cover that grammar completely, and the
/// alternative costs about 1.4 MB of `regex-automata` in the binary.
///
/// Differences from the regex it replaces, none of which `ipmitool` can emit:
/// it rejects `45degrees C` (no space), and it accepts `+5` or `1e3` as the
/// number, because `f64::from_str` is more liberal than `-?\d+(\.\d+)?`.
/// Non-finite values are rejected explicitly, so `inf degrees C` cannot become
/// a threshold comparison.
fn parse_celsius(value: &str) -> Option<f64> {
    let mut tokens = value.split_whitespace();
    let number: f64 = tokens.next()?.parse().ok()?;
    let degrees = tokens.next()?;
    let unit = tokens.next()?;

    let is_degrees =
        degrees.eq_ignore_ascii_case("degree") || degrees.eq_ignore_ascii_case("degrees");
    if !is_degrees || !unit.eq_ignore_ascii_case("c") || !number.is_finite() {
        return None;
    }

    Some(number)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{parse_celsius, parse_sdr, read_sdr};
    use crate::error::Error;

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
        assert_eq!(parse_celsius("  41 degrees C  "), Some(41.0));
        assert_eq!(parse_celsius("41 C"), None);
        assert_eq!(parse_celsius("41 degrees F"), None);
    }

    #[test]
    fn malformed_readings_are_rejected() {
        assert_eq!(parse_celsius(""), None);
        assert_eq!(parse_celsius("no reading"), None);
        assert_eq!(parse_celsius("45 degrees"), None);
        assert_eq!(parse_celsius("45 degrees C (sensor failed)"), Some(45.0));
    }

    #[test]
    fn non_finite_readings_are_rejected() {
        // A threshold comparison against `inf` would pin the fans at 100 %
        // forever, so a value that parsed must also be a real measurement.
        assert_eq!(parse_celsius("inf degrees C"), None);
        assert_eq!(parse_celsius("NaN degrees C"), None);
    }

    #[test]
    fn a_missing_binary_is_not_a_hardware_fault() {
        let missing = PathBuf::from("fanctl-does-not-exist-9c3e");
        match read_sdr(&missing, Duration::from_secs(5)) {
            Err(Error::IpmitoolMissing { path }) => assert_eq!(path, missing),
            other => panic!("expected IpmitoolMissing, got {other:?}"),
        }
    }
}
