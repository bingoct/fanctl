//! Validation rules that guard against unsafe or meaningless configuration.
//!
//! The shipped examples are parsed through `include_str!` so that a broken
//! example fails the test suite instead of shipping.

use std::path::PathBuf;

use fanctl::config::LoadedConfig;
use tempfile::TempDir;

const PLATFORM: &str = include_str!("../examples/platform.toml");
const POLICY: &str = include_str!("../examples/config.toml");

/// Writes both files into a temporary directory and returns
/// `(dir, policy_path, platform_path)`. The `TempDir` must stay alive.
fn fixture(platform: &str, policy: &str) -> (TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let platform_path = dir.path().join("platform.toml");
    let policy_path = dir.path().join("config.toml");
    std::fs::write(&platform_path, platform).expect("write platform.toml");
    std::fs::write(&policy_path, policy).expect("write config.toml");
    (dir, policy_path, platform_path)
}

/// Loads `policy` against the example platform, returning the error text.
fn load_error(policy: &str) -> String {
    load_error_with_platform(PLATFORM, policy)
}

/// Loads `policy` against a custom platform, returning the error text.
fn load_error_with_platform(platform: &str, policy: &str) -> String {
    let (_dir, policy_path, platform_path) = fixture(platform, policy);
    match LoadedConfig::load(&policy_path, &platform_path) {
        Ok(_) => String::new(),
        Err(err) => err.to_string(),
    }
}

fn policy_with(thresholds: &str, debounce: f64) -> String {
    format!(
        r#"
[global]
interval_seconds = 10
min_pwm = 25
max_pwm = 100
fallback_pwm = 60

[[zones]]
name = "cpu_zone"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]
default_duty_cycle = 30
debounce_temp = {debounce}
{thresholds}
"#
    )
}

const ASCENDING: &str = r#"
[[zones.thresholds]]
temperature = 55
duty_cycle = 40

[[zones.thresholds]]
temperature = 70
duty_cycle = 60
"#;

#[test]
fn shipped_examples_are_valid() {
    let (_dir, policy_path, platform_path) = fixture(PLATFORM, POLICY);
    let loaded = LoadedConfig::load(&policy_path, &platform_path).expect("examples must load");
    assert_eq!(loaded.policy.zones.len(), 3);
    assert_eq!(loaded.policy.follows.len(), 2);
    assert_eq!(loaded.platform.pins.len(), 3);
    assert_eq!(loaded.platform.sensors.len(), 3);
}

#[test]
fn unsorted_thresholds_are_rejected() {
    let unsorted = r#"
[[zones.thresholds]]
temperature = 70
duty_cycle = 60

[[zones.thresholds]]
temperature = 55
duty_cycle = 40
"#;
    let error = load_error(&policy_with(unsorted, 2.0));
    assert!(error.contains("ascending"), "unexpected error: {error}");
}

#[test]
fn debounce_covering_a_whole_step_is_rejected() {
    let tight = r#"
[[zones.thresholds]]
temperature = 55
duty_cycle = 40

[[zones.thresholds]]
temperature = 56
duty_cycle = 60
"#;
    let error = load_error(&policy_with(tight, 2.0));
    assert!(error.contains("debounce_temp"), "unexpected error: {error}");
}

#[test]
fn unknown_sensor_is_rejected() {
    let policy = policy_with(ASCENDING, 2.0).replace(
        r#"monitor_sources = ["CPU_Tctl"]"#,
        r#"monitor_sources = ["CPU_Tctl", "CHASSIS_INTAKE"]"#,
    );
    let error = load_error(&policy);
    assert!(
        error.contains("not defined in platform.toml [sensors]"),
        "unexpected error: {error}"
    );
}

#[test]
fn unknown_pin_is_rejected() {
    let policy = policy_with(ASCENDING, 2.0).replace(
        r#"target_pins = ["CPU_FAN"]"#,
        r#"target_pins = ["SYS_FAN_9"]"#,
    );
    let error = load_error(&policy);
    assert!(
        error.contains("not defined in platform.toml [pins]"),
        "unexpected error: {error}"
    );
}

