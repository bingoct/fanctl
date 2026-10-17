//! Business logic (`config.toml`).
//!
//! This module is pure data plus validation. It talks about *semantic* names
//! only (`CPU_Tctl`, `CPU_FAN`); resolving those to raw addresses is the job of
//! [`crate::config::platform`].

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use crate::name::normalize;
use crate::sensors::Source;

use super::PlatformConfig;

/// Contents of `config.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanConfig {
    /// Global limits and timings.
    pub global: GlobalSettings,
    /// Fan zones; every zone owns one curve and one or more pins.
    #[serde(default)]
    pub zones: Vec<ZoneConfig>,
    /// Cross-zone coupling rules ("pin A must never be slower than pin B").
    #[serde(default)]
    pub follows: Vec<FollowRule>,
}

/// Daemon wide settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalSettings {
    /// Seconds between two evaluation cycles.
    pub interval_seconds: u64,
    /// Hard floor applied to every pin; keeps fans spinning.
    pub min_pwm: u8,
    /// Hard ceiling applied to every pin.
    pub max_pwm: u8,
    /// Duty cycle used when no sensor can be read at all.
    pub fallback_pwm: u8,
    /// Deadline for a single `ipmitool` invocation.
    #[serde(default = "default_command_timeout")]
    pub command_timeout_seconds: u64,
}

fn default_command_timeout() -> u64 {
    5
}

/// One independently regulated group of fans.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZoneConfig {
    /// Unique name, used in logs.
    pub name: String,
    /// Semantic sensor names; `"GPU"` is reserved for the NVIDIA readings.
    pub monitor_sources: Vec<String>,
    /// How the source temperatures are combined.
    #[serde(default)]
    pub aggregation: Aggregation,
    /// Pins driven by this zone.
    pub target_pins: Vec<String>,
    /// Duty cycle while the temperature stays below the first threshold.
    pub default_duty_cycle: u8,
    /// Single sided hysteresis in degrees Celsius, applied on the way *down*.
    #[serde(default = "default_debounce")]
    pub debounce_temp: f64,
    /// Curve breakpoints, must be sorted by ascending temperature.
    #[serde(default)]
    pub thresholds: Vec<Threshold>,
}

fn default_debounce() -> f64 {
    2.0
}

/// A single curve breakpoint.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Threshold {
    /// Temperature in degrees Celsius at which this step engages.
    pub temperature: f64,
    /// Duty cycle used at or above `temperature`.
    pub duty_cycle: u8,
}

/// How several sensor readings become one temperature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aggregation {
    /// Highest reading of all sources - the safe default.
    #[default]
    Max,
    /// Arithmetic mean of the available readings.
    Mean,
}

impl Aggregation {
    /// Lowercase name, as used in `config.toml` and in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Aggregation::Max => "max",
            Aggregation::Mean => "mean",
        }
    }
}

/// "The pin must never be slower than another pin" rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowRule {
    /// Pin that gets raised.
    pub pin: String,
    /// Pin whose duty cycle becomes the floor for `pin`.
    pub min_of: String,
}

impl FanConfig {
    /// Rewrites every reference to the spelling declared in `platform.toml`.
    ///
    /// Names are matched case-insensitively, so `cpu_fan` in `config.toml` and
    /// `CPU_FAN` in `platform.toml` are the same pin. Doing the rewriting once,
    /// here, means the control and actuator layers can keep comparing exactly
    /// instead of each re-implementing the rule. Unknown references are left
    /// untouched for [`FanConfig::validate`] to report.
    pub fn resolve_names(&mut self, platform: &PlatformConfig) {
        for zone in &mut self.zones {
            for source in &mut zone.monitor_sources {
                if let Some(declared) = platform.find_sensor(source) {
                    *source = declared.to_string();
                }
            }
            for pin in &mut zone.target_pins {
                if let Some(declared) = platform.find_pin(pin) {
                    *pin = declared.to_string();
                }
            }
        }
        for rule in &mut self.follows {
            if let Some(declared) = platform.find_pin(&rule.pin) {
                rule.pin = declared.to_string();
            }
            if let Some(declared) = platform.find_pin(&rule.min_of) {
                rule.min_of = declared.to_string();
            }
        }
    }

