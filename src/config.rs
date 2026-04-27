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
                identity_file: h.identity_file.clone().or_else(|| d.identity_file.clone()),
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
            rows.push(SidebarRow::Group {
                name: g.name.clone(),
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(raw: RawConfig) -> Config {
        Config { raw }
    }

    fn raw_host(hostname: &str) -> RawHost {
        RawHost {
            hostname: hostname.to_string(),
            user: None,
            port: None,
            use_sudo: None,
            identity_file: None,
        }
    }

    // ── resolved_hosts ────────────────────────────────────────────────────────

    #[test]
    fn resolved_hosts_empty_config() {
        let cfg = make_config(RawConfig::default());
        assert!(cfg.resolved_hosts().is_empty());
    }

    #[test]
    fn resolved_hosts_applies_defaults() {
        let cfg = make_config(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                port: Some(2222),
                use_sudo: Some(false),
                identity_file: Some("/home/alice/.ssh/id_rsa".into()),
            },
            hosts: vec![raw_host("host1.example.com")],
            ..Default::default()
        });
        let hosts = cfg.resolved_hosts();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].user, "alice");
        assert_eq!(hosts[0].port, 2222);
        assert!(!hosts[0].use_sudo);
        assert_eq!(
            hosts[0].identity_file.as_deref(),
            Some("/home/alice/.ssh/id_rsa".as_ref())
        );
    }

    #[test]
    fn resolved_hosts_built_in_fallbacks_when_no_defaults() {
        let cfg = make_config(RawConfig {
            hosts: vec![RawHost {
                hostname: "host1.example.com".to_string(),
                user: Some("bob".to_string()), // need user to avoid calling whoami
                port: None,
                use_sudo: None,
                identity_file: None,
            }],
            ..Default::default()
        });
        let hosts = cfg.resolved_hosts();
        assert_eq!(hosts[0].port, 22);
        assert!(hosts[0].use_sudo);
        assert!(hosts[0].identity_file.is_none());
    }

    #[test]
    fn resolved_hosts_group_level_overrides_defaults() {
        let cfg = make_config(RawConfig {
            defaults: Defaults {
                user: Some("default-user".to_string()),
                port: Some(22),
                use_sudo: Some(true),
                identity_file: None,
            },
            groups: vec![RawGroup {
                name: "webservers".to_string(),
                user: Some("group-user".to_string()),
                port: Some(2222),
                use_sudo: Some(false),
                identity_file: None,
                hosts: vec![raw_host("web1.example.com")],
            }],
            ..Default::default()
        });
        let hosts = cfg.resolved_hosts();
        assert_eq!(hosts[0].user, "group-user");
        assert_eq!(hosts[0].port, 2222);
        assert!(!hosts[0].use_sudo);
    }

    #[test]
    fn resolved_hosts_host_level_overrides_group_and_defaults() {
        let cfg = make_config(RawConfig {
            defaults: Defaults {
                user: Some("default-user".to_string()),
                port: Some(22),
                use_sudo: Some(true),
                identity_file: None,
            },
            groups: vec![RawGroup {
                name: "webservers".to_string(),
                user: Some("group-user".to_string()),
                port: Some(2222),
                use_sudo: Some(false),
                identity_file: None,
                hosts: vec![RawHost {
                    hostname: "web1.example.com".to_string(),
                    user: Some("host-user".to_string()),
                    port: Some(9022),
                    use_sudo: Some(true),
                    identity_file: Some("/custom/key".into()),
                }],
            }],
            ..Default::default()
        });
        let hosts = cfg.resolved_hosts();
        assert_eq!(hosts[0].user, "host-user");
        assert_eq!(hosts[0].port, 9022);
        assert!(hosts[0].use_sudo);
        assert_eq!(
            hosts[0].identity_file.as_deref(),
            Some("/custom/key".as_ref())
        );
    }

    #[test]
    fn resolved_hosts_identity_file_inherited_through_levels() {
        // defaults has identity_file; group and host do not — host should inherit defaults
        let cfg = make_config(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                identity_file: Some("/default/key".into()),
                ..Default::default()
            },
            groups: vec![RawGroup {
                name: "g".to_string(),
                user: None,
                port: None,
                use_sudo: None,
                identity_file: None,
                hosts: vec![raw_host("h1.example.com")],
            }],
            ..Default::default()
        });
        let hosts = cfg.resolved_hosts();
        assert_eq!(
            hosts[0].identity_file.as_deref(),
            Some("/default/key".as_ref())
        );
    }

    #[test]
    fn resolved_hosts_group_identity_file_overrides_defaults() {
        let cfg = make_config(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                identity_file: Some("/default/key".into()),
                ..Default::default()
            },
            groups: vec![RawGroup {
                name: "g".to_string(),
                user: None,
                port: None,
                use_sudo: None,
                identity_file: Some("/group/key".into()),
                hosts: vec![raw_host("h1.example.com")],
            }],
            ..Default::default()
        });
        let hosts = cfg.resolved_hosts();
        assert_eq!(
            hosts[0].identity_file.as_deref(),
            Some("/group/key".as_ref())
        );
    }

    #[test]
    fn resolved_hosts_ungrouped_hosts_appended_after_groups() {
        let cfg = make_config(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            groups: vec![RawGroup {
                name: "g".to_string(),
                user: None,
                port: None,
                use_sudo: None,
                identity_file: None,
                hosts: vec![raw_host("grouped.example.com")],
            }],
            hosts: vec![raw_host("ungrouped.example.com")],
        });
        let hosts = cfg.resolved_hosts();
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].hostname, "grouped.example.com");
        assert_eq!(hosts[1].hostname, "ungrouped.example.com");
        assert_eq!(hosts[0].group.as_deref(), Some("g"));
        assert!(hosts[1].group.is_none());
    }

    // ── sidebar_rows ──────────────────────────────────────────────────────────

    #[test]
    fn sidebar_rows_empty_config() {
        let cfg = make_config(RawConfig::default());
        let hosts = cfg.resolved_hosts();
        assert!(cfg.sidebar_rows(&hosts).is_empty());
    }

    #[test]
    fn sidebar_rows_group_then_hosts_then_ungrouped() {
        let cfg = make_config(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            groups: vec![RawGroup {
                name: "web".to_string(),
                user: None,
                port: None,
                use_sudo: None,
                identity_file: None,
                hosts: vec![raw_host("web1.example.com"), raw_host("web2.example.com")],
            }],
            hosts: vec![raw_host("standalone.example.com")],
        });
        let hosts = cfg.resolved_hosts();
        let rows = cfg.sidebar_rows(&hosts);
        assert_eq!(rows.len(), 4); // Group + 2 hosts + 1 ungrouped
        assert_eq!(
            rows[0],
            SidebarRow::Group {
                name: "web".to_string()
            }
        );
        assert_eq!(rows[1], SidebarRow::Host { host_idx: 0 });
        assert_eq!(rows[2], SidebarRow::Host { host_idx: 1 });
        assert_eq!(rows[3], SidebarRow::Host { host_idx: 2 });
    }

    #[test]
    fn sidebar_rows_empty_group_emits_group_row_only() {
        let cfg = make_config(RawConfig {
            groups: vec![RawGroup {
                name: "empty".to_string(),
                user: None,
                port: None,
                use_sudo: None,
                identity_file: None,
                hosts: vec![],
            }],
            ..Default::default()
        });
        let hosts = cfg.resolved_hosts();
        let rows = cfg.sidebar_rows(&hosts);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            SidebarRow::Group {
                name: "empty".to_string()
            }
        );
    }

    #[test]
    fn sidebar_rows_host_indices_are_sequential_across_groups() {
        let cfg = make_config(RawConfig {
            defaults: Defaults {
                user: Some("alice".to_string()),
                ..Default::default()
            },
            groups: vec![
                RawGroup {
                    name: "a".to_string(),
                    user: None,
                    port: None,
                    use_sudo: None,
                    identity_file: None,
                    hosts: vec![raw_host("a1.example.com"), raw_host("a2.example.com")],
                },
                RawGroup {
                    name: "b".to_string(),
                    user: None,
                    port: None,
                    use_sudo: None,
                    identity_file: None,
                    hosts: vec![raw_host("b1.example.com")],
                },
            ],
            ..Default::default()
        });
        let hosts = cfg.resolved_hosts();
        let rows = cfg.sidebar_rows(&hosts);
        // Group a: idx 0, 1 — Group b: idx 2
        assert_eq!(rows[1], SidebarRow::Host { host_idx: 0 });
        assert_eq!(rows[2], SidebarRow::Host { host_idx: 1 });
        assert_eq!(rows[4], SidebarRow::Host { host_idx: 2 });
    }
}
