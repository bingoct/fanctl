//! Temperature acquisition.
//!
//! Two providers exist: IPMI (`ipmitool sdr`, covers CPU and board sensors) and
//! NVML (covers NVIDIA GPUs). Both are optional at runtime - a missing GPU or a
//! temporarily unreachable BMC must never stop the loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use tracing::debug;

use crate::config::PlatformConfig;

pub mod gpu;
pub mod ipmi;

/// The one reserved source name that is not an IPMI sensor.
pub const GPU_SOURCE: &str = "GPU";

/// A single temperature source referenced from `monitor_sources`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A sensor declared in `platform.toml` `[sensors]`.
    Ipmi(String),
    /// All NVIDIA GPUs reported by NVML.
    Gpu,
}

impl Source {
    /// Parses a `monitor_sources` entry.
    ///
    /// `"GPU"` (case insensitive) is reserved; everything else is looked up in
    /// `platform.toml` `[sensors]`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("monitor source name must not be empty".to_string());
        }
        if trimmed.eq_ignore_ascii_case(GPU_SOURCE) {
            return Ok(Source::Gpu);
        }
        Ok(Source::Ipmi(trimmed.to_string()))
    }

    /// Name used in logs and reports.
    pub fn label(&self) -> &str {
        match self {
            Source::Ipmi(key) => key,
            Source::Gpu => GPU_SOURCE,
        }
    }
}

/// Everything read during one cycle.
#[derive(Debug, Default)]
pub struct SensorSnapshot {
    /// Semantic sensor name -> degrees Celsius.
    pub ipmi: HashMap<String, f64>,
    /// Per GPU name and temperature, empty when NVML is unavailable.
    pub gpus: Vec<(String, f64)>,
    /// Non fatal problems worth logging.
    pub errors: Vec<String>,
}

impl SensorSnapshot {
    /// Current value of `source`, if it could be read this cycle.
    pub fn get(&self, source: &Source) -> Option<f64> {
        match source {
            Source::Ipmi(key) => self.ipmi.get(key).copied(),
            Source::Gpu => self.gpus.iter().map(|(_, celsius)| *celsius).fold(
                None,
                |best: Option<f64>, celsius| {
                    Some(best.map_or(celsius, |current| current.max(celsius)))
                },
            ),
        }
    }

    /// `true` when not a single reading could be obtained.
    pub fn is_empty(&self) -> bool {
        self.ipmi.is_empty() && self.gpus.is_empty()
    }
}

/// Polls every provider and folds the results into one [`SensorSnapshot`].
pub struct SensorHub {
    ipmitool: PathBuf,
    /// Lowercased SDR name -> semantic name.
    sdr_to_semantic: HashMap<String, String>,
    /// Semantic name -> configured SDR name, for diagnostics.
    semantic_to_sdr: HashMap<String, String>,
    gpu: gpu::GpuMonitor,
}

impl SensorHub {
    /// Builds the hub from the platform description.
    pub fn new(platform: &PlatformConfig) -> Self {
        let sdr_to_semantic = platform
            .sensors
            .iter()
            .map(|(semantic, sdr)| (sdr.trim().to_ascii_lowercase(), semantic.clone()))
            .collect();
        let semantic_to_sdr = platform
            .sensors
            .iter()
            .map(|(semantic, sdr)| (semantic.clone(), sdr.clone()))
            .collect();

        Self {
            ipmitool: platform.info.ipmitool_path.clone(),
            sdr_to_semantic,
            semantic_to_sdr,
            gpu: gpu::GpuMonitor::new(),
        }
    }

    /// Reads all sensors once.
    ///
    /// Failures are recorded in [`SensorSnapshot::errors`] instead of being
    /// returned: a single bad cycle must not tear down the daemon.
    pub fn collect(&mut self, timeout: Duration) -> SensorSnapshot {
        let mut snapshot = SensorSnapshot::default();

        match ipmi::read_sdr(&self.ipmitool, timeout) {
            Ok(entries) => {
                for entry in entries {
                    let key = entry.name.trim().to_ascii_lowercase();
                    if let Some(semantic) = self.sdr_to_semantic.get(&key) {
                        if let Some(celsius) = entry.celsius {
                            snapshot.ipmi.insert(semantic.clone(), celsius);
                        }
                    }
                }
                for (semantic, sdr_name) in &self.semantic_to_sdr {
                    if !snapshot.ipmi.contains_key(semantic) {
                        debug!(sensor = %semantic, sdr = %sdr_name, "no temperature reading this cycle");
                    }
                }
            }
            Err(err) => snapshot.errors.push(err.to_string()),
        }

        // A missing GPU is a normal state, not an error: log it at DEBUG only.
        match self.gpu.read() {
            Ok(readings) => snapshot.gpus = readings,
            Err(err) => debug!(error = %err, "GPU temperature unavailable"),
        }

        snapshot
    }
}
