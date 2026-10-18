//! Business logic (`config.toml`).
//!
//! This module is pure data plus validation. It talks about *semantic* names
//! only (`CPU_Tctl`, `CPU_FAN`); resolving those to raw addresses is the job of
//! [`crate::config::platform`].

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use crate::sensors::Source;
use crate::util::name::normalize;

use super::{PlatformConfig, DUTY_MAX, DUTY_MIN};

/// Contents of `config.toml`, with every zone fully resolved.
#[derive(Debug, Clone)]
pub struct FanConfig {
    /// Global limits, timings and inherited defaults.
    pub global: GlobalSettings,
    /// Fan zones; every zone owns one curve and one or more pins.
    pub zones: Vec<ZoneConfig>,
    /// Cross-zone coupling rules ("pin A must never be slower than pin B").
    pub follows: Vec<FollowRule>,
}

/// Daemon wide settings, and the defaults every zone inherits.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalSettings {
    /// Seconds between two evaluation cycles.
    pub interval_seconds: u64,
    /// Hard floor applied to every pin; keeps fans spinning.
    pub min_duty_cycle: u8,
    /// Hard ceiling applied to every pin.
    pub max_duty_cycle: u8,
    /// Duty cycle used when *nothing* can be read - the blind value.
    ///
    /// Not the same question as `default_duty_cycle`, which means "the readings
    /// are fine and the machine is cool". Blind is not cool: if the BMC stops
    /// answering, the fans must not settle at the quietest setting, so this
    /// value is validated to be at least as fast as every zone's standby.
    pub fallback_duty_cycle: u8,
    /// Deadline for a single `ipmitool` invocation.
    #[serde(default = "super::defaults::command_timeout_seconds")]
    pub command_timeout_seconds: u64,
    /// Standby duty cycle inherited by zones that do not set their own: used
    /// below the first threshold, and written back on shutdown.
    pub default_duty_cycle: u8,
    /// Hysteresis in degrees Celsius inherited by zones that do not set their own.
    #[serde(default = "super::defaults::debounce_celsius")]
    pub debounce_celsius: f64,
    /// Aggregation inherited by zones that do not set their own.
    #[serde(default = "super::defaults::aggregation")]
    pub aggregation: Aggregation,
}

/// `config.toml` exactly as written.
///
/// A zone may leave `aggregation`, `default_duty_cycle` and `debounce_celsius`
/// out and inherit them from `[global]`; [`FanConfig::from_raw`] folds the
/// inheritance in once, so the rest of the crate only ever sees concrete
/// values and never has to decide what "unset" means.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawFanConfig {
    /// Daemon wide settings and inherited defaults.
    pub global: GlobalSettings,
    /// Fan zones, before inheritance.
    #[serde(default)]
    pub zones: Vec<RawZone>,
    /// Cross-zone coupling rules.
    #[serde(default)]
    pub follows: Vec<FollowRule>,
}

/// One zone as written; `None` means "inherit from `[global]`".
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawZone {
    /// Unique name, used in logs.
    pub name: String,
    /// Semantic sensor names; `"GPU"` is reserved for the NVIDIA readings.
    pub monitor_sources: Vec<String>,
    /// How the source temperatures are combined.
    #[serde(default)]
    pub aggregation: Option<Aggregation>,
    /// Pins driven by this zone.
    pub target_pins: Vec<String>,
    /// Duty cycle while the temperature stays below the first threshold.
    #[serde(default)]
    pub default_duty_cycle: Option<u8>,
    /// Single sided hysteresis in degrees Celsius, applied on the way *down*.
    #[serde(default)]
    pub debounce_celsius: Option<f64>,
    /// Curve breakpoints, must be sorted by ascending temperature.
    #[serde(default)]
    pub thresholds: Vec<Threshold>,
}

