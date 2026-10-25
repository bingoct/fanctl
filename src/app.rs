//! Subcommand orchestration: `check` and `run`, plus signal handling.

pub mod cli;

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use signal_hook::consts::{SIGINT, SIGTERM};
use tracing::{error, info, warn};

use self::cli::{Cli, Command};
use crate::config::{LoadedConfig, PlatformConfig, SensorSpec};
use crate::control::{standby_targets, Engine};
use crate::error::Error;
use crate::sensors::{gpu, ipmi, Source};
use crate::util::name::sdr_key;

/// Tolerance when comparing the raw byte against `ipmitool sdr`, in Celsius.
///
/// The two readings are seconds apart (`ipmitool sdr` is a 2.9 s scan), so a
/// machine under light load can move a degree. A board whose M/B coefficients
/// are not 1:1 is off by tens of degrees, not by two.
const RAW_READ_TOLERANCE_CELSIUS: f64 = 2.0;

/// How far a tachometer reading may move between the two `check` samples.
///
/// Not the same problem as the temperature tolerance: a fan's speed is quantised
/// to one count of `rpm_per_count` (138 RPM here), so a reading that flickers by
/// one count is normal - and the two samples are seconds apart, with the SDR scan
/// in between. Measured on the S8030's coolant pump at 100 %, one sample apart:
/// 43, 44 and 45 counts, all three of them, so a single count of slack rejects
/// good readings two times in five.
///
/// Three counts still catches every way of getting the coefficient wrong: a
/// missing factor of 138 is off by two orders of magnitude, a factor of two by
/// 43 counts on this pump. The check exists to catch a scale factor that prints
/// a confident RPM for a fan nobody is measuring, not to police a rotor.
const TACHOMETER_TOLERANCE_COUNTS: f64 = 3.0;

/// Entry point used by `main`.
///
/// Failures are reported where they happen: `check` prints one line per
/// problem and never returns an `Err`, so the user does not see the same
/// message twice (once as `[fail]`, once as a log record).
pub fn run(cli: Cli) -> ExitCode {
    match &cli.command {
        Command::Check => check(&cli),
        Command::Run { dry_run } => match run_daemon(&cli, *dry_run) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                error!("{err}");
                ExitCode::FAILURE
            }
        },
    }
}

/// `true` when a raw tachometer count and an SDR reading describe the same fan.
///
/// The SDR reading has the coefficient applied, so dividing it back out has to
/// land on the raw count - up to the noise between two samples, which is one
/// count of quantisation either way.
fn tachometer_agrees(count: f64, rpm: f64, scale: f64) -> bool {
    (rpm / scale - count).abs() <= TACHOMETER_TOLERANCE_COUNTS
}

