//! The evaluation loop.
//!
//! One cycle is: read every sensor, evaluate every zone, merge the demands,
//! send only the pins whose duty cycle actually changed.
//!
//! Failure policy (deliberately conservative):
//!
//! | situation | reaction |
//! |-----------|----------|
//! | one zone has no reading | hold that zone's last duty cycle for `fallback_after_cycles` cycles |
//! | the grace period ran out | that zone's `fallback_duty_cycle` |
//! | a zone never had a reading | that zone's `fallback_duty_cycle` |
//! | *no* sensor at all | every zone's `fallback_duty_cycle`, skipping the grace period |
//! | `ipmitool` write fails | log, retry next cycle, never exit |
//! | SIGINT / SIGTERM | each controlled pin -> its zone's `default_duty_cycle`, then exit 0 |
//!
//! Only pins a zone drives are ever written. `platform.toml` describes the whole
//! board, and the spare headers in it are not the daemon's to touch.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, info, trace, warn};

use crate::actuator::PwmActuator;
use crate::config::{FanConfig, LoadedConfig, PlatformConfig, ZoneConfig};
use crate::error::Error;
use crate::sensors::{SensorHub, SensorSnapshot, Source};
use crate::util::name::GPU_SOURCE;

use super::curve::{duty_for_band, next_band, ZoneState};
use super::merge::{driven_pins, fallback_targets, standby_targets, TargetMap};

/// How often the shutdown flag is checked while sleeping.
const SHUTDOWN_POLL: Duration = Duration::from_millis(200);

/// How often an unchanged cycle line is repeated at INFO.
///
/// It is the only routine evidence that the daemon is running: without it a
/// healthy idle machine and a dead one both produce silence.
const HEARTBEAT: Duration = Duration::from_secs(300);

/// Source names of a zone, for the log lines that explain a missing reading.
fn source_labels(zone: &ZoneConfig) -> Vec<String> {
    zone.sources.iter().map(Source::label).collect()
}

/// Every temperature a snapshot holds, in a stable order.
///
/// Sorted because the IPMI readings live in a `HashMap`: an unstable order
/// would make two identical cycles print differently, and a log whose lines
/// cannot be compared is not evidence of anything.
fn temperatures(snapshot: &SensorSnapshot) -> Vec<(String, f64)> {
    let mut temps: Vec<(String, f64)> = snapshot
        .ipmi
        .iter()
        .map(|(name, celsius)| (name.clone(), *celsius))
        .collect();
    temps.sort_by(|left, right| left.0.cmp(&right.0));
    temps.extend(
        snapshot
            .gpus
            .iter()
            .map(|(index, _, celsius)| (format!("{GPU_SOURCE}{index}"), *celsius)),
    );
    temps
}

/// Fans that turned last cycle and report nothing this cycle.
///
/// `0` is not a speed, it is the absence of one - a stopped rotor, a fan that
/// is no longer plugged in, or a header with no power. No fan on a running
/// machine reads zero at a non-zero duty cycle, which is why the caller also
/// checks that a duty cycle was actually written for the pin.
fn stalled<'a>(previous: Option<&[(String, u32)]>, current: &'a [(String, u32)]) -> Vec<&'a str> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    current
        .iter()
        .filter(|(pin, rpm)| {
            *rpm == 0
                && previous
                    .iter()
                    .any(|(was_pin, was)| was_pin == pin && *was > 0)
        })
        .map(|(pin, _)| pin.as_str())
        .collect()
}

/// The temperatures as one space-free field, for the duty cycle lines.
///
/// The cycle line carries the temperatures on its own once every
/// [`HEARTBEAT`]. This is the copy that travels with the event they caused, so
/// a duty cycle step can be read without hunting for the nearest `cycle temps`.
///
/// ```text
/// pin=SYS_FAN_2 from=30 to=35 temps=CPU_Tctl:26.0,GPU0:55.0,GPU1:26.0
/// ```
fn temperatures_field(snapshot: &SensorSnapshot) -> String {
    let temps = temperatures(snapshot);
    if temps.is_empty() {
        return String::from("none");
    }
    temps
        .iter()
        .map(|(name, celsius)| format!("{name}:{celsius:.1}"))
        .collect::<Vec<String>>()
        .join(",")
}

