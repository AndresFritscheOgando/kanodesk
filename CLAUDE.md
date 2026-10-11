# KanoDesk: instructions for AI agents

KanoDesk is a remote-desktop app in Rust inspired by RustDesk. `docs/` is the spec: reverse-engineering notes on RustDesk 1.4.9 plus a design blueprint. Before starting work, read `docs/README.md`, `docs/16-clone-blueprint.md`, `docs/02-architecture.md` and `PROGRESS.md`.

## Ground rules

- We build **option B** from `docs/16-clone-blueprint.md` §1: our own protocol on QUIC, not wire-compatible with RustDesk.
- **Never copy RustDesk source code** (it is AGPL-3.0). Use the docs only as a behaviour spec. Our licence is **MIT OR Apache-2.0**.
- Target **Windows 10/11 x64** first. Keep OS-specific code behind traits in `crates/platform`, `crates/capture` and `crates/input`.
- **Every crate gets unit tests.** These must pass before a task counts as done:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
- **Keep `PROGRESS.md` updated**: what's done, what's next, known issues, decisions taken.

## Conventions

- Packages are named `kanodesk-<dir>` (for example `crates/proto` → `kanodesk-proto`). Shared metadata comes from `[workspace.package]`.
- Add third-party dependencies to `[workspace.dependencies]` in the root `Cargo.toml` first, then opt in from the member with `foo.workspace = true`.
- Each crate's `lib.rs` starts with a module doc comment that gives its purpose and the `docs/` files that specify it, mirrored in `SPEC_DOCS`.
- The workspace enables `missing_docs`, so public items need doc comments.
- Binaries keep `main.rs` thin; logic lives in the crate's library so it can be unit-tested.
