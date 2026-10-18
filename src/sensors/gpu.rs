//! NVIDIA GPU temperatures through NVML.
//!
//! NVML is loaded lazily: a host without an NVIDIA driver (or without the
//! `gpu` build feature) must keep the daemon running, so every failure here is
//! reported as [`GpuError`] and the caller only logs it.

use thiserror::Error;

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
    use tracing::debug;

    use super::GpuError;

    /// Lazily initialised NVML handle.
    pub struct GpuMonitor {
        nvml: Option<nvml_wrapper::Nvml>,
    }

    impl GpuMonitor {
        /// Creates a monitor without touching the driver yet.
        pub fn new() -> Self {
            Self { nvml: None }
        }

        /// Returns `(name, celsius)` for every visible GPU.
        ///
        /// The handle is cached on success and retried on every call after a
        /// failure, so a driver that is loaded after `fanctl` still gets picked
        /// up on the next cycle.
        pub fn read(&mut self) -> Result<Vec<(String, f64)>, GpuError> {
            if self.nvml.is_none() {
                match nvml_wrapper::Nvml::init() {
                    Ok(nvml) => self.nvml = Some(nvml),
                    Err(err) => return Err(GpuError::Init(err.to_string())),
                }
            }
            let nvml = match self.nvml.as_ref() {
                Some(nvml) => nvml,
                None => return Err(GpuError::Init("handle not initialised".to_string())),
            };

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

    fn read_device(nvml: &nvml_wrapper::Nvml, index: u32) -> Result<(String, f64), String> {
        let device = nvml
            .device_by_index(index)
            .map_err(|err| format!("cannot open device {index}: {err}"))?;
        let celsius = device
            .temperature(nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu)
            .map_err(|err| format!("device {index} temperature: {err}"))?;
        let name = device.name().unwrap_or_else(|_| format!("GPU{index}"));
        Ok((name, f64::from(celsius)))
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
        pub fn read(&mut self) -> Result<Vec<(String, f64)>, GpuError> {
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
