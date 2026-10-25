//! Temperature acquisition.
//!
//! Two providers exist: IPMI (`ipmitool sdr`, covers CPU and board sensors) and
//! NVML (covers NVIDIA GPUs). Both are optional at runtime - a missing GPU or a
//! temporarily unreachable BMC must never stop the loop.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use tracing::{debug, error, warn};

use crate::config::PlatformConfig;
use crate::util::name::{parse_gpu_source, GPU_SOURCE};

pub mod gpu;
pub mod ipmi;

/// Exclusive lower bound: a *running* server is never at or below freezing, so
/// `0` and `-1` are the values a BMC invents for a sensor it cannot read.
const PLAUSIBLE_MIN_CELSIUS: f64 = 0.0;
/// Above this, the BMC is reporting nonsense: server silicon tops out near 115.
const PLAUSIBLE_MAX_CELSIUS: f64 = 150.0;

/// `true` when `celsius` is a temperature a running server could really have.
///
/// Only used to decide whether to shout, never to discard: a sensor that
/// reports `0 degrees C` while the CPU is cooking drags its zone to standby,
/// and silently dropping the reading instead would freeze the zone at its last
/// duty cycle - possibly a fast one, hiding the fault until someone looks.
/// Falling back to standby *and* screaming is the honest reaction; alerting on
/// the log line is the operator's job.
fn is_plausible(celsius: f64) -> bool {
    celsius > PLAUSIBLE_MIN_CELSIUS && celsius <= PLAUSIBLE_MAX_CELSIUS
}

/// A single temperature source referenced from `monitor_sources`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A sensor declared in `platform.toml` `[sensors]`.
    Ipmi(String),
    /// NVIDIA readings: `None` folds every device NVML reports, `Some(index)`
    /// selects one device by its NVML index.
    Gpu(Option<u32>),
}

impl Source {
    /// Parses a `monitor_sources` entry.
    ///
    /// `"GPU"` and `"GPU<n>"` (any spelling) are reserved - the first folds
    /// every device, the second selects one by NVML index; everything else is a
    /// semantic name from `platform.toml` `[sensors]`, folded onto its declared
    /// spelling later by [`crate::config::FanConfig::resolve_names`].
    ///
    /// Parsing is infallible and happens once, when the configuration is
    /// loaded: the control loop must never re-derive a source from a string.
    /// An empty or unknown name is carried through unchanged, so validation can
    /// report it against the zone it was written in.
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        match parse_gpu_source(trimmed) {
            Some(index) => Source::Gpu(index),
            None => Source::Ipmi(trimmed.to_string()),
        }
    }

    /// Name used in logs and reports.
    ///
    /// Owned rather than borrowed because an indexed source has no stable
    /// spelling in memory; this only runs on the logging and `check` paths, not
    /// per cycle.
    pub fn label(&self) -> String {
        match self {
            Source::Ipmi(key) => key.clone(),
            Source::Gpu(None) => GPU_SOURCE.to_string(),
            Source::Gpu(Some(index)) => format!("{GPU_SOURCE}{index}"),
        }
    }
}

/// Everything read during one cycle.
#[derive(Debug, Default)]
pub struct SensorSnapshot {
    /// Semantic sensor name -> degrees Celsius.
    pub ipmi: HashMap<String, f64>,
    /// Per GPU `(NVML index, name, temperature)`, empty when NVML is
    /// unavailable. The index is kept explicitly because a device that fails to
    /// report is skipped, which would otherwise shift every later reading.
    pub gpus: Vec<(u32, String, f64)>,
    /// Pin name -> measured speed, in the order declared in `[tachometers]`.
    ///
    /// Reports only: nothing here ever changes a duty cycle. A fan that stops
    /// turning is worth *seeing* next to the duty the daemon asked for, but it
    /// is the BMC's job to raise a threshold event for it.
    pub fans: Vec<(String, u32)>,
    /// Non fatal problems worth logging.
    pub errors: Vec<String>,
}

