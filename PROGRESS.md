# Progress

Roadmap: [docs/16-clone-blueprint.md](docs/16-clone-blueprint.md) §6. Build prompts: [docs/next-session-prompts.md](docs/next-session-prompts.md).

## Done

- **2026-10-11: workspace scaffold** (prompt 1).
  - Cargo workspace with the phase 0–1 crates: 11 library crates in `crates/`, 3 apps in `apps/`, 2 servers in `server/`.
  - Each crate has a `lib.rs` with a module doc comment (purpose and spec docs), a `SPEC_DOCS` constant, and a unit test that checks the referenced docs exist. Apps and servers also have a thin `main.rs`.
  - `[workspace.dependencies]` lists the crates from 16 §3 with their current versions. The whole table was checked to resolve together (`cargo generate-lockfile` in a scratch crate).
  - Repo files: `rust-toolchain.toml` (stable), `.gitignore`, `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`, `CLAUDE.md`, and a GitHub Actions workflow (`.github/workflows/ci.yml`) that runs fmt, clippy and test on `windows-latest`.

## Next

1. **Phase 0 spike, part A** (prompt 2): `Capturer` trait and DXGI backend in `crates/capture`; `Encoder`/`Decoder` traits and VP9 (or openh264 fallback) in `crates/codec`; `capture_bench` example. Target: 1080p ≥ 30 fps.
2. **Phase 0 spike, part B** (prompt 3): quinn wrapper in `crates/transport`, plus `spike-host` and `spike-viewer` binaries (winit + wgpu, YUV→RGB in WGSL). Target: 1080p30 under 100 ms.

## Known issues

- No crate has real functionality yet; the binaries only print their version.
- No internal dependencies are wired between crates yet; they get added as code needs them.
- The CI workflow has not run yet: there is no GitHub remote.

## Decisions

| Date | Decision | Why |
|---|---|---|
| 2026-10-11 | Option B: own protocol on QUIC, MIT OR Apache-2.0 | Ground rules; 16 §1 |
| 2026-10-11 | Package names `kanodesk-<dir>`; binaries `kanodesk`, `kanodesk-service`, `kanodesk-cli`, `kanodesk-idserver`, `kanodesk-relay` | Avoids clashes with crates.io names such as `proto` or `config` |
| 2026-10-11 | Edition 2024, resolver 3, `rust-version = 1.85` | Current stable is 1.99; 1.85 is the minimum for edition 2024 |
| 2026-10-11 | Apps and servers have `lib.rs` + thin `main.rs` | Lets every crate carry unit tests, as the ground rules require |
| 2026-10-11 | Pre-release crates pinned to the last stable line: `rustls 0.23`, `spake2 0.4`, `winit 0.30`, `audiopus 0.2` | crates.io's latest for these is a dev/pre/beta/rc release; quinn 0.11 also needs rustls 0.23 |
| 2026-10-11 | RustDesk-compat crypto crates (`crypto_box`, `xsalsa20poly1305`, `sha2`) left out of 16 §3's list | Only needed for the optional `rd-compat` crate, which is out of scope |
| 2026-10-11 | Added `rcgen`, `winit`, `tokio-util`, `bytes`, `zeroize`, `rand`, `tracing-subscriber`, `thiserror`, `anyhow`, `clap` to the table | Needed by the phase 0–1 prompts (self-signed certs, viewer window, CLI, errors) |
| 2026-10-11 | Workspace lints: `missing_docs = warn` (an error under clippy `-D warnings`) | Keeps the docs-first convention enforced |
| 2026-10-11 | UI toolkit still open: both `slint` and `eframe` are in the table | Decide before the phase 1 main window; must take a wgpu texture without per-frame copies (02 §6) |
