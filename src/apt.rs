/// A single upgradable apt package.
#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub new_version: String,
    pub current_version: Option<String>,
}

/// Represents a package held back from upgrades, along with the reason.
#[derive(Debug, Clone)]
pub struct HeldPackage {
    pub name: String,
    pub reason: HoldReason,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HoldReason {
    /// Held via `apt-mark hold`
    ManualHold,
    /// Kept back by apt resolver (e.g. new dependencies needed)
    KeptBack,
}

/// A package with dpkg rc status (removed but config files remain).
#[derive(Debug, Clone)]
pub struct RcPackage {
    pub name: String,
}

/// All gathered information for a single host.
#[derive(Debug, Clone, Default)]
pub struct HostInfo {
    pub running_kernel: String,
    pub latest_kernel: Option<String>,
    pub reboot_required: bool,
    pub upgradable: Vec<Package>,
    pub rc_packages: Vec<RcPackage>,
    pub held_packages: Vec<HeldPackage>,
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
    // Skip suite
    let mut parts = after.splitn(3, ' ');
    parts.next()?; // suite
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
    Some(Package {
        name,
        new_version,
        current_version,
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

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── parse_kept_back ───────────────────────────────────────────────────────

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
