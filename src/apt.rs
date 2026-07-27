use serde::{Deserialize, Serialize};

/// A single upgradable apt package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub new_version: String,
    pub current_version: Option<String>,
    /// True if the package's origin suite looks like a security repo
    /// (e.g. `bookworm-security`, `jammy-security`).
    pub is_security: bool,
}

/// Represents a package held back from upgrades, along with the reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeldPackage {
    pub name: String,
    pub reason: HoldReason,
    /// Human-readable explanation of why the package is kept back, if available.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HoldReason {
    /// Held via `apt-mark hold`
    ManualHold,
    /// Kept back by apt resolver (e.g. new dependencies needed)
    KeptBack,
}

/// A package with dpkg rc status (removed but config files remain).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RcPackage {
    pub name: String,
}

/// A config file that a package shipped a new version of, which dpkg left
/// beside the live file rather than installing (because aptmatic upgrades with
/// `--force-confold`). Nothing has changed on the host until it is resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingConffile {
    /// What dpkg wrote, e.g. `/etc/ssh/sshd_config.dpkg-dist`.
    pub pending_path: String,
    /// The live config it would replace, e.g. `/etc/ssh/sshd_config`.
    pub live_path: String,
    /// Package owning the live path, when dpkg can attribute it.
    pub package: Option<String>,
}

/// Suffixes dpkg and ucf use for "here is the new version, you decide".
/// `.dpkg-old` is deliberately absent: that is a backup of the *previous*
/// file, not a pending change.
const PENDING_SUFFIXES: &[&str] = &[".dpkg-dist", ".dpkg-new", ".ucf-dist"];

/// All gathered information for a single host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostInfo {
    pub running_kernel: String,
    pub latest_kernel: Option<String>,
    pub reboot_required: bool,
    pub upgradable: Vec<Package>,
    pub rc_packages: Vec<RcPackage>,
    pub held_packages: Vec<HeldPackage>,
    pub autoremovable: Vec<String>,
    /// Defaulted so that caches written before this field existed still load.
    #[serde(default)]
    pub pending_conffiles: Vec<PendingConffile>,
}

impl HostInfo {
    /// Number of upgradable packages originating from a security suite.
    pub fn security_count(&self) -> usize {
        self.upgradable.iter().filter(|p| p.is_security).count()
    }

    /// Names of upgradable packages originating from a security suite.
    pub fn security_package_names(&self) -> Vec<String> {
        self.upgradable
            .iter()
            .filter(|p| p.is_security)
            .map(|p| p.name.clone())
            .collect()
    }
}

/// Parse the output of the `find` in `gather::gather` that locates pending
/// conffiles. One path per line; anything without a recognised suffix is
/// ignored. Results are sorted so the review list is stable between gathers.
pub fn parse_pending_conffiles(output: &str) -> Vec<PendingConffile> {
    let mut files: Vec<PendingConffile> = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|path| {
            let suffix = PENDING_SUFFIXES.iter().find(|s| path.ends_with(**s))?;
            Some(PendingConffile {
                pending_path: path.to_string(),
                live_path: path[..path.len() - suffix.len()].to_string(),
                package: None,
            })
        })
        .collect();
    files.sort_by(|a, b| a.pending_path.cmp(&b.pending_path));
    files
}

/// Attach owning packages using the output of `dpkg -S <live paths…>`, whose
/// lines look like `openssh-server: /etc/ssh/sshd_config`. Paths dpkg cannot
/// attribute (it writes those to stderr) simply keep `package: None`.
pub fn attach_conffile_owners(files: &mut [PendingConffile], dpkg_search_output: &str) {
    for line in dpkg_search_output.lines() {
        let Some((pkgs, path)) = line.rsplit_once(": ") else {
            continue;
        };
        let path = path.trim();
        // Diversions render as `diversion by x from: /path`; those have no
        // package name to offer, and `pkgs` would be nonsense.
        if pkgs.contains(char::is_whitespace) {
            continue;
        }
        // A path can be shipped by several packages, listed comma-separated.
        let owner = pkgs.split(',').next().unwrap_or(pkgs).trim();
        if owner.is_empty() {
            continue;
        }
        for f in files.iter_mut().filter(|f| f.live_path == path) {
            f.package = Some(owner.to_string());
        }
    }
}