/// Validates the configuration and probes the running hardware.
///
/// Exit code is non-zero when a configured sensor cannot be read, because in
/// that state the daemon would constantly fall back to `fallback_duty_cycle`. A
/// tachometer whose two readings disagree is also fatal: it means
/// `rpm_per_count` is wrong, and every speed in the log would be wrong with it.
fn check(cli: &Cli) -> ExitCode {
    println!("fanctl {} - configuration check", crate::VERSION);
    println!("  policy   : {}", cli.config.display());
    println!("  platform : {}", cli.platform.display());
    println!();

    let loaded = match LoadedConfig::load(&cli.config, &cli.platform) {
        Ok(loaded) => loaded,
        Err(err) => {
            println!("[fail] {err}");
            return ExitCode::FAILURE;
        }
    };

    let platform = &loaded.platform;
    let global = &loaded.policy.global;
    println!(
        "[ ok ] board `{}`, ipmitool `{}`",
        platform.info.name,
        platform.info.ipmitool_path.display()
    );
    println!(
        "[ ok ] {} pin(s), {} sensor(s), {} tachometer(s), {} zone(s), {} follow rule(s)",
        platform.pins.len(),
        platform.sensors.len(),
        platform.tachometers.len(),
        loaded.policy.zones.len(),
        loaded.policy.follows.len()
    );
    println!(
        "[ ok ] interval {}s, duty_cycle {}..={}, fallback {}, standby {}",
        global.interval_seconds,
        global.min_duty_cycle,
        global.max_duty_cycle,
        global.fallback_duty_cycle,
        global.default_duty_cycle
    );

    for zone in &loaded.policy.zones {
        println!(
            "[ ok ] zone `{}`: [{}] {} -> [{}], standby {}%, fallback {}% after {} cycle(s), \
             debounce {} C, {} threshold(s)",
            zone.name,
            zone.sources
                .iter()
                .map(Source::label)
                .collect::<Vec<_>>()
                .join(", "),
            zone.aggregation.as_str(),
            zone.target_pins.join(", "),
            zone.default_duty_cycle,
            zone.fallback_duty_cycle,
            zone.fallback_after_cycles,
            zone.debounce_celsius,
            zone.thresholds.len()
        );
    }

    let standby = standby_targets(
        &loaded.policy.zones,
        global.min_duty_cycle,
        global.max_duty_cycle,
    );
    let plan = standby
        .iter()
        .map(|(pin, duty_cycle)| format!("{pin} {duty_cycle}%"))
        .collect::<Vec<_>>()
        .join(", ");
    println!("[ ok ] shutdown restores: {plan}");

    let timeout = Duration::from_secs(global.command_timeout_seconds.max(1));
    let entries = match ipmi::read_sdr(&platform.info.ipmitool_path, timeout) {
        Ok(entries) => entries,
        Err(err) => {
            println!("[fail] {err}");
            return ExitCode::FAILURE;
        }
    };
    println!("[ ok ] `ipmitool sdr` returned {} line(s)", entries.len());

    let index: HashMap<String, Option<f64>> = entries
        .iter()
        .map(|entry| (sdr_key(&entry.name), entry.celsius))
        .collect();

    // Read the same sensors by number, in one process: this is the path the
    // control loop uses. It proves two things at once - that the numbers in
    // `platform.toml` address the sensors named next to them, and that the raw
    // byte really is degrees Celsius on this board (the M/B coefficients from
    // the SDR record are not applied anywhere).
    let numbers: Vec<u8> = platform.sensors.values().map(|spec| spec.id).collect();
    let script = ipmi::build_script(&numbers);
    let raw_values: Vec<Option<f64>> = match ipmi::read_sensors(
        &platform.info.ipmitool_path,
        &script,
        numbers.len(),
        timeout,
    ) {
        Ok(batch) => {
            if let Some(details) = &batch.failure {
                println!("[warn] batched raw read: {details}");
            }
            batch.values
        }
        Err(err) => {
            println!("[fail] batched raw read failed: {err}");
            vec![None; numbers.len()]
        }
    };

    let mut missing = 0_usize;
    let mut raw_readings = raw_values.into_iter();
    for (semantic, spec) in &platform.sensors {
        let raw_value = raw_readings.next().flatten();

        let listed = match index.get(&sdr_key(&spec.sdr)) {
            None => {
                missing += 1;
                println!(
                    "[fail] sensor `{semantic}` <- `{}` (0x{:02x}) is absent from `ipmitool sdr`",
                    spec.sdr, spec.id
                );
                continue;
            }
            Some(None) => {
                missing += 1;
                println!(
                    "[fail] sensor `{semantic}` <- `{}` (0x{:02x}) reports no temperature",
                    spec.sdr, spec.id
                );
                continue;
            }
            Some(Some(celsius)) => *celsius,
        };

        let detail =
            match ipmi::read_sensor_detail(&platform.info.ipmitool_path, &spec.sdr, timeout) {
                Ok(detail) => detail,
                Err(err) => {
                    println!("[warn] sensor `{semantic}`: {err}");
                    continue;
                }
            };
        if !detail.found {
            missing += 1;
            println!(
                "[fail] sensor `{semantic}` <- `{}`: the BMC has no sensor with that name",
                spec.sdr
            );
            continue;
        }
        // The number is what the control loop actually reads. A wrong one does
        // not fail there, it samples a different sensor - so it is fatal here.
        if let Some(reported) = detail.number {
            if reported != spec.id {
                missing += 1;
                println!(
                    "[fail] sensor `{semantic}`: platform.toml says 0x{:02x}, but the BMC says \
                     `{}` is sensor 0x{:02x} - the raw read would sample a different sensor",
                    spec.id, spec.sdr, reported
                );
                continue;
            }
        }

        let threshold = detail
            .upper_critical
            .map(|limit| format!(", upper critical {limit} C"))
            .unwrap_or_default();

        match raw_value {
            Some(raw) if (raw - listed).abs() <= RAW_READ_TOLERANCE_CELSIUS => println!(
                "[ ok ] sensor `{semantic}` <- `{}` (0x{:02x}) = {listed} C, raw {raw} C{threshold}",
                spec.sdr, spec.id
            ),
            Some(raw) => {
                missing += 1;
                println!(
                    "[fail] sensor `{semantic}` (0x{:02x}): raw reads {raw} C but `sdr` says \
                     {listed} C - the raw byte is not 1:1 on this board, so fanctl cannot use it \
                     without applying the SDR coefficients{threshold}",
                    spec.id
                );
            }
            None => {
                missing += 1;
                println!(
                    "[fail] sensor `{semantic}` (0x{:02x}): `sdr` has {listed} C but the raw read \
                     returned nothing{threshold}",
                    spec.id
                );
            }
        }
    }

    // Tachometers are reported, never used to decide anything - but a wrong
    // number or a wrong scale factor would print a confident RPM figure for a
    // fan nobody is measuring, which is worse than printing nothing.
    if !platform.tachometers.is_empty() {
        let scale = platform.info.rpm_per_count.unwrap_or(1.0);
        let numbers: Vec<u8> = platform.tachometers.values().map(|spec| spec.id).collect();
        let script = ipmi::build_script(&numbers);
        let counts = match ipmi::read_sensors(
            &platform.info.ipmitool_path,
            &script,
            numbers.len(),
            timeout,
        ) {
            Ok(batch) => {
                if let Some(details) = &batch.failure {
                    println!("[warn] batched tachometer read: {details}");
                }
                batch.values
            }
            Err(err) => {
                println!("[fail] batched tachometer read failed: {err}");
                vec![None; numbers.len()]
            }
        };

        let mut counts = counts.into_iter();
        for (pin, spec) in &platform.tachometers {
            let count = counts.next().flatten();
            let detail =
                match ipmi::read_sensor_detail(&platform.info.ipmitool_path, &spec.sdr, timeout) {
                    Ok(detail) if detail.found => detail,
                    Ok(_) => {
                        missing += 1;
                        println!(
                            "[fail] tachometer `{pin}` <- `{}`: the BMC has no sensor with that \
                             name",
                            spec.sdr
                        );
                        continue;
                    }
                    Err(err) => {
                        println!("[warn] tachometer `{pin}`: {err}");
                        continue;
                    }
                };
            if let Some(reported) = detail.number {
                if reported != spec.id {
                    missing += 1;
                    println!(
                        "[fail] tachometer `{pin}`: platform.toml says 0x{:02x}, but `{}` is \
                         sensor 0x{:02x} - the reported speed would be another fan's",
                        spec.id, spec.sdr, reported
                    );
                    continue;
                }
            }

            // `sdr` prints the fan's own unit, so this is RPM; the raw read
            // returns the count that the SDR coefficients would have scaled.
            match (count, detail.celsius) {
                (Some(count), Some(rpm)) => {
                    if tachometer_agrees(count, rpm, scale) {
                        println!(
                            "[ ok ] tachometer `{pin}` <- `{}` (0x{:02x}) = {rpm} RPM, raw \
                             {count} x {scale}",
                            spec.sdr, spec.id
                        );
                    } else {
                        missing += 1;
                        println!(
                            "[fail] tachometer `{pin}` (0x{:02x}): raw {count} x rpm_per_count \
                             {scale} = {} RPM, but `sdr` says {rpm} RPM - [platform].rpm_per_count \
                             does not match this board",
                            spec.id,
                            (count * scale).round()
                        );
                    }
                }
                (None, _) => {
                    missing += 1;
                    println!(
                        "[fail] tachometer `{pin}` (0x{:02x}): the raw read returned nothing",
                        spec.id
                    );
                }
                (_, None) => {
                    missing += 1;
                    println!(
                        "[fail] tachometer `{pin}` (0x{:02x}): the BMC reports no reading",
                        spec.id
                    );
                }
            }
        }
    }

    let driven: HashSet<&str> = loaded
        .policy
        .zones
        .iter()
        .flat_map(|zone| zone.target_pins.iter().map(String::as_str))
        .collect();
    for (pin, id) in &platform.pins {
        match find_fan_sensor(&entries, pin) {
            Some(name) => println!("[ ok ] pin `{pin}` ({id}) <- fan sensor `{name}`"),
            None => println!(
                "[warn] pin `{pin}` ({id}): no fan sensor with a matching name; the id was only \
                 syntax-checked, not verified on hardware"
            ),
        }
        if !driven.contains(pin.as_str()) {
            println!(
                "[warn] pin `{pin}` ({id}) is not driven by any zone: never set, never restored"
            );
        }
    }

    let mut monitor = gpu::GpuMonitor::new();
    match monitor.read() {
        Ok(readings) => {
            // The index is printed because `monitor_sources` addresses cards by
            // it (`GPU0`, `GPU1`, ...); the model name alone cannot tell two
            // identical cards apart.
            for (index, name, celsius) in readings {
                println!("[ ok ] GPU{index} `{name}` = {celsius} C");
            }
        }
        Err(err) => println!("[warn] GPU: {err}"),
    }

    println!();
    if missing > 0 {
        // Deliberately not "[sensors]": a tachometer and a scale factor are
        // also counted here, and the message used to send the reader to the
        // wrong table.
        println!("[fail] {missing} configured reading(s) could not be verified - see above");
        return ExitCode::FAILURE;
    }

    println!("[ ok ] configuration check passed");
    ExitCode::SUCCESS
}

