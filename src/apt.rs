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
    Some(Package { name, new_version, current_version })
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
                Some(RcPackage { name: name.to_string() })
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
        .map(|l| HeldPackage { name: l.trim().to_string(), reason: HoldReason::ManualHold })
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
                        pkgs.push(HeldPackage { name: name.to_string(), reason: HoldReason::KeptBack });
                    }
                }
            } else {
                in_section = false;
            }
        }
    }
    pkgs
}