impl SensorSnapshot {
    /// Current value of `source`, if it could be read this cycle.
    pub fn get(&self, source: &Source) -> Option<f64> {
        match source {
            Source::Ipmi(key) => self.ipmi.get(key).copied(),
            Source::Gpu(None) => self.gpus.iter().map(|(_, _, celsius)| *celsius).fold(
                None,
                |best: Option<f64>, celsius| {
                    Some(best.map_or(celsius, |current| current.max(celsius)))
                },
            ),
            // A missing index yields `None`, which the engine already treats as
            // "no reading": it holds the previous duty cycle and logs a warning
            // instead of silently driving the zone from another card.
            Source::Gpu(Some(wanted)) => self
                .gpus
                .iter()
                .find(|(index, _, _)| index == wanted)
                .map(|(_, _, celsius)| *celsius),
        }
    }

    /// `true` when not a single temperature could be obtained.
    ///
    /// Fan speeds deliberately do not count: a cycle that reads tachometers
    /// but no temperatures still has to fall back, because it cannot decide a
    /// duty cycle.
    pub fn is_empty(&self) -> bool {
        self.ipmi.is_empty() && self.gpus.is_empty()
    }
}

/// A sensor some zone actually watches.
///
/// Built once at startup, so the control loop never re-derives a name, a number
/// or an allocation from the configuration.
#[derive(Debug, Clone)]
struct WatchedSensor {
    /// Name used in `config.toml` and in logs; also the snapshot key.
    semantic: String,
    /// Name as `ipmitool sdr` prints it, for diagnostics only.
    sdr: String,
    /// IPMI sensor number used by the raw read.
    number: u8,
}

/// A tachometer, read for the log only.
#[derive(Debug, Clone)]
struct WatchedTachometer {
    /// Pin the tachometer belongs to; the key in `[tachometers]`.
    pin: String,
}

/// Converts a raw tachometer count into RPM.
///
/// `raw 0x04 0x2d` returns the count, not the value `ipmitool sdr` prints: the
/// SDR record's M coefficient is applied by `ipmitool`, not by the BMC.
fn counts_to_rpm(count: f64, per_count: f64) -> u32 {
    // `count` is a byte and `per_count` was validated as positive and finite,
    // so the cast saturates at `u32::MAX` rather than wrapping.
    (count * per_count).round() as u32
}

/// Polls every provider and folds the results into one [`SensorSnapshot`].
pub struct SensorHub {
    ipmitool: PathBuf,
    /// Only the sensors a zone references. A sensor declared in `platform.toml`
    /// but never watched costs nothing - it is not read at all.
    watched: Vec<WatchedSensor>,
    /// Tachometers to report. Independent of `watched`: a pin can have one
    /// without any zone watching it, and vice versa.
    tachometers: Vec<WatchedTachometer>,
    /// RPM per raw count; validated positive, so the `unwrap_or` never matters.
    rpm_per_count: f64,
    /// The `ipmitool exec` command list, built once: the sensor numbers do not
    /// change, so the control loop never formats or allocates it again.
    script: String,
    gpu: gpu::GpuMonitor,
    /// Whether some zone watches `GPU`. A CPU-only host would otherwise retry
    /// `dlopen("libnvidia-ml.so")` on every single cycle, forever.
    uses_gpu: bool,
}

