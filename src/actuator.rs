//! The only place that talks to the BMC.
//!
//! Renders the template from `platform.toml` and hands the result to
//! `ipmitool` without a shell, so a pin id can never be interpreted as shell
//! syntax. In dry-run mode the command is logged and skipped - nothing is sent.
//!
//! Duty cycles are written as one `ipmitool exec` process rather than one
//! process per pin: on a TYAN S8030 four separate `raw` calls cost 356 ms and
//! the same four commands inside one `exec` cost 125 ms. The protocol that makes
//! the batch attributable lives in [`crate::bmc`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::bmc;
use crate::config::PlatformConfig;
use crate::error::Error;
use crate::util::process::{run_capturing_with_stdin, run_checked};

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
        let rendered = self.render(pin, duty)?;

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

    /// Sets several pins in one `ipmitool exec` process.
    ///
    /// Returns the pins whose command was **not** confirmed, so the caller can
    /// keep retrying exactly those and leave the confirmed ones alone. An empty
    /// vector means every command answered.
    ///
    /// `Err` is reserved for "the batch never ran": a missing binary, a timeout,
    /// or a pin that is not in `platform.toml`. A BMC that refuses a single
    /// command still returns `Ok` with that pin listed, because the other pins
    /// in the batch did take effect.
    pub fn apply_batch(&self, targets: &[(&str, u8)]) -> Result<Vec<String>, Error> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }

        let mut rendered = Vec::with_capacity(targets.len());
        for (pin, duty) in targets {
            rendered.push(self.render(pin, *duty)?);
        }

        if self.dry_run {
            for ((pin, duty), command) in targets.iter().zip(&rendered) {
                info!(
                    pin,
                    duty,
                    command = %format!("{} {command}", self.ipmitool.display()),
                    "dry-run: command not sent"
                );
            }
            return Ok(Vec::new());
        }

        debug!(
            pins = targets.len(),
            commands = ?rendered,
            "sending PWM batch"
        );

        let script = bmc::script(&rendered);
        let mut command = Command::new(&self.ipmitool);
        command.args(["exec", bmc::SCRIPT_PATH]);

        // `run_capturing_with_stdin`, not the checked variant: `ipmitool exec`
        // exits non-zero as soon as *any* command in the list fails, including
        // the separators, so the exit code cannot tell a partial failure from a
        // dead BMC. Which command answered is decided by the slots, below.
        let output = run_capturing_with_stdin(command, script.as_bytes(), self.timeout)?;

        let text = String::from_utf8_lossy(&output.stdout);
        let acks = bmc::parse_acks(&text, targets.len());
        let unconfirmed: Vec<String> = targets
            .iter()
            .zip(&acks)
            .filter(|(_, confirmed)| !**confirmed)
            .map(|((pin, _), _)| (*pin).to_string())
            .collect();

        if unconfirmed.is_empty() {
            debug!(pins = targets.len(), "PWM batch confirmed");
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(
                pins = ?unconfirmed,
                status = output.status.code(),
                error = %stderr.lines().next().unwrap_or(""),
                "PWM batch was not fully confirmed"
            );
        }

        Ok(unconfirmed)
    }

    /// Fills the pin id and the duty cycle into the configured template.
    fn render(&self, pin: &str, duty: u8) -> Result<String, Error> {
        let id = self
            .pins
            .get(pin)
            .ok_or_else(|| Error::UnknownPin(pin.to_string()))?;

        Ok(self
            .template
            .replace("{id}", id)
            .replace("{pwm}", &format!("0x{duty:02x}")))
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

    #[test]
    fn dry_run_batch_reports_every_pin_as_confirmed() {
        let actuator = PwmActuator::new(&platform(), true, Duration::from_secs(1));
        let unconfirmed = actuator
            .apply_batch(&[("CPU_FAN", 40)])
            .expect("dry run must not start a process");
        assert!(unconfirmed.is_empty());
    }

    #[test]
    fn a_batch_with_an_unknown_pin_fails_before_running() {
        let actuator = PwmActuator::new(&platform(), true, Duration::from_secs(1));
        match actuator.apply_batch(&[("SYS_FAN_9", 40)]) {
            Err(Error::UnknownPin(pin)) => assert_eq!(pin, "SYS_FAN_9"),
            other => panic!("expected UnknownPin, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_batch_needs_no_process() {
        let actuator = PwmActuator::new(&platform(), false, Duration::from_secs(1));
        assert!(actuator
            .apply_batch(&[])
            .expect("nothing to do is not a failure")
            .is_empty());
    }
}