/// Validates, installs signal handlers, and hands control to the engine.
fn run_daemon(cli: &Cli, dry_run: bool) -> Result<(), Error> {
    let loaded = LoadedConfig::load(&cli.config, &cli.platform)?;

    // Catches a wrong sensor number before the loop starts. It is checked once
    // here rather than every cycle: `ipmitool sdr get` costs 160 ms per sensor,
    // which is exactly the cost the raw read exists to avoid.
    let problems = verify_sensor_numbers(&loaded);
    if !problems.is_empty() {
        let details = problems
            .iter()
            .map(|problem| format!("  - {problem}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(Error::Validation(format!(
            "{} sensor problem(s) found:\n{details}",
            problems.len()
        )));
    }

    // A wrong GPU index is not fatal - the driver may come up after `fanctl`,
    // and a zone without a reading holds its previous duty cycle - but it must
    // not be silent: without this line the zone simply never reacts.
    warn_unknown_gpu_indices(&loaded);

    info!(
        board = %loaded.platform.info.name,
        zones = loaded.policy.zones.len(),
        dry_run,
        "configuration loaded"
    );
    if dry_run {
        info!("dry-run: temperatures are read, but no PWM command will be sent");
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [SIGTERM, SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))
            .map_err(|err| Error::Signal(err.to_string()))?;
    }

    Engine::new(loaded, dry_run, shutdown).run()
}

