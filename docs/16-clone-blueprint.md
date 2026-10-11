# 16: Clone Blueprint (Rust)

Everything in this file is **[D]**: design recommendations built on docs 01–15 and the bottlenecks in [00-review.md](00-review.md) §3.

## 1. First decision: compatible or independent?

| | **A. Wire-compatible with RustDesk 1.4.x** | **B. Own protocol** |
|---|---|---|
| Interop | Works with official clients and servers | Your own clients/servers only |
| Protocol | `proto/message.proto` + `rendezvous.proto`, TCP framing, sodium handshake | QUIC (`quinn`), multiplexed streams, modern auth (SPAKE2/Noise) |
| Bottlenecks | Inherits head-of-line blocking and the ack-window FPS cap | Can fix them |
| Licence | Implementing a protocol from docs is fine. **Copying RustDesk code ⇒ AGPL-3.0.** | Free to choose (MIT/Apache) |
| Effort | Lower at first (spec known), harder to evolve | Higher at first |

**Recommendation**: build **B**, with a **compatibility adapter** crate (`rd-compat`) added later if you need interop. Keep the domain model (sessions, displays, input events, file jobs) independent of the wire format.

## 2. Workspace layout

```
clone/
├─ Cargo.toml                      (workspace)
├─ crates/
│  ├─ proto/          prost/protobuf types (own .proto; optional rustdesk compat .proto)
│  ├─ crypto/         identity keys (ed25519), handshake (Noise IK / SPAKE2), key storage (DPAPI/keyring)
│  ├─ transport/      QUIC (quinn) + fallbacks: TCP+TLS, WebSocket; framing; NAT traversal; relay client
│  ├─ rendezvous/     client side of ID server protocol (register, punch, relay request, online query)
│  ├─ capture/        screen capture trait + backends (win: DXGI/WGC, mac: ScreenCaptureKit, linux: PipeWire/X11)
│  ├─ codec/          encoder/decoder trait + backends (vpx, openh264, MF/NVENC HW, dav1d)
│  ├─ input/          input event model + injection backends (per OS)
│  ├─ audio/          cpal capture/playback + opus
│  ├─ clipboard/      arboard + change listener; file-clipboard later
│  ├─ fs-transfer/    file jobs, chunking, hashing (blake3), resume
│  ├─ host/           controlled-side engine: services, permissions, connection handling
│  ├─ client/         controller-side engine: session state machine, decode, render handoff
│  ├─ config/         typed config (serde+toml), options catalogue, migration
│  ├─ ipc/            local IPC (named pipes / unix sockets) with typed binary messages
│  └─ platform/       service install, session follow, SAS, virtual display, autostart
├─ apps/
│  ├─ desktop/        UI app (Slint/egui or Flutter shell) — main window, viewer, file manager, CM
│  ├─ service/        Windows service / launchd / systemd unit wrapper
│  └─ cli/            provisioning: --get-id, --set-password, --import-config, --deploy
└─ server/
   ├─ idserver/       hbbs equivalent (registration, punch broker, online status, key signing)
   ├─ relay/          hbbr equivalent (UUID pairing, QUIC/TCP relay, rate limiting)
   └─ api/            optional: accounts, address book, audit (axum + sqlx)
```

## 3. Crate choices

| Concern | Choice | Notes |
|---|---|---|
| Async | `tokio` | Same as RustDesk |
| Protobuf | `prost` + `prost-build` | Wire-compatible with rust-protobuf |
| Transport | `quinn` (QUIC), `tokio-tungstenite` fallback | QUIC gives per-stream ordering, datagrams for audio/video, built-in congestion control |
| NAT traversal | `socket2` for reuse-port; STUN via `stun-rs`; optionally ICE via `webrtc-ice` | QUIC over UDP hole punching works well |
| Crypto | `ed25519-dalek`, `x25519-dalek`, `snow` (Noise) or `spake2`, `chacha20poly1305`, `blake3` | For compat mode: `crypto_box`, `xsalsa20poly1305`, `sha2` |
| Secrets at rest | Windows DPAPI via `windows` crate; `keyring` cross-platform | |
| Capture (Win) | `windows` crate DXGI duplication; or `windows-capture` (WGC) | |
| Colour convert | `yuvutils-rs` or a GPU compute shader | |
| Video codecs | `env-libvpx-sys`/`vpx-encode` (VP9), `openh264`, `dav1d` (AV1 decode), Media Foundation via `windows` for HW | Avoid FFmpeg licensing complexity if possible |
| Rendering | `wgpu` | Upload YUV planes and convert in a shader |
| Input | `enigo` (injection) | Keep injection behind a trait per OS |
| Audio | `cpal`, `opus`/`audiopus`, `rubato`, `ringbuf` | |
| Clipboard | `arboard`, `clipboard-master` | |
| Terminal | `portable-pty` (host), `alacritty_terminal` or `vt100` (client) | |
| Config | `serde`, `toml`, `directories` | |
| IPC | `interprocess` or `parity-tokio-ipc` + `postcard` | Binary, typed |
| HTTP API (client) | `reqwest` + rustls | |
| Server API | `axum`, `sqlx` (SQLite/Postgres) | |
| UI | **Slint** (native look, small) or **egui/eframe** (fast to build); or Flutter + `flutter_rust_bridge` v2 | See [15](15-ui-and-ffi.md) §7 |
| Service | `windows-service` | |
| Logging | `tracing` + `tracing-appender` (rolling files) | |
| Packaging | `cargo-wix` (MSI), `cargo-packager` | |

## 4. Design decisions that fix the observed bottlenecks

