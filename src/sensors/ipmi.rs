//! IPMI readers: the full `sdr` table for diagnostics, and a batched raw read
//! for the control loop.
//!
//! The output of `ipmitool sdr` is line oriented:
//!
//! ```text
//! CPU_Tctl_Value   | 45 degrees C      | ok
//! FAN1             | 3200 RPM          | ok
//! ```
//!
//! Only the first two fields matter for that path: the sensor name and the
//! reading. A reading that is not a temperature (`RPM`, `Volts`, `no reading`,
//! ...) yields `None` so the caller can tell "sensor exists but is not a
//! temperature" apart from "sensor does not exist".
//!
//! Reading the whole table costs 2.9 s on a TYAN S8030, because every line is
//! its own Get Sensor Reading round trip. The control loop therefore reads only
//! the sensors it actually watches, one Get Sensor Reading each, through a
//! single `ipmitool exec` process - measured at 105 ms for one sensor and
//! 155 ms for five.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::error::Error;
use crate::util::process::{run_capturing_with_stdin, run_checked};

/// `raw 0x06 0x01` is Get Device ID: it always prints 15 bytes, which makes it
/// a separator the BMC cannot skip.
const SEPARATOR_COMMAND: &str = "raw 0x06 0x01";
/// Number of bytes a Get Device ID response prints.
const SEPARATOR_BYTES: usize = 15;
/// `byte 2`, bit 5 of a Get Sensor Reading response: reading unavailable.
const READING_UNAVAILABLE: u8 = 0x20;
/// Where `ipmitool exec` reads its command list from.
const SCRIPT_PATH: &str = "/dev/stdin";

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

/// Result of one batched read.
#[derive(Debug)]
pub struct SensorBatch {
    /// One entry per requested number, in the order requested. `None` means the
    /// command failed for that sensor or the BMC reported it unavailable.
    pub values: Vec<Option<f64>>,
    /// First stderr line when `ipmitool` exited non-zero, for logging. A
    /// partial failure is normal here: one dead sensor does not invalidate the
    /// others.
    pub failure: Option<String>,
}

/// Builds the command list a batched read feeds to `ipmitool exec`.
///
/// The numbers never change after startup, so the hub builds this once and
/// reuses it: the control loop does no formatting and no allocation here.
pub fn build_script(numbers: &[u8]) -> String {
    // Roughly 40 bytes per command.
    let mut script = String::with_capacity(numbers.len() * 48);
    for number in numbers {
        script.push_str(SEPARATOR_COMMAND);
        script.push('\n');
        // netfn 0x04 = sensor/event, command 0x2d = Get Sensor Reading.
        let _ = writeln!(script, "raw 0x04 0x2d 0x{number:02x}");
    }
    script
}

/// Reads every sensor in `script` with a single `ipmitool exec` process.
///
/// Each read is preceded by a Get Device ID command whose 15 byte response acts
/// as a separator. Without it a failed read would silently shift every
/// following reading onto the wrong sensor: `ipmitool exec` keeps going after a
/// failure but prints nothing for it.
///
/// `expected` is the number of reads in `script`; the result is in that order,
/// so the caller can zip it back onto its own labels.
pub fn read_sensors(
    ipmitool: &Path,
    script: &str,
    expected: usize,
    timeout: Duration,
) -> Result<SensorBatch, Error> {
    if expected == 0 {
        return Ok(SensorBatch {
            values: Vec::new(),
            failure: None,
        });
    }

    let mut command = Command::new(ipmitool);
    command.args(["exec", SCRIPT_PATH]);

    let output = run_capturing_with_stdin(command, script.as_bytes(), timeout).map_err(|err| {
        if err.is_missing_program() {
            // A missing dependency is not a transient hardware fault.
            Error::IpmitoolMissing {
                path: ipmitool.to_path_buf(),
            }
        } else {
            Error::Command(err)
        }
    })?;

    let text = String::from_utf8_lossy(&output.stdout);
    let values = parse_batch(&text, expected);

    let failure = if output.status.success() {
        None
    } else {
        Some(first_line(&output.stderr))
    };

    Ok(SensorBatch { values, failure })
}

