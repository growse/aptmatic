use anyhow::{Context, Result, anyhow, bail};
use dirs::home_dir;
use ssh2::{CheckResult, KnownHostFileKind, Session};
use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;

use crate::config::HostConfig;

pub struct SshSession {
    session: Session,
}

impl SshSession {
    /// Connect to a host, verify its key, and authenticate.
    pub fn connect(cfg: &HostConfig) -> Result<Self> {
        let addr = format!("{}:{}", cfg.hostname, cfg.port);
        let tcp = TcpStream::connect(&addr).with_context(|| format!("TCP connect to {addr}"))?;
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
        tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;

        let mut session = Session::new().context("create SSH session")?;
        session.set_tcp_stream(tcp);
        session
            .handshake()
            .with_context(|| format!("SSH handshake with {addr}"))?;

        verify_host_key(&session, &cfg.hostname, cfg.port)?;

        authenticate(&session, &cfg.user, cfg.identity_file.as_deref())
            .with_context(|| format!("authenticate to {addr} as {}", cfg.user))?;

        Ok(Self { session })
    }

    /// Execute a command and return combined stdout. Fails on non-zero exit status.
    pub fn exec(&self, cmd: &str) -> Result<String> {
        let mut channel = self.session.channel_session().context("open channel")?;
        channel.exec(cmd).with_context(|| format!("exec: {cmd}"))?;
        let mut out = String::new();
        channel.read_to_string(&mut out).context("read stdout")?;
        channel.wait_close().context("wait close")?;
        Ok(out)
    }

    /// Execute a command, calling `on_line` for each stdout line. Returns exit code.
    pub fn exec_streaming(&self, cmd: &str, mut on_line: impl FnMut(String)) -> Result<i32> {
        let mut channel = self.session.channel_session().context("open channel")?;
        channel.exec(cmd).with_context(|| format!("exec: {cmd}"))?;

        let mut buf = String::new();
        let mut raw = [0u8; 4096];
        loop {
            match channel.read(&mut raw) {
                Ok(0) => break,
                Ok(n) => {
                    buf.push_str(&String::from_utf8_lossy(&raw[..n]));
                    while let Some(pos) = buf.find('\n') {
                        let line = buf.drain(..=pos).collect::<String>();
                        on_line(line.trim_end_matches('\n').to_string());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(e) => return Err(e.into()),
            }
        }
        // Flush any remaining partial line
        if !buf.trim().is_empty() {
            on_line(buf.trim_end_matches('\n').to_string());
        }
        channel.wait_close().context("wait close")?;
        channel.exit_status().context("exit status")
    }
}

fn verify_host_key(session: &Session, hostname: &str, port: u16) -> Result<()> {
    let key_data = match session.host_key() {
        Some((k, _)) => k,
        None => bail!("server provided no host key"),
    };

    let home = home_dir().unwrap_or_else(|| PathBuf::from("/root"));
    let kh_path = home.join(".ssh").join("known_hosts");

    if !kh_path.exists() {
        // No known_hosts file; accept (TOFU).
        return Ok(());
    }

    let mut known = session.known_hosts().context("init known_hosts")?;
    known
        .read_file(&kh_path, KnownHostFileKind::OpenSSH)
        .with_context(|| format!("read {}", kh_path.display()))?;

    let host_str = if port == 22 {
        hostname.to_string()
    } else {
        format!("[{hostname}]:{port}")
    };
    match known.check(&host_str, key_data) {
        CheckResult::Match => Ok(()),
        CheckResult::NotFound => {
            // TOFU: key is new, we accept it silently.
            Ok(())
        }
        CheckResult::Mismatch => Err(anyhow!(
            "HOST KEY MISMATCH for {hostname}:{port} — possible MITM attack! \
             Remove the old key from ~/.ssh/known_hosts to continue."
        )),
        CheckResult::Failure => Err(anyhow!("known_hosts check failed for {hostname}:{port}")),
    }
}

fn authenticate(
    session: &Session,
    user: &str,
    identity_file: Option<&std::path::Path>,
) -> Result<()> {
    let home = home_dir().unwrap_or_else(|| PathBuf::from("/root"));
    let ssh_dir = home.join(".ssh");

    // 1. SSH agent — ignore the return value; some libssh2 builds return Err
    //    even when a key was accepted. The only reliable signal is session.authenticated().
    let agent_result = session.userauth_agent(user);
    if session.authenticated() {
        return Ok(());
    }

    // 2. Explicitly configured identity file (unencrypted or agent-held).
    if let Some(key) = identity_file
        && try_pubkey_auth(session, user, key)
    {
        return Ok(());
    }

    // 3. Default key files (unencrypted only; use the agent for passphrase-protected keys).
    let mut tried_keys: Vec<String> = Vec::new();
    for name in &["id_ed25519", "id_rsa", "id_ecdsa", "id_dsa"] {
        let key = ssh_dir.join(name);
        if key.exists() {
            tried_keys.push(name.to_string());
            if try_pubkey_auth(session, user, &key) {
                return Ok(());
            }
        }
    }

    let agent_err = match agent_result {
        Ok(_) => "agent returned ok but session not authenticated".to_string(),
        Err(e) => format!("agent: {e}"),
    };
    let keys_tried = if tried_keys.is_empty() {
        "no default key files found".to_string()
    } else {
        format!("tried key files: {}", tried_keys.join(", "))
    };
    bail!("all authentication methods failed for user {user} ({agent_err}; {keys_tried})")
}

fn try_pubkey_auth(session: &Session, user: &str, key: &std::path::Path) -> bool {
    // Prefer an OpenSSH certificate over the plain public key if one exists.
    // Convention: private key `id_ed25519` → certificate `id_ed25519-cert.pub`.
    let stem = key.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let cert = key.with_file_name(format!("{stem}-cert.pub"));
    let pub_key = key.with_extension("pub");

    let pub_ref: Option<&std::path::Path> = if cert.exists() {
        Some(&cert)
    } else if pub_key.exists() {
        Some(&pub_key)
    } else {
        None
    };

    session
        .userauth_pubkey_file(user, pub_ref, key, None)
        .is_ok()
        && session.authenticated()
}