    /// Returns every problem found in the file; an empty vector means "valid".
    ///
    /// Validation is exhaustive instead of fail-fast so that `fanctl check`
    /// prints the whole list of mistakes at once.
    pub fn validate(&self, platform: &PlatformConfig) -> Vec<String> {
        let mut problems = Vec::new();
        self.validate_global(&mut problems);
        self.validate_zones(platform, &mut problems);
        self.validate_follows(platform, &mut problems);
        problems
    }

    fn validate_global(&self, problems: &mut Vec<String>) {
        let global = &self.global;
        if global.interval_seconds == 0 {
            problems.push("[global].interval_seconds must be at least 1".to_string());
        }
        if global.command_timeout_seconds == 0 {
            problems.push("[global].command_timeout_seconds must be at least 1".to_string());
        }
        if global.min_pwm == 0 {
            problems.push("[global].min_pwm must be at least 1 (fans must never stop)".to_string());
        }
        if global.max_pwm > 100 || global.min_pwm > 100 || global.fallback_pwm > 100 {
            problems.push("pwm values must stay within 0..=100".to_string());
        }
        if global.min_pwm > global.max_pwm {
            problems.push(format!(
                "[global].min_pwm ({}) must not exceed max_pwm ({})",
                global.min_pwm, global.max_pwm
            ));
        }
        if global.fallback_pwm < global.min_pwm || global.fallback_pwm > global.max_pwm {
            problems.push(format!(
                "[global].fallback_pwm ({}) must be within min_pwm..=max_pwm ({}..={})",
                global.fallback_pwm, global.min_pwm, global.max_pwm
            ));
        }
    }

    fn validate_zones(&self, platform: &PlatformConfig, problems: &mut Vec<String>) {
        if self.zones.is_empty() {
            problems.push("no [[zones]] defined: there is nothing to control".to_string());
        }

        let mut seen_names = HashSet::new();
        for zone in &self.zones {
            let label = format!("zone `{}`", zone.name);
            if zone.name.trim().is_empty() {
                problems.push("a zone has an empty name".to_string());
            } else if !seen_names.insert(normalize(&zone.name)) {
                problems.push(format!("duplicate zone name `{}`", zone.name));
            }

            if zone.monitor_sources.is_empty() {
                problems.push(format!("{label}: monitor_sources must not be empty"));
            }
            for raw in &zone.monitor_sources {
                match Source::parse(raw) {
                    Err(err) => problems.push(format!("{label}: {err}")),
                    Ok(Source::Ipmi(key)) => {
                        if !platform.sensors.contains_key(&key) {
                            problems.push(format!(
                                "{label}: sensor `{key}` is not defined in platform.toml [sensors]"
                            ));
                        }
                    }
                    Ok(Source::Gpu) => {}
                }
            }

            if zone.target_pins.is_empty() {
                problems.push(format!("{label}: target_pins must not be empty"));
            }
            for pin in &zone.target_pins {
                if !platform.pins.contains_key(pin) {
                    problems.push(format!(
                        "{label}: pin `{pin}` is not defined in platform.toml [pins]"
                    ));
                }
            }

            if zone.default_duty_cycle == 0 || zone.default_duty_cycle > 100 {
                problems.push(format!(
                    "{label}: default_duty_cycle ({}) must be within 1..=100",
                    zone.default_duty_cycle
                ));
            }
            if zone.debounce_temp < 0.0 {
                problems.push(format!(
                    "{label}: debounce_temp ({}) must not be negative",
                    zone.debounce_temp
                ));
            }
            if zone.thresholds.is_empty() {
                problems.push(format!(
                    "{label}: at least one [[zones.thresholds]] entry is required"
                ));
            }

            let mut previous: Option<f64> = None;
            for threshold in &zone.thresholds {
                if threshold.duty_cycle == 0 || threshold.duty_cycle > 100 {
                    problems.push(format!(
                        "{label}: duty_cycle ({}) must be within 1..=100",
                        threshold.duty_cycle
                    ));
                }
                if let Some(previous_temp) = previous {
                    if threshold.temperature <= previous_temp {
                        problems.push(format!(
                            "{label}: thresholds must be sorted by ascending temperature \
                             ({} C follows {} C)",
                            threshold.temperature, previous_temp
                        ));
                    } else {
                        let gap = threshold.temperature - previous_temp;
                        if zone.debounce_temp >= gap {
                            problems.push(format!(
                                "{label}: debounce_temp ({}) must be smaller than the gap between \
                                 {} C and {} C ({}) - otherwise the step can never be left",
                                zone.debounce_temp, previous_temp, threshold.temperature, gap
                            ));
                        }
                    }
                }
                previous = Some(threshold.temperature);
            }
        }
    }

