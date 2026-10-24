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
use crate::config::LoadedConfig;
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

/// Validates the configuration and probes the running hardware.
///
/// Exit code is non-zero when a configured sensor cannot be read, because in
/// that state the daemon would constantly fall back to `fallback_duty_cycle`.
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
        "[ ok ] {} pin(s), {} sensor(s), {} zone(s), {} follow rule(s)",
        platform.pins.len(),
        platform.sensors.len(),
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
        println!(
            "[fail] {missing} configured sensor(s) cannot be read - fix platform.toml [sensors]"
        );
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

    for (semantic, spec) in &platform.sensors {
        match ipmi::read_sensor_detail(&platform.info.ipmitool_path, &spec.sdr, timeout) {
            Ok(detail) if !detail.found => problems.push(format!(
                "[sensors].{semantic}: the BMC has no sensor called `{}`",
                spec.sdr
            )),
            Ok(detail) => {
                if let Some(reported) = detail.number {
                    if reported != spec.id {
                        problems.push(format!(
                            "[sensors].{semantic}: platform.toml says 0x{:02x}, but `{}` is \
                             sensor 0x{:02x} - the daemon would read a different sensor",
                            spec.id, spec.sdr, reported
                        ));
                    }
                }
            }
            Err(err) => warn!(
                sensor = %semantic,
                error = %err,
                "cannot verify the sensor number right now, continuing without that check"
            ),
        }
    }

    problems
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
    use super::{ends_mid_number, find_fan_sensor};
    use crate::sensors::ipmi::SdrEntry;

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
