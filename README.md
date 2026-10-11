# KanoDesk

KanoDesk is a remote-desktop app written in Rust, inspired by RustDesk. It uses its own protocol on QUIC and is **not** wire-compatible with RustDesk.

Status: early scaffold. See [PROGRESS.md](PROGRESS.md) for what works and what's next.

## Spec

[`docs/`](docs/README.md) holds reverse-engineering notes on RustDesk 1.4.9 plus the design blueprint for this project. Start with [docs/16-clone-blueprint.md](docs/16-clone-blueprint.md) (roadmap and crate layout) and [docs/02-architecture.md](docs/02-architecture.md). The notes are a **behaviour spec only**: no RustDesk source code (AGPL-3.0) is used here.

## Layout

| Path | Purpose |
|---|---|
| `crates/proto` | Wire messages and stream framing |
| `crates/crypto` | Device identity, Noise IK handshake, SPAKE2 login |
| `crates/transport` | QUIC transport, NAT traversal, relay client |
| `crates/rendezvous` | ID server client |
| `crates/capture` | Screen capture trait and per-OS backends |
| `crates/codec` | Video encoder/decoder traits and backends |
| `crates/input` | Input event model and injection backends |
| `crates/host` | Controlled-side engine |
| `crates/client` | Controller-side engine |
| `crates/config` | Typed configuration |
| `crates/platform` | OS integration (service, sessions, autostart) |
| `apps/desktop` | Desktop UI (`kanodesk`) |
| `apps/service` | Windows service wrapper (`kanodesk-service`) |
| `apps/cli` | Provisioning CLI (`kanodesk-cli`) |
| `server/idserver` | ID/rendezvous server (`kanodesk-idserver`) |
| `server/relay` | Relay server (`kanodesk-relay`) |

## Build

Windows 10/11 x64 is the first target. The toolchain is pinned to stable by `rust-toolchain.toml`.

```bash
cargo build --workspace
```

```bash
cargo test --workspace
```

Before you send changes, check that these pass:

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work, as defined in the Apache-2.0 licence, is dual-licensed as above, without any additional terms or conditions.
