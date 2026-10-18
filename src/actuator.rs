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
use crate::util::process::run_checked;

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
        let id = self
            .pins
            .get(pin)
            .ok_or_else(|| Error::UnknownPin(pin.to_string()))?;

        let rendered = self
            .template
            .replace("{id}", id)
            .replace("{pwm}", &format!("0x{duty:02x}"));

        if self.dry_run {
            info!(
                pin,
                duty,
                command = %format!("{} {rendered}", self.ipmitool.display()),
                "dry-run: command not sent"
            );
            return Ok(());
        }

        // Built inside the macro on purpose: `tracing` only evaluates field
        // values when the level is enabled, so a normal run at INFO does not
        // format a command line it will never print.
        debug!(
            pin,
            duty,
            command = %format!("{} {rendered}", self.ipmitool.display()),
            "sending PWM command"
        );
        let mut command = Command::new(&self.ipmitool);
        command.args(rendered.split_whitespace());

        run_checked(command, self.timeout)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::PwmActuator;
    use crate::config::PlatformConfig;
    use crate::error::Error;

    fn platform() -> PlatformConfig {
        toml::from_str(
            r#"
[platform]
name = "Test"
cmd_template = "raw 0x2e {id} {pwm}"

[pins]
"CPU_FAN" = "0x00"

[sensors]
"CPU_Tctl" = { sdr = "CPU_Tctl_Value", id = "0x01" }
"#,
        )
        .expect("test platform must parse")
    }

    #[test]
    fn unknown_pin_has_its_own_error() {
        let actuator = PwmActuator::new(&platform(), true, Duration::from_secs(1));
        match actuator.apply("SYS_FAN_9", 40) {
            Err(Error::UnknownPin(pin)) => assert_eq!(pin, "SYS_FAN_9"),
            other => panic!("expected UnknownPin, got {other:?}"),
        }
    }

    #[test]
    fn dry_run_never_runs_the_command() {
        let actuator = PwmActuator::new(&platform(), true, Duration::from_secs(1));
        actuator
            .apply("CPU_FAN", 40)
            .expect("dry run must succeed even without ipmitool");
    }
}
