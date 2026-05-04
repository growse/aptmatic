use anyhow::Result;

use crate::apt::{
    HostInfo, parse_held_manually, parse_install_dry_run, parse_kept_back, parse_rc_packages,
    parse_upgradable,
};
use crate::config::HostConfig;
use crate::ssh::SshSession;

const LC: &str = "LC_ALL=C";

pub fn gather(cfg: &HostConfig) -> Result<HostInfo> {
    let sess = SshSession::connect(cfg)?;

    let sudo = if cfg.use_sudo { "sudo -n " } else { "" };

    // Running kernel
    let running_kernel = sess.exec("uname -r").unwrap_or_default().trim().to_string();

    // Latest installed kernel package
    let latest_kernel = {
        let out = sess
            .exec(&format!(
                "{LC} dpkg -l linux-image-[0-9]* 2>/dev/null | awk '/^ii/{{print $3}}' | sort -V | tail -1"
            ))
            .unwrap_or_default();
        let v = out.trim().to_string();
        if v.is_empty() { None } else { Some(v) }
    };

    // Reboot required flag
    let reboot_required = sess
        .exec(&format!(
            "{sudo}test -f /var/run/reboot-required && echo yes || echo no"
        ))
        .map(|o| o.trim() == "yes")
        .unwrap_or(false);

    // Upgradable packages
    let upgradable_out = sess
        .exec(&format!("{LC} apt list --upgradable 2>/dev/null"))
        .unwrap_or_default();
    let upgradable = parse_upgradable(&upgradable_out);

    // RC packages
    let rc_out = sess
        .exec(&format!("{LC} dpkg -l 2>/dev/null"))
        .unwrap_or_default();
    let rc_packages = parse_rc_packages(&rc_out);

    // Manually held packages
    let showhold_out = sess
        .exec(&format!("{LC} apt-mark showhold 2>/dev/null"))
        .unwrap_or_default();
    let manual_held = parse_held_manually(&showhold_out);

    // Kept-back packages (from simulate upgrade)
    let sim_out = sess
        .exec(&format!("{LC} apt-get -s upgrade 2>&1"))
        .unwrap_or_default();
    let kept_back = parse_kept_back(&sim_out, &manual_held);

    let mut held_packages = manual_held;
    held_packages.extend(kept_back);

    // For each kept-back package, run a dry-run install to discover why it
    // cannot be upgraded with a plain `apt upgrade` (e.g. new deps needed).
    for pkg in held_packages
        .iter_mut()
        .filter(|p| p.reason == crate::apt::HoldReason::KeptBack)
    {
        let install_sim = sess
            .exec(&format!("{LC} apt-get -s install {} 2>&1", pkg.name))
            .unwrap_or_default();
        let (new_deps, removals) = parse_install_dry_run(&install_sim);
        let mut parts: Vec<String> = Vec::new();
        if !new_deps.is_empty() {
            parts.push(format!("needs: {}", new_deps.join(", ")));
        }
        if !removals.is_empty() {
            parts.push(format!("removes: {}", removals.join(", ")));
        }
        if !parts.is_empty() {
            pkg.detail = Some(parts.join("; "));
        }
    }

    Ok(HostInfo {
        running_kernel,
        latest_kernel,
        reboot_required,
        upgradable,
        rc_packages,
        held_packages,
    })
}
