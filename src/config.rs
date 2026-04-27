use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Default, Clone)]
pub struct Defaults {
    pub user: Option<String>,
    pub port: Option<u16>,
    pub use_sudo: Option<bool>,
    pub identity_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RawHost {
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub use_sudo: Option<bool>,
    pub identity_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RawGroup {
    pub name: String,
    pub hosts: Vec<RawHost>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub use_sudo: Option<bool>,
    pub identity_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RawConfig {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub groups: Vec<RawGroup>,
    #[serde(default)]
    pub hosts: Vec<RawHost>,
}

/// Fully resolved host configuration with all defaults applied.
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub use_sudo: bool,
    pub identity_file: Option<PathBuf>,
    #[allow(dead_code)]
    pub group: Option<String>,
}

/// Flat list of all hosts, in order: grouped hosts first (in group order), then top-level hosts.
pub struct Config {
    pub raw: RawConfig,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let raw: RawConfig = toml::from_str(&contents)
            .with_context(|| format!("parsing config {}", path.display()))?;
        Ok(Self { raw })
    }

    /// Return the resolved host list including default config paths.
    pub fn resolved_hosts(&self) -> Vec<HostConfig> {
        let d = &self.raw.defaults;
        let default_user = d.user.clone().unwrap_or_else(whoami);
        let default_port = d.port.unwrap_or(22);
        let default_sudo = d.use_sudo.unwrap_or(true);

        let mut out = Vec::new();

        for g in &self.raw.groups {
            let group_user = g.user.as_ref().or(d.user.as_ref());
            let group_port = g.port.or(d.port);
            let group_sudo = g.use_sudo.or(d.use_sudo);
            let group_id = g.identity_file.as_ref().or(d.identity_file.as_ref());
            for h in &g.hosts {
                out.push(HostConfig {
                    hostname: h.hostname.clone(),
                    user: h
                        .user
                        .clone()
                        .or_else(|| group_user.cloned())
                        .unwrap_or_else(|| default_user.clone()),
                    port: h.port.or(group_port).unwrap_or(default_port),
                    use_sudo: h.use_sudo.or(group_sudo).unwrap_or(default_sudo),
                    identity_file: h
                        .identity_file
                        .clone()
                        .or_else(|| group_id.cloned())
                        .or_else(|| d.identity_file.clone()),
                    group: Some(g.name.clone()),
                });
            }
        }

        for h in &self.raw.hosts {
            out.push(HostConfig {
                hostname: h.hostname.clone(),
                user: h
                    .user
                    .clone()
                    .or_else(|| d.user.clone())
                    .unwrap_or_else(|| default_user.clone()),
                port: h.port.or(d.port).unwrap_or(default_port),
                use_sudo: h.use_sudo.or(d.use_sudo).unwrap_or(default_sudo),
                identity_file: h
                    .identity_file
                    .clone()
                    .or_else(|| d.identity_file.clone()),
                group: None,
            });
        }

        out
    }

    /// Return ordered sidebar rows: Group headers interleaved with their hosts, then ungrouped hosts.
    pub fn sidebar_rows(&self, hosts: &[HostConfig]) -> Vec<SidebarRow> {
        let mut rows = Vec::new();
        let mut idx = 0;

        for g in &self.raw.groups {
            rows.push(SidebarRow::Group { name: g.name.clone() });
            for _ in &g.hosts {
                rows.push(SidebarRow::Host { host_idx: idx });
                idx += 1;
            }
        }
        for _ in &self.raw.hosts {
            rows.push(SidebarRow::Host { host_idx: idx });
            idx += 1;
        }
        let _ = hosts; // hosts slice used for ordering; idx tracks position
        rows
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SidebarRow {
    Group { name: String },
    Host { host_idx: usize },
}

fn whoami() -> String {
    whoami::username()
}
