# aptmatic 🤖📦

> Because SSHing into 40 servers one by one to run `apt-get upgrade` is a cry for help.

[![CI](https://github.com/growse/aptmatic/actions/workflows/ci.yml/badge.svg)](https://github.com/growse/aptmatic/actions/workflows/ci.yml)

A snappy terminal UI for wrangling `apt` across a fleet of Debian/Ubuntu hosts — written in Rust, because I don't know how to code in OCaml.

```
╭─ aptmatic ─────────────────────────────────────────────────────────────╮
│ Hosts          │  Detail                                               │
│ ▸ webservers   │  web1.example.com                                     │
│    ● web1  [2] │  user: ubuntu  port: 22  sudo: true                   │
│    ● web2      │                                                       │
│ ▸ databases    │  Status: 2 upgrade(s) available                       │
│    ⠸ db1       │                                                       │
│    ● db2       │  Kernel                                               │
│                │  Running: 6.1.0-28-amd64                              │
│                │  Latest:  linux-image-6.1.0-32-amd64 ← reboot to activate │
│                │                                                       │
│                │  Upgradable                                           │
│                │  curl (7.88.1-10 →) 7.88.1-10+deb12u8                │
│                │  libcurl4 (7.88.1-10 →) 7.88.1-10+deb12u8            │
╰────────────────────────────────────────────────────────────────────────╯
 r:refresh  R:refresh all  u:update  U:upgrade  p:purge-rc  t:task output  z:zoom  q:quit
```

## Features

- 🖥️ **Multi-host dashboard** — see every host's status at a glance
- 👥 **Groups** — organise hosts and trigger actions on a whole group at once
- 🔑 **SSH native** — talks directly to each host over SSH, no agents or daemons required
- 🌀 **Live task output** — watch `apt-get upgrade` scroll by in real time
- 🐧 **Kernel tracking** — know which hosts are silently waiting for a reboot
- 📦 **Held/kept-back packages** — spot the stragglers and why they're stuck
- 🧹 **RC package purging** — one key to purge all those half-removed ghosts
- 🖱️ **Draggable divider** — because you deserve to customise your own TUI
- 🦀 **Written in Rust** — guaranteed\* to have no bugs

<sub>\* guarantee void where prohibited by logic</sub>

## Installation

```bash
cargo install --path .
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
| `r` | Refresh selected host(s) |
| `R` | Refresh **all** hosts |
| `u` | `apt-get update` on selected |
| `U` | `apt-get upgrade` on selected |
| `p` | Purge RC packages on selected |
| `t` / `Enter` | View live task output |
| `z` | Zoom — hide sidebar for clean copy/paste |
| `q` / `Esc` | Quit |

The sidebar divider is also mouse-draggable if you're feeling fancy.

## Development

```bash
just build   # build
just fmt     # format
just lint    # fmt check + clippy
```

## Why?

Managing a modest fleet of Linux boxes with `apt` should not require an orchestration platform, a PhD in Ansible, or accepting a cookie banner. aptmatic is a single binary, a TOML file, and a spare SSH key away from a good time.
