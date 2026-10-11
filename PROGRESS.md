# Progress

Roadmap: [docs/16-clone-blueprint.md](docs/16-clone-blueprint.md) §6. Build prompts: [docs/next-session-prompts.md](docs/next-session-prompts.md).

## In progress (prompt 5: crypto) — paused 2026-10-11, uncommitted

Done and tested (all workspace tests pass, fmt + clippy clean):
- `crates/crypto`: ed25519 `DeviceIdentity` (also used for Noise via Edwards→Montgomery), `KeyStore` trait with `MemoryKeyStore` and `DpapiKeyStore` (user/machine scope, entropy bound to entry name, atomic writes), Noise IK (`noise.rs`, prologue = label + channel binding), SPAKE2 login with temporary/permanent slots and two-way key confirmation bound to the Noise handshake hash (`login.rs`), temporary/permanent passwords (`password.rs`), throttling per 04 §7 (`throttle.rs`; the prompt's "04 §5" is 2FA). Golden vectors in `golden.rs` (RFC 8032 test 1 external; the rest regression vectors). Tests cover wrong password, replay, tampering, channel-binding mismatch and identity mismatch.
- `control.proto`: `NoiseHandshake`, `Sealed`, slot-based `AuthStart`/`AuthFinish`/`AuthConfirm`, `LoginResult.retry_after_ms`/`confirmation`, `CloseReason::RateLimited`.
- `kanodesk_transport::endpoint::channel_binding` (TLS exporter), tested.
- `apps/spike/src/secure.rs`: `client_session` / `host_session` running Noise → sealed `Hello` → SPAKE2 login → `LoginResult` with throttling and temporary-password rotation; in-memory tests for success, wrong password, rate limiting, rotation, wrong host key and binding mismatch.

Still to do:
1. Wire `secure.rs` into `spike-host` (DPAPI key dir, print device key + temporary password, `--password-length`, `--numeric`, `--permanent-password`; authenticate **before** replacing the active viewer; open the video stream only after login; rotate the temporary password after each session) and `spike-viewer` (`--host-key` required, `--password` or prompt, `--pin` optional; sealed control loop with `select!`).
2. Live check of the done criterion: wrong password gets no video, right password streams; add a viewer probe that skips login and confirms no video stream is offered.
3. Decisions/known issues for this prompt into the tables below (one key for Ed25519+X25519, TLS channel binding, client-proves-first login, permanent password stored DPAPI-protected rather than via OPAQUE), then commit.

## Done

- **2026-10-11: protocol crate** (prompt 4).
  - `crates/proto/proto/kanodesk/v1/`: our own `.proto` files for the 16 §8 MVP set. `control.proto`: `StreamHeader`, `Hello{version, min_version, capabilities, agent}`, `AuthStart`/`AuthFinish` (SPAKE2 placeholders), `LoginResult{PeerInfo | LoginError}`, `Permission`, `Close{reason, detail}`, `Ping`/`Pong`, and the `ControlMessage` envelope. `video.proto`: `Codec`, `StreamStart`, `VideoFrame{display, seq, pts_ms, key, data, capture_us, host_delay_us}`, `KeyframeRequest`, `VideoMessage`. `input.proto`: `MouseEvent`, `KeyEvent` (USB HID codes, Map/Translate), `TextInput`, `InputMessage{seq, …}`. `stats.proto`: `ReceiverReport`. `display.proto`: `DisplayInfo`, `Displays`, `SelectDisplay`, `SetResolution`. Compiled in `build.rs` by **protox** + prost-build, so no `protoc` binary is needed.
  - `hello.rs`: version negotiation. Each side sends its `[min_version, version]` range and capabilities; `negotiate` picks the highest common version and the intersection of known capabilities, symmetrically, so no extra round trip is needed. `PROTOCOL_VERSION = 1`.
  - `codec.rs`: `FrameCodec` (u32 big-endian length prefix; the length is checked against the limit, default 16 MiB, **before** reserving memory) and `ProtoCodec<T>`, both tokio-util `Decoder`/`Encoder`s. `io.rs`: async `MessageReader`/`MessageWriter` with `into_typed` (switch message type after a stream header without losing buffered bytes). `From` impls for every envelope arm.
  - `kanodesk-transport` now uses it: its own `framing.rs` is gone; streams start with a proto `StreamHeader` and return typed `ControlReader/Writer` and `VideoReader/Writer`. ALPN changed from `kanodesk-spike/0` to an unversioned `kanodesk`.
  - The spike's hand-rolled `wire.rs` is gone. The viewer and host exchange `Hello` first; the host refuses incompatible viewers with `Close{IncompatibleVersion}` (or `Error` if the viewer lacks VP9), and sends `StreamStart` before frames. Viewer flag `--protocol-version N` exists to test this.
  - Fuzzing: `crates/proto/fuzz` (cargo-fuzz, own workspace, nightly) target `decode_frames`: arbitrary bytes in arbitrary chunks through the frame decoder, every frame decoded as each envelope, decode→encode→decode stability checked. The body lives in `kanodesk_proto::fuzz` so the stable test suite runs it too. **Runs: 1.8 M inputs in 60 s and 0.5 M in 21 s under AddressSanitizer, no crashes.**
  - Tests: 24 in `kanodesk-proto`, including 8 proptests (frames survive any chunking; control/video/input messages round-trip; random bytes never panic the decoder; `Hello` arm preserved; negotiation symmetric and within both ranges), plus truncation, oversize-without-allocation, garbage payloads, stable enum values, and header-then-messages in one read. Transport tests rewritten on proto messages (8). All 88 workspace tests pass; the 3 `#[ignore]`d hardware/GPU tests pass locally.
  - **Spike re-measured after the switch** (same setup as part B, 20 s runs, 1920×1080): 30.0 fps, latency avg / p95 / p99 / max **17.6 / 20.6 / 21.9 / 29.6 ms** and **13.4 / 15.6 / 16.7 / 19.0 ms** in two runs; a third had p50 10.4 / p99 30.5 ms. Same range as before the change, so no regression. Incompatible-version viewer: refused with the host's reason shown; the next normal viewer connected fine.
- **2026-10-11: phase 0 spike, part B: send, decode, render** (prompt 3). **Phase 0 exit criterion met.**
  - `crates/transport`: quinn wrapper. TLS 1.3 (rustls, ring provider) on a per-run self-signed certificate; the client pins it by BLAKE3 fingerprint (`tls.rs`; `--insecure` skips the check for local tests). `framing.rs`: `u32` big-endian length prefix, 16 MiB cap, clean-EOF vs truncation. `streams.rs`: stream layout from 16 §4 — the client opens one **control** bidi stream (priority 10), the server opens one **video** uni stream per display (priority 0); each starts with a `KD` + kind header. `endpoint.rs`: server/client endpoints, keep-alive 5 s, idle timeout 30 s, 8 MiB stream / 32 MiB connection windows, `close`/`shutdown` helpers so peers notice disconnects at once.
  - `apps/spike` (`kanodesk-spike`): `spike-host` (capture → I420 → VP9 → video stream, paced at `--fps`; frames sent only when the desktop changed or a keyframe was requested) and `spike-viewer` (QUIC client thread → decoder thread → one-slot "latest frame" mailbox → winit + wgpu window). Y/U/V uploaded as three `R8Unorm` textures; `video.wgsl` converts BT.601 limited range to RGB, letterboxes, and draws the overlay (frame number, latency avg/p95, fps, size) with a built-in 3×5 bitmap font. The viewer asks for a keyframe on connect and after any decode error (then drops frames until a keyframe; requests rate-limited to one per 250 ms). Spike wire format (`wire.rs`) is a throwaway until prompt 4.
  - Tests: 13 in `kanodesk-transport` (framing, headers, fingerprints, pin rejection, loopback control+video streams, prompt close) and 15 in `kanodesk-spike` (wire, stats, font, letterbox, uniform layout, WGSL validated with naga incl. the uniform size), plus an `#[ignore]`d GPU test that renders offscreen and checks BT.601 red, the letterbox bar and overlay pixels (passes locally).
  - **Measured on this machine** (same hardware as part A; both binaries in release on one machine over loopback; 1920×1080 primary display; VP9 3000 kbps, speed 6). Latency = viewer `present()` − DXGI present time of the captured image, both on the QPC clock. The viewer's own scan-out and display lag are **not** included (add up to one 60 Hz refresh plus panel lag for true glass-to-glass).

    | Run | fps | Latency avg / p50 / p95 / p99 / max | Host (capture→encoded) | Network | Decode | Render | Bitrate |
    |---|---|---|---|---|---|---|---|
    | Mailbox present, 20 s | **30.0** | **11.9** / 12.2 / 13.2 / 13.6 / 14.7 ms | 8.8 ms | 0.3 ms | 1.9 ms | 1.0 ms | ~2.9 Mbit/s |
    | Mailbox present, 20 s (earlier run) | 30.0 | 15.6 / 15.7 / 17.0 / 17.9 / 19.1 ms | 12.5 ms | 0.3 ms | 1.8 ms | 1.0 ms | ~3.0 Mbit/s |
    | Fifo (vsync), 10 s | 30.0 | 14.0 / 14.1 / 15.8 / 16.6 / 17.6 ms | 11.0 ms | 0.3 ms | 2.1 ms | 0.7 ms | — |
    | Second viewer taking over, 5 s | 30.2 | 20.1 / 20.4 / 21.6 / 21.9 / 22.6 ms | 17.2 ms | 0.3 ms | 1.8 ms | 0.9 ms | — |

    Host encode time was 2.9–4.0 ms avg (p95 ≤ 4.6 ms). Most of "host" is the wait between the desktop update and the next 33 ms capture tick, which is why it varies between runs (8.8–17.2 ms). **Exit criterion met on the numbers: 1080p30 at well under 100 ms.** Verified by the agent: every run presented 1920×1080 frames at 30 fps, and the offscreen GPU test confirms the shader's colour conversion, letterbox and overlay. Not verified by the agent: how the window looked on screen (it had no screenshot access); a person should glance at it once.
- **2026-10-11: phase 0 spike, part A: capture and encode** (prompt 2).
  - `crates/capture`: `Capturer` trait (`displays()`, `open()`, `next_frame(timeout)` → tightly packed BGRA `Frame` with `timestamp_us` and a `dirty` flag; `Ok(None)` = no change within the timeout). DXGI Desktop Duplication backend (`dxgi.rs`): device created on the output's own adapter, GPU copy to a staging texture, row copy that strips the pitch. `ACCESS_LOST`, device removed/reset → rebuild on the next call. `E_ACCESSDENIED`, `NOT_CURRENTLY_AVAILABLE`, `SESSION_DISCONNECTED` → wait out the timeout and retry. Timeout → `Ok(None)`. Pointer-only updates → previous image with `dirty = false`. Timestamps are QPC-based µs (`clock.rs`), comparable across processes on one machine.
  - `crates/codec`: `Encoder`/`Decoder` traits, `I420Frame`, `EncoderConfig`. BGRA⇄I420 via `yuvutils-rs` (BT.601 limited range). libvpx VP9 backend (`vpx.rs`), with realtime settings from 06 §10: one pass, CBR, `lag_in_frames 0`, error resilient, `cpu-used 6`, screen-content tuning, row-MT, tile columns by width, cyclic-refresh AQ, keyframes at start and on request. Decoder copies libvpx output into an `I420Frame`.
  - `capture_bench` example (`crates/codec/examples/`): paced or unpaced capture → convert → encode, printing fps, per-stage avg/p95 ms, bytes per frame and bitrate.
  - Tests: 27 unit tests in the two crates (conversion accuracy, VP9 round trip with PSNR > 30 dB, forced keyframes, odd sizes, size mismatch, garbage input, DXGI error classification, pitch copy) plus 2 `#[ignore]`d hardware tests (enumerate displays, first frame), which pass locally.
  - **Measured on this machine** (Ryzen 9 7900X, RTX 5060 + AMD iGPU, two 1920×1080 displays at 60 Hz; release build; VP9 3000 kbps, speed 6, 8 threads), on the live desktop. Both displays were updating about 30 times a second during the runs. A test animation was opened in the app's browser pane, but the pane was hidden at some point, so the exact on-screen content is not known. Every frame counted below was a real desktop change unless marked.

    | Run | Encoded fps | Capture copy | BGRA→I420 | VP9 encode avg / p95 | Bytes/frame (key / inter) |
    |---|---|---|---|---|---|
    | 1080p, `--fps 30`, display 1 | **30.0** (300/300 ticks had a new image) | 1.27 ms | 0.54 ms | 4.13 / 5.27 ms | 71 KB / 12.3 KB |
    | 1080p, `--fps 30`, display 0 | **30.0** (300/300 new) | 1.19 ms | 0.51 ms | 2.90 / 3.47 ms | 140 KB / 12.0 KB |
    | 1080p, `--fps 30 --always-encode`, display 0 | **30.0** (287 new + 13 repeats) | 1.42 ms | 0.55 ms | 2.88 / 3.48 ms | 134 KB / 11.9 KB |
    | 1080p, `--fps 60`, display 1 | 34.6 (the screen only changed 34.6×/s), 0 late ticks | 1.41 ms | 0.54 ms | 4.18 / 5.60 ms | 71 KB / 12.1 KB |

    Capture + convert + encode costs about 6 ms per changed 1080p frame, so there is roughly 5× headroom over 30 fps. **Exit criterion met: 1080p ≥ 30 fps.** Still worth re-measuring with known full-screen high-motion content (for example a full-screen video) in part B.
- **2026-10-11: workspace scaffold** (prompt 1).
  - Cargo workspace with the phase 0–1 crates: 11 library crates in `crates/`, 3 apps in `apps/`, 2 servers in `server/`.
  - Each crate has a `lib.rs` with a module doc comment (purpose and spec docs), a `SPEC_DOCS` constant, and a unit test that checks the referenced docs exist. Apps and servers also have a thin `main.rs`.
  - `[workspace.dependencies]` lists the crates from 16 §3 with their current versions. The whole table was checked to resolve together (`cargo generate-lockfile` in a scratch crate).
  - Repo files: `rust-toolchain.toml` (stable), `.gitignore`, `LICENSE-MIT`, `LICENSE-APACHE`, `README.md`, `CLAUDE.md`, and a GitHub Actions workflow (`.github/workflows/ci.yml`) that runs fmt, clippy and test on `windows-latest`.

## Next

1. Next prompt in `docs/next-session-prompts.md` (phase 1 starts).
2. Re-measure the spike across two machines on a LAN (needs a latency method that does not rely on a shared clock, e.g. an on-screen timestamp captured remotely, 16 §7).

## Known issues

- **libvpx is pinned to 1.13.1** because `env-libvpx-sys` only ships pregenerated bindings up to 1.13.0 and generating them needs libclang. 1.13.1 lacks later fixes, such as CVE-2024-5197 (`vpx_img_alloc`/`vpx_img_wrap` integer overflow; we only wrap images whose size we control). Before phase 1 ships, move to current libvpx with generated bindings (install LLVM, enable `env-libvpx-sys/generate`) or our own bindings.
- The cursor is not drawn into frames. DXGI reports pointer shape and position separately; sending them is part of input work (07).
- Rotated displays are not handled: the duplicated image comes back unrotated.
- HDR / 10-bit desktops are not handled: `IDXGIOutput1::DuplicateOutput` gives BGRA8, but `IDXGIOutput5::DuplicateOutput1` would be needed for proper HDR.
- No GDI fallback yet for where DXGI fails (some RDP sessions, Windows 7). The secure desktop needs the service/SYSTEM setup from phase 1 (14).
- Dirty rectangles are not used yet; every changed frame is copied and encoded in full (06 §10 lists dirty-rect skipping as the next optimisation).
- `next_frame` returns `Ok(None)` forever if duplication never becomes possible again (for example a display that stays on the secure desktop); a caller-side timeout and a GDI fallback are needed later.
- Fuzzing is not in CI (it needs nightly). The fuzz body does run in CI on stable through the `decoder_never_panics_on_random_bytes` proptest, but without coverage guidance or ASan. On Windows, `cargo fuzz run` needs the MSVC ASan DLL on `PATH` (see README).
- `AuthStart`/`AuthFinish`/`LoginResult`/`Permission`/`Ping`/`ReceiverReport`/input messages are defined but not used yet; their shape may still change before phase 1 ships (protocol is still v1 and unreleased, so changing them now is fine).
- The transport has no test for a stream with the wrong header kind (it needs a raw QUIC stream); the error path is simple but unexercised.
- **Spike security is placeholder only**: a self-signed certificate pinned by fingerprint, no user authentication, no access control. Anyone who can reach the port and knows the fingerprint gets the screen, and `--insecure` disables even that. Do not expose `spike-host` beyond localhost/LAN tests. Phase 1 replaces it (Noise IK + SPAKE2, 16 §5).
- Video frames are sent **reliably** on the video stream. 16 §4 wants unreliable delivery with keyframe recovery; on a lossy link, reliable delivery adds head-of-line delay. Revisit with QoS.
- The spike latency number relies on both processes reading the same QPC clock, so it only works on one machine, and it stops at the viewer's `present()` call (scan-out not included).
- `spike-host` serves one viewer at a time; a new viewer replaces the current one. DXGI allows one duplication per output per process (a second `DuplicateOutput` fails with `E_INVALIDARG`), so several viewers need one shared capture per display fanned out to per-viewer encoders.
- The viewer's mailbox skips 8–14 frames per session, all during start-up while the window and swapchain are created; none in steady state.
- The host logs a viewer's normal disconnect as "ended: I/O: connection lost"; the wording should distinguish a clean close.
- Capture is paced on a fixed 33 ms tick, which adds 0–33 ms of waiting (the "host" stage). Capturing on the desktop's update instead of a tick would cut average latency further.
- No internal dependencies are wired between the phase 1 crates yet; only `kanodesk-spike` and the `capture_bench` example use the others.
- The CI workflow has not run yet: there is no GitHub remote. Its first run builds libvpx (~3 min); later runs hit the cache.

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
| 2026-10-11 | **VP9 via libvpx, not openh264.** Native libraries come from vcpkg in manifest mode (`vcpkg.json`, triplet `x64-windows-static-md`, installed into `./vcpkg_installed/`); `.cargo/config.toml` sets `VPX_LIB_DIR`/`VPX_INCLUDE_DIR`/`VPX_VERSION` | libvpx built in under 3 minutes with vcpkg, so the openh264 fallback was not needed. Static libs with the dynamic CRT match Rust's MSVC default; GitHub's Windows runners can use the same flow |
| 2026-10-11 | libvpx pinned to 1.13.1 (vcpkg override) with `env-libvpx-sys`'s 1.13.0 bindings | Newest version with pregenerated bindings; 1.13.1 is ABI-identical (security-fix release). See Known issues |
| 2026-10-11 | Own thin safe wrapper over `env-libvpx-sys` instead of `vpx-encode` | Need the realtime controls (cpu-used, tune-content, row-MT, AQ, error resilience) and a decoder; `vpx-encode` covers neither |
| 2026-10-11 | Colour: BT.601 limited range for BGRA→I420, signalled in the VP9 bitstream | Matches libyuv/RustDesk conventions; the part B renderer must use the same matrix |
| 2026-10-11 | Frames are tightly packed copies owned by the capturer and lent to the caller | One CPU copy (~1 ms at 1080p) keeps the API simple; the GPU/VRAM zero-copy path comes with HW encode in phase 3 |
| 2026-10-11 | Capture timestamps are QPC microseconds (DXGI `LastPresentTime`) | System-wide clock, so the part B latency overlay can compare host and viewer on one machine |
| 2026-10-11 | DXGI hardware tests are `#[ignore]`d | GitHub runners have no interactive desktop; run locally with `-- --ignored` |
| 2026-10-11 | quinn and rustls use the **ring** provider, with default features off (no aws-lc-rs, no platform verifier) | aws-lc-sys needs CMake/NASM on Windows; ring builds with MSVC alone |
| 2026-10-11 | Spike TLS: per-run self-signed cert pinned by BLAKE3 fingerprint, not "accept any" | Barely more code than skipping verification, and it keeps the habit of authenticating the host; throwaway until 16 §5 |
| 2026-10-11 | Spike binaries live in their own crate, `apps/spike` (`kanodesk-spike`), with the shared wire/stats/render code in its library | Keeps throwaway code out of the real crates while still unit-testable |
| 2026-10-11 | Viewer threading: tokio network thread → decoder thread → one-slot mailbox → UI thread renders on arrival | The UI only ever shows the newest frame, so a slow frame never builds a queue (16 §4 row 3.6) |
| 2026-10-11 | Viewer present mode: Mailbox, else Immediate, else Fifo; `desired_maximum_frame_latency = 1`; `--vsync` forces Fifo | Lowest latency without tearing where available; measured Fifo costs ~2 ms more here |
| 2026-10-11 | Non-sRGB swapchain format (`Bgra8Unorm`) | The shader already outputs display-referred values; an sRGB target would gamma-encode them twice |
| 2026-10-11 | Overlay text drawn by the fragment shader from a 3×5 bitmap font in the uniform buffer | No font/text crates or version coupling with wgpu for a spike |
| 2026-10-11 | Host serves one viewer at a time; new viewer replaces the old | DXGI's one-duplication-per-output-per-process limit; fan-out comes with the real host engine |
| 2026-10-11 | `.proto` compiled with **protox** (pure Rust) instead of `protoc` | No binary to install or download; builds the same on CI |
| 2026-10-11 | One envelope per stream type (`ControlMessage`, `VideoMessage`, `InputMessage`), each stream opened by a `StreamHeader` | Matches the 16 §4 stream layout; a stream can only carry messages that belong on it |
| 2026-10-11 | Fixed 4-byte big-endian length header, not RustDesk's 1–4 byte varint header (05 §1) | QUIC already frames streams; a fixed header keeps the decoder trivial and easy to fuzz |
| 2026-10-11 | `Hello` carries a version **range** (`min_version`..`version`); negotiation is symmetric and picks the highest overlap | Both sides reach the same answer without another round trip; old peers can be supported by lowering `MIN_PROTOCOL_VERSION` |
| 2026-10-11 | ALPN is the unversioned `kanodesk` | Versioning is `Hello`'s job; a versioned ALPN would fail the TLS handshake before a useful error can be sent |
| 2026-10-11 | `VideoFrame` carries `capture_us` and `host_delay_us` | Lets any viewer split latency into host and network parts; also input for QoS (16 §4 row 3.2) |
| 2026-10-11 | Input key codes are USB HID usages | Platform-neutral physical key ids for Map mode (07); each OS backend maps them to its scan codes |
| 2026-10-11 | Fuzz target body is library code (`kanodesk_proto::fuzz`) | Stable CI exercises it via proptest; `cargo fuzz` adds coverage-guided search and ASan |
