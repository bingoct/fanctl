//! Configuration loading and cross-file validation.

pub mod platform;
pub mod policy;

use std::path::Path;

use serde::de::DeserializeOwned;

pub use platform::{parse_hex_byte, PlatformConfig, PlatformInfo};
pub use policy::{Aggregation, FanConfig, FollowRule, GlobalSettings, Threshold, ZoneConfig};

use crate::error::Error;

/// Both configuration files, already validated against each other.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// Board description.
    pub platform: PlatformConfig,
    /// Business logic.
    pub policy: FanConfig,
}

impl LoadedConfig {
    /// Reads, parses, and cross-validates both files.
    ///
    /// Every validation problem is collected before failing, so a single run
    /// reports every mistake instead of one per attempt.
    pub fn load(policy_path: &Path, platform_path: &Path) -> Result<Self, Error> {
        let platform: PlatformConfig = read_toml(platform_path)?;
        let mut policy: FanConfig = read_toml(policy_path)?;
        // Names are case-insensitive: fold every reference onto the spelling
        // declared in `platform.toml` before validating, so that `cpu_fan` and
        // `CPU_FAN` cannot end up as two different pins downstream.
        policy.resolve_names(&platform);

        let mut problems = platform.validate();
        problems.extend(policy.validate(&platform));
        if !problems.is_empty() {
            let details = problems
                .iter()
                .map(|problem| format!("  - {problem}"))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(Error::Validation(format!(
                "{} problem(s) found:\n{details}",
                problems.len()
            )));
        }

        Ok(Self { platform, policy })
    }
}

fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| Error::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}
