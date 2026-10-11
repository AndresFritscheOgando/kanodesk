# Build Prompts: Starting the KanoDesk Clone

Copy-paste prompts for **building** the Rust remote-desktop app from these docs. They follow the roadmap in [16-clone-blueprint.md](16-clone-blueprint.md) §6: phase 0 (spike), then phase 1 (MVP).

**How to use**
- Open Claude Code in the repo root (`D:\Workflow\repos\kanodesk`), not in `docs/`.
- Send **one prompt per message**. Don't move on until the prompt's "done when" check passes.
- For big prompts (marked 🧭), press Shift+Tab to enter plan mode first, review the plan, then let it build.
- Commit after every prompt that passes, so you can roll back.
- Start a fresh session when the context gets long. Prompt 0 plus `PROGRESS.md` brings it back up to speed.

---

## 0. Orientation and ground rules (send this first in every new session)
```
This repo builds KanoDesk, a remote-desktop app in Rust inspired by RustDesk. docs/ is the spec:
reverse-engineering notes on RustDesk 1.4.9 plus a design blueprint. Read docs/README.md,
docs/16-clone-blueprint.md and docs/02-architecture.md in full. If PROGRESS.md exists, read it too.

Ground rules for all work in this repo:
- We build option B from 16 §1: our own protocol on QUIC, not wire-compatible with RustDesk.
- Never copy RustDesk source code (it is AGPL-3.0). Use the docs only as a behaviour spec.
  Our licence is MIT OR Apache-2.0.
- Target Windows 10/11 x64 first. Keep OS-specific code behind traits in crates/platform,
  crates/capture and crates/input.
- Every crate gets unit tests; `cargo fmt`, `cargo clippy -- -D warnings` and `cargo test` must
  pass before you say a task is done.
- Keep PROGRESS.md updated: what's done, what's next, known issues, decisions taken.

Then summarize the phase 0 and phase 1 scope back to me in ten lines or fewer. Don't write code yet.
```

## 1. Scaffold the workspace 🧭
```
Create the cargo workspace from docs/16-clone-blueprint.md §2. Only create the crates needed for
phases 0–1 for now: proto, crypto, transport, rendezvous, capture, codec, input, host, client,
config, platform; apps/desktop, apps/service, apps/cli; server/idserver, server/relay.
Each crate gets a lib.rs with a module doc comment that says its purpose and which docs/ file
specifies it. Use a workspace-level [workspace.dependencies] table with the crates from 16 §3.
Also add: git init with a Rust .gitignore, rust-toolchain.toml (stable), LICENSE-MIT,
LICENSE-APACHE, a README, a CLAUDE.md with the ground rules from my previous message, PROGRESS.md,
and a GitHub Actions workflow running fmt, clippy and test on windows-latest.
Done when: `cargo build --workspace` and `cargo test --workspace` pass.
```

## 2. Spike, part A: capture and encode
```
Phase 0 spike, part A. Read docs/06-video-pipeline.md.
In crates/capture: define a `Capturer` trait (displays(), next_frame() -> BGRA frame with
timestamp, dirty flag) and a Windows DXGI Desktop Duplication backend using the `windows` crate.
Handle ACCESS_LOST (re-create the duplication) and timeouts (no new frame).
In crates/codec: define `Encoder`/`Decoder` traits and a VP9 backend (libvpx). Include BGRA→I420
conversion. If libvpx won't build on this machine within a reasonable effort, use openh264 for the
spike instead, tell me, and record the decision in PROGRESS.md.
Add an example `capture_bench` that captures the primary display for 10 s, encodes it, and prints
fps, average encode ms and average bytes per frame.
Done when: capture_bench runs at 1080p ≥ 30 fps on this machine.
```

## 3. Spike, part B: send, decode, render 🧭
```
Phase 0 spike, part B. Read docs/16-clone-blueprint.md §4 (the QUIC stream layout).
In crates/transport: a minimal quinn wrapper. Self-signed cert for now, a control bidi stream,
and one unidirectional stream per video display, using length-prefixed frames.
Add two spike binaries:
- `spike-host`: capture → encode → send over QUIC, listening on a port.
- `spike-viewer`: connect → decode → render in a window with winit + wgpu. Upload the Y/U/V planes
  as textures and convert to RGB in a WGSL shader. Request a keyframe on connect and after decode errors.
Overlay a frame counter and the measured latency (the host embeds a capture timestamp; assume the
clocks are on the same machine for now).
Done when: both binaries running on this machine show my screen at 1080p30 with under 100 ms latency.
Write the measured numbers into PROGRESS.md.
```

