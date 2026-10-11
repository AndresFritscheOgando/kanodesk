# Progress

Roadmap: [docs/16-clone-blueprint.md](docs/16-clone-blueprint.md) §6. Build prompts: [docs/next-session-prompts.md](docs/next-session-prompts.md).

## Done

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

1. **Protocol crate** (prompt 4): own `.proto` files for the 16 §8 MVP message set with prost, `Hello` version negotiation, and the QUIC framing codec; replace the spike's hand-rolled `wire.rs`.
2. Re-measure the spike across two machines on a LAN (needs a latency method that does not rely on a shared clock, e.g. an on-screen timestamp captured remotely, 16 §7).

## Known issues

- **libvpx is pinned to 1.13.1** because `env-libvpx-sys` only ships pregenerated bindings up to 1.13.0 and generating them needs libclang. 1.13.1 lacks later fixes, such as CVE-2024-5197 (`vpx_img_alloc`/`vpx_img_wrap` integer overflow; we only wrap images whose size we control). Before phase 1 ships, move to current libvpx with generated bindings (install LLVM, enable `env-libvpx-sys/generate`) or our own bindings.
- The cursor is not drawn into frames. DXGI reports pointer shape and position separately; sending them is part of input work (07).
- Rotated displays are not handled: the duplicated image comes back unrotated.
- HDR / 10-bit desktops are not handled: `IDXGIOutput1::DuplicateOutput` gives BGRA8, but `IDXGIOutput5::DuplicateOutput1` would be needed for proper HDR.
- No GDI fallback yet for where DXGI fails (some RDP sessions, Windows 7). The secure desktop needs the service/SYSTEM setup from phase 1 (14).
- Dirty rectangles are not used yet; every changed frame is copied and encoded in full (06 §10 lists dirty-rect skipping as the next optimisation).
- `next_frame` returns `Ok(None)` forever if duplication never becomes possible again (for example a display that stays on the secure desktop); a caller-side timeout and a GDI fallback are needed later.
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