/// Parse output of `LC_ALL=C apt list --upgradable 2>/dev/null`
pub fn parse_upgradable(output: &str) -> Vec<Package> {
    let mut pkgs = Vec::new();
    for line in output.lines() {
        if line.contains("WARNING") || line.starts_with("Listing") || line.trim().is_empty() {
            continue;
        }
        if let Some(pkg) = parse_upgradable_line(line) {
            pkgs.push(pkg);
        }
    }
    pkgs
}

fn parse_upgradable_line(line: &str) -> Option<Package> {
    // Format: name/suite version arch [upgradable from: old_version]
    let slash = line.find('/')?;
    let name = line[..slash].trim().to_string();
    let after = &line[slash + 1..];
    let mut parts = after.splitn(3, ' ');
    let suite = parts.next()?;
    let new_version = parts.next()?.trim().to_string();
    let current_version = parts.next().and_then(|rest| {
        let tag = "upgradable from: ";
        let start = rest.find(tag)? + tag.len();
        let end = rest[start..].find(']').map(|e| start + e)?;
        Some(rest[start..end].trim().to_string())
    });
    if name.is_empty() || new_version.is_empty() {
        return None;
    }
    // Debian/Ubuntu security suites are named e.g. `bookworm-security`,
    // `jammy-security`; multi-origin packages report a comma-separated list
    // such as `jammy-updates,jammy-security`.
    let is_security = suite.to_lowercase().contains("security");
    Some(Package {
        name,
        new_version,
        current_version,
        is_security,
    })
}

