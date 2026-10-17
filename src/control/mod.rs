//! Decision layer: aggregation, curves, merging.
//!
//! Nothing in here executes anything, which is why the whole module can be unit
//! tested without a BMC.

pub mod curve;
pub mod engine;
pub mod merge;

pub use curve::{band_for_temperature, duty_for_band, next_band, ZoneState};
pub use engine::Engine;
pub use merge::{standby_targets, TargetMap};

use crate::config::{Aggregation, ZoneConfig};
use crate::sensors::{SensorSnapshot, Source};

/// Combines a zone's sensor readings into the single temperature its curve sees.
///
/// Sources that produced no reading this cycle are skipped rather than treated
/// as zero: a missing GPU must not silently drag a `mean` zone into standby.
/// Returns `None` only when *every* source of the zone is missing.
pub fn aggregate(zone: &ZoneConfig, snapshot: &SensorSnapshot) -> Option<f64> {
    let mut highest: Option<f64> = None;
    let mut sum = 0.0;
    let mut count = 0_usize;

    for raw in &zone.monitor_sources {
        // Validation already rejected unparsable sources; skip defensively.
        let source = match Source::parse(raw) {
            Ok(source) => source,
            Err(_) => continue,
        };
        let value = match snapshot.get(&source) {
            Some(value) => value,
            None => continue,
        };
        highest = Some(highest.map_or(value, |current: f64| current.max(value)));
        sum += value;
        count += 1;
    }

    if count == 0 {
        return None;
    }

    match zone.aggregation {
        Aggregation::Max => highest,
        Aggregation::Mean => Some(sum / count as f64),
    }
}
