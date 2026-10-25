//! Validation rules that guard against unsafe or meaningless configuration.
//!
//! The shipped examples are parsed through `include_str!` so that a broken
//! example fails the test suite instead of shipping.

use std::path::PathBuf;

use fanctl::config::{Aggregation, LoadedConfig};
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

/// Loads `policy` against the example platform, panicking on any error.
fn load(policy: &str) -> LoadedConfig {
    let (_dir, policy_path, platform_path) = fixture(PLATFORM, policy);
    LoadedConfig::load(&policy_path, &platform_path).expect("configuration must load")
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
min_duty_cycle = 25
max_duty_cycle = 100
fallback_duty_cycle = 60
default_duty_cycle = 30

[[zones]]
name = "cpu_zone"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]
default_duty_cycle = 30
debounce_celsius = {debounce}
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
    assert!(
        error.contains("debounce_celsius"),
        "unexpected error: {error}"
    );
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
min_duty_cycle = 25
max_duty_cycle = 100
fallback_duty_cycle = 60
default_duty_cycle = 30

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
        .replace("min_duty_cycle = 25", "min_duty_cycle = 100")
        .replace("max_duty_cycle = 100", "max_duty_cycle = 50");
    let error = load_error(&policy);
    assert!(
        error.contains("min_duty_cycle"),
        "unexpected error: {error}"
    );
}

#[test]
fn missing_required_field_fails_to_parse() {
    let policy = policy_with(ASCENDING, 2.0).replace("interval_seconds = 10", "");
    let error = load_error(&policy);
    assert!(error.contains("cannot parse"), "unexpected error: {error}");
}

#[test]
fn unknown_keys_fail_to_parse() {
    let policy = policy_with(ASCENDING, 2.0)
        .replace("min_duty_cycle = 25", "min_duty_cycle = 25\nmin_pwn = 25");
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
    assert_eq!(loaded.policy.zones[0].sources[0].label(), "CPU_Tctl");
    assert_eq!(loaded.policy.zones[0].target_pins[0], "CPU_FAN");
}

#[test]
fn follow_rules_are_case_insensitive_too() {
    let policy = format!(
        r#"
[global]
interval_seconds = 10
min_duty_cycle = 25
max_duty_cycle = 100
fallback_duty_cycle = 60
default_duty_cycle = 30

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
        r#""SYS_Air_Outlet" = { sdr = "SYS_Air_Outlet", id = "0x31" }"#,
        "\"SYS_Air_Outlet\" = { sdr = \"SYS_Air_Outlet\", id = \"0x31\" }\n\
         \"GPU\" = { sdr = \"GPU_Temp\", id = \"0x02\" }",
    );
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(error.contains("reserved"), "unexpected error: {error}");
}

#[test]
fn a_board_sensor_called_gpu_with_an_index_is_rejected() {
    let platform = PLATFORM.replace(
        r#""SYS_Air_Outlet" = { sdr = "SYS_Air_Outlet", id = "0x31" }"#,
        "\"SYS_Air_Outlet\" = { sdr = \"SYS_Air_Outlet\", id = \"0x31\" }\n\
         \"GPU0\" = { sdr = \"GPU_Temp\", id = \"0x02\" }",
    );
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(error.contains("reserved"), "unexpected error: {error}");
}

#[test]
fn indexed_gpu_sources_are_accepted() {
    // The index is resolved against NVML at runtime, so loading must accept it
    // without a driver present; a wrong index surfaces as "no reading" later.
    let policy = policy_with(ASCENDING, 2.0).replace(
        r#"monitor_sources = ["CPU_Tctl"]"#,
        r#"monitor_sources = ["CPU_Tctl", "gpu1", "GPU2"]"#,
    );
    let (_dir, policy_path, platform_path) = fixture(PLATFORM, &policy);
    LoadedConfig::load(&policy_path, &platform_path).expect("`gpu1`/`GPU2` must be accepted");
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

#[test]
fn a_zone_inherits_the_global_policy() {
    let policy = r#"
[global]
interval_seconds = 10
min_duty_cycle = 25
max_duty_cycle = 100
fallback_duty_cycle = 60
default_duty_cycle = 35
debounce_celsius = 3
aggregation = "mean"

[[zones]]
name = "cpu_zone"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]

[[zones.thresholds]]
temperature = 55
duty_cycle = 40

[[zones.thresholds]]
temperature = 70
duty_cycle = 60
"#;
    let (_dir, policy_path, platform_path) = fixture(PLATFORM, policy);
    let loaded = LoadedConfig::load(&policy_path, &platform_path).expect("valid");
    let zone = &loaded.policy.zones[0];
    assert_eq!(zone.default_duty_cycle, 35);
    assert_eq!(zone.debounce_celsius, 3.0);
    assert_eq!(zone.aggregation, Aggregation::Mean);
}