## 4. Protocol crate
```
Read docs/05-wire-protocol.md and docs/16-clone-blueprint.md §8.
In crates/proto: write our own .proto files (control, video, input, stats, display) for the MVP
message set in 16 §8, built with prost-build. Add a `Hello{version, capabilities}` exchange with
version negotiation. Add a framing codec for QUIC streams (u32 length prefix, max-size check) with
proptest round-trip tests and a fuzz target (cargo-fuzz) for the decoder.
Replace the ad-hoc framing in the spike with this crate.
Done when: the spike still works and the proptests pass.
```

## 5. Identity and secure handshake 🧭
```
Read docs/04-security-crypto.md and docs/16-clone-blueprint.md §5.
In crates/crypto:
- Device identity: an ed25519 key pair, created on first run and stored with Windows DPAPI
  (behind a `KeyStore` trait, with an in-memory implementation for tests).
- Session handshake: Noise IK (the `snow` crate), carried as the first messages on the QUIC
  control stream. QUIC TLS stays as transport protection; Noise provides end-to-end device
  authentication that survives a relay.
- Password login: SPAKE2 over the Noise channel, bound to the Noise handshake hash.
  Support a temporary password (random, regenerated after each session; length configurable) and
  an optional permanent password.
- Login throttling: per-peer and global failure counters with backoff (behaviour in 04 §5).
Include golden test vectors and tests for wrong password, replay, and a tampered transcript.
Done when: the spike requires the password before any video flows.
```

## 6. ID server and rendezvous client 🧭
```
Read docs/03-networking-rendezvous.md.
Build server/idserver (tokio + quinn, state in memory, persisted to SQLite with sqlx):
- Device registration: device ID ↔ public key, first-come key binding, server-signed (id, pubkey)
  records. The server's signing public key is pinned in the client config.
- Keepalive over a long-lived QUIC connection, online status, and a query for "is ID online".
- A connect request broker: forward the controller's request to the host with both sides'
  observed public addresses (for hole punching in the next prompt).
- Rate limits per IP and per target ID.
Build crates/rendezvous (the client side): register, keep alive, look up a peer, request a connection.
Device IDs: 9-digit numeric, generated by the server on first registration.
Done when: an integration test starts idserver plus two clients, and client A can look up client B
by ID and get its verified public key.
```

## 7. NAT traversal and relay 🧭
```
Read docs/03-networking-rendezvous.md (NAT test, hole punching, relay sections) and
docs/16-clone-blueprint.md §4 (review item 3.4).
- crates/transport: NAT type detection against the idserver, UDP hole punching with QUIC (both
  sides send to each other's public address while the broker coordinates timing), LAN direct
  connect when both are on the same subnet, and IPv6 when available.
- server/relay: pair two connections by a session UUID issued by the idserver and forward QUIC
  datagrams and streams blindly (the relay never sees Noise keys). Add bandwidth limits per session.
- Connection strategy in crates/client: try LAN, then a direct punch with a timeout, then the
  relay. Log which path won.
Done when: integration tests cover direct and relayed paths, and I can connect two machines on
different networks (I'll test manually; tell me the steps).
```

## 8. Remote input
```
Read docs/07-input-control.md (message semantics and clone notes).
- crates/proto: Mouse and Key messages as in 16 §8 (Map mode only: scancodes plus modifiers).
- crates/client: capture mouse and keyboard in the viewer window, map window coordinates to remote
  display coordinates (handle scaling and multi-monitor offsets), and coalesce mouse moves to one
  per frame interval. Send input on its own high-priority QUIC stream.
- crates/input: an `InputSink` trait with a Windows implementation (the `enigo` crate is fine).
  Release all held keys and buttons on disconnect or focus loss.
- Respect a host-side "keyboard/mouse allowed" permission flag.
Done when: I can control this machine from the viewer, and no keys get stuck after closing the viewer.
```

