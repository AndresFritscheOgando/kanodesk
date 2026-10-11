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
| `apps/spike` | Phase 0 spike: `spike-host` and `spike-viewer` |
| `server/idserver` | ID/rendezvous server (`kanodesk-idserver`) |
| `server/relay` | Relay server (`kanodesk-relay`) |

## Build

Windows 10/11 x64 is the first target. The toolchain is pinned to stable by `rust-toolchain.toml`.

Prerequisites:

- Visual Studio Build Tools with the C++ workload (MSVC).
- [vcpkg](https://github.com/microsoft/vcpkg) for native libraries (currently libvpx). `vcpkg.json` pins the versions; `.cargo/config.toml` points the build at `./vcpkg_installed/`.

Install the native libraries once from the repo root (about 3 minutes the first time):

```bash
vcpkg install --triplet x64-windows-static-md
```

Then build:

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

Tests that need a real interactive desktop (DXGI capture) are `#[ignore]`d because CI runners have none. Run them locally with:

```bash
cargo test -p kanodesk-capture -- --ignored
```

### Phase 0 spike: stream your screen

Two binaries in `apps/spike`. Start the host; it prints a certificate fingerprint and the exact viewer command:

```bash
cargo run --release -p kanodesk-spike --bin spike-host -- --listen 127.0.0.1:9000 --fps 30
```

Then, in a second terminal, run the viewer with the fingerprint the host printed:

```bash
cargo run --release -p kanodesk-spike --bin spike-viewer -- --connect 127.0.0.1:9000 --pin <fingerprint>
```

The viewer window shows the host's primary display with a frame counter and latency overlay, and prints per-second stats. Add `--seconds N` to stop after N seconds with a summary, or `--vsync` for Fifo presentation. The latency figure is only meaningful when both run on the same machine. The spike has no real authentication; keep it on localhost or a trusted LAN.

### Capture benchmark

Captures the primary display, converts to I420 and encodes VP9, then prints fps, per-stage times and bytes per frame. Put something moving on screen first. The flags are listed at the top of `crates/codec/examples/capture_bench.rs`.

```bash
cargo run --release -p kanodesk-codec --example capture_bench -- --seconds 10 --fps 30
```

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work, as defined in the Apache-2.0 licence, is dual-licensed as above, without any additional terms or conditions.
