//! Subcommand orchestration: `check` and `run`, plus signal handling.

pub mod cli;

use std::collections::{HashMap, HashSet};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use signal_hook::consts::{SIGINT, SIGTERM};
use tracing::{error, info};

use self::cli::{Cli, Command};
use crate::config::LoadedConfig;
use crate::control::{standby_targets, Engine};
use crate::error::Error;
use crate::sensors::{gpu, ipmi};
use crate::util::name::sdr_key;

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
            "[ ok ] zone `{}`: [{}] {} -> [{}], standby {}%, debounce {} C, {} threshold(s)",
            zone.name,
            zone.monitor_sources.join(", "),
            zone.aggregation.as_str(),
            zone.target_pins.join(", "),
            zone.default_duty_cycle,
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

    let mut missing = 0_usize;
    for (semantic, sdr_name) in &platform.sensors {
        match index.get(&sdr_key(sdr_name)) {
            Some(Some(celsius)) => {
                println!("[ ok ] sensor `{semantic}` <- `{sdr_name}` = {celsius} C");
            }
            Some(None) => {
                missing += 1;
                println!("[fail] sensor `{semantic}` <- `{sdr_name}` reports no temperature");
            }
            None => {
                missing += 1;
                println!("[fail] sensor `{semantic}` <- `{sdr_name}` absent from `ipmitool sdr`");
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
        match find_fan_sensor(&index, pin) {
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
            for (name, celsius) in readings {
                println!("[ ok ] GPU `{name}` = {celsius} C");
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

/// Best effort: finds a fan sensor whose name contains the pin name.
///
/// A pin id can only be verified for real by writing to it, which `check` must
/// not do, so this only warns.
fn find_fan_sensor(index: &HashMap<String, Option<f64>>, pin: &str) -> Option<String> {
    let wanted = sdr_key(pin);
    index
        .keys()
        .find(|name| name.as_str().contains(wanted.as_str()))
        .cloned()
}
