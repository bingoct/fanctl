//! Lifecycle guarantees of the control loop.
//!
//! These tests never touch a BMC: `ipmitool_path` points at a path that does not
//! exist, which is exactly the "BMC is gone" failure mode the daemon must
//! survive. The loop has to keep its timing, log the failure, and still stop on
//! the shutdown flag.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fanctl::config::LoadedConfig;
use fanctl::control::Engine;
use tempfile::TempDir;

const PLATFORM: &str = r#"
[platform]
name = "Dead BMC"
cmd_template = "raw 0x2e 0x44 0xfd 0x19 0x00 {id} 0x01 {pwm}"
ipmitool_path = "/nonexistent/ipmitool"

[pins]
"CPU_FAN" = "0x00"

[sensors]
"CPU_Tctl" = "CPU_Tctl_Value"
"#;

const POLICY: &str = r#"
[global]
interval_seconds = 1
min_pwm = 25
max_pwm = 100
fallback_pwm = 60

[[zones]]
name = "cpu_zone"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]
default_duty_cycle = 30
debounce_temp = 2

[[zones.thresholds]]
temperature = 55
duty_cycle = 40

[[zones.thresholds]]
temperature = 70
duty_cycle = 60
"#;

fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let platform_path = dir.path().join("platform.toml");
    let policy_path = dir.path().join("config.toml");
    std::fs::write(&platform_path, PLATFORM).expect("write platform.toml");
    std::fs::write(&policy_path, POLICY).expect("write config.toml");
    (dir, policy_path, platform_path)
}

#[test]
fn loop_keeps_running_without_a_bmc_and_stops_on_signal() {
    let (_dir, policy_path, platform_path) = fixture();
    let loaded = LoadedConfig::load(&policy_path, &platform_path).expect("config must load");

    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        flag.store(true, Ordering::Relaxed);
    });

    let started = Instant::now();
    Engine::new(loaded, false, shutdown)
        .run()
        .expect("a dead BMC must not abort the loop");
    let elapsed = started.elapsed();

    trigger.join().expect("trigger thread");
    assert!(
        elapsed < Duration::from_secs(5),
        "shutdown took {elapsed:?}, the loop did not react to the flag"
    );
}

#[test]
fn already_requested_shutdown_returns_immediately() {
    let (_dir, policy_path, platform_path) = fixture();
    let loaded = LoadedConfig::load(&policy_path, &platform_path).expect("config must load");

    let started = Instant::now();
    Engine::new(loaded, true, Arc::new(AtomicBool::new(true)))
        .run()
        .expect("shutdown path must succeed");
    assert!(started.elapsed() < Duration::from_secs(5));
}
