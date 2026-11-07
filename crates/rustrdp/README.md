# RustRDP

[![Rust](https://img.shields.io/badge/rust-1.99.1%2B-orange.svg)](Cargo.toml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)

A Sans-I/O RDP client protocol library. Applications provide networking, verified
TLS authentication (CredSSP or RDSTLS handoffs), rendering and operating-system integration.

Default builds use only the standard library. Optional features add CAL exchange
(`licensing`), the graphics pipeline (`graphics`) and AAC/ADPCM audio (`audio-codecs`).

## Protocol coverage

Capability advertisements must match the implemented messages, enabled features
and connection settings. Server confirmation determines the codecs actually
permitted on the connection. Unsupported extensions must stay unadvertised.

| Area | Implemented scope and boundaries |
| --- | --- |
| Connection | NLA/HYBRID_EX and explicit RDSTLS negotiation; GCC, MCS channel joins, licensing completion, activation and reactivation. The caller supplies verified TLS and authentication. |
| Desktop settings | Color depth, desktop scale, monitor layout, font smoothing, independent audio playback and microphone flags. Extended Info uses an unnamed UTC time zone; local time-zone redirection is not implemented. |
| Session | Coalesced Share PDUs, startup status/error notifications, fast-path fragmentation, disconnects and frame acknowledgements. Packet errors terminate the state machine. |
| Bitmap graphics | Raw 8/15/16/24/32-bit, interleaved RLE, planar, NSCodec, palettes, surface commands and pointers. NSCodec and lossy planar modes are opt-in. |
| Graphics pipeline | RemoteFX, Progressive, ClearCodec, planar, alpha, AVC420 and AVC444/v2; surface copies, bitmap cache, reset and output mapping. AVC follows the confirmed version and flags. Scaled mapping and RemoteApp window mapping are not advertised. |
| Input | Fast/slow-path scancodes, Unicode, mouse buttons and wheels, with capability checks and release of tracked keys/buttons. Local lock-key synchronization and server IME/LED integration are not implemented. |
| Virtual channels | Bounded static-channel reassembly and dynamic-channel versions 1/2; unsupported listeners are declined. Dynamic-channel version 3 compression is not negotiated. |
| RemoteApp | Standard RAIL handshake, application execution, launch results, window/desktop orders, focus, system commands and local move/resize messages. The caller owns native windows and input. Enhanced RemoteApp/GFX window mapping and notification-area integration are not implemented. |
| Clipboard | Unicode text, image/HTML formats, file streaming and locking, subject to caller policy and peer capabilities. |
| Audio | Playback and microphone negotiation, PCM and optional AAC/ADPCM codecs; the caller owns devices and capture permission. |
| Display and devices | Dynamic monitor layout and drive-redirection messages; the caller performs filesystem operations. Printer, serial, smart-card and USB redirection are not implemented. |
| Multitransport | UDP transports, authenticated tunnel framing, channel migration and network measurement. The caller supplies TLS/DTLS, sockets and timing. |
| Licensing | Optional CAL challenge, issuance and cached-license exchange; requires caller-supplied identity, fresh entropy and secure storage. |

Server-redirection payloads are exposed to the caller for authenticated handoffs.
RustTerm currently follows same-endpoint RDSTLS handoffs; general connection-broker
redirection is not implemented. Gateway, Enhanced RemoteApp, GDI drawing orders, Standard
RDP Security and automatic reconnect cookies are also outside the current scope.

## Verification

The release build runs protocol tests for the shipped feature set on Ubuntu and
Windows, and minimal/all-feature suites on Ubuntu. Regression coverage includes
the offline server transcript, truncation at every connection phase, capability
selection, codec fixtures, channel framing and session lifecycle.

From this directory:

```sh
cargo test --no-default-features
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
```

Real-server coverage remains separate: the opt-in RustTerm RDP integration test
checks connection, decoded pixels and presentation acknowledgements. Unit tests
do not establish visual parity with mstsc or compatibility with every server.

Wire formats and capability rules follow Microsoft's
[RDP core specification](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/732394f5-e2b5-4ac5-8a0a-35345386b0d1),
[graphics negotiation specification](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/31c6e2b1-335b-4a75-9454-bb2309958c21)
and the extension sections cited in the corresponding source modules.

See the [API reference](https://docs.rs/rustrdp) and
[offline example](examples/replay.rs). To build the documentation from the crate directory:

```sh
cargo doc --all-features --no-deps
```