/// Parse output of `LC_ALL=C dpkg -l`
pub fn parse_rc_packages(output: &str) -> Vec<RcPackage> {
    output
        .lines()
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let status = tokens.next()?;
            let name = tokens.next()?;
            if status == "rc" {
                Some(RcPackage {
                    name: name.to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Parse output of `LC_ALL=C apt-mark showhold`
pub fn parse_held_manually(output: &str) -> Vec<HeldPackage> {
    output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| HeldPackage {
            name: l.trim().to_string(),
            reason: HoldReason::ManualHold,
            detail: None,
        })
        .collect()
}

/// Parse output of `LC_ALL=C apt-get -s upgrade 2>&1`
/// Extracts packages listed under "The following packages have been kept back:"
pub fn parse_kept_back(output: &str, manually_held: &[HeldPackage]) -> Vec<HeldPackage> {
    let manual_names: std::collections::HashSet<&str> =
        manually_held.iter().map(|h| h.name.as_str()).collect();

    let mut in_section = false;
    let mut pkgs = Vec::new();

    for line in output.lines() {
        if line.contains("kept back:") {
            in_section = true;
            continue;
        }
        if in_section {
            if line.starts_with("  ") || line.starts_with('\t') {
                for name in line.split_whitespace() {
                    if !name.is_empty() && !manual_names.contains(name) {
                        pkgs.push(HeldPackage {
                            name: name.to_string(),
                            reason: HoldReason::KeptBack,
                            detail: None,
                        });
                    }
                }
            } else {
                in_section = false;
            }
        }
    }
    pkgs
}

/// Parse output of `LC_ALL=C apt-get -s install <pkg> 2>&1`
/// Returns `(new_packages, removals)` — packages that would be newly installed
/// and packages that would be removed, both of which explain why a package is
/// kept back during a regular `apt upgrade`.
pub fn parse_install_dry_run(output: &str) -> (Vec<String>, Vec<String>) {
    let mut new_pkgs: Vec<String> = Vec::new();
    let mut removals: Vec<String> = Vec::new();
    let mut in_new = false;
    let mut in_remove = false;

    for line in output.lines() {
        if line.contains("NEW packages will be installed") {
            in_new = true;
            in_remove = false;
            continue;
        }
        if line.contains("packages will be REMOVED") {
            in_remove = true;
            in_new = false;
            continue;
        }
        if line.starts_with("  ") || line.starts_with('\t') {
            let names = line.split_whitespace().filter(|s| !s.is_empty());
            if in_new {
                new_pkgs.extend(names.map(str::to_string));
            } else if in_remove {
                removals.extend(names.map(str::to_string));
            }
        } else if !line.trim().is_empty() {
            // Any non-indented non-empty line ends the current section
            in_new = false;
            in_remove = false;
        }
    }

    (new_pkgs, removals)
}

/// Parse output of `LC_ALL=C apt-get -s autoremove --purge 2>/dev/null`
/// Returns package names listed under "The following packages will be REMOVED:".
pub fn parse_autoremovable(output: &str) -> Vec<String> {
    let mut packages = Vec::new();
    let mut in_removed = false;

    for line in output.lines() {
        if line.contains("packages will be REMOVED:") {
            in_removed = true;
            continue;
        }
        if in_removed {
            if line.starts_with("  ") || line.starts_with('\t') {
                packages.extend(
                    line.split_whitespace()
                        .map(|s| s.trim_end_matches('*').to_string()),
                );
            } else {
                in_removed = false;
            }
        }
    }
    packages
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_pending_conffiles ───────────────────────────────────────────────

    #[test]
    fn parse_pending_conffiles_empty() {
        assert!(parse_pending_conffiles("").is_empty());
    }

    #[test]
    fn parse_pending_conffiles_derives_live_path_per_suffix() {
        let input =
            "/etc/ssh/sshd_config.dpkg-dist\n/etc/sysctl.conf.dpkg-new\n/etc/foo.ucf-dist\n";
        let files = parse_pending_conffiles(input);
        assert_eq!(files.len(), 3);
        let live: Vec<&str> = files.iter().map(|f| f.live_path.as_str()).collect();
        assert!(live.contains(&"/etc/ssh/sshd_config"));
        assert!(live.contains(&"/etc/sysctl.conf"));
        assert!(live.contains(&"/etc/foo"));
    }

    /// `.dpkg-old` is the backup of the file that was replaced — there is
    /// nothing pending about it, and offering to "apply" it would restore an
    /// old config.
    #[test]
    fn parse_pending_conffiles_ignores_dpkg_old_backups() {
        let input = "/etc/ssh/sshd_config.dpkg-old\n/etc/hosts\n";
        assert!(parse_pending_conffiles(input).is_empty());
    }

    #[test]
    fn parse_pending_conffiles_sorted_for_stable_ordering() {
        let input = "/etc/z.conf.dpkg-dist\n/etc/a.conf.dpkg-dist\n";
        let files = parse_pending_conffiles(input);
        assert_eq!(files[0].pending_path, "/etc/a.conf.dpkg-dist");
        assert_eq!(files[1].pending_path, "/etc/z.conf.dpkg-dist");
    }

    // ── attach_conffile_owners ────────────────────────────────────────────────

    #[test]
    fn attach_conffile_owners_matches_live_path() {
        let mut files = parse_pending_conffiles("/etc/ssh/sshd_config.dpkg-dist");
        attach_conffile_owners(&mut files, "openssh-server: /etc/ssh/sshd_config\n");
        assert_eq!(files[0].package.as_deref(), Some("openssh-server"));
    }

    #[test]
    fn attach_conffile_owners_takes_first_of_several_packages() {
        let mut files = parse_pending_conffiles("/etc/shared.conf.dpkg-dist");
        attach_conffile_owners(&mut files, "pkg-a,pkg-b: /etc/shared.conf\n");
        assert_eq!(files[0].package.as_deref(), Some("pkg-a"));
    }

    #[test]
    fn attach_conffile_owners_skips_diversion_lines() {
        let mut files = parse_pending_conffiles("/etc/foo.conf.dpkg-dist");
        attach_conffile_owners(&mut files, "diversion by other from: /etc/foo.conf\n");
        assert!(files[0].package.is_none());
    }

    #[test]
    fn attach_conffile_owners_leaves_unmatched_paths_alone() {
        let mut files = parse_pending_conffiles("/etc/foo.conf.dpkg-dist");
        attach_conffile_owners(&mut files, "somepkg: /etc/unrelated.conf\n");
        assert!(files[0].package.is_none());
    }

    // ── parse_upgradable ──────────────────────────────────────────────────────

    #[test]
    fn parse_upgradable_empty() {
        assert!(parse_upgradable("").is_empty());
    }

    #[test]
    fn parse_upgradable_skips_header_and_warnings() {
        let input = "Listing... Done\nWARNING: apt does not have a stable CLI interface\n";
        assert!(parse_upgradable(input).is_empty());
    }

    #[test]
    fn parse_upgradable_single_package_with_current_version() {
        let input = "curl/stable 7.88.1-10+deb12u8 amd64 [upgradable from: 7.88.1-10]";
        let pkgs = parse_upgradable(input);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "curl");
        assert_eq!(pkgs[0].new_version, "7.88.1-10+deb12u8");
        assert_eq!(pkgs[0].current_version.as_deref(), Some("7.88.1-10"));
        assert!(!pkgs[0].is_security);
    }

    #[test]
    fn parse_upgradable_debian_security_suite() {
        let input =
            "openssl/bookworm-security 3.0.15-1~deb12u1 amd64 [upgradable from: 3.0.11-1~deb12u2]";
        let pkgs = parse_upgradable(input);
        assert!(pkgs[0].is_security);
    }

    #[test]
    fn parse_upgradable_ubuntu_security_suite() {
        let input =
            "curl/jammy-security 7.81.0-1ubuntu1.16 amd64 [upgradable from: 7.81.0-1ubuntu1.15]";
        let pkgs = parse_upgradable(input);
        assert!(pkgs[0].is_security);
    }

    #[test]
    fn parse_upgradable_multi_origin_security_suite() {
        let input = "curl/jammy-updates,jammy-security 7.81.0-1ubuntu1.16 amd64 [upgradable from: 7.81.0-1ubuntu1.15]";
        let pkgs = parse_upgradable(input);
        assert!(pkgs[0].is_security);
    }

    #[test]
    fn parse_upgradable_non_security_suite_is_not_flagged() {
        let input = "vim/jammy-updates 2:8.2.3995-1ubuntu2.15 amd64 [upgradable from: 2:8.2.3995-1ubuntu2.14]";
        let pkgs = parse_upgradable(input);
        assert!(!pkgs[0].is_security);
    }

    #[test]
    fn security_count_counts_only_security_packages() {
        let input = "\
curl/jammy-security 7.81.0-1ubuntu1.16 amd64 [upgradable from: 7.81.0-1ubuntu1.15]
vim/jammy-updates 2:8.2.3995-1ubuntu2.15 amd64 [upgradable from: 2:8.2.3995-1ubuntu2.14]
";
        let info = HostInfo {
            upgradable: parse_upgradable(input),
            ..Default::default()
        };
        assert_eq!(info.security_count(), 1);
        assert_eq!(info.security_package_names(), vec!["curl"]);
    }

    #[test]
    fn parse_upgradable_single_package_without_current_version() {
        let input = "vim/stable 2:9.0.1378-2 amd64";
        let pkgs = parse_upgradable(input);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "vim");
        assert_eq!(pkgs[0].new_version, "2:9.0.1378-2");
        assert!(pkgs[0].current_version.is_none());
    }

    #[test]
    fn parse_upgradable_epoch_and_complex_version() {
        let input =
            "libssl3/stable-security 3.0.15-1~deb12u1 amd64 [upgradable from: 3.0.11-1~deb12u2]";
        let pkgs = parse_upgradable(input);
        assert_eq!(pkgs[0].new_version, "3.0.15-1~deb12u1");
        assert_eq!(pkgs[0].current_version.as_deref(), Some("3.0.11-1~deb12u2"));
    }

    #[test]
    fn parse_upgradable_multiple_packages() {
        let input = "\
Listing... Done
curl/stable 7.88.1-10+deb12u8 amd64 [upgradable from: 7.88.1-10]
vim/stable 2:9.0.1378-2 amd64 [upgradable from: 2:9.0.0-1]
";
        let pkgs = parse_upgradable(input);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "curl");
        assert_eq!(pkgs[1].name, "vim");
    }

    #[test]
    fn parse_upgradable_ignores_malformed_lines() {
        let input = "this line has no slash at all\n";
        assert!(parse_upgradable(input).is_empty());
    }

    // ── parse_rc_packages ─────────────────────────────────────────────────────

    #[test]
    fn parse_rc_packages_empty() {
        assert!(parse_rc_packages("").is_empty());
    }

    #[test]
    fn parse_rc_packages_includes_rc_excludes_others() {
        let input = "\
rc  old-lib            1.2.3   amd64  Some old lib
ii  bash               5.2-2   amd64  GNU Bourne Again shell
un  missing-pkg        <none>  <none> (no description)
rc  another-ghost      0.9     amd64  Ghost config
";
        let pkgs = parse_rc_packages(input);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "old-lib");
        assert_eq!(pkgs[1].name, "another-ghost");
    }

    #[test]
    fn parse_rc_packages_ignores_dpkg_header_lines() {
        let input = "\
Desired=Unknown/Install/Remove/Purge/Hold
| Status=Not/Inst/Conf-files/Unpacked/halF-conf/Half-inst/trig-aWait/Trig-pend
|/ Err?=(none)/Reinst-required (Status,Err: uppercase=bad)
||/ Name           Version      Architecture Description
+++-==============-============-============-=================================
rc  orphan-pkg     1.0          amd64        An orphaned package
ii  live-pkg       2.0          amd64        A live package
";
        let pkgs = parse_rc_packages(input);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "orphan-pkg");
    }

    // ── parse_held_manually ───────────────────────────────────────────────────

    #[test]
    fn parse_held_manually_empty() {
        assert!(parse_held_manually("").is_empty());
    }

    #[test]
    fn parse_held_manually_returns_packages_with_manual_hold_reason() {
        let input = "linux-image-amd64\ngrub-pc\n";
        let pkgs = parse_held_manually(input);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "linux-image-amd64");
        assert_eq!(pkgs[0].reason, HoldReason::ManualHold);
        assert_eq!(pkgs[1].name, "grub-pc");
    }

    #[test]
    fn parse_held_manually_trims_whitespace() {
        let input = "  linux-image-amd64  \n";
        let pkgs = parse_held_manually(input);
        assert_eq!(pkgs[0].name, "linux-image-amd64");
    }

    // ── parse_install_dry_run ─────────────────────────────────────────────────

    #[test]
    fn parse_install_dry_run_empty() {
        let (new, rem) = parse_install_dry_run("");
        assert!(new.is_empty());
        assert!(rem.is_empty());
    }

    #[test]
    fn parse_install_dry_run_new_packages() {
        let input = "\
Reading package lists... Done
Building dependency tree... Done
The following NEW packages will be installed:
  linux-image-6.1.0-28-amd64 linux-headers-6.1.0-28-amd64
The following packages will be upgraded:
  linux-image-amd64
1 upgraded, 2 newly installed, 0 to remove and 5 not upgraded.
";
        let (new, rem) = parse_install_dry_run(input);
        assert_eq!(
            new,
            vec!["linux-image-6.1.0-28-amd64", "linux-headers-6.1.0-28-amd64"]
        );
        assert!(rem.is_empty());
    }

    #[test]
    fn parse_install_dry_run_removals() {
        let input = "\
Reading package lists... Done
The following packages will be REMOVED:
  old-conflicting-pkg
1 to remove and 0 not upgraded.
";
        let (new, rem) = parse_install_dry_run(input);
        assert!(new.is_empty());
        assert_eq!(rem, vec!["old-conflicting-pkg"]);
    }

    #[test]
    fn parse_install_dry_run_new_and_removals() {
        let input = "\
The following NEW packages will be installed:
  new-dep
The following packages will be REMOVED:
  old-dep
";
        let (new, rem) = parse_install_dry_run(input);
        assert_eq!(new, vec!["new-dep"]);
        assert_eq!(rem, vec!["old-dep"]);
    }

    #[test]
    fn parse_install_dry_run_multiline_new_packages() {
        let input = "\
The following NEW packages will be installed:
  pkg-a pkg-b
  pkg-c
The following packages will be upgraded:
  mypkg
";
        let (new, _) = parse_install_dry_run(input);
        assert_eq!(new, vec!["pkg-a", "pkg-b", "pkg-c"]);
    }

    // ── parse_autoremovable ───────────────────────────────────────────────────

    #[test]
    fn parse_autoremovable_empty() {
        assert!(parse_autoremovable("").is_empty());
    }

    #[test]
    fn parse_autoremovable_no_removed_section() {
        let input = "0 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\n";
        assert!(parse_autoremovable(input).is_empty());
    }

    #[test]
    fn parse_autoremovable_single_line() {
        let input = "\
Reading package lists... Done
Building dependency tree... Done
The following packages will be REMOVED:
  libfoo1 libbar2 libbaz3
0 upgraded, 0 newly installed, 3 to remove and 0 not upgraded.
";
        let pkgs = parse_autoremovable(input);
        assert_eq!(pkgs, vec!["libfoo1", "libbar2", "libbaz3"]);
    }

    #[test]
    fn parse_autoremovable_multiline() {
        let input = "\
The following packages will be REMOVED:
  pkg-a pkg-b
  pkg-c
0 to remove.
";
        let pkgs = parse_autoremovable(input);
        assert_eq!(pkgs, vec!["pkg-a", "pkg-b", "pkg-c"]);
    }

    #[test]
    fn parse_autoremovable_strips_asterisk_suffixes() {
        let input = "\
The following packages will be REMOVED:
  libfoo1* libbar2*
";
        let pkgs = parse_autoremovable(input);
        assert_eq!(pkgs, vec!["libfoo1", "libbar2"]);
    }

    #[test]
    fn parse_kept_back_empty_output() {
        assert!(parse_kept_back("", &[]).is_empty());
    }

    #[test]
    fn parse_kept_back_no_section() {
        let input = "0 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\n";
        assert!(parse_kept_back(input, &[]).is_empty());
    }

    #[test]
    fn parse_kept_back_collects_packages_from_section() {
        let input = "\
Reading package lists...
The following packages have been kept back:
  linux-image-amd64 linux-headers-amd64
0 upgraded, 2 not upgraded.
";
        let pkgs = parse_kept_back(input, &[]);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].name, "linux-image-amd64");
        assert_eq!(pkgs[0].reason, HoldReason::KeptBack);
        assert_eq!(pkgs[1].name, "linux-headers-amd64");
    }

    #[test]
    fn parse_kept_back_multiple_indented_lines() {
        let input = "\
The following packages have been kept back:
  pkg-a pkg-b
  pkg-c
Inst pkg-d
";
        let pkgs = parse_kept_back(input, &[]);
        assert_eq!(pkgs.len(), 3);
        assert_eq!(pkgs[0].name, "pkg-a");
        assert_eq!(pkgs[1].name, "pkg-b");
        assert_eq!(pkgs[2].name, "pkg-c");
    }

    #[test]
    fn parse_kept_back_excludes_manually_held() {
        let manually_held = vec![HeldPackage {
            name: "linux-image-amd64".to_string(),
            reason: HoldReason::ManualHold,
            detail: None,
        }];
        let input = "\
The following packages have been kept back:
  linux-image-amd64 linux-headers-amd64
";
        let pkgs = parse_kept_back(input, &manually_held);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "linux-headers-amd64");
    }
}
