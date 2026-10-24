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

    /// `true` when not a single reading could be obtained.
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

/// Polls every provider and folds the results into one [`SensorSnapshot`].
pub struct SensorHub {
    ipmitool: PathBuf,
    /// Only the sensors a zone references. A sensor declared in `platform.toml`
    /// but never watched costs nothing - it is not read at all.
    watched: Vec<WatchedSensor>,
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

        let numbers: Vec<u8> = watched.iter().map(|sensor| sensor.number).collect();
        let script = ipmi::build_script(&numbers);

        Self {
            ipmitool: platform.info.ipmitool_path.clone(),
            watched,
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

        if !self.watched.is_empty() {
            let expected = self.watched.len();
            match ipmi::read_sensors(&self.ipmitool, &self.script, expected, timeout) {
                Ok(batch) => {
                    if let Some(details) = batch.failure {
                        // Partial failure is normal: the separator protocol
                        // already isolated it to a specific sensor below.
                        warn!(error = %details, "ipmitool reported a failed read");
                    }
                    for (sensor, value) in self.watched.iter().zip(batch.values) {
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
    use super::is_plausible;

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