/// Warns when `monitor_sources` names a GPU index NVML does not report.
///
/// The index is a runtime fact: a card can be pulled, and two identical cards
/// are indistinguishable by name, so this cannot be validated against the TOML
/// files. It is a warning rather than an error because NVML may legitimately be
/// unavailable at startup (driver loaded later, `gpu` feature disabled).
fn warn_unknown_gpu_indices(loaded: &LoadedConfig) {
    let wanted: HashSet<u32> = loaded
        .policy
        .zones
        .iter()
        .flat_map(|zone| zone.sources.iter())
        .filter_map(|source| match source {
            Source::Gpu(Some(index)) => Some(*index),
            _ => None,
        })
        .collect();
    if wanted.is_empty() {
        return;
    }

    let mut monitor = gpu::GpuMonitor::new();
    match monitor.read() {
        Ok(devices) => {
            for index in wanted {
                if !devices.iter().any(|(seen, _, _)| *seen == index) {
                    warn!(
                        gpu = index,
                        present = devices.len(),
                        "monitor_sources references a GPU index NVML did not report; that zone \
                         will keep running without a reading"
                    );
                }
            }
        }
        Err(err) => warn!(error = %err, "cannot verify GPU indices at startup"),
    }
}

/// Confirms every configured sensor number belongs to the name written next to
/// it.
///
/// A run-time read by number cannot tell a typo from a working sensor: on the
/// TYAN S8030, `raw 0x04 0x2d 0x99` answers 172 C because 0x99 *is* a sensor -
/// just not `CPU_Tctl_Value`. The loop would then control the fans from the
/// wrong temperature, which is worse than having no reading at all.
///
/// A BMC that does not answer in time is deliberately not a failure: the loop
/// is built to survive that, and refusing to start would turn a slow BMC into a
/// restart loop.
fn verify_sensor_numbers(loaded: &LoadedConfig) -> Vec<String> {
    let platform = &loaded.platform;
    let timeout = Duration::from_secs(loaded.policy.global.command_timeout_seconds.max(1));
    let mut problems = Vec::new();

    verify_numbers(
        "[sensors]",
        platform,
        platform.sensors.iter(),
        timeout,
        &mut problems,
    );
    // A tachometer never makes a decision, but a wrong number quietly reports
    // another fan's speed next to the duty cycle this one was given.
    verify_numbers(
        "[tachometers]",
        platform,
        platform.tachometers.iter(),
        timeout,
        &mut problems,
    );

    problems
}

