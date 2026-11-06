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
- **File transfers** — an integrated SFTP browser and ZMODEM (`sz` / `rz`, Linux).
- **Remote desktops** — embedded RDP with TLS/NLA, clipboard integration, and display controls.
- **Data tools** — native Redis, Elasticsearch, MySQL, PostgreSQL, and SQLite clients alongside your terminals.
- **Desktop integration** — saved connection profiles, system credential storage, and English / Simplified Chinese interfaces.

## Installation

Download your platform's x86_64 package from [Releases](https://github.com/oksyd/RustTerm/releases). Each package includes a SHA-256 checksum.

**Ubuntu 24.04 / 26.04:** download the `.deb` package and install it (replace `<version>` with the release version):

```sh
sudo apt install ./rustterm-<version>-linux-x86_64.deb
```

Launch **RustTerm** from your application menu or run `rustterm`. In-app updates download the new `.deb` and request system authorization to install it through APT. You can also install a newer `.deb` manually; uninstall with `sudo apt remove rustterm`.

For installation without `sudo`, extract the `.tar.gz` and run `./install.sh` (Python 3.11+). This installs to `~/.local` and supports in-app updates.

An X11 or Wayland desktop, a working graphics driver, and system font libraries are required. Saving passwords requires a Secret Service; ZMODEM additionally requires `lrzsz`.

**Windows:** extract the portable `.zip` and launch `rustterm.exe`. Windows packages are preview builds; runtime validation is pending. RDP audio, file/folder redirection, exclusive keyboard capture, and SQL SSH tunnels currently require Linux.

Manage update checks and an optional HTTP proxy in **Settings → Updates**. Replace the portable folder to update Windows.

## License

Licensed under [Apache-2.0](LICENSE).
