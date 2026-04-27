use anyhow::Result;

use crate::apt::{
    parse_held_manually, parse_kept_back, parse_rc_packages, parse_upgradable, HostInfo,
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
        .exec(&format!("{sudo}test -f /var/run/reboot-required && echo yes || echo no"))
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

    Ok(HostInfo { running_kernel, latest_kernel, reboot_required, upgradable, rc_packages, held_packages })
}
