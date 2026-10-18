//! Turning per-zone demands into one duty cycle per pin.
//!
//! Three rules, in this order:
//!
//! 1. **Conflict**: several zones asking for the same pin -> the highest wins.
//!    A zone must never be able to slow a fan another zone wants fast.
//! 2. **Follow**: `pin` is raised to at least `min_of` (fixed point until the
//!    map stops changing, so chains like `SYS_FAN_2 -> SYS_FAN_1 -> CPU_FAN`
//!    resolve regardless of declaration order).
//! 3. **Limits**: every value is clamped into `[min_duty_cycle, max_duty_cycle]`.

use std::collections::BTreeMap;

use crate::config::{FollowRule, ZoneConfig};

/// Pin -> duty cycle map under construction.
#[derive(Debug, Default, Clone)]
pub struct TargetMap {
    duties: BTreeMap<String, u8>,
}

impl TargetMap {
    /// An empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests `duty_cycle` for `pin`, keeping the higher value on conflict.
    pub fn raise(&mut self, pin: &str, duty_cycle: u8) {
        let entry = self.duties.entry(pin.to_string()).or_insert(duty_cycle);
        if *entry < duty_cycle {
            *entry = duty_cycle;
        }
    }

    /// Duty currently assigned to `pin`.
    pub fn get(&self, pin: &str) -> Option<u8> {
        self.duties.get(pin).copied()
    }

    /// Number of pins in the map.
    pub fn len(&self) -> usize {
        self.duties.len()
    }

    /// `true` when no pin has been requested yet.
    pub fn is_empty(&self) -> bool {
        self.duties.is_empty()
    }

    /// Pin -> duty_cycle pairs, sorted by pin name.
    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, String, u8> {
        self.duties.iter()
    }

    /// Applies every follow rule until a fixed point is reached.
    pub fn apply_follows(&mut self, rules: &[FollowRule]) {
        if rules.is_empty() || self.duties.is_empty() {
            return;
        }

        // A chain cannot be longer than the number of pins, so this always
        // terminates; cycles are rejected during validation.
        let max_passes = self.duties.len() + rules.len() + 1;
        for _ in 0..max_passes {
            let mut changed = false;
            for rule in rules {
                let floor = match self.duties.get(&rule.min_of) {
                    Some(floor) => *floor,
                    None => continue,
                };
                if let Some(slot) = self.duties.get_mut(&rule.pin) {
                    if *slot < floor {
                        *slot = floor;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    /// Clamps every duty cycle into `min..=max` (the bounds are ordered first).
    pub fn clamp(&mut self, min: u8, max: u8) {
        let (lower, upper) = if min <= max { (min, max) } else { (max, min) };
        for duty_cycle in self.duties.values_mut() {
            *duty_cycle = (*duty_cycle).clamp(lower, upper);
        }
    }
}

/// Standby duty cycle of every pin the daemon drives, used on shutdown.
///
/// Each controlled pin falls back to its zone's `default_duty_cycle` - the same
/// value the curve uses below the first threshold. A pin driven by several
/// zones takes the highest one, matching the conflict rule above. Pins that no
/// zone drives are absent on purpose: the daemon never wrote them, so inventing
/// a value on the way out would add risk instead of restoring anything.
pub fn standby_targets(zones: &[ZoneConfig], min: u8, max: u8) -> TargetMap {
    let mut targets = TargetMap::new();
    for zone in zones {
        for pin in &zone.target_pins {
            targets.raise(pin, zone.default_duty_cycle);
        }
    }
    targets.clamp(min, max);
    targets
}

#[cfg(test)]
mod tests {
    use super::{standby_targets, TargetMap};
    use crate::config::{Aggregation, FollowRule, ZoneConfig};

    fn rule(pin: &str, min_of: &str) -> FollowRule {
        FollowRule {
            pin: pin.to_string(),
            min_of: min_of.to_string(),
        }
    }

    fn zone(name: &str, pin: &str, default_duty_cycle: u8) -> ZoneConfig {
        ZoneConfig {
            name: name.to_string(),
            monitor_sources: vec!["CPU_Tctl".to_string()],
            aggregation: Aggregation::Max,
            target_pins: vec![pin.to_string()],
            default_duty_cycle,
            debounce_celsius: 2.0,
            thresholds: Vec::new(),
        }
    }

    #[test]
    fn conflicting_zones_take_the_highest_demand() {
        let mut map = TargetMap::new();
        map.raise("CPU_FAN", 40);
        map.raise("CPU_FAN", 100);
        map.raise("CPU_FAN", 30);
        assert_eq!(map.get("CPU_FAN"), Some(100));
    }

    #[test]
    fn follow_rules_never_lower_a_duty() {
        let mut map = TargetMap::new();
        map.raise("CPU_FAN", 80);
        map.raise("SYS_FAN_1", 30);
        map.apply_follows(&[rule("SYS_FAN_1", "CPU_FAN")]);
        assert_eq!(map.get("SYS_FAN_1"), Some(80));
        assert_eq!(map.get("CPU_FAN"), Some(80));
    }

    #[test]
    fn follow_chains_resolve_in_any_order() {
        let mut map = TargetMap::new();
        map.raise("CPU_FAN", 90);
        map.raise("SYS_FAN_1", 30);
        map.raise("SYS_FAN_2", 30);
        // Declared "backwards" on purpose.
        map.apply_follows(&[rule("SYS_FAN_2", "SYS_FAN_1"), rule("SYS_FAN_1", "CPU_FAN")]);
        assert_eq!(map.get("SYS_FAN_1"), Some(90));
        assert_eq!(map.get("SYS_FAN_2"), Some(90));
    }

    #[test]
    fn clamping_applies_global_limits() {
        let mut map = TargetMap::new();
        map.raise("CPU_FAN", 5);
        map.raise("SYS_FAN_1", 100);
        map.clamp(25, 90);
        assert_eq!(map.get("CPU_FAN"), Some(25));
        assert_eq!(map.get("SYS_FAN_1"), Some(90));
    }

    #[test]
    fn unknown_pins_in_follow_rules_are_ignored() {
        let mut map = TargetMap::new();
        map.raise("SYS_FAN_1", 30);
        map.apply_follows(&[rule("SYS_FAN_1", "NOT_CONFIGURED")]);
        assert_eq!(map.get("SYS_FAN_1"), Some(30));
    }

    #[test]
    fn clamp_survives_swapped_bounds() {
        let mut map = TargetMap::new();
        map.raise("CPU_FAN", 60);
        map.clamp(90, 25);
        assert_eq!(map.get("CPU_FAN"), Some(60));
    }

    #[test]
    fn standby_uses_each_zone_default_and_skips_unmanaged_pins() {
        let zones = vec![zone("cpu", "CPU_FAN", 30), zone("exhaust", "SYS_FAN_1", 45)];
        let map = standby_targets(&zones, 25, 100);
        assert_eq!(map.get("CPU_FAN"), Some(30));
        assert_eq!(map.get("SYS_FAN_1"), Some(45));
        assert_eq!(map.get("SYS_FAN_2"), None);
    }

    #[test]
    fn standby_takes_the_highest_default_on_a_shared_pin() {
        let zones = vec![zone("a", "CPU_FAN", 30), zone("b", "CPU_FAN", 60)];
        assert_eq!(standby_targets(&zones, 25, 100).get("CPU_FAN"), Some(60));
    }

    #[test]
    fn standby_respects_the_global_floor() {
        let zones = vec![zone("a", "CPU_FAN", 10)];
        assert_eq!(standby_targets(&zones, 25, 100).get("CPU_FAN"), Some(25));
    }
}
