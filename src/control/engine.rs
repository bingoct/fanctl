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
//! | a zone never had a reading | use `global.fallback_pwm` |
//! | *no* sensor at all | `global.fallback_pwm` on every pin |
//! | `ipmitool` write fails | log, retry next cycle, never exit |
//! | SIGINT / SIGTERM | each controlled pin -> its zone's `default_duty_cycle`, then exit 0 |

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tracing::{debug, info, trace, warn};

use crate::actuator::PwmActuator;
use crate::config::{FanConfig, LoadedConfig, PlatformConfig};
use crate::error::Error;
use crate::sensors::SensorHub;

use super::curve::{duty_for_band, next_band, ZoneState};
use super::merge::{standby_targets, TargetMap};

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
    shutdown: Arc<AtomicBool>,
    timeout: Duration,
}

impl Engine {
    /// Builds the engine from validated configuration.
    pub fn new(loaded: LoadedConfig, dry_run: bool, shutdown: Arc<AtomicBool>) -> Self {
        let zone_count = loaded.policy.zones.len();
        let timeout = Duration::from_secs(loaded.policy.global.command_timeout_seconds.max(1));
        let hub = SensorHub::new(&loaded.platform);
        let actuator = PwmActuator::new(&loaded.platform, dry_run, timeout);

        Self {
            platform: loaded.platform,
            policy: loaded.policy,
            hub,
            actuator,
            zone_states: vec![None; zone_count],
            zone_duties: vec![None; zone_count],
            last_sent: BTreeMap::new(),
            shutdown,
            timeout,
        }
    }

    /// Runs until the shutdown flag is raised.
    pub fn run(mut self) -> Result<(), Error> {
        info!(
            board = %self.platform.info.name,
            zones = self.policy.zones.len(),
            pins = self.platform.pins.len(),
            interval_seconds = self.policy.global.interval_seconds,
            dry_run = self.actuator.is_dry_run(),
            "control loop started"
        );

        let interval = Duration::from_secs(self.policy.global.interval_seconds.max(1));
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }
            self.tick();
            if self.sleep_interruptibly(interval) {
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
        let standby = standby_targets(&self.policy.zones, bounds.min_pwm, bounds.max_pwm);
        info!(
            pins = standby.len(),
            "shutdown requested, restoring standby duty cycle"
        );

        for (pin, duty) in standby.iter() {
            let duty = *duty;
            match self.actuator.apply(pin, duty) {
                Ok(()) => {
                    self.last_sent.insert(pin.clone(), duty);
                    info!(pin = %pin, duty, "standby duty cycle written");
                }
                Err(err) => warn!(
                    pin = %pin,
                    duty,
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
            let fallback = self.policy.global.fallback_pwm;
            warn!(
                duty = fallback,
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

        // Cloned so the loop body never holds a borrow of `self` while the
        // state vectors are updated. Zones are a handful of small structs.
        let zones = self.policy.zones.clone();
        let mut targets = TargetMap::new();

        for (index, zone) in zones.iter().enumerate() {
            let duty = match super::aggregate(zone, &snapshot) {
                Some(temperature) => {
                    let state = next_band(zone, self.zone_states[index], temperature);
                    self.zone_states[index] = Some(state);
                    let duty = duty_for_band(zone, state.band);
                    trace!(
                        zone = %zone.name,
                        temperature,
                        band = state.band,
                        duty,
                        "zone evaluated"
                    );
                    duty
                }
                None => {
                    let held = self.zone_duties[index].unwrap_or(self.policy.global.fallback_pwm);
                    warn!(
                        zone = %zone.name,
                        sources = ?zone.monitor_sources,
                        duty = held,
                        "no reading for zone, holding previous duty cycle"
                    );
                    held
                }
            };

            self.zone_duties[index] = Some(duty);
            for pin in &zone.target_pins {
                targets.raise(pin, duty);
            }
        }

        targets.apply_follows(&self.policy.follows);
        targets.clamp(self.policy.global.min_pwm, self.policy.global.max_pwm);
        self.dispatch(&targets);
    }

    /// Sends the pins whose duty cycle differs from what was last accepted.
    fn dispatch(&mut self, targets: &TargetMap) {
        for (pin, duty) in targets.iter() {
            let duty = *duty;
            if self.last_sent.get(pin).copied() == Some(duty) {
                continue;
            }

            match self.actuator.apply(pin, duty) {
                Ok(()) => {
                    let previous = self.last_sent.insert(pin.clone(), duty);
                    match previous {
                        Some(from) => info!(pin = %pin, from, to = duty, "duty cycle changed"),
                        None => info!(pin = %pin, to = duty, "duty cycle set"),
                    }
                }
                Err(err) => warn!(
                    pin = %pin,
                    duty,
                    error = %err,
                    "could not apply duty cycle, retrying next cycle"
                ),
            }
        }
    }

    fn apply_to_all_pins(&mut self, duty: u8) {
        let pins: Vec<String> = self.platform.pins.keys().cloned().collect();
        for pin in pins {
            match self.actuator.apply(&pin, duty) {
                Ok(()) => {
                    self.last_sent.insert(pin, duty);
                }
                Err(err) => warn!(
                    pin = %pin,
                    duty,
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