/// Builds the one-line summary of a cycle: every temperature, every measured
/// fan speed, then every duty cycle.
///
/// The format is meant to be read by a person *and* scraped for a temperature
/// curve, so it is `key=value` throughout and each third is introduced by a
/// bare word:
///
/// ```text
/// cycle temps CPU_Tctl=22.0 GPU0=78.0 | fan CPU_FAN=690 SYS_FAN_2=1104 | duty cpu=30 radiator=30
/// ```
///
/// Speeds sit next to the duty cycles on purpose: the only way to tell "the
/// write landed" from "the header is not driving anything" is to see both.
fn cycle_line<'a>(
    snapshot: &SensorSnapshot,
    duties: impl Iterator<Item = (&'a str, Option<u8>)>,
) -> String {
    let mut line = String::from("cycle temps");

    let temps = temperatures(snapshot);
    for (name, celsius) in &temps {
        line.push_str(&format!(" {name}={celsius:.1}"));
    }
    if temps.is_empty() {
        line.push_str(" none");
    }

    if !snapshot.fans.is_empty() {
        line.push_str(" | fan");
        for (pin, rpm) in &snapshot.fans {
            line.push_str(&format!(" {pin}={rpm}"));
        }
    }

    line.push_str(" | duty");
    for (name, duty_cycle) in duties {
        // A zone with no reading this cycle has no duty cycle to report. The
        // `?` keeps the field in place so the columns line up.
        match duty_cycle {
            Some(duty_cycle) => line.push_str(&format!(" {name}={duty_cycle}")),
            None => line.push_str(&format!(" {name}=?")),
        }
    }

    line
}

/// The daemon state machine.
pub struct Engine {
    platform: PlatformConfig,
    policy: FanConfig,
    hub: SensorHub,
    actuator: PwmActuator,
    zone_states: Vec<Option<ZoneState>>,
    zone_duties: Vec<Option<u8>>,
    /// Consecutive cycles each zone has gone without a reading.
    zone_misses: Vec<u32>,
    last_sent: BTreeMap<String, u8>,
    /// Fan speeds as of the last reported cycle, for stall detection, and when
    /// the cycle line was last emitted at INFO.
    last_fans: Option<Vec<(String, u32)>>,
    last_report_at: Option<Instant>,
    /// Pins at least one zone drives; the only ones the daemon may write.
    driven: Vec<String>,
    shutdown: Arc<AtomicBool>,
    timeout: Duration,
}

impl Engine {
    /// Builds the engine from validated configuration.
    pub fn new(loaded: LoadedConfig, dry_run: bool, shutdown: Arc<AtomicBool>) -> Self {
        let zone_count = loaded.policy.zones.len();
        let timeout = Duration::from_secs(loaded.policy.global.command_timeout_seconds.max(1));
        let all_sources = || {
            loaded
                .policy
                .zones
                .iter()
                .flat_map(|zone| zone.sources.iter())
        };
        let uses_gpu = all_sources().any(|source| matches!(source, Source::Gpu(_)));
        let hub = SensorHub::new(&loaded.platform, all_sources(), uses_gpu);
        let actuator = PwmActuator::new(&loaded.platform, dry_run, timeout);
        let driven = driven_pins(&loaded.policy.zones)
            .into_iter()
            .map(str::to_string)
            .collect();

        Self {
            platform: loaded.platform,
            policy: loaded.policy,
            hub,
            actuator,
            zone_states: vec![None; zone_count],
            zone_duties: vec![None; zone_count],
            zone_misses: vec![0; zone_count],
            last_sent: BTreeMap::new(),
            last_fans: None,
            last_report_at: None,
            driven,
            shutdown,
            timeout,
        }
    }