| Review § | Problem in RustDesk | Clone decision |
|---|---|---|
| 3.1 | One ordered stream for everything | QUIC: **control** stream (reliable, high priority); **input** as datagrams with sequence numbers or a dedicated stream; **video** as one unidirectional stream per display, with frames sent unreliably and keyframe recovery; **audio** as datagrams; **files** as a separate stream per job (low priority) |
| 3.2 | Frame-count ack window caps FPS at high RTT | Rate control driven by QUIC congestion state (BBR in quinn) plus receiver reports (loss, jitter, decode time). Pace by **bytes**, not frames in flight. |
| 3.3 | Shared encoder per display | Per-viewer encoder when resources allow; otherwise simulcast (two quality layers) with a per-viewer layer choice |
| 3.4 | TCP relay | Relay QUIC/UDP datagrams (TURN-like). Relays stay blind (E2E keys never reach them). |
| 3.5 | CPU colour conversion | GPU BGRA→NV12 (D3D11 video processor) feeding the HW encoder; zero-copy where possible |
| 3.6 | Frames through the UI layer | Decoder thread writes straight into a `wgpu` texture; the UI only composites |
| 3.7 | JSON IPC with file blocks | `postcard` IPC; file data streams through a user-context helper with its own pipe |
| 3.9 | Full clipboard push | Announce formats, transfer on demand (lazy), size caps |
| 3.10 | Heartbeat load | Registration over a long-lived QUIC connection with keepalive; hbbs-equivalent stores state in memory, persists to SQLite |

## 5. Security baseline

1. Device identity = an ed25519 key generated at install, stored with DPAPI (machine scope for the service).
2. ID server signs `(id, device_pubkey)`. The client pins the server key (shipped in the build or the deployment config).
3. Handshake: **Noise IK** (the client knows the host's static key from the signed lookup) → forward secrecy plus mutual authentication of the device keys.
4. User authentication over the Noise channel: **SPAKE2** with the access password (temporary or permanent). There is no password-equivalent hash to steal. The proof is bound to the handshake hash.
5. Optional TOTP second factor and trusted-device list ([04](04-security-crypto.md) §5).
6. Click-approve dialog in a separate interactive-desktop process, with a click delay (anti-spoofing).
7. Rate limiting per IP **and** per target ID, on both the server and the host.
8. Every remote permission is explicit, visible in the CM, revocable live, and audited.

## 6. Phased roadmap

| Phase | Scope | Exit criteria |
|---|---|---|
| **0. Spike** (1–2 wk) | Two processes on a LAN: DXGI capture → VP9 → QUIC → decode → wgpu window | 1080p30 at under 100 ms glass-to-glass on a LAN |
| **1. MVP** | ID server + relay; registration, hole punch, relay fallback; Noise + SPAKE2 login; screen view + mouse/keyboard (Map mode); temp password; minimal main window; service install on Windows | Connect by ID across two NATs; relay fallback works; passes a basic security review |
| **2. Daily-driver** | Multi-monitor, quality presets, adaptive bitrate, clipboard text/image, file manager (upload/download/resume), audio, chat, CM with accept/deny and permission toggles, permanent password, peer lists (recent/favourites) | Replaces RustDesk for basic support work |
| **3. Power features** | HW encode/decode (MF/NVENC), Translate keyboard mode, port forwarding, terminal, recording, screenshots, privacy/block input, Ctrl+Alt+Del, unattended service + session follow, LAN discovery, WOL | Feature parity for most users |
| **4. Management** | Accounts (OIDC), address book, device groups, audit, strategies/options push, deploy CLI, custom-branded builds | Self-hosted fleet management |
| **5. Extras** | File clipboard (delayed-render IDataObject), virtual display driver, remote printing, camera, switch sides, plugins, macOS/Linux backends | Parity+ |
| **(opt) Compat** | `rd-compat` crate: speak RustDesk `message.proto`/rendezvous to official peers | Interop test matrix green |

## 7. Testing strategy

| Layer | Approach |
|---|---|
| Protocol | Property tests (`proptest`) for framing/codec round-trips; golden vectors for the handshake |
| NAT traversal | Docker/netns lab with full-cone, restricted, port-restricted and symmetric NATs (`iptables MASQUERADE` variants); assert direct vs relay outcomes |
| Network conditions | `tc netem` / Clumsy (Windows): 0–300 ms RTT, 0–5 % loss, jitter; measure FPS, latency and input lag |
| Video | Glass-to-glass latency with an on-screen timestamp, captured remotely; PSNR/SSIM at given bitrates |
| Security | Fuzz all decoders (`cargo-fuzz`), wrong-password lockout, replay tests, MITM tests on the relay |
| Platform | VM matrix: Win10/11, RDP session, fast user switching, lock screen, UAC prompt, headless (no monitor) |
| Compat (if built) | Automated sessions against an official RustDesk client on a self-hosted keyless server |

## 8. Minimum message set for the MVP (own protocol)

```
Control:  Hello{version, caps}, AuthStart/AuthFinish (SPAKE2), LoginResult{peer_info|error},
          Permission{kind, enabled}, Close{reason}, Ping/Pong{ts}
Video:    StreamStart{display, codec, w, h}, Frame{display, pts, key, data}, KeyframeRequest{display}
Input:    Mouse{kind, buttons, x, y, dx, dy}, Key{code, down, mods, mode}, Text{utf8}
Stats:    ReceiverReport{display, lost, jitter_ms, decode_ms, rendered_fps}
Display:  Displays{list}, SelectDisplay{ids}, SetResolution{display, w, h}
```
Add clipboard, file, audio and chat messages in phase 2. For field-level ideas, borrow freely from [`proto/message.proto`](proto/message.proto). It is a well-tested set of messages.
