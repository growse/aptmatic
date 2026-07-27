# aptmatic 🤖📦

> Because SSHing into 40 servers one by one to run `apt-get upgrade` is a cry for help.

[![CI](https://github.com/growse/aptmatic/actions/workflows/ci.yml/badge.svg)](https://github.com/growse/aptmatic/actions/workflows/ci.yml)

A snappy terminal UI for wrangling `apt` across a fleet of Debian/Ubuntu hosts — written in Rust, because I don't know how to code in OCaml.

```
╭─ aptmatic ──────────────────────────────────────────────────────╮
│ Hosts          │Detail                                          │
│ ▸ webservers   │web1.example.com                                │
│    ● web1  [2] │user: ubuntu  port: 22  sudo: true              │
│    ● web2      │                                                │
│ ▸ databases    │Status: 2 upgrade(s) available (1 security)     │
│    ⠼ db1       │                                                │
│    ● db2       │Kernel                                          │
│                │Running: 6.1.0-28-amd64                         │
│                │Latest:  linux-image-6.1.0-32-amd64 ← reboot    │
│                │                                                │
│                │Upgradable                                      │
│                │[sec] curl (7.88.1-10 → 7.88.1-10+deb12u8)      │
│                │      libcurl4 (7.88.1-10 → 7.88.1-10+deb12u8)  │
╰─────────────────────────────────────────────────────────────────╯
 r:update+refresh  R:refresh all  u:upgrade  U:upgrade all
 f:full-upgrade  F:full-upgrade all  s:sec-upgrade  S:sec-upgrade all
 a:autoremove  A:autoremove all  p:purge-rc  c:config files  b:reboot
 t:task output  z:zoom  /:search  q:quit
```

## Features

- 🖥️ **Multi-host dashboard** — see every host's status at a glance
- 👥 **Groups** — organise hosts and trigger actions on a whole group at once
- 🔑 **SSH native** — talks directly to each host over SSH, no agents or daemons required
- 🌀 **Live task output** — watch `apt-get upgrade` scroll by in real time
- 🐧 **Kernel tracking** — know which hosts are silently waiting for a reboot
- 📦 **Held/kept-back packages** — spot the stragglers and why they're stuck
- 🛡️ **Security-update badge** — upgradable packages from a security suite are called out separately, with a key to upgrade just those
- 🔍 **Sidebar search** — `/` to filter hosts/groups by name in a big fleet
- 💾 **Cached last-known state** — the dashboard isn't blank on startup while it reconnects
- 🚦 **Bounded connection concurrency** — "all hosts" actions queue instead of opening a connection per host at once
- 🧹 **RC package purging** — one key to purge all those half-removed ghosts
- 📝 **Pending config files** — upgrades never stop to ask about a changed conffile; the new version is counted per host and reviewed later, with a diff, on your schedule
- ⬆️ **Full-upgrade & autoremove** — `apt-get full-upgrade` and `apt-get autoremove --purge`, on selected hosts or the whole fleet
- 🔁 **Confirmed reboot** — type the hostname to confirm before a host goes down
- 🖱️ **Draggable divider** — because you deserve to customise your own TUI
- 🦀 **Written in Rust** — guaranteed\* to have no bugs

<sub>\* guarantee void where prohibited by logic</sub>

## Installation

```bash
cargo install aptmatic
```

Or build from source:

```bash
cargo build --release
# binary at ./target/release/aptmatic
```

## Configuration

aptmatic looks for its config at `~/.config/aptmatic.toml` by default. Pass `-c /path/to/config.toml` to override.

```toml
[defaults]
user = "ubuntu"
port = 22
use_sudo = true

[[groups]]
name = "webservers"

[[groups.hosts]]
hostname = "web1.example.com"

[[groups.hosts]]
hostname = "web2.example.com"
user = "admin"   # override per-host

[[groups]]
name = "databases"

[[groups.hosts]]
hostname = "db1.example.com"
```

## Keybindings

| Key | Action |
|-----|--------|
| `↑` / `k` | Move up |
| `↓` / `j` | Move down |
| `/` | Search/filter the sidebar by hostname or group name |
| `r` | `apt-get update` + refresh on selected |
| `R` | `apt-get update` + refresh on **all** hosts |
| `u` | `apt-get upgrade` on selected |
| `U` | `apt-get upgrade` on **all** hosts |
| `f` | `apt-get full-upgrade` on selected |
| `F` | `apt-get full-upgrade` on **all** hosts |
| `s` | Upgrade **security-only** packages on selected |
| `S` | Upgrade **security-only** packages on **all** hosts |
| `a` | `apt-get autoremove --purge` on selected |
| `A` | `apt-get autoremove --purge` on **all** hosts |
| `p` | Purge RC packages on selected |
| `c` | Review pending config files on the selected host |
| `b` | Reboot the selected host (type the hostname to confirm) |
| `t` / `Enter` | View live task output |
| `z` | Zoom — hide sidebar for clean copy/paste |
| `q` / `Esc` | Quit |

The sidebar divider is also mouse-draggable if you're feeling fancy.

### Pending config files

Upgrades run with `--force-confdef --force-confold`, so dpkg never stops to ask what to do about a config file you've edited — it keeps yours and drops the maintainer's version next to it as `.dpkg-dist`. Hosts carrying unresolved files show a `[n cfg]` badge in the sidebar; `c` opens a review pane listing them with a diff against the live file:

| Key | Action |
| --- | --- |
| `↑` / `↓` | Select a file |
| `PgUp` / `PgDn` | Scroll the diff |
| `d` | Discard the new version, keeping your current config |
| `a` | Install the new version, backing your current one up to `.dpkg-old` (asks first) |
| `Esc` | Close |

Applying a config file does not restart anything — restart the affected service yourself once you're happy with it.

While searching, type to filter, `↑`/`↓` to jump between matches, `Enter`/`Esc` to stop editing (the filter stays applied — clear it by backspacing to empty).

Actions on a whole group or "all hosts" are queued through a small connection pool (8 at a time) rather than opening an SSH connection per host simultaneously.

## Development

```bash
just build   # build
just fmt     # format
just lint    # fmt check + clippy
```

## Why?

Managing a modest fleet of Linux boxes with `apt` should not require an orchestration platform, a PhD in Ansible, or accepting a cookie banner. aptmatic is a single binary, a TOML file, and a spare SSH key away from a good time.