    /// Runs until the shutdown flag is raised.
    ///
    /// `interval_seconds` is the period between two cycles, not the time slept
    /// after one: `ipmitool sdr` alone takes seconds on a real BMC, so sleeping
    /// the full interval would turn a 10 s period into a 13 s one.
    pub fn run(mut self) -> Result<(), Error> {
        info!(
            board = %self.platform.info.name,
            zones = self.policy.zones.len(),
            pins = self.driven.len(),
            interval_seconds = self.policy.global.interval_seconds,
            dry_run = self.actuator.is_dry_run(),
            "control loop started"
        );

        let interval = Duration::from_secs(self.policy.global.interval_seconds.max(1));
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            let started = Instant::now();
            self.tick();
            let elapsed = started.elapsed();
            if elapsed > interval {
                warn!(
                    elapsed = ?elapsed,
                    interval_seconds = interval.as_secs(),
                    "cycle overran the interval: the BMC cannot be polled this fast"
                );
            }

            // Overrun means sleep zero, not sleep negative. The cycle itself
            // costs seconds, so this cannot become a busy loop.
            if self.sleep_interruptibly(interval.saturating_sub(elapsed)) {
                break;
            }
        }

        self.restore_standby();
        info!("control loop stopped");
        Ok(())
    }

    /// Restores the standby duty cycle of every controlled pin.
    ///
    /// The BMC keeps the last value it was given, so leaving the fans at
    /// whatever the last hot cycle demanded would leave the machine loud until
    /// the next boot. `default_duty_cycle` is the right value to fall back to:
    /// it is the same "room temperature, no load" setting the curve uses.
    ///
    /// Pins no zone drives are skipped - the daemon never wrote them, so it has
    /// nothing to restore and must not invent a value.
    fn restore_standby(&mut self) {
        let bounds = &self.policy.global;
        let standby = standby_targets(
            &self.policy.zones,
            bounds.min_duty_cycle,
            bounds.max_duty_cycle,
        );
        info!(
            pins = standby.len(),
            "shutdown requested, restoring standby duty cycle"
        );

        let pending: Vec<(&str, u8)> = standby
            .iter()
            .map(|(pin, duty_cycle)| (pin.as_str(), *duty_cycle))
            .collect();

        // One process for all pins: this runs while systemd is waiting for the
        // process to exit, so time spent here is time added to shutdown.
        match self.actuator.apply_batch(&pending) {
            Ok(unconfirmed) => {
                for (pin, duty_cycle) in standby.iter() {
                    if unconfirmed.iter().any(|failed| failed == pin) {
                        warn!(
                            pin = %pin,
                            duty_cycle,
                            "could not write standby duty cycle"
                        );
                        continue;
                    }
                    self.last_sent.insert(pin.clone(), *duty_cycle);
                    // The actuator already logged the command it would have
                    // run; saying "written" in a dry run would be a lie.
                    if !self.actuator.is_dry_run() {
                        info!(pin = %pin, duty_cycle, "standby duty cycle written");
                    }
                }
            }
            Err(err) => warn!(error = %err, "could not write standby duty cycles"),
        }

        for pin in self.platform.pins.keys() {
            if standby.get(pin).is_none() {
                debug!(pin = %pin, "no zone drives this pin, left untouched");
            }
        }
    }
    /// One evaluation cycle. Never returns an error: failures are logged and
    /// retried on the next cycle.
    fn tick(&mut self) {
        let snapshot = self.hub.collect(self.timeout);
        for err in &snapshot.errors {
            warn!(error = %err, "sensor read failed");
        }

        if snapshot.is_empty() {
            // Nothing at all came back: waiting out the grace period would keep
            // every fan wherever it happened to be while the BMC is unreachable,
            // so go straight to the blind value of each zone.
            let bounds = &self.policy.global;
            let targets = fallback_targets(
                &self.policy.zones,
                bounds.min_duty_cycle,
                bounds.max_duty_cycle,
            );
            warn!(
                pins = targets.len(),
                "no sensor could be read, applying fallback duty cycles"
            );

            self.zone_misses.fill(0);
            for (index, zone) in self.policy.zones.iter().enumerate() {
                self.zone_states[index] = None;
                self.zone_duties[index] = Some(zone.fallback_duty_cycle);
            }

            // Forced, not deduplicated: a BMC that just rebooted has forgotten
            // the duty cycles, and `last_sent` still believes otherwise.
            self.write_targets(&targets, true, &snapshot);
            // Still reported: a BMC that answers nothing is exactly when an
            // operator needs to see that the loop is running.
            self.report_cycle(&snapshot);
            return;
        }

        // Field-by-field borrows: the loop reads `policy` while it writes the
        // two state vectors. Cloning the zones every cycle only existed to
        // dodge the borrow checker.
        let targets = {
            let Self {
                policy,
                zone_states,
                zone_duties,
                zone_misses,
                ..
            } = self;

            let mut targets = TargetMap::new();

            for (index, zone) in policy.zones.iter().enumerate() {
                let duty_cycle = match super::aggregate(zone, &snapshot) {
                    Some(temperature) => {
                        zone_misses[index] = 0;
                        let state = next_band(zone, zone_states[index], temperature);
                        zone_states[index] = Some(state);
                        let duty_cycle = duty_for_band(zone, state.band);
                        trace!(
                            zone = %zone.name,
                            temperature,
                            band = state.band,
                            duty_cycle,
                            "zone evaluated"
                        );
                        duty_cycle
                    }
                    None => {
                        zone_misses[index] += 1;
                        let misses = zone_misses[index];
                        match zone_duties[index] {
                            // A single failed read must not step the fan: hold
                            // the last value for the configured grace period.
                            Some(held) if misses <= zone.fallback_after_cycles => {
                                if misses == 1 {
                                    warn!(
                                        zone = %zone.name,
                                        sources = ?source_labels(zone),
                                        duty_cycle = held,
                                        grace_cycles = zone.fallback_after_cycles,
                                        "no reading for zone, holding previous duty cycle"
                                    );
                                } else {
                                    debug!(
                                        zone = %zone.name,
                                        misses,
                                        duty_cycle = held,
                                        "still no reading for zone"
                                    );
                                }
                                held
                            }
                            // Either the grace period ran out, or this zone
                            // never produced a reading to hold on to.
                            _ => {
                                let already_there =
                                    zone_duties[index] == Some(zone.fallback_duty_cycle);
                                zone_states[index] = None;
                                // A dead BMC is a real fault, but repeating the
                                // same warning every cycle for an hour buries
                                // the cycle that actually changed something.
                                if already_there {
                                    debug!(
                                        zone = %zone.name,
                                        misses,
                                        duty_cycle = zone.fallback_duty_cycle,
                                        "still unreadable, keeping fallback duty cycle"
                                    );
                                } else {
                                    warn!(
                                        zone = %zone.name,
                                        sources = ?source_labels(zone),
                                        duty_cycle = zone.fallback_duty_cycle,
                                        "no reading for zone, falling back"
                                    );
                                }
                                zone.fallback_duty_cycle
                            }
                        }
                    }
                };

                zone_duties[index] = Some(duty_cycle);
                for pin in &zone.target_pins {
                    targets.raise(pin, duty_cycle);
                }
            }

            targets.apply_follows(&policy.follows);
            targets.clamp(policy.global.min_duty_cycle, policy.global.max_duty_cycle);
            targets
        };

        self.dispatch(&targets, &snapshot);
        self.report_cycle(&snapshot);
    }

    /// Sends the pins whose duty cycle differs from what was last accepted.
    fn dispatch(&mut self, targets: &TargetMap, snapshot: &SensorSnapshot) {
        self.write_targets(targets, false, snapshot);
    }

    /// Writes every pin in `targets` in one `ipmitool exec`.
    ///
    /// `force` skips the `last_sent` comparison. It is used when the BMC may
    /// have forgotten what we told it - a reboot drops every manual duty cycle,
    /// so re-asserting the same value is not a no-op even though `last_sent`
    /// says it is.
    ///
    /// Only pins the batch confirmed are recorded, so a pin the BMC refused is
    /// retried on the next cycle instead of being remembered as applied.
    fn write_targets(&mut self, targets: &TargetMap, force: bool, snapshot: &SensorSnapshot) {
        let pending: Vec<(&str, u8)> = targets
            .iter()
            .filter(|(pin, duty_cycle)| force || self.last_sent.get(*pin) != Some(*duty_cycle))
            .map(|(pin, duty_cycle)| (pin.as_str(), *duty_cycle))
            .collect();

        if pending.is_empty() {
            return;
        }

        let previous: Vec<Option<u8>> = pending
            .iter()
            .map(|(pin, _)| self.last_sent.get(*pin).copied())
            .collect();

        match self.actuator.apply_batch(&pending) {
            Ok(unconfirmed) => {
                // The effect carries its cause. The cycle line is only a
                // five-minute sample, so without this a step at 14:59 would be
                // explained by a temperature from 14:55.
                let temps = temperatures_field(snapshot);
                for (index, &(pin, duty_cycle)) in pending.iter().enumerate() {
                    if unconfirmed.iter().any(|failed| failed.as_str() == pin) {
                        continue;
                    }
                    self.last_sent.insert(pin.to_string(), duty_cycle);
                    // In a dry run the actuator logged the command it would
                    // have sent; reporting "changed" here would claim a write
                    // that never happened.
                    if self.actuator.is_dry_run() {
                        continue;
                    }
                    match previous[index] {
                        Some(from) => info!(
                            pin = %pin,
                            from,
                            to = duty_cycle,
                            temps = %temps,
                            "duty cycle changed"
                        ),
                        // The first write of a pin has no previous value and no
                        // temperature to blame: it is the daemon asserting the
                        // curve, not a reaction to anything. The cycle line that
                        // follows carries the readings.
                        None => info!(pin = %pin, to = duty_cycle, "duty cycle initialised"),
                    }
                }
            }
            Err(err) => warn!(
                pins = pending.len(),
                error = %err,
                "could not apply duty cycles, retrying next cycle"
            ),
        }
    }

    /// Emits the per-cycle temperature line.
    ///
    /// At INFO once [`HEARTBEAT`] has passed, at DEBUG otherwise, so the line is
    /// a predictable sample rather than a change log. A temperature that moved
    /// is only interesting when it moved a fan, and that case has a line of its
    /// own with the temperatures attached ([`temperatures_field`]).
    ///
    /// Fan speeds are carried in the line but do not decide it: a tachometer
    /// wanders by a count or two on its own, and a speed that changed while the
    /// temperature did not is not worth a line of its own.
    ///
    /// This is the daemon's only routine statement that it is still turning,
    /// and the source a temperature curve is drawn from.
    fn report_cycle(&mut self, snapshot: &SensorSnapshot) {
        let line = cycle_line(
            snapshot,
            self.policy
                .zones
                .iter()
                .map(|zone| zone.name.as_str())
                .zip(self.zone_duties.iter().copied()),
        );

        let fans = snapshot.fans.clone();

        // A fan that stops turning gets a line of its own even though the cycle
        // line shows the zero: the line is rate limited and easy to skim past,
        // and this is the one failure the duty cycles cannot show. Only pins
        // that were actually driven can stall - a pin is never written a zero
        // duty cycle, so a speed of zero there is always unexpected.
        for pin in stalled(self.last_fans.as_deref(), &fans) {
            if self.last_sent.get(pin).copied().unwrap_or(0) > 0 {
                warn!(
                    pin = %pin,
                    duty_cycle = self.last_sent.get(pin),
                    "fan reported rotation last cycle and none now"
                );
            }
        }

        let due = self
            .last_report_at
            .is_none_or(|at| at.elapsed() >= HEARTBEAT);

        if !due {
            debug!("{line}");
            return;
        }

        info!("{line}");
        self.last_fans = Some(fans);
        self.last_report_at = Some(Instant::now());
    }

    /// Sleeps up to `interval`, returning `true` as soon as shutdown is requested.
    fn sleep_interruptibly(&self, interval: Duration) -> bool {
        let mut remaining = interval;
        while !remaining.is_zero() {
            if self.shutdown.load(Ordering::Relaxed) {
                return true;
            }
            let slice = remaining.min(SHUTDOWN_POLL);
            thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);
        }
        self.shutdown.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::{cycle_line, stalled, temperatures_field};
    use crate::sensors::SensorSnapshot;

    fn duties(
        pairs: &'static [(&'static str, Option<u8>)],
    ) -> impl Iterator<Item = (&'static str, Option<u8>)> {
        pairs.iter().copied()
    }

    #[test]
    fn a_cycle_reads_as_three_tables() {
        let mut snapshot = SensorSnapshot::default();
        snapshot.ipmi.insert("CPU_Tctl".to_string(), 22.0);
        snapshot.gpus.push((1, "RTX 4080".to_string(), 61.5));
        snapshot.fans.push(("SYS_FAN_1".to_string(), 966));

        assert_eq!(
            cycle_line(&snapshot, duties(&[("cpu", Some(30)), ("pump", Some(100))])),
            "cycle temps CPU_Tctl=22.0 GPU1=61.5 | fan SYS_FAN_1=966 | duty cpu=30 pump=100"
        );
    }

    #[test]
    fn ipmi_names_are_sorted_so_equal_cycles_compare_equal() {
        // The snapshot holds temperatures in a HashMap; without sorting, two
        // identical cycles could print in different orders and the change
        // detection that rate limits this line would see a difference every
        // time, turning the heartbeat into a flood.
        let mut first = SensorSnapshot::default();
        first.ipmi.insert("B".to_string(), 1.0);
        first.ipmi.insert("A".to_string(), 2.0);
        let mut second = SensorSnapshot::default();
        second.ipmi.insert("A".to_string(), 2.0);
        second.ipmi.insert("B".to_string(), 1.0);

        assert_eq!(
            cycle_line(&first, duties(&[])),
            cycle_line(&second, duties(&[]))
        );
    }

    #[test]
    fn a_zone_without_a_reading_keeps_its_column() {
        let line = cycle_line(&SensorSnapshot::default(), duties(&[("cpu", None)]));
        assert_eq!(line, "cycle temps none | duty cpu=?");
    }

    #[test]
    fn the_duty_line_carries_the_temperatures_that_caused_it() {
        let mut snapshot = SensorSnapshot::default();
        snapshot.ipmi.insert("CPU_Tctl".to_string(), 26.0);
        snapshot.gpus.push((0, "RTX 3080".to_string(), 55.0));

        assert_eq!(temperatures_field(&snapshot), "CPU_Tctl:26.0,GPU0:55.0");
    }

    #[test]
    fn a_duty_line_without_readings_says_none() {
        assert_eq!(temperatures_field(&SensorSnapshot::default()), "none");
    }

    fn fans(pairs: &[(&str, u32)]) -> Vec<(String, u32)> {
        pairs
            .iter()
            .map(|(pin, rpm)| ((*pin).to_string(), *rpm))
            .collect()
    }

    #[test]
    fn a_fan_that_stops_is_a_stall() {
        let previous = fans(&[("CPU_FAN", 690)]);
        let current = fans(&[("CPU_FAN", 0)]);
        assert_eq!(stalled(Some(&previous), &current), vec!["CPU_FAN"]);
    }

    #[test]
    fn a_fan_that_was_already_stopped_does_not_stall_again() {
        // Otherwise a dead fan would warn on every cycle, which trains the
        // operator to ignore the one line that matters.
        let previous = fans(&[("CPU_FAN", 0)]);
        let current = fans(&[("CPU_FAN", 0)]);
        assert!(stalled(Some(&previous), &current).is_empty());
    }

    #[test]
    fn the_first_cycle_warns_about_nothing() {
        // Without a previous cycle there is no transition to report; the cycle
        // line itself carries the zero.
        assert!(stalled(None, &fans(&[("CPU_FAN", 0)])).is_empty());
    }
}