/// Splits a batched `raw 0x04 0x2d` output into one value per target.
///
/// A separator starts a slot; the first non-separator line after it belongs to
/// that slot. Two separators in a row therefore mean "that read produced
/// nothing", not "the following readings moved up one".
pub(crate) fn parse_batch(text: &str, expected: usize) -> Vec<Option<f64>> {
    let mut values = vec![None; expected];
    let mut filled = vec![false; expected];
    let mut slot: Option<usize> = None;

    for line in text.lines() {
        // Walked as an iterator rather than collected into a Vec: this runs
        // twice per sensor, every cycle, and the token count is all that is
        // needed to tell a separator from a reading.
        let mut tokens = line.split_whitespace();
        let Some(first) = tokens.next() else {
            continue;
        };
        let second = tokens.next();
        let length = if second.is_some() {
            2 + tokens.count()
        } else {
            1
        };

        if length == SEPARATOR_BYTES {
            slot = Some(match slot {
                Some(index) => index + 1,
                None => 0,
            });
            continue;
        }

        // Anything before the first separator is not a reading we asked for.
        if let Some(index) = slot {
            if index < expected && !filled[index] {
                if let Some(status) = second {
                    values[index] = parse_sensor_reading(first, status);
                }
                filled[index] = true;
            }
        }
    }

    values
}

/// Decodes a Get Sensor Reading response into degrees Celsius.
///
/// The first byte is the raw reading and the second carries the sensor state;
/// bit 5 of that byte means the BMC has no value for this sensor. Nothing here
/// applies the M/B linearisation coefficients from the SDR record: on the TYAN
/// S8030 every temperature sensor happens to be 1:1, and `check` verifies that
/// by comparing the raw value against `ipmitool sdr get` before anything is
/// trusted.
fn parse_sensor_reading(reading: &str, status: &str) -> Option<f64> {
    let reading = u8::from_str_radix(reading, 16).ok()?;
    let status = u8::from_str_radix(status, 16).ok()?;

    if status & READING_UNAVAILABLE != 0 {
        return None;
    }

    Some(f64::from(reading))
}

/// Best available one-line reason for a non-zero exit.
fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("ipmitool exec reported a failure")
        .to_string()
}

/// What `ipmitool sdr get <name>` reports about one sensor.
#[derive(Debug, Default)]
pub struct SensorDetail {
    /// `false` when no sensor with that name exists. `ipmitool` still exits 0
    /// in that case, so the text is the only evidence.
    pub found: bool,
    /// Sensor number the BMC associates with that name.
    ///
    /// Compared against `platform.toml` at startup: a wrong number does not
    /// fail at run time, it quietly reads whichever sensor that number is.
    pub number: Option<u8>,
    /// Temperature, `None` when the BMC reports no reading.
    pub celsius: Option<f64>,
    /// Upper critical threshold, when the sensor defines one.
    pub upper_critical: Option<f64>,
}

