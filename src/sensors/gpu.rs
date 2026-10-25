//! NVIDIA GPU temperatures through NVML.
//!
//! NVML is loaded lazily: a host without an NVIDIA driver (or without the
//! `gpu` build feature) must keep the daemon running, so every failure here is
//! reported as [`GpuError`] and the caller only logs it.

use thiserror::Error;

/// Library names tried in order when NVML is first needed.
///
/// `nvml-wrapper` defaults to the unversioned `libnvidia-ml.so`, which only
/// exists when the driver's development package is installed. Debian and
/// Ubuntu ship just the SONAME `libnvidia-ml.so.1`, so a stock host would
/// otherwise be unable to read any GPU temperature.
#[cfg(target_os = "linux")]
const NVML_LIB_PATHS: &[&str] = &["libnvidia-ml.so", "libnvidia-ml.so.1"];
/// Same idea for Windows, where the driver installs `nvml.dll`.
#[cfg(not(target_os = "linux"))]
const NVML_LIB_PATHS: &[&str] = &["nvml.dll", "libnvidia-ml.so"];

/// Why no GPU reading could be produced.
#[derive(Debug, Error)]
pub enum GpuError {
    /// The crate was compiled without the `gpu` feature.
    #[error("fanctl was built without the `gpu` feature")]
    Disabled,
    /// NVML is not installed or the driver refused to initialise.
    #[error("NVML init failed: {0}")]
    Init(String),
    /// NVML works but no device could be read this cycle.
    #[error("no GPU temperature available: {0}")]
    Unavailable(String),
}

#[cfg(feature = "gpu")]
mod imp {
    use std::ffi::OsStr;

    use tracing::debug;

    use super::{GpuError, NVML_LIB_PATHS};

    /// Lazily initialised NVML handle.
    pub struct GpuMonitor {
        nvml: Option<nvml_wrapper::Nvml>,
    }

    impl GpuMonitor {
        /// Creates a monitor without touching the driver yet.
        pub fn new() -> Self {
            Self { nvml: None }
        }

        /// Returns `(index, name, celsius)` for every visible GPU.
        ///
        /// The index is the NVML device index, which is stable as long as the
        /// card and the driver stay put - that is the contract `GPU0`, `GPU1`
        /// in `monitor_sources` relies on.
        ///
        /// The handle is cached on success and retried on every call after a
        /// failure, so a driver that is loaded after `fanctl` still gets picked
        /// up on the next cycle.
        pub fn read(&mut self) -> Result<Vec<(u32, String, f64)>, GpuError> {
            if self.nvml.is_none() {
                self.nvml = Some(init_nvml()?);
            }
            let nvml = self
                .nvml
                .as_ref()
                .ok_or_else(|| GpuError::Init("handle not initialised".to_string()))?;

            let count = nvml
                .device_count()
                .map_err(|err| GpuError::Unavailable(format!("cannot count devices: {err}")))?;

            let mut readings = Vec::with_capacity(count as usize);
            let mut last_error = None;
            for index in 0..count {
                match read_device(nvml, index) {
                    Ok(reading) => readings.push(reading),
                    Err(err) => {
                        debug!(gpu = index, error = %err, "skipping GPU");
                        last_error = Some(err);
                    }
                }
            }

            if readings.is_empty() {
                return Err(GpuError::Unavailable(
                    last_error.unwrap_or_else(|| "NVML reported no devices".to_string()),
                ));
            }
            Ok(readings)
        }
    }

    /// Loads NVML from the first library name that resolves.
    ///
    /// The failure reported to the caller mentions every candidate, so a host
    /// with a broken driver install still gets a usable diagnostic.
    fn init_nvml() -> Result<nvml_wrapper::Nvml, GpuError> {
        let mut tried = Vec::with_capacity(NVML_LIB_PATHS.len());
        for path in NVML_LIB_PATHS {
            match nvml_wrapper::Nvml::builder()
                .lib_path(OsStr::new(path))
                .init()
            {
                Ok(nvml) => {
                    debug!(library = path, "NVML loaded");
                    return Ok(nvml);
                }
                Err(err) => tried.push(format!("{path}: {err}")),
            }
        }
        Err(GpuError::Init(tried.join("; ")))
    }

    fn read_device(nvml: &nvml_wrapper::Nvml, index: u32) -> Result<(u32, String, f64), String> {
        let device = nvml
            .device_by_index(index)
            .map_err(|err| format!("cannot open device {index}: {err}"))?;
        let celsius = device
            .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
            .map_err(|err| format!("device {index} temperature: {err}"))?;
        let name = device.name().unwrap_or_else(|_| format!("GPU{index}"));
        Ok((index, name, f64::from(celsius)))
    }

    impl Default for GpuMonitor {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(not(feature = "gpu"))]
mod imp {
    use super::GpuError;

    /// Placeholder used when the crate is built without the `gpu` feature.
    pub struct GpuMonitor;

    impl GpuMonitor {
        /// Creates the placeholder monitor.
        pub fn new() -> Self {
            Self
        }

        /// Always fails: this build cannot read GPUs.
        pub fn read(&mut self) -> Result<Vec<(u32, String, f64)>, GpuError> {
            Err(GpuError::Disabled)
        }
    }

    impl Default for GpuMonitor {
        fn default() -> Self {
            Self::new()
        }
    }
}

pub use imp::GpuMonitor;
