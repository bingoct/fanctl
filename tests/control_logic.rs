//! Behaviour of the decision layer through the public API.

use fanctl::config::{Aggregation, FollowRule, Threshold, ZoneConfig};
use fanctl::control::{aggregate, duty_for_band, next_band, TargetMap, ZoneState};
use fanctl::sensors::{SensorSnapshot, Source};

fn zone_with(sources: &[&str], aggregation: Aggregation) -> ZoneConfig {
    ZoneConfig {
        name: "test_zone".to_string(),
        sources: sources.iter().map(|raw| Source::parse(raw)).collect(),
        aggregation,
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

fn snapshot(cpu: Option<f64>, mosfet: Option<f64>, gpus: &[f64]) -> SensorSnapshot {
    let mut snapshot = SensorSnapshot::default();
    if let Some(value) = cpu {
        snapshot.ipmi.insert("CPU_Tctl".to_string(), value);
    }
    if let Some(value) = mosfet {
        snapshot.ipmi.insert("CPU_MOSFET".to_string(), value);
    }
    snapshot.gpus = gpus
        .iter()
        .enumerate()
        .map(|(index, celsius)| (index as u32, format!("GPU{index}"), *celsius))
        .collect();
    snapshot
}

#[test]
fn max_takes_the_hottest_source() {
    let zone = zone_with(&["CPU_Tctl", "GPU"], Aggregation::Max);
    let readings = snapshot(Some(50.0), None, &[61.0, 72.0]);
    assert_eq!(aggregate(&zone, &readings), Some(72.0));
}

#[test]
fn mean_averages_only_the_available_sources() {
    let zone = zone_with(&["CPU_Tctl", "CPU_MOSFET", "GPU"], Aggregation::Mean);
    // No GPU on this host: the mean must not be dragged down by a phantom 0 C.
    let readings = snapshot(Some(40.0), Some(60.0), &[]);
    assert_eq!(aggregate(&zone, &readings), Some(50.0));
}

#[test]
fn a_zone_without_any_reading_yields_nothing() {
    let zone = zone_with(&["GPU"], Aggregation::Max);
    let readings = snapshot(Some(40.0), None, &[]);
    assert_eq!(aggregate(&zone, &readings), None);
}

#[test]
fn hot_gpu_drives_the_intake_zone() {
    let zone = zone_with(&["CPU_Tctl", "GPU"], Aggregation::Max);
    let readings = snapshot(Some(45.0), None, &[78.0]);
    let temperature = aggregate(&zone, &readings).expect("GPU is present");

    let state = next_band(&zone, Some(ZoneState { band: 0 }), temperature);
    assert_eq!(state.band, 2);
    assert_eq!(duty_for_band(&zone, state.band), 60);
}

#[test]
fn indexed_gpu_source_reads_only_that_card() {
    // Three cards, the middle one is the coolest: `GPU1` must report 45, not
    // the 80 that the folded `GPU` source would pick.
    let indexed = zone_with(&["GPU1"], Aggregation::Max);
    let folded = zone_with(&["GPU"], Aggregation::Max);
    let readings = snapshot(None, None, &[80.0, 45.0, 50.0]);

    assert_eq!(aggregate(&indexed, &readings), Some(45.0));
    assert_eq!(aggregate(&folded, &readings), Some(80.0));
}

#[test]
fn indexed_gpu_source_yields_nothing_when_the_card_is_absent() {
    let zone = zone_with(&["GPU3"], Aggregation::Max);
    let readings = snapshot(None, None, &[80.0, 45.0]);
    assert_eq!(aggregate(&zone, &readings), None);
}

#[test]
fn merged_demands_respect_conflicts_follows_and_limits() {
    let mut targets = TargetMap::new();
    // Two zones share SYS_FAN_1; the higher demand wins.
    targets.raise("CPU_FAN", 70);
    targets.raise("SYS_FAN_1", 45);
    targets.raise("SYS_FAN_1", 65);
    targets.raise("SYS_FAN_2", 30);

    targets.apply_follows(&[
        FollowRule {
            pin: "SYS_FAN_2".to_string(),
            min_of: "SYS_FAN_1".to_string(),
        },
        FollowRule {
            pin: "SYS_FAN_1".to_string(),
            min_of: "CPU_FAN".to_string(),
        },
    ]);
    targets.clamp(25, 100);

    assert_eq!(targets.get("CPU_FAN"), Some(70));
    assert_eq!(targets.get("SYS_FAN_1"), Some(70));
    assert_eq!(targets.get("SYS_FAN_2"), Some(70));
}

#[test]
fn limits_are_applied_after_coupling() {
    let mut targets = TargetMap::new();
    targets.raise("CPU_FAN", 100);
    targets.raise("SYS_FAN_1", 30);
    targets.apply_follows(&[FollowRule {
        pin: "SYS_FAN_1".to_string(),
        min_of: "CPU_FAN".to_string(),
    }]);
    targets.clamp(25, 90);
    assert_eq!(targets.get("CPU_FAN"), Some(90));
    assert_eq!(targets.get("SYS_FAN_1"), Some(90));
}