## 9. Host engine and connection manager 🧭
```
Read docs/11-host-server-side.md and docs/02-architecture.md (process roles and IPC).
- crates/host: the controlled-side engine. Accept a connection → handshake → login → ask the
  connection manager for approval → start the video and input services → stop them on disconnect.
  Permissions per session (keyboard, clipboard later, file transfer later), changeable live.
- crates/config: typed config with serde + toml (non-secret settings) and the option table pattern
  from docs/12-config-storage.md §6. Secrets go through the crypto KeyStore.
- crates/ipc (add it): named-pipe IPC with postcard messages between the host process and the UI.
- In apps/desktop, a connection-manager window: shows who is connecting, Accept/Deny, live permission
  toggles, and Disconnect. Approval modes: password only, click only, or both.
Done when: an incoming session shows the CM window, and denying or disconnecting stops video immediately.
```

## 10. Desktop UI (MVP) 🧭
```
Read docs/15-ui-and-ffi.md (windows and pages) and docs/01-feature-inventory.md (table A).
Build apps/desktop with egui/eframe (or Slint if you think it fits better; explain why first):
- Main window: my ID, my temporary password (with copy and refresh), a "connect to ID" box,
  a recent-sessions list, and a settings page (ID server address, server key, password options).
- Viewer window: the wgpu video from the spike embedded in the UI; toolbar with display picker,
  fit/original size, quality preset, Ctrl+Alt+Del (placeholder until the service exists), and disconnect.
- Error and status dialogs (connecting, wrong password, peer offline, relayed connection).
Keep the UI thin: all logic lives in crates/client and crates/host.
Done when: I can use the app end to end through the UI without the spike binaries.
```

## 11. Windows service and installer 🧭
```
Read docs/14-windows-platform.md (service, session follow, SAS, install modes).
- apps/service: a LocalSystem Windows service (windows-service crate) that runs the host process in
  the active console session and relaunches it when the session changes (lock/unlock, user switch).
- Make the UAC prompt and lock screen visible to the controller (that is why the service exists).
- Ctrl+Alt+Del through SendSAS from the service, with the policy handling described in 14 §3.
- apps/cli: --install, --uninstall, --get-id, --set-permanent-password, --import-config.
- An MSI built with cargo-wix, including firewall rules and silent install.
Done when: after installing the MSI and rebooting, I can connect before anyone logs in and see the
login screen.
```

## 12. MVP exit check
```
Phase 1 exit review against docs/16-clone-blueprint.md §6 and §7:
1. Build the NAT test lab from 16 §7 (Docker or VMs; tell me what you need from me) and record
   direct vs relay outcomes.
2. Run a security review of crates/crypto, crates/host and server/*: auth bypasses, missing rate
   limits, panics on malformed input, secrets in logs. Fix what you find.
3. Measure latency, fps and CPU at 0, 50 and 150 ms RTT (use clumsy on Windows) and write the
   numbers into PROGRESS.md.
4. List every phase 1 item as done, partial or missing.
```

---

## After the MVP: phase 2 onwards
Use this template for each feature in phase 2 to 5 of the roadmap (16 §6), one feature per prompt:
```
Next feature: <feature, e.g. "clipboard text and image sync">. Read <relevant doc, e.g.
docs/09-clipboard-file-transfer.md> for how RustDesk behaves and docs/16-clone-blueprint.md §4 for
our design decisions. Propose the messages, crate changes and tests first, wait for my OK, then
implement. Done when: <a check you can verify yourself>.
```
Suggested order for phase 2: multi-monitor → quality presets and adaptive bitrate (06, 16 §4) →
clipboard (09) → file transfer with resume (09) → audio (08) → chat (10) → permanent password and
peer lists (12).

## Tips
- If a prompt is too large for one go, ask for "the plan only", then send "do step 1", "do step 2", and so on.
- Prompts 2 and 3 are the riskiest (native codec builds, GPU rendering). If they stall, simplify:
  openh264 instead of libvpx, or a CPU YUV→RGB path and `pixels`/`softbuffer` instead of wgpu.
- The docs mark facts as [O] observed, [R] reconstructed, [D] design. Treat [R] values (timeouts,
  QoS constants) as starting points to tune, not as requirements.
