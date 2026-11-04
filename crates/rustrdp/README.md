# RustRDP

[![Rust](https://img.shields.io/badge/rust-1.99.1%2B-orange.svg)](Cargo.toml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)

A Sans-I/O RDP client protocol library. Applications provide networking, verified
TLS/CredSSP authentication, rendering and operating-system integration.

Default builds use only the standard library. Optional features add CAL exchange
(`licensing`), the graphics pipeline (`graphics`) and AAC/ADPCM audio (`audio-codecs`).

See the [API reference](https://docs.rs/rustrdp) and
[offline example](examples/replay.rs). To build the documentation from the crate directory:

```sh
cargo doc --all-features --no-deps
```
