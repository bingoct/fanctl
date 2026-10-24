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
use crate::sensors::{SensorHub, Source};

use super::curve::{duty_for_band, next_band, ZoneState};
use super::merge::{driven_pins, fallback_targets, standby_targets, TargetMap};

/// How often the shutdown flag is checked while sleeping.
const SHUTDOWN_POLL: Duration = Duration::from_millis(200);

/// Source names of a zone, for the log lines that explain a missing reading.
fn source_labels(zone: &ZoneConfig) -> Vec<String> {
    zone.sources.iter().map(Source::label).collect()
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
            self.write_targets(&targets, true);
            return;
        }

        debug!(
            ipmi = ?snapshot.ipmi,
            gpus = ?snapshot.gpus,
            "cycle readings"
        );

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

        self.dispatch(&targets);
    }

    /// Sends the pins whose duty cycle differs from what was last accepted.
    fn dispatch(&mut self, targets: &TargetMap) {
        self.write_targets(targets, false);
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
    fn write_targets(&mut self, targets: &TargetMap, force: bool) {
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
                        Some(from) => {
                            info!(pin = %pin, from, to = duty_cycle, "duty cycle changed")
                        }
                        None => info!(pin = %pin, to = duty_cycle, "duty cycle set"),
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