/// Checks every `sdr` name against the number `platform.toml` claims for it.
///
/// A wrong number does not fail anywhere at run time: it reads whichever sensor
/// that number is, and the daemon trusts the reading.
fn verify_numbers<'a>(
    section: &str,
    platform: &PlatformConfig,
    specs: impl Iterator<Item = (&'a String, &'a SensorSpec)>,
    timeout: Duration,
    problems: &mut Vec<String>,
) {
    for (name, spec) in specs {
        match ipmi::read_sensor_detail(&platform.info.ipmitool_path, &spec.sdr, timeout) {
            Ok(detail) if !detail.found => problems.push(format!(
                "{section}.{name}: the BMC has no sensor called `{}`",
                spec.sdr
            )),
            Ok(detail) => {
                if let Some(reported) = detail.number {
                    if reported != spec.id {
                        problems.push(format!(
                            "{section}.{name}: platform.toml says 0x{:02x}, but `{}` is \
                             sensor 0x{:02x} - the daemon would read a different sensor",
                            spec.id, spec.sdr, reported
                        ));
                    }
                }
            }
            Err(err) => warn!(
                sensor = %name,
                error = %err,
                "cannot verify the sensor number right now, continuing without that check"
            ),
        }
    }
}

/// Best effort: finds a fan sensor whose name corresponds to the pin name.
///
/// A pin id can only be verified for real by writing to it, which `check` must
/// not do, so this only warns.
///
/// Matching is strict on purpose. An exact name wins; a containment hit only
/// counts when the character right after the match is not a digit, because
/// otherwise the pin `SYS_FAN_1` "verifies" itself against the SDR entry
/// `SYS_FAN_10` - a false `[ok]` in exactly the check that exists to catch a
/// bad pin mapping. Ties resolve to the lexicographically first name so the
/// report never depends on hash iteration order.
fn find_fan_sensor<'a>(entries: &'a [ipmi::SdrEntry], pin: &str) -> Option<&'a str> {
    let wanted = sdr_key(pin);

    if let Some(entry) = entries.iter().find(|entry| sdr_key(&entry.name) == wanted) {
        return Some(&entry.name);
    }

    // Other boards decorate the header number (`FAN_CPU1`): accept a
    // containment hit as long as it does not stop in the middle of a number.
    entries
        .iter()
        .filter(|entry| {
            let key = sdr_key(&entry.name);
            key.match_indices(wanted.as_str())
                .any(|(start, _)| !ends_mid_number(&key, start + wanted.len()))
        })
        .map(|entry| entry.name.as_str())
        .min()
}