    fn validate_follows(&self, platform: &PlatformConfig, problems: &mut Vec<String>) {
        let controlled: HashSet<&str> = self
            .zones
            .iter()
            .flat_map(|zone| zone.target_pins.iter().map(String::as_str))
            .collect();

        for rule in &self.follows {
            if !platform.pins.contains_key(&rule.pin) {
                problems.push(format!(
                    "follow rule: pin `{}` is not defined in platform.toml [pins]",
                    rule.pin
                ));
            }
            if !platform.pins.contains_key(&rule.min_of) {
                problems.push(format!(
                    "follow rule: pin `{}` is not defined in platform.toml [pins]",
                    rule.min_of
                ));
            }
            if rule.pin == rule.min_of {
                problems.push(format!(
                    "follow rule: pin `{}` cannot follow itself",
                    rule.pin
                ));
            }
            if !controlled.contains(rule.pin.as_str()) {
                problems.push(format!(
                    "follow rule: pin `{}` is not driven by any zone, the rule has no effect",
                    rule.pin
                ));
            }
            if !controlled.contains(rule.min_of.as_str()) {
                problems.push(format!(
                    "follow rule: pin `{}` is not driven by any zone, the floor is unknown",
                    rule.min_of
                ));
            }
        }

        validate_follow_cycles(&self.follows, problems);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visit {
    InProgress,
    Done,
}

/// Rejects chains like `A -> B -> A`, which would make the merge order
/// dependent instead of deterministic.
fn validate_follow_cycles(follows: &[FollowRule], problems: &mut Vec<String>) {
    let mut edges: HashMap<&str, Vec<&str>> = HashMap::new();
    for rule in follows {
        edges
            .entry(rule.pin.as_str())
            .or_default()
            .push(rule.min_of.as_str());
    }

    let mut state: HashMap<&str, Visit> = HashMap::new();
    let mut reported: HashSet<String> = HashSet::new();
    for rule in follows {
        visit_follow_chain(
            rule.pin.as_str(),
            &edges,
            &mut state,
            problems,
            &mut reported,
        );
    }
}

fn visit_follow_chain<'a>(
    node: &'a str,
    edges: &HashMap<&'a str, Vec<&'a str>>,
    state: &mut HashMap<&'a str, Visit>,
    problems: &mut Vec<String>,
    reported: &mut HashSet<String>,
) {
    match state.get(node) {
        Some(Visit::Done) => return,
        Some(Visit::InProgress) => {
            let message = format!("follow rules contain a cycle through pin `{node}`");
            if reported.insert(message.clone()) {
                problems.push(message);
            }
            return;
        }
        None => {}
    }

    state.insert(node, Visit::InProgress);
    if let Some(successors) = edges.get(node) {
        for successor in successors.iter().copied() {
            visit_follow_chain(successor, edges, state, problems, reported);
        }
    }
    state.insert(node, Visit::Done);
}
