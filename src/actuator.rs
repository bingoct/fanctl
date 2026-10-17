//! The only place that talks to the BMC.
//!
//! Renders the template from `platform.toml` and hands the result to
//! `ipmitool` without a shell, so a pin id can never be interpreted as shell
//! syntax. In dry-run mode the command is logged and skipped - nothing is sent.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tracing::{debug, info};

use crate::config::PlatformConfig;
use crate::error::Error;
use crate::process::output_with_timeout;

/// Writes PWM duty cycles to the board.
pub struct PwmActuator {
    ipmitool: PathBuf,
    template: String,
    pins: BTreeMap<String, String>,
    dry_run: bool,
    timeout: Duration,
}

impl PwmActuator {
    /// Builds an actuator for `platform`.
    pub fn new(platform: &PlatformConfig, dry_run: bool, timeout: Duration) -> Self {
        Self {
            ipmitool: platform.info.ipmitool_path.clone(),
            template: platform.info.cmd_template.clone(),
            pins: platform.pins.clone(),
            dry_run,
            timeout,
        }
    }

    /// `true` when commands are logged but not executed.
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Sets `pin` to `duty` percent.
    ///
    /// Returns an error on any failure; the caller logs it and retries on the
    /// next cycle, which is the desired behaviour for a rebooting BMC.
    pub fn apply(&self, pin: &str, duty: u8) -> Result<(), Error> {
        let id = self.pins.get(pin).ok_or_else(|| {
            Error::Validation(format!(
                "pin `{pin}` is not defined in platform.toml [pins]"
            ))
        })?;

        let rendered = self
            .template
            .replace("{id}", id)
            .replace("{pwm}", &format!("0x{duty:02x}"));
        let command_line = format!("{} {rendered}", self.ipmitool.display());

        if self.dry_run {
            info!(pin, duty, command = %command_line, "dry-run: command not sent");
            return Ok(());
        }

        debug!(pin, duty, command = %command_line, "sending PWM command");
        let mut command = Command::new(&self.ipmitool);
        command.args(rendered.split_whitespace());

        let output = output_with_timeout(command, self.timeout).map_err(|source| {
            Error::Ipmi(format!(
                "could not run `{}`: {source}",
                self.ipmitool.display()
            ))
        })?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(Error::Ipmi(format!(
            "`ipmitool {rendered}` failed with {}: {} {}",
            output.status,
            stdout.trim(),
            stderr.trim()
        )))
    }
}