/// `true` when `name` continues with a digit at `offset`.
///
/// `offset` always sits on a character boundary: it is the end of a match.
fn ends_mid_number(name: &str, offset: usize) -> bool {
    name[offset..].starts_with(|c: char| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{ends_mid_number, find_fan_sensor, tachometer_agrees};
    use crate::sensors::ipmi::SdrEntry;

    /// One tachometer count on the board this was written for.
    const SCALE: f64 = 138.0;

    #[test]
    fn a_pump_reading_that_wanders_still_agrees() {
        // Measured on the S8030's coolant pump at 100 %: three consecutive
        // samples read 43, 44 and 45 counts. A single count of slack rejected
        // two runs in five, on the command that gates a deployment.
        assert!(tachometer_agrees(45.0, 5934.0, SCALE)); // 43 counts
        assert!(tachometer_agrees(43.0, 6210.0, SCALE)); // 45 counts
        assert!(tachometer_agrees(43.0, 6348.0, SCALE)); // 46 counts, the edge
    }

    #[test]
    fn a_coefficient_that_is_wrong_by_a_factor_is_rejected() {
        // The failure this check exists for: a plausible RPM for a fan nobody
        // is measuring. Neither a missing factor of 138 nor a doubled one can
        // hide inside the tolerance.
        assert!(!tachometer_agrees(45.0, 5934.0, 1.0));
        assert!(!tachometer_agrees(45.0, 5934.0, 276.0));
    }

    fn entries(names: &[&str]) -> Vec<SdrEntry> {
        names
            .iter()
            .map(|name| SdrEntry {
                name: (*name).to_string(),
                celsius: None,
            })
            .collect()
    }

    #[test]
    fn exact_pin_name_wins_over_a_longer_one() {
        // The real reason this check exists: the TYAN S8030 exposes
        // SYS_FAN_1 together with SYS_FAN_10..12 and only the first is wired.
        let all = entries(&["SYS_FAN_10", "SYS_FAN_1", "SYS_FAN_11", "SYS_FAN_12"]);
        assert_eq!(find_fan_sensor(&all, "SYS_FAN_1"), Some("SYS_FAN_1"));
    }

    #[test]
    fn a_missing_pin_never_borrows_a_neighbours_name() {
        let all = entries(&["SYS_FAN_10", "SYS_FAN_11"]);
        assert_eq!(find_fan_sensor(&all, "SYS_FAN_1"), None);
    }

    #[test]
    fn decorated_names_still_match() {
        let all = entries(&["FAN_CPU1", "CPU_FAN_SPEED"]);
        assert_eq!(find_fan_sensor(&all, "CPU_FAN"), Some("CPU_FAN_SPEED"));
    }

    #[test]
    fn separators_and_case_are_ignored() {
        let all = entries(&["cpu fan"]);
        assert_eq!(find_fan_sensor(&all, "CPU_FAN"), Some("cpu fan"));
    }

    #[test]
    fn a_digit_after_the_match_means_a_different_fan() {
        assert!(ends_mid_number("sysfan10", "sysfan1".len()));
        assert!(!ends_mid_number("sysfan1", "sysfan1".len()));
        assert!(!ends_mid_number("sysfan1rpm", "sysfan1".len()));
    }
}
