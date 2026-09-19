//! The user's preferences, persisted as JSON. Secrets never live here; accounts
//! reference credentials by id.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::model::AccountRef;

pub const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_INTERVAL: u64 = 60;
pub const MIN_INTERVAL: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortMode {
    /// The accounts array order, which the user controls.
    #[default]
    Manual,
    /// Worst first: health, then highest usage.
    Smart,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default)]
    pub sort: SortMode,
    /// Explicit auto-detection. Absent (a hand-written or older config) means "scan the
    /// machine when the account list is empty".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detect: Option<bool>,
    /// Display order. Accounts missing from this list are not shown.
    #[serde(default)]
    pub accounts: Vec<AccountRef>,
}

fn default_version() -> u32 {
    CONFIG_VERSION
}

fn default_interval() -> u64 {
    DEFAULT_INTERVAL
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            interval_secs: DEFAULT_INTERVAL,
            sort: SortMode::Manual,
            detect: None,
            accounts: Vec::new(),
        }
    }
}

impl Config {
    /// Whether this run should scan the machine instead of trusting the account list.
    /// Skipping setup writes `detect: true`; removing every account writes `false`, so
    /// the two can be told apart.
    pub fn auto_detect(&self) -> bool {
        self.detect.unwrap_or(self.accounts.is_empty())
    }

    pub fn interval(&self) -> u64 {
        self.interval_secs.max(MIN_INTERVAL)
    }

}

/// The first free id for a provider among everything already taken, config and
/// in-progress wizard connections alike.
pub fn free_id<'a>(
    provider: crate::model::ProviderId,
    taken: impl Iterator<Item = &'a str>,
) -> String {
    let taken: Vec<&str> = taken.collect();
    let base = provider.slug();
    if !taken.contains(&base) {
        return base.to_string();
    }
    for n in 2..1000 {
        let candidate = format!("{base}-{n}");
        if !taken.contains(&candidate.as_str()) {
            return candidate;
        }
    }
    format!("{base}-{}", std::process::id())
}

pub fn dir() -> PathBuf {
    if let Ok(dir) = std::env::var("USAGEBAR_CONFIG_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    fsutil::xdg_dir("XDG_CONFIG_HOME", "usagebar", ".config/usagebar")
}

pub fn config_path(dir: &Path) -> PathBuf {
    dir.join("config.json")
}

pub fn credentials_path(dir: &Path) -> PathBuf {
    dir.join("credentials.json")
}

/// `Ok(None)` means there is no config yet, which is what starts onboarding.
pub fn load(path: &Path) -> Result<Option<Config>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn save(path: &Path, config: &Config) -> Result<(), String> {
    let mut doc = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    doc.push('\n');
    fsutil::write_atomic(path, doc.as_bytes(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProviderId;

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("usagebar-config-{}-{name}", std::process::id()))
    }

    #[test]
    fn round_trips_accounts_order_and_preferences() {
        let dir = scratch("roundtrip");
        let path = config_path(&dir);
        let mut config = Config {
            interval_secs: 15,
            sort: SortMode::Smart,
            ..Config::default()
        };
        config
            .accounts
            .push(AccountRef::new("claude-work", ProviderId::Claude));
        config.accounts.push(AccountRef {
            hidden: true,
            ..AccountRef::new("codex", ProviderId::Codex)
        });
        save(&path, &config).unwrap();

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.interval(), 15);
        assert_eq!(loaded.sort, SortMode::Smart);
        assert_eq!(loaded.accounts[0].id, "claude-work");
        assert!(loaded.accounts[1].hidden);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_means_no_config() {
        let path = scratch("missing").join("config.json");
        assert!(load(&path).unwrap().is_none());
    }

    #[test]
    fn partial_and_unknown_fields_stay_loadable() {
        let raw = r#"{"interval_secs": 30, "future_key": true, "accounts": [
            {"id": "claude", "provider": "claude"}]}"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        assert_eq!(config.interval(), 30);
        assert_eq!(config.sort, SortMode::Manual);
        assert_eq!(config.accounts[0].label, None);
        assert!(!config.accounts[0].hidden);
    }

    #[test]
    fn interval_has_a_floor() {
        let config = Config {
            interval_secs: 1,
            ..Config::default()
        };
        assert_eq!(config.interval(), MIN_INTERVAL);
    }

    #[test]
    fn ids_stay_unique_per_provider() {
        let mut config = Config::default();
        let first = free_id(ProviderId::Claude, config.accounts.iter().map(|a| a.id.as_str()));
        config
            .accounts
            .push(AccountRef::new(first.clone(), ProviderId::Claude));
        let second = free_id(ProviderId::Claude, config.accounts.iter().map(|a| a.id.as_str()));
        assert_eq!(first, "claude");
        assert_eq!(second, "claude-2");
    }

    /// Two connections of one provider in a single wizard session must not collide.
    #[test]
    fn free_id_counts_accounts_that_are_not_in_the_config_yet() {
        let pending = ["claude", "claude-2"];
        let third = free_id(ProviderId::Claude, pending.iter().copied());
        assert_eq!(third, "claude-3");
    }

    /// Skipping setup and removing every account both leave an empty list, and they
    /// have to mean different things.
    #[test]
    fn detection_is_explicit_when_it_is_written() {
        let skipped = Config {
            detect: Some(true),
            ..Config::default()
        };
        assert!(skipped.auto_detect());
        let emptied = Config {
            detect: Some(false),
            ..Config::default()
        };
        assert!(!emptied.auto_detect());
        // A config that never mentions detection falls back to the account list.
        assert!(Config::default().auto_detect());
        let hand_written: Config =
            serde_json::from_str(r#"{"accounts":[{"id":"claude","provider":"claude"}]}"#).unwrap();
        assert!(!hand_written.auto_detect());
    }
}
