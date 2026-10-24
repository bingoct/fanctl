//! The evaluation loop.
//!
//! One cycle is: read every sensor, evaluate every zone, merge the demands,
//! send only the pins whose duty cycle actually changed.
//!
//! Failure policy (deliberately conservative):
//!
//! | situation | reaction |
//! |-----------|----------|
//! | one zone has no reading | hold that zone's last duty cycle |
//! | a zone never had a reading | use `global.fallback_duty_cycle` |
//! | *no* sensor at all | `global.fallback_duty_cycle` on every driven pin |
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
use crate::config::{FanConfig, LoadedConfig, PlatformConfig};
use crate::error::Error;
use crate::sensors::{SensorHub, Source};

use super::curve::{duty_for_band, next_band, ZoneState};
use super::merge::{driven_pins, standby_targets, TargetMap};

/// How often the shutdown flag is checked while sleeping.
const SHUTDOWN_POLL: Duration = Duration::from_millis(200);

/// The daemon state machine.
pub struct Engine {
    platform: PlatformConfig,
    policy: FanConfig,
    hub: SensorHub,
    actuator: PwmActuator,
    zone_states: Vec<Option<ZoneState>>,
    zone_duties: Vec<Option<u8>>,
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

        for (pin, duty_cycle) in standby.iter() {
            let duty_cycle = *duty_cycle;
            match self.actuator.apply(pin, duty_cycle) {
                Ok(()) => {
                    self.last_sent.insert(pin.clone(), duty_cycle);
                    info!(pin = %pin, duty_cycle, "standby duty cycle written");
                }
                Err(err) => warn!(
                    pin = %pin,
                    duty_cycle,
                    error = %err,
                    "could not write standby duty cycle"
                ),
            }
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
            let fallback = self.policy.global.fallback_duty_cycle;
            warn!(
                duty_cycle = fallback,
                "no sensor could be read, applying fallback duty cycle"
            );
            self.apply_to_all_pins(fallback);
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
                ..
            } = self;

            let mut targets = TargetMap::new();

            for (index, zone) in policy.zones.iter().enumerate() {
                let duty_cycle = match super::aggregate(zone, &snapshot) {
                    Some(temperature) => {
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
                        let held = zone_duties[index].unwrap_or(policy.global.fallback_duty_cycle);
                        warn!(
                            zone = %zone.name,
                            sources = ?zone.sources.iter().map(Source::label).collect::<Vec<_>>(),
                            duty_cycle = held,
                            "no reading for zone, holding previous duty cycle"
                        );
                        held
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
        for (pin, duty_cycle) in targets.iter() {
            let duty_cycle = *duty_cycle;
            if self.last_sent.get(pin).copied() == Some(duty_cycle) {
                continue;
            }

            match self.actuator.apply(pin, duty_cycle) {
                Ok(()) => {
                    let previous = self.last_sent.insert(pin.clone(), duty_cycle);
                    match previous {
                        Some(from) => {
                            info!(pin = %pin, from, to = duty_cycle, "duty cycle changed")
                        }
                        None => info!(pin = %pin, to = duty_cycle, "duty cycle set"),
                    }
                }
                Err(err) => warn!(
                    pin = %pin,
                    duty_cycle,
                    error = %err,
                    "could not apply duty cycle, retrying next cycle"
                ),
            }
        }
    }

    /// Applies `duty_cycle` to every pin a zone drives.
    ///
    /// Pins no zone drives are skipped for the same reason shutdown skips them:
    /// the daemon never took responsibility for those headers, and since the
    /// shutdown path would not restore them, writing them here would leave the
    /// BMC holding a value nobody owns.
    fn apply_to_all_pins(&mut self, duty_cycle: u8) {
        let pins = self.driven.clone();
        for pin in pins {
            match self.actuator.apply(&pin, duty_cycle) {
                Ok(()) => {
                    self.last_sent.insert(pin, duty_cycle);
                }
                Err(err) => warn!(
                    pin = %pin,
                    duty_cycle,
                    error = %err,
                    "could not apply safe duty cycle"
                ),
            }
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