/// Reads one sensor by name, including its threshold.
///
/// Only `check` uses this: it costs about 160 ms per sensor, because
/// `ipmitool` has to search the SDR record for the name. The control loop
/// reads by number instead.
pub fn read_sensor_detail(
    ipmitool: &Path,
    sdr_name: &str,
    timeout: Duration,
) -> Result<SensorDetail, Error> {
    let mut command = Command::new(ipmitool);
    command.args(["sdr", "get", sdr_name]);

    let output = run_checked(command, timeout).map_err(|err| {
        if err.is_missing_program() {
            Error::IpmitoolMissing {
                path: ipmitool.to_path_buf(),
            }
        } else {
            Error::Command(err)
        }
    })?;

    Ok(parse_sensor_detail(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// Parses the `key : value` layout of `ipmitool sdr get`.
fn parse_sensor_detail(text: &str) -> SensorDetail {
    let mut detail = SensorDetail::default();

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            // "Unable to find sensor id 'x'" has no colon and leaves `found`
            // false, which is exactly what the caller needs.
            continue;
        };
        let value = value.trim();

        match key.trim() {
            // "CPU_Tctl_Value (0x1)"
            "Sensor ID" => {
                detail.found = true;
                detail.number = value
                    .rsplit_once('(')
                    .and_then(|(_, tail)| tail.strip_suffix(')'))
                    .and_then(|hex| {
                        u8::from_str_radix(hex.trim().trim_start_matches("0x"), 16).ok()
                    });
            }
            "Sensor Reading" => {
                detail.celsius = if value.starts_with("No Reading") {
                    None
                } else {
                    value.split_whitespace().next().and_then(|t| t.parse().ok())
                };
            }
            // "na" when the sensor has no upper critical threshold.
            "Upper critical" => detail.upper_critical = value.parse().ok(),
            _ => {}
        }
    }

    detail
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::{parse_batch, parse_celsius, parse_sdr, parse_sensor_detail, read_sdr};
    use crate::error::Error;

    /// A real Get Device ID response: 15 bytes, the same every time.
    const SEP: &str = " 20 81 06 02 02 bf fd 19 00 5e 1f 00 00 00 00";

    #[test]
    fn batch_splits_readings_on_separators() {
        let text = format!("{SEP}\n 14 c0 c0 00\n{SEP}\n 1b c0 c0 00\n");
        assert_eq!(parse_batch(&text, 2), vec![Some(20.0), Some(27.0)]);
    }

    #[test]
    fn a_missing_read_does_not_shift_the_following_ones() {
        // The middle command failed, so nothing was printed for it. Two
        // separators in a row must leave a hole rather than renumber the rest.
        let text = format!("{SEP}\n 14 c0 c0 00\n{SEP}\n{SEP}\n 1a c0 c0 00\n");
        assert_eq!(parse_batch(&text, 3), vec![Some(20.0), None, Some(26.0)]);
    }

    #[test]
    fn the_unavailable_bit_is_not_zero_degrees() {
        // A dead sensor answers `00 e0 c0 00`: reading byte 0, but bit 5 of the
        // status byte says there is no value. Treating it as 0 C would drop the
        // zone to standby while claiming the machine is cold.
        let text = format!("{SEP}\n 00 e0 c0 00\n");
        assert_eq!(parse_batch(&text, 1), vec![None]);
    }

    #[test]
    fn lines_before_the_first_separator_are_ignored() {
        let text = format!("Some warning on stdout\n{SEP}\n 15 c0 c0 00\n");
        assert_eq!(parse_batch(&text, 1), vec![Some(21.0)]);
    }

    #[test]
    fn extra_readings_do_not_overflow_the_slot_list() {
        // More separators than requested should be harmless, not a panic.
        let text = format!("{SEP}\n 14 c0 c0 00\n{SEP}\n 15 c0 c0 00\n");
        assert_eq!(parse_batch(&text, 1), vec![Some(20.0)]);
    }

    #[test]
    fn detail_extracts_reading_and_threshold() {
        // Shape of a real `ipmitool sdr get` response.
        let text = "\
Sensor ID              : CPU_Tctl_Value (0x1)
 Entity ID              : 3.1 (Processor)
 Sensor Reading        : 21 (+/- 0) degrees C
 Status                 : ok
 Upper critical        : 95.000
";
        let detail = parse_sensor_detail(text);
        assert!(detail.found);
        assert_eq!(detail.number, Some(1));
        assert_eq!(detail.celsius, Some(21.0));
        assert_eq!(detail.upper_critical, Some(95.0));
    }

    #[test]
    fn detail_reports_a_dead_sensor_as_missing_a_reading() {
        let text = "\
Sensor ID              : SYS_Air_Inlet (0x30)
 Sensor Reading        : No Reading
 Status                 : Not Available
 Upper critical        : 40.000
";
        let detail = parse_sensor_detail(text);
        assert!(detail.found);
        assert_eq!(detail.number, Some(0x30));
        assert_eq!(detail.celsius, None);
        assert_eq!(detail.upper_critical, Some(40.0));
    }

    #[test]
    fn detail_treats_an_unknown_name_as_not_found() {
        // The exit status is 0 here, so this text is the only signal.
        let detail = parse_sensor_detail("Unable to find sensor id 'NoSuchSensor_XYZ'\n");
        assert!(!detail.found);
        assert_eq!(detail.number, None);
        assert_eq!(detail.celsius, None);
    }

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