#[test]
fn a_zone_can_override_the_global_policy() {
    let policy = r#"
[global]
interval_seconds = 10
min_duty_cycle = 25
max_duty_cycle = 100
fallback_duty_cycle = 60
default_duty_cycle = 30
debounce_celsius = 2
aggregation = "max"

[[zones]]
name = "cpu_zone"
monitor_sources = ["CPU_Tctl"]
target_pins = ["CPU_FAN"]
default_duty_cycle = 35
debounce_celsius = 4
aggregation = "mean"

[[zones.thresholds]]
temperature = 55
duty_cycle = 40

[[zones.thresholds]]
temperature = 70
duty_cycle = 60
"#;
    let (_dir, policy_path, platform_path) = fixture(PLATFORM, policy);
    let loaded = LoadedConfig::load(&policy_path, &platform_path).expect("valid");
    let zone = &loaded.policy.zones[0];
    assert_eq!(zone.default_duty_cycle, 35);
    assert_eq!(zone.debounce_celsius, 4.0);
    assert_eq!(zone.aggregation, Aggregation::Mean);
}

#[test]
fn a_standby_step_above_the_first_threshold_is_rejected() {
    // 50 % below 55 C, then 40 % at 55 C: hotter would mean quieter.
    let policy =
        policy_with(ASCENDING, 2.0).replace("default_duty_cycle = 30", "default_duty_cycle = 50");
    let error = load_error(&policy);
    assert!(
        error.contains("must not exceed"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_global_standby_step_outside_the_window_is_rejected() {
    let policy = policy_with(ASCENDING, 2.0).replace("min_duty_cycle = 25", "min_duty_cycle = 40");
    let error = load_error(&policy);
    assert!(
        error.contains("default_duty_cycle (30)"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_blind_fallback_must_not_be_quieter_than_standby() {
    // `fallback_duty_cycle` belongs to "no readings", `default_duty_cycle` to
    // "readings are fine and it is cool". If losing the sensor produced the
    // quietest state the machine has, a dead BMC would be silent and invisible.
    // The rule is applied per zone, so the message names the zone.
    let policy =
        policy_with(ASCENDING, 2.0).replace("fallback_duty_cycle = 60", "fallback_duty_cycle = 25");
    let error = load_error(&policy);
    assert!(
        error.contains("zone `cpu_zone`") && error.contains("fallback_duty_cycle (25)"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_zone_may_override_the_blind_fallback() {
    // The pump case: 100 % standby and 100 % blind, while every other zone keeps
    // the global mid fallback. This is exactly what the old global rule rejected.
    let policy = format!(
        "{}{}",
        policy_with(ASCENDING, 2.0),
        r#"
[[zones]]
name = "pump"
monitor_sources = ["CPU_Tctl"]
target_pins = ["SYS_FAN_2"]
default_duty_cycle = 100
fallback_duty_cycle = 100
fallback_after_cycles = 1

[[zones.thresholds]]
temperature = 0
duty_cycle = 100
"#
    );
    let loaded = load(&policy);
    assert_eq!(loaded.policy.zones.len(), 2);
    assert_eq!(loaded.policy.zones[0].fallback_duty_cycle, 60);
    assert_eq!(loaded.policy.zones[0].fallback_after_cycles, 2);
    assert_eq!(loaded.policy.zones[1].fallback_duty_cycle, 100);
    assert_eq!(loaded.policy.zones[1].fallback_after_cycles, 1);
}

#[test]
fn a_zero_grace_period_is_rejected() {
    let policy =
        policy_with(ASCENDING, 2.0).replace("[global]", "[global]\nfallback_after_cycles = 0");
    let error = load_error(&policy);
    assert!(
        error.contains("fallback_after_cycles must be at least 1"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_empty_monitor_source_is_rejected() {
    let policy = policy_with(ASCENDING, 2.0).replace(
        r#"monitor_sources = ["CPU_Tctl"]"#,
        r#"monitor_sources = ["CPU_Tctl", "  "]"#,
    );
    let error = load_error(&policy);
    assert!(
        error.contains("monitor source name is empty"),
        "unexpected error: {error}"
    );
}

#[test]
fn an_omitted_timeout_gets_the_documented_default() {
    // `policy_with` leaves `command_timeout_seconds` out on purpose: this test
    // is the machine-checked link between `config/defaults.rs` and README §3.2.
    let (_dir, policy_path, platform_path) = fixture(PLATFORM, &policy_with(ASCENDING, 2.0));
    let loaded = LoadedConfig::load(&policy_path, &platform_path).expect("valid");
    assert_eq!(loaded.policy.global.command_timeout_seconds, 10);
    assert_eq!(loaded.policy.global.debounce_celsius, 2.0);
    // The fourth default, and the only one that used to live outside the file.
    assert_eq!(loaded.policy.global.aggregation, Aggregation::Max);
}

#[test]
fn a_sensor_number_that_is_not_a_byte_is_rejected() {
    let platform = PLATFORM.replace(r#"id = "0x20""#, r#"id = "0x100""#);
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(error.contains("single byte"), "unexpected error: {error}");
}

#[test]
fn two_sensors_cannot_share_one_number() {
    // Reading the same number twice would silently give one zone another
    // zone's temperature; there is no configuration where that is intended.
    let platform = PLATFORM.replace(r#"id = "0x20""#, r#"id = "0x01""#);
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(
        error.contains("both use sensor number"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_tachometer_without_a_scale_factor_is_rejected() {
    // Without the factor the raw count is printed as if it were RPM: 7 instead
    // of 966. That looks like a dead fan, not like a missing setting.
    let platform = PLATFORM.replace("\nrpm_per_count = 138", "");
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(
        error.contains("rpm_per_count is required"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_tachometer_scale_factor_must_be_positive() {
    let platform = PLATFORM.replace("rpm_per_count = 138", "rpm_per_count = 0");
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(
        error.contains("must be a positive number"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_tachometer_on_an_unknown_pin_is_rejected() {
    let platform = PLATFORM.replace(
        r#""CPU_FAN"   = { sdr = "CPU_FAN",   id = "0x60" }"#,
        "\"CPU_FAN\"   = { sdr = \"CPU_FAN\",   id = \"0x60\" }\n\
         \"SYS_FAN_9\" = { sdr = \"SYS_FAN_9\", id = \"0x69\" }",
    );
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(
        error.contains("is not declared in [pins]"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_tachometer_must_not_reuse_a_temperature_sensor_number() {
    // 0x01 is CPU_Tctl: reading it as a fan speed would print a temperature in
    // the fan column, which no reader would question.
    let platform = PLATFORM.replace(
        r#""CPU_FAN"   = { sdr = "CPU_FAN",   id = "0x60" }"#,
        r#""CPU_FAN"   = { sdr = "CPU_FAN",   id = "0x01" }"#,
    );
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(
        error.contains("already used by [sensors]"),
        "unexpected error: {error}"
    );
}

#[test]
fn the_shipped_platform_declares_a_tachometer_per_pin() {
    let loaded = load(POLICY);
    let platform = &loaded.platform;
    assert_eq!(platform.tachometers.len(), 3);
    assert!(platform.info.rpm_per_count.is_some());
}

#[test]
fn a_platform_without_tachometers_still_loads() {
    // Speeds are optional: a board that exposes none must not fail validation,
    // and must not be forced to state a scale factor it cannot use.
    let platform = PLATFORM
        .replace("\nrpm_per_count = 138", "")
        .replace("[tachometers]", "# [tachometers] removed")
        .replace(r#""CPU_FAN"   = { sdr = "CPU_FAN",   id = "0x60" }"#, "")
        .replace(r#""SYS_FAN_1" = { sdr = "SYS_FAN_1", id = "0x61" }"#, "")
        .replace(r#""SYS_FAN_2" = { sdr = "SYS_FAN_2", id = "0x62" }"#, "");
    let error = load_error_with_platform(&platform, &policy_with(ASCENDING, 2.0));
    assert!(error.is_empty(), "unexpected error: {error}");
}
