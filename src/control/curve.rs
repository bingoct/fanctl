//! The per-zone curve and its asymmetric debounce_celsius.
//!
//! A band is the position on the curve: band `0` means "below the first
//! threshold" and uses `default_duty_cycle`, band `n` uses the duty cycle of the
//! `n`-th threshold.
//!
//! Rising temperature is acted on immediately - hardware protection beats
//! acoustic comfort. Falling temperature is debounced by `debounce_celsius`, which
//! is what keeps the fans from oscillating around a threshold.
//!
//! ```text
//! duty_cycle
//!  100 |               +-----------  band 3  (>= 85 C)
//!   60 |         +-----+             band 2  (>= 70 C)  drops back at 68 C
//!   40 |   +-----+                   band 1  (>= 55 C)  drops back at 53 C
//!   30 +---+                         band 0             standby, no debounce
//!      +---+-----+-----+-----+-----> temperature
//!         55    68    70    85
//! ```

use crate::config::ZoneConfig;

/// Position of a zone on its curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneState {
    /// `0` = standby, `n` = at or above the `n`-th threshold.
    pub band: usize,
}

/// Band implied by `temperature` alone, ignoring any debounce_celsius.
pub fn band_for_temperature(zone: &ZoneConfig, temperature: f64) -> usize {
    zone.thresholds
        .iter()
        .filter(|threshold| temperature >= threshold.temperature)
        .count()
}

/// Next band, applying the asymmetric debounce_celsius.
///
/// * `previous == None` (first evaluation) adopts the temperature band as is.
/// * Going up is immediate, including several bands at once.
/// * Going down requires the temperature to be at least `debounce_celsius` below
///   the threshold that opened the current band.
/// * Dropping below `first_threshold - debounce_celsius` jumps straight to standby.
pub fn next_band(zone: &ZoneConfig, previous: Option<ZoneState>, temperature: f64) -> ZoneState {
    let target = band_for_temperature(zone, temperature);

    let previous_band = match previous {
        Some(state) => state.band,
        None => return ZoneState { band: target },
    };

    if target >= previous_band {
        return ZoneState { band: target };
    }
    if previous_band == 0 {
        return ZoneState { band: 0 };
    }

    let debounce = if zone.debounce_celsius.is_finite() && zone.debounce_celsius > 0.0 {
        zone.debounce_celsius
    } else {
        0.0
    };

    let first_threshold = zone
        .thresholds
        .first()
        .map(|threshold| threshold.temperature)
        .unwrap_or(f64::NEG_INFINITY);
    if temperature <= first_threshold - debounce {
        return ZoneState { band: 0 };
    }

    match zone.thresholds.get(previous_band - 1) {
        Some(threshold) if temperature <= threshold.temperature - debounce => ZoneState {
            band: previous_band - 1,
        },
        _ => ZoneState {
            band: previous_band,
        },
    }
}

/// Duty cycle of `band`, clamped to a duty cycle a fan can actually use.
pub fn duty_for_band(zone: &ZoneConfig, band: usize) -> u8 {
    let duty_cycle = if band == 0 {
        zone.default_duty_cycle
    } else {
        zone.thresholds
            .get(band - 1)
            .map(|threshold| threshold.duty_cycle)
            .unwrap_or(zone.default_duty_cycle)
    };
    duty_cycle.clamp(1, 100)
}

#[cfg(test)]
mod tests {
    use super::{band_for_temperature, duty_for_band, next_band, ZoneState};
    use crate::config::{Aggregation, Threshold, ZoneConfig};
    use crate::sensors::Source;

    fn cpu_zone() -> ZoneConfig {
        ZoneConfig {
            name: "cpu_zone".to_string(),
            sources: vec![Source::parse("CPU_Tctl")],
            aggregation: Aggregation::Max,
            target_pins: vec!["CPU_FAN".to_string()],
            default_duty_cycle: 30,
            fallback_duty_cycle: 60,
            fallback_after_cycles: 2,
            debounce_celsius: 2.0,
            thresholds: vec![
                Threshold {
                    temperature: 55.0,
                    duty_cycle: 40,
                },
                Threshold {
                    temperature: 70.0,
                    duty_cycle: 60,
                },
                Threshold {
                    temperature: 85.0,
                    duty_cycle: 100,
                },
            ],
        }
    }

    #[test]
    fn bands_follow_thresholds() {
        let zone = cpu_zone();
        assert_eq!(band_for_temperature(&zone, 20.0), 0);
        assert_eq!(band_for_temperature(&zone, 55.0), 1);
        assert_eq!(band_for_temperature(&zone, 69.9), 1);
        assert_eq!(band_for_temperature(&zone, 70.0), 2);
        assert_eq!(band_for_temperature(&zone, 95.0), 3);
    }

    #[test]
    fn first_evaluation_adopts_the_curve() {
        let zone = cpu_zone();
        assert_eq!(next_band(&zone, None, 90.0), ZoneState { band: 3 });
        assert_eq!(next_band(&zone, None, 40.0), ZoneState { band: 0 });
    }

    #[test]
    fn rising_temperature_is_immediate() {
        let zone = cpu_zone();
        // One band up, no debounce.
        assert_eq!(next_band(&zone, Some(ZoneState { band: 1 }), 70.0).band, 2);
        // Several bands at once when the CPU spikes.
        assert_eq!(next_band(&zone, Some(ZoneState { band: 0 }), 90.0).band, 3);
    }

    #[test]
    fn falling_temperature_is_debounced() {
        let zone = cpu_zone();
        let in_band_2 = Some(ZoneState { band: 2 });
        // 70 - 2 = 68: above the debounce floor, stay at band 2.
        assert_eq!(next_band(&zone, in_band_2, 69.0).band, 2);
        assert_eq!(next_band(&zone, in_band_2, 68.5).band, 2);
        // At the floor, one band down.
        assert_eq!(next_band(&zone, in_band_2, 68.0).band, 1);
        assert_eq!(next_band(&zone, in_band_2, 60.0).band, 1);
    }

    #[test]
    fn dropping_below_the_first_threshold_goes_to_standby() {
        let zone = cpu_zone();
        let in_band_2 = Some(ZoneState { band: 2 });
        // 55 - 2 = 53: straight to standby instead of stepping through band 1.
        assert_eq!(next_band(&zone, in_band_2, 52.0).band, 0);
        assert_eq!(next_band(&zone, in_band_2, 54.0).band, 1);
    }

    #[test]
    fn duty_is_read_from_the_band() {
        let zone = cpu_zone();
        assert_eq!(duty_for_band(&zone, 0), 30);
        assert_eq!(duty_for_band(&zone, 1), 40);
        assert_eq!(duty_for_band(&zone, 2), 60);
        assert_eq!(duty_for_band(&zone, 3), 100);
    }
}
