use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::apt::HostInfo;
use crate::config::HostConfig;

/// A previously gathered `HostInfo`, persisted to disk so the TUI has
/// something to show immediately on startup instead of a blank "gathering…"
/// state while the real SSH gather is still in flight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub info: HostInfo,
    pub fetched_at_unix: u64,
}

pub type Cache = HashMap<String, CacheEntry>;

/// Key identifying a host's cache entry independent of its position in the
/// config file, so reordering groups/hosts doesn't invalidate the cache.
pub fn host_key(cfg: &HostConfig) -> String {
    format!("{}@{}:{}", cfg.user, cfg.hostname, cfg.port)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn cache_path() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("aptmatic")
        .join("hosts.json")
}

/// Load the on-disk gather cache. Returns an empty cache on any error
/// (missing file, corrupt JSON, permissions, …) — this is purely a startup
/// convenience and never a source of truth, so failures are silent.
pub fn load() -> Cache {
    load_from_path(&cache_path())
}

fn load_from_path(path: &Path) -> Cache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the gather cache to disk. Callers treat failures as non-fatal —
/// caching is a convenience, not something that should disrupt the TUI.
pub fn save(cache: &Cache) -> Result<()> {
    save_to_path(cache, &cache_path())
}

fn save_to_path(cache: &Cache, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let json = serde_json::to_string(cache).context("serializing gather cache")?;
    std::fs::write(path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostConfig;

    fn host_cfg(hostname: &str) -> HostConfig {
        HostConfig {
            hostname: hostname.to_string(),
            user: "alice".to_string(),
            port: 22,
            use_sudo: true,
            identity_file: None,
            group: None,
        }
    }

    #[test]
    fn host_key_includes_user_hostname_and_port() {
        assert_eq!(
            host_key(&host_cfg("web1.example.com")),
            "alice@web1.example.com:22"
        );
    }

    #[test]
    fn host_key_differs_by_port() {
        let mut cfg = host_cfg("web1.example.com");
        cfg.port = 2222;
        assert_ne!(host_key(&cfg), host_key(&host_cfg("web1.example.com")));
    }

    #[test]
    fn load_from_path_missing_file_returns_empty() {
        let path = std::env::temp_dir().join("aptmatic-test-missing-cache.json");
        let _ = std::fs::remove_file(&path);
        assert!(load_from_path(&path).is_empty());
    }

    #[test]
    fn load_from_path_corrupt_json_returns_empty() {
        let path = std::env::temp_dir().join("aptmatic-test-corrupt-cache.json");
        std::fs::write(&path, "not valid json").unwrap();
        assert!(load_from_path(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = std::env::temp_dir().join("aptmatic-test-roundtrip-cache.json");
        let _ = std::fs::remove_file(&path);

        let mut cache: Cache = HashMap::new();
        cache.insert(
            host_key(&host_cfg("web1.example.com")),
            CacheEntry {
                info: HostInfo {
                    running_kernel: "6.1.0-28-amd64".to_string(),
                    ..Default::default()
                },
                fetched_at_unix: 12345,
            },
        );

        save_to_path(&cache, &path).unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.len(), 1);
        let entry = &loaded[&host_key(&host_cfg("web1.example.com"))];
        assert_eq!(entry.info.running_kernel, "6.1.0-28-amd64");
        assert_eq!(entry.fetched_at_unix, 12345);

        let _ = std::fs::remove_file(&path);
    }
}