#[test]
fn follow_cycle_is_rejected() {
    let policy = format!(
        r#"
[global]
interval_seconds = 10
min_pwm = 25
max_pwm = 100
fallback_pwm = 60

[[zones]]
name = "cpu_zone"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]
default_duty_cycle = 30
{ASCENDING}

[[zones]]
name = "intake_zone"
monitor_sources = ["CPU_Tctl"]
target_pins = ["SYS_FAN_1"]
default_duty_cycle = 30
{ASCENDING}

[[follows]]
pin = "SYS_FAN_1"
min_of = "CPU_FAN"

[[follows]]
pin = "CPU_FAN"
min_of = "SYS_FAN_1"
"#
    );
    let error = load_error(&policy);
    assert!(error.contains("cycle"), "unexpected error: {error}");
}

#[test]
fn inverted_pwm_window_is_rejected() {
    let policy = policy_with(ASCENDING, 2.0)
        .replace("min_pwm = 25", "min_pwm = 100")
        .replace("max_pwm = 100", "max_pwm = 50");
    let error = load_error(&policy);
    assert!(error.contains("min_pwm"), "unexpected error: {error}");
}

#[test]
fn missing_required_field_fails_to_parse() {
    let policy = policy_with(ASCENDING, 2.0).replace("interval_seconds = 10", "");
    let error = load_error(&policy);
    assert!(error.contains("cannot parse"), "unexpected error: {error}");
}

#[test]
fn unknown_keys_fail_to_parse() {
    let policy = policy_with(ASCENDING, 2.0).replace("min_pwm = 25", "min_pwm = 25\nmin_pwn = 25");
    let error = load_error(&policy);
    assert!(error.contains("cannot parse"), "unexpected error: {error}");
}

#[test]
fn names_are_case_insensitive_and_resolved_to_the_declared_spelling() {
    let policy = policy_with(ASCENDING, 2.0)
        .replace(
            r#"monitor_sources = ["CPU_Tctl"]"#,
            r#"monitor_sources = ["cpu_tctl"]"#,
        )
        .replace(
            r#"target_pins = ["CPU_FAN"]"#,
            r#"target_pins = ["cpu_fan"]"#,
        );
    let (_dir, policy_path, platform_path) = fixture(PLATFORM, &policy);
    let loaded = LoadedConfig::load(&policy_path, &platform_path).expect("case must not matter");
    assert_eq!(loaded.policy.zones[0].monitor_sources[0], "CPU_Tctl");
    assert_eq!(loaded.policy.zones[0].target_pins[0], "CPU_FAN");
}

#[test]
fn follow_rules_are_case_insensitive_too() {
    let policy = format!(
        r#"
[global]
interval_seconds = 10
min_pwm = 25
max_pwm = 100
fallback_pwm = 60

[[zones]]
name = "cpu_zone"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]
default_duty_cycle = 30
{ASCENDING}

[[zones]]
name = "intake_zone"
monitor_sources = ["CPU_Tctl"]
target_pins = ["SYS_FAN_1"]
default_duty_cycle = 30
{ASCENDING}

[[follows]]
pin = "sys_fan_1"
min_of = "cpu_fan"
"#
    );
    let (_dir, policy_path, platform_path) = fixture(PLATFORM, &policy);
    let loaded = LoadedConfig::load(&policy_path, &platform_path).expect("case must not matter");
    assert_eq!(loaded.policy.follows[0].pin, "SYS_FAN_1");
    assert_eq!(loaded.policy.follows[0].min_of, "CPU_FAN");
}

#[test]
fn platform_names_that_only_differ_in_case_are_rejected() {
    let platform = PLATFORM.replace(
        r#""SYS_FAN_1" = "0x02""#,
        "\"SYS_FAN_1\" = \"0x02\"\n\"sys_fan_1\" = \"0x04\"",
    );
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(error.contains("same name"), "unexpected error: {error}");
}

#[test]
fn a_board_sensor_called_gpu_is_rejected() {
    let platform = PLATFORM.replace(
        r#""SYS_Air_Outlet" = "SYS_Air_Outlet""#,
        "\"SYS_Air_Outlet\" = \"SYS_Air_Outlet\"\n\"GPU\" = \"GPU_Temp\"",
    );
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(error.contains("reserved"), "unexpected error: {error}");
}

#[test]
fn the_gpu_source_is_case_insensitive() {
    let policy = policy_with(ASCENDING, 2.0).replace(
        r#"monitor_sources = ["CPU_Tctl"]"#,
        r#"monitor_sources = ["CPU_Tctl", "gpu"]"#,
    );
    let (_dir, policy_path, platform_path) = fixture(PLATFORM, &policy);
    LoadedConfig::load(&policy_path, &platform_path).expect("`gpu` must be accepted");
}