impl SensorHub {
    /// Builds the hub from the platform description and the sources in use.
    ///
    /// `sources` is every `monitor_sources` entry of every zone; `uses_gpu` is
    /// derived from the same list by the caller. Neither allocates a temporary
    /// collection: the iterator is walked once, here.
    pub fn new<'a>(
        platform: &PlatformConfig,
        sources: impl IntoIterator<Item = &'a Source>,
        uses_gpu: bool,
    ) -> Self {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut watched = Vec::new();

        for source in sources {
            // Duplicates are normal: several zones may watch the same sensor,
            // and it only needs reading once.
            if let Source::Ipmi(name) = source {
                if !seen.insert(name.as_str()) {
                    continue;
                }
                // Validation guarantees the name exists; skipping silently is
                // only a safety net, not a configuration path.
                if let Some(spec) = platform.sensors.get(name.as_str()) {
                    watched.push(WatchedSensor {
                        semantic: name.clone(),
                        sdr: spec.sdr.clone(),
                        number: spec.id,
                    });
                }
            }
        }

        let mut numbers: Vec<u8> = watched.iter().map(|sensor| sensor.number).collect();

        let mut tachometers = Vec::new();
        for (pin, spec) in &platform.tachometers {
            // Validation already rejected an unknown pin; skipping keeps the
            // control loop running if it ever slips through.
            let Some(declared) = platform.find_pin(pin) else {
                continue;
            };
            tachometers.push(WatchedTachometer {
                pin: declared.to_string(),
            });
            numbers.push(spec.id);
        }

        let script = ipmi::build_script(&numbers);

        Self {
            ipmitool: platform.info.ipmitool_path.clone(),
            watched,
            tachometers,
            rpm_per_count: platform.info.rpm_per_count.unwrap_or(1.0),
            script,
            gpu: gpu::GpuMonitor::new(),
            uses_gpu,
        }
    }

    /// Reads all watched sensors once.
    ///
    /// Failures are recorded in [`SensorSnapshot::errors`] instead of being
    /// returned: a single bad cycle must not tear down the daemon.
    pub fn collect(&mut self, timeout: Duration) -> SensorSnapshot {
        let mut snapshot = SensorSnapshot::default();

        if !self.watched.is_empty() || !self.tachometers.is_empty() {
            let expected = self.watched.len() + self.tachometers.len();
            match ipmi::read_sensors(&self.ipmitool, &self.script, expected, timeout) {
                Ok(batch) => {
                    if let Some(details) = batch.failure {
                        // Partial failure is normal: the separator protocol
                        // already isolated it to a specific sensor below.
                        warn!(error = %details, "ipmitool reported a failed read");
                    }
                    // One batch, one order: temperatures first, then
                    // tachometers, exactly as the script was built.
                    let mut values = batch.values.into_iter();
                    for (sensor, value) in self.watched.iter().zip(values.by_ref()) {
                        match value {
                            Some(celsius) => {
                                if !is_plausible(celsius) {
                                    error!(
                                        sensor = %sensor.semantic,
                                        sdr = %sensor.sdr,
                                        celsius,
                                        "implausible temperature reported by the BMC; using it \
                                         anyway, the zone will drop to standby - check the sensor"
                                    );
                                }
                                snapshot.ipmi.insert(sensor.semantic.clone(), celsius);
                            }
                            None => debug!(
                                sensor = %sensor.semantic,
                                sdr = %sensor.sdr,
                                "no temperature reading this cycle"
                            ),
                        }
                    }
                    for (tachometer, value) in self.tachometers.iter().zip(values) {
                        match value {
                            Some(count) => snapshot.fans.push((
                                tachometer.pin.clone(),
                                counts_to_rpm(count, self.rpm_per_count),
                            )),
                            None => debug!(
                                pin = %tachometer.pin,
                                "no tachometer reading this cycle"
                            ),
                        }
                    }
                }
                Err(err) => snapshot.errors.push(err.to_string()),
            }
        }

        // A missing GPU is a normal state, not an error: log it at DEBUG only.
        // Skipped entirely when no zone watches `GPU`.
        if self.uses_gpu {
            match self.gpu.read() {
                Ok(readings) => snapshot.gpus = readings,
                Err(err) => debug!(error = %err, "GPU temperature unavailable"),
            }
        }

        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::{counts_to_rpm, is_plausible};

    #[test]
    fn a_tach_count_scales_to_the_rpm_sdr_prints() {
        // Measured on a TYAN S8030: `raw 0x04 0x2d 0x61` answers `07 ...` while
        // `ipmitool sdr get SYS_FAN_1` says 966 RPM.
        assert_eq!(counts_to_rpm(7.0, 138.0), 966);
        assert_eq!(counts_to_rpm(44.0, 138.0), 6072);
        assert_eq!(counts_to_rpm(0.0, 138.0), 0);
    }

    #[test]
    fn normal_server_temperatures_are_plausible() {
        for celsius in [5.0, 18.5, 20.0, 65.0, 95.0, 115.0] {
            assert!(is_plausible(celsius), "{celsius} C should be plausible");
        }
    }

    #[test]
    fn dead_sensor_values_are_not_plausible() {
        // BMCs report exactly these for a sensor they cannot actually read; the
        // reading is still used, but the operator has to hear about it.
        for celsius in [-128.0, -1.0, 0.0, 200.0, f64::MIN, f64::MAX] {
            assert!(!is_plausible(celsius), "{celsius} C should be implausible");
        }
    }
}