/// One independently regulated group of fans, with defaults already folded in.
#[derive(Debug, Clone)]
pub struct ZoneConfig {
    /// Unique name, used in logs.
    pub name: String,
    /// Sources already parsed from `monitor_sources`. Parsed once at load time
    /// so that the control loop never turns a string into a source again.
    pub sources: Vec<Source>,
    /// How the source temperatures are combined.
    pub aggregation: Aggregation,
    /// Pins driven by this zone.
    pub target_pins: Vec<String>,
    /// Duty cycle while the temperature stays below the first threshold.
    pub default_duty_cycle: u8,
    /// Single sided hysteresis in degrees Celsius, applied on the way *down*.
    pub debounce_celsius: f64,
    /// Curve breakpoints, must be sorted by ascending temperature.
    pub thresholds: Vec<Threshold>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Aggregation {
    /// Highest reading of all sources - the safe default.
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
    /// Folds `[global]` defaults into every zone that left them out.
    pub fn from_raw(raw: RawFanConfig) -> Self {
        let RawFanConfig {
            global,
            zones,
            follows,
        } = raw;

        let zones = zones
            .into_iter()
            .map(|zone| ZoneConfig {
                name: zone.name,
                sources: zone
                    .monitor_sources
                    .iter()
                    .map(|raw| Source::parse(raw))
                    .collect(),
                aggregation: zone.aggregation.unwrap_or(global.aggregation),
                target_pins: zone.target_pins,
                default_duty_cycle: zone.default_duty_cycle.unwrap_or(global.default_duty_cycle),
                debounce_celsius: zone.debounce_celsius.unwrap_or(global.debounce_celsius),
                thresholds: zone.thresholds,
            })
            .collect();

        Self {
            global,
            zones,
            follows,
        }
    }

    /// Rewrites every reference to the spelling declared in `platform.toml`.
    ///
    /// Names are matched case-insensitively, so `cpu_fan` in `config.toml` and
    /// `CPU_FAN` in `platform.toml` are the same pin. Doing the rewriting once,
    /// here, means the control and actuator layers can keep comparing exactly
    /// instead of each re-implementing the rule. Unknown references are left
    /// untouched for [`FanConfig::validate`] to report.
    pub fn resolve_names(&mut self, platform: &PlatformConfig) {
        for zone in &mut self.zones {
            for source in &mut zone.sources {
                if let Source::Ipmi(name) = source {
                    if let Some(declared) = platform.find_sensor(name) {
                        *name = declared.to_string();
                    }
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
        self.validate_fallback(&mut problems);
        self.validate_follows(platform, &mut problems);
        problems
    }

    /// `fallback_duty_cycle` must not be slower than the loudest standby.
    ///
    /// Both values answer "what do we do without a temperature", but they
    /// answer different questions: standby means "measured, and it is cool",
    /// fallback means "we cannot measure anything at all". Losing every sensor
    /// must never be quieter than a healthy idle, otherwise a dead BMC would
    /// silently become the quietest state the machine has.
    fn validate_fallback(&self, problems: &mut Vec<String>) {
        let loudest_standby = self
            .zones
            .iter()
            .map(|zone| zone.default_duty_cycle)
            .max()
            .unwrap_or(0);
        if self.global.fallback_duty_cycle < loudest_standby {
            problems.push(format!(
                "[global].fallback_duty_cycle ({}) must not be lower than the standby duty cycle \
                 of the loudest zone ({}): with no readings at all the fans must not spin slower \
                 than they do at room temperature",
                self.global.fallback_duty_cycle, loudest_standby
            ));
        }
    }

    fn validate_global(&self, problems: &mut Vec<String>) {
        let global = &self.global;
        if global.interval_seconds == 0 {
            problems.push("[global].interval_seconds must be at least 1".to_string());
        }
        if global.command_timeout_seconds == 0 {
            problems.push("[global].command_timeout_seconds must be at least 1".to_string());
        }
        if global.min_duty_cycle < DUTY_MIN {
            problems.push(format!(
                "[global].min_duty_cycle must be at least {DUTY_MIN} (fans must never stop)"
            ));
        }
        if global.max_duty_cycle > DUTY_MAX
            || global.min_duty_cycle > DUTY_MAX
            || global.fallback_duty_cycle > DUTY_MAX
        {
            problems.push(format!(
                "pwm values must stay within {DUTY_MIN}..={DUTY_MAX}"
            ));
        }
        if global.min_duty_cycle > global.max_duty_cycle {
            problems.push(format!(
                "[global].min_duty_cycle ({}) must not exceed max_duty_cycle ({})",
                global.min_duty_cycle, global.max_duty_cycle
            ));
        }
        if global.fallback_duty_cycle < global.min_duty_cycle
            || global.fallback_duty_cycle > global.max_duty_cycle
        {
            problems.push(format!(
                "[global].fallback_duty_cycle ({}) must be within min_duty_cycle..=max_duty_cycle ({}..={})",
                global.fallback_duty_cycle, global.min_duty_cycle, global.max_duty_cycle
            ));
        }
        if !(DUTY_MIN..=DUTY_MAX).contains(&global.default_duty_cycle) {
            problems.push(format!(
                "[global].default_duty_cycle must be within {DUTY_MIN}..={DUTY_MAX}"
            ));
        } else if global.default_duty_cycle < global.min_duty_cycle
            || global.default_duty_cycle > global.max_duty_cycle
        {
            problems.push(format!(
                "[global].default_duty_cycle ({}) must be within min_duty_cycle..=max_duty_cycle \
                 ({}..={})",
                global.default_duty_cycle, global.min_duty_cycle, global.max_duty_cycle
            ));
        }
        if !global.debounce_celsius.is_finite() || global.debounce_celsius < 0.0 {
            problems.push("[global].debounce_celsius must be a finite number >= 0".to_string());
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

            if zone.sources.is_empty() {
                problems.push(format!("{label}: monitor_sources must not be empty"));
            }
            for source in &zone.sources {
                if let Source::Ipmi(key) = source {
                    if key.is_empty() {
                        problems.push(format!("{label}: a monitor source name is empty"));
                    } else if !platform.sensors.contains_key(key) {
                        problems.push(format!(
                            "{label}: sensor `{key}` is not defined in platform.toml [sensors]"
                        ));
                    }
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

            if !(DUTY_MIN..=DUTY_MAX).contains(&zone.default_duty_cycle) {
                problems.push(format!(
                    "{label}: default_duty_cycle ({}) must be within {DUTY_MIN}..={DUTY_MAX}",
                    zone.default_duty_cycle
                ));
            }
            if zone.debounce_celsius < 0.0 {
                problems.push(format!(
                    "{label}: debounce_celsius ({}) must not be negative",
                    zone.debounce_celsius
                ));
            }
            if zone.thresholds.is_empty() {
                problems.push(format!(
                    "{label}: at least one [[zones.thresholds]] entry is required"
                ));
            }
            // The standby step sits below the first threshold, so it must also
            // be the slowest one: otherwise crossing that threshold would make
            // the fans *quieter* exactly when the machine gets hotter.
            if let Some(first) = zone.thresholds.first() {
                if zone.default_duty_cycle > first.duty_cycle {
                    problems.push(format!(
                        "{label}: default_duty_cycle ({}) must not exceed the duty_cycle of the \
                         first threshold ({}) - the curve would slow the fans down as it heats up",
                        zone.default_duty_cycle, first.duty_cycle
                    ));
                }
            }

            let mut previous: Option<f64> = None;
            for threshold in &zone.thresholds {
                if !(DUTY_MIN..=DUTY_MAX).contains(&threshold.duty_cycle) {
                    problems.push(format!(
                        "{label}: duty_cycle ({}) must be within {DUTY_MIN}..={DUTY_MAX}",
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
                        if zone.debounce_celsius >= gap {
                            problems.push(format!(
                                "{label}: debounce_celsius ({}) must be smaller than the gap between \
                                 {} C and {} C ({}) - otherwise the step can never be left",
                                zone.debounce_celsius, previous_temp, threshold.temperature, gap
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
