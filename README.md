<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/wordmark-dark.svg">
    <img src="assets/brand/wordmark.svg" width="320" height="64" alt="RustTerm">
  </picture>
</h1>

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/Built%20with-Rust-orange.svg)](Cargo.toml)
[![Platforms: Ubuntu and Windows](https://img.shields.io/badge/Platforms-Ubuntu%20%7C%20Windows-0078D4.svg)](#installation)
[![Release](https://github.com/oksyd/RustTerm/actions/workflows/release.yml/badge.svg)](https://github.com/oksyd/RustTerm/actions/workflows/release.yml)
[![Download](https://img.shields.io/badge/Download-Releases-2ea44f.svg)](https://github.com/oksyd/RustTerm/releases)

A native desktop workspace for terminals, remote desktops, and databases, built with Rust, GPUI, and the Alacritty terminal engine.

## Features

- **Terminal sessions** — local shells and SSH, searchable scrollback, output highlighting, optional line numbers and timestamps.
- **Organized workspaces** — draggable tabs, automatic SSH tab groups, and split panes.
- **Docker over SSH** — Compose project grouping, container details, bounded streaming logs, on-demand statistics, lifecycle controls, and container terminals.
- **Resource monitoring** — focused Linux SSH host and container CPU, memory, and network rates in the status bar, with shared detail panels and an optional sampling toggle.
- **File transfers** — an integrated SFTP browser and ZMODEM (`sz` / `rz`, Linux).
- **Remote desktops** — embedded RDP with TLS/NLA, clipboard integration, display controls, and RemoteApp windows.
- **Data tools** — native Redis, Elasticsearch, MySQL, PostgreSQL, ClickHouse, and SQLite clients alongside your terminals.
- **Desktop integration** — saved connection profiles, system credential storage, and English / Simplified Chinese interfaces.

To launch a published Windows application, select **RemoteApp** in an RDP connection and enter its alias (for example, `||notepad`). The server must have RemoteApp configured. Optional arguments and a working directory are supported. Standard RemoteApp is implemented; Windows server interoperability has not yet been verified on a live deployment.

ClickHouse connects over HTTP/HTTPS (8123/8443), with SSH tunnels, SQL queries, and CSV export. Its table editor supports browsing; use SQL to modify data.

Open **Docker** from a connected SSH terminal. The remote Linux host needs Docker CLI and permission to access its Unix socket; rootless and custom sockets are supported. No local Docker installation or exposed Docker TCP port is needed. Panel collection pauses when hidden; the active container console can continue sampling for its status bar. Container consoles follow Docker exec semantics: use `exit` to end the shell; closing a console disconnects it and may leave its process running.

## Installation

Download your platform's x86_64 package from [Releases](https://github.com/oksyd/RustTerm/releases). Each package includes a SHA-256 checksum.

**Ubuntu 24.04 / 26.04:** download the `.deb` package and install it (replace `<version>` with the release version):

```sh
sudo apt install ./rustterm-<version>-linux-x86_64.deb
```

Launch **RustTerm** from your application menu or run `rustterm`. In-app updates download the new `.deb` and request system authorization to install it through APT. You can also install a newer `.deb` manually; uninstall with `sudo apt remove rustterm`.

For installation without `sudo`, extract the `.tar.gz` and run `./install.sh` (Python 3.11+). This installs to `~/.local` and supports in-app updates.

An X11 or Wayland desktop, a working graphics driver, and system font libraries are required. Saving passwords requires a Secret Service; ZMODEM additionally requires `lrzsz`.

**Windows:** extract the portable `.zip` and launch `rustterm.exe`. Windows packages are preview builds; runtime validation is pending. RDP audio, file/folder redirection, exclusive keyboard capture, and MySQL/PostgreSQL SSH tunnels currently require Linux.

Manage update checks and an optional HTTP proxy in **Settings → Updates**. Replace the portable folder to update Windows.

## User files

| Content | Ubuntu (default) | Windows |
| --- | --- | --- |
| Settings, connections, SSH trust | `~/.config/rustterm/` | `%LOCALAPPDATA%\RustTerm\` |
| Session restore state | `~/.local/state/rustterm/` | `%LOCALAPPDATA%\RustTerm\state\` |
| Diagnostic logs | `~/.local/state/rustterm/logs/` | `%LOCALAPPDATA%\RustTerm\state\logs\` |
| Disposable cache | `~/.cache/rustterm/` | `%LOCALAPPDATA%\RustTerm\cache\` |

Ubuntu respects `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, and `XDG_CACHE_HOME`. Windows resolves LocalAppData through the system's Known Folder API. Passwords stay in the system credential store. Existing inline session state and diagnostic logs migrate on startup; connections and saved password references remain intact.

Use `rustterm --config-dir <directory>` for an isolated profile: its configuration, `state/`, and `cache/` live under that directory. Credentials still use the system vault. Back up the configuration directory to preserve settings and connections; include the state directory to preserve session restoration. User-selected databases, SSH keys, and downloaded files stay at their chosen locations.

## License

Licensed under [Apache-2.0](LICENSE).
