# 00: Review of This Documentation Set (gaps, bottlenecks, missing coverage)

Review date: 2026-10-11. Scope: every file in `docs/` as of this date. Checks run: broken links; FFI export coverage (each of the 354 `librustdesk.dll` exports grepped against the docs); proto message coverage; spot-checks of claims against the binary's strings.

## 1. Completeness status

> **Progress note (2026-10-11, update 2):** docs 07–16 are now written. All links resolve. FFI export coverage is 354/354 and proto type coverage is 119/119 (some through brace notation or field names). Sections 2–6 below are the original review findings; see "Status after update 2" at the end for which ones are resolved.

| Doc | Status |
|---|---|
| README, 00–16, `proto/message.proto`, `proto/rendezvous.proto`, `next-session-prompts.md` | Written |

### Coverage numbers (after update 2)
- **FFI exports**: all 354 are mentioned. The catalogue is in `15-ui-and-ffi.md` §4, the CM API in `11` §4.2, and the file API in `09` §3.4.
- **Proto types**: all 119 are covered in prose (07 input, 08 audio, 09 clipboard/files, 10 sessions, 11 permissions).
- At the first review it was 68/354 exports and 62/119 types.

## 2. Features missing from the inventory (01)

Found in the binary but absent from `01-feature-inventory.md`:

| Feature | Evidence | Notes |
|---|---|---|
| **Force relay per connection** (append `/r` to the ID) | `wire_main_handle_relay_id` **[O]** | Common user feature; belongs with section A |
| **Key fingerprint display** (verify host identity out of band) | `wire_main_get_fingerprint`, `libs/hbb_common/src/fingerprint.rs`, config key `fingerprint` **[O]** | Security feature; belongs in 04 |
| **"Refresh random password" button** | UI string "Refresh random password" **[O]** | The temporary-password regeneration *policy* is unverified (see §4) |
| **Default settings for new peers** | `wire_main_get/set_user_default_option` **[O]** | Per-peer defaults: view style, quality, codec |
| **Enforced (locked) options** | `wire_main_is_option_fixed`, `wire_main_get_buildin_option` **[O]** | Managed and custom deployments |
| **Host audio input source selection** | `wire_main_get_sound_inputs`, option `audio-input`, `wire_main_audio_support_loopback` **[O]** | Choose which device the host streams |
| **Peer management** (alias, forget password, remove peer, sort) | `wire_main_set_peer_alias`, `wire_main_forget_password`, `wire_main_remove_peer`, `peer-sorting` **[O]** | |
| **Multi-UI-session (several Windows user sessions)** | `wire_is_support_multi_ui_session`, `wire_session_is_multi_ui_session` **[O]** | Related to the `WindowsSessions` picker |
| **Clip cursor / set cursor position** | `wire_main_clip_cursor`, `wire_main_set_cursor_position` **[O]** | Relative-mouse support on the client |
| **CM click-time anti-spoofing** | `wire_cm_check_click_time`, `wire_cm_get_click_time` **[O]** | Probably verifies that the "Accept" click came from the local user and not from an already-connected remote controller **[R]**. Needs confirmation. |
| **Login device info** (shown to the host) | `wire_main_get_login_device_info` **[O]** | |
| **HTTP proxied through Rust** | `wire_main_http_request`, `enable-flutter-http-on-rust` **[O]** | Lets UI HTTP requests honour proxy/TLS settings |
| **Install options** (shortcuts, path, printer) | `wire_install_install_options`, `wire_install_install_path` **[O]** | |
| **Conn token / audit GUID** | `wire_session_get_conn_token`, `wire_session_get/set_audit_guid` **[O]** | Ties a session to Pro audit records |
| **Non-Windows platform hooks** | `wire_main_is_can_screen_recording`, `wire_main_is_can_input_monitoring`, `wire_main_hide_dock` (macOS); `wire_main_current_is_wayland`, `wire_main_handle_wayland_screencast_restore_token`, `wire_main_start_dbus_server`, `wire_is_selinux_enforcing` (Linux) **[O]** | The docs cover Windows only. A cross-platform clone needs a platform-matrix doc (macOS TCC permissions, Linux X11 vs Wayland/PipeWire portal). |

## 3. Performance bottlenecks to document (and design around in a clone)

These are architectural risks visible in the current design. None was measured here; they are inferences from the protocol and module structure, so all are **[R]/[D]**.

1. **One stream for everything (head-of-line blocking).** Video, input, clipboard, file blocks and audio share one ordered TCP/KCP stream (05 §1). A large file transfer or a lost TCP segment delays mouse events and video. **Clone**: separate channels, using QUIC streams or at least priority queues with file blocks interleaved in small chunks.
2. **The video ack window caps FPS on high-RTT links.** With `video_ack_required` and only 1–2 unacked frames in flight (06 §4), FPS cannot exceed about 1–2 frames per RTT. At 200 ms RTT that is roughly 5–10 fps, whatever the bandwidth. **Clone**: a bandwidth-based window (BBR/GCC-style) instead of a frame-count window.
3. **One encoder per display shared by every viewer.** The host must pick a codec all viewers can decode and the lowest common FPS (06 §3.2), so one weak viewer degrades everyone. **Clone**: per-viewer encoders when CPU/GPU allows, or simulcast layers.
4. **Relay is a plain TCP pipe.** A relayed session stacks two TCP legs (TCP-over-relay), which hurts on lossy links, and hbbr bandwidth becomes a central limit. **Clone**: relay over UDP/QUIC; document capacity planning for the relay.
5. **CPU colour conversion on the software path.** BGRA→I420 runs on the CPU for every frame unless the VRAM path is active. At 4K60 this is expensive. **Clone**: GPU conversion (D3D11 video processor / compute shader).
6. **Dart RGBA fallback.** When texture rendering is unavailable, every frame crosses FFI as an RGBA buffer that Dart pulls (06 §7). **Clone**: never route frame data through the UI scripting layer.
7. **JSON IPC.** UI↔server↔CM messages, including file-write blocks (`Data::FS::WriteBlock`), are serde-JSON. Binary data in JSON costs CPU and memory on large transfers to the host. **Clone**: binary IPC (postcard/bincode), or have the CM open the file and receive data through shared memory.
8. **HW codec probing in a child process.** `--check-hwcodec-config` runs at startup and on driver changes. It is robust, but the first connection may fall back to software until the probe finishes. Document the startup ordering.
9. **Clipboard polling and full-content sync.** Clipboard changes push whole payloads (images included), compressed with zstd. Large images on every copy cost bandwidth. **Clone**: lazy, on-request format data, as cliprdr already does for files.
10. **Registration heartbeat scaling.** UDP `RegisterPeer` about every 12 s per device (03 §3) gives roughly 83k packets/s per 1M devices on hbbs. Fine, but it is the main sizing parameter for a self-hosted server; document it in the server section of 16.

## 4. Accuracy issues found and corrections

| Location | Issue | Action |
|---|---|---|
| 02 §1.1 `--server` | Tagged [R]; the binary contains "start --server with user" | **Fixed** → [O] |
| 05 §2 | "115 messages and enums"; actual count is 119 | **Fixed** |
| 05 §3 | "~35 Misc sub-messages"; actual is 33 | **Fixed** |
| README | "~330 wire_ functions"; actual is 327 | **Fixed** |
| 01 + 04 temporary password | "Regenerated on each new session" is unverified. The strings only show manual "Refresh random password" | **Fixed (update 3)**: 01 and 04 now say on-demand refresh is [O] and automatic regeneration is [R]. A further trigger was found in the strings: rotation after too many consecutive wrong attempts [O] (04 §4, §7). Rotation after each session is still unconfirmed. |
| 06 §4 QoS ratios | Placeholder values with `?` (`Best:1.5?`…) | **Open**: needs decompilation of `video_qos.rs`, or measurement |
| 03 §1 ports | 21118 is both the hbbs WebSocket port (server) and the default `direct-access-port` (client host), and 21119 is both the hbbr WebSocket port (TCP) and LAN discovery (UDP). This is correct but confusing. | **Fixed (update 3)**: "Port reuse note" added to 03 §1, including the one real conflict (hbbs + direct-access host on one machine) |
| 03 §5.2 diagram | Leaves out `ControlPermissions` in `PunchHole`/`RequestRelay` (present in rendezvous.proto), which Pro strategies use to restrict a session | **Fixed (update 3)**: added to the §5.2 `PunchHole` box and the §6 forwarded `RequestRelay`, plus an explanatory bullet. Who sets it (hbbs, not the client) is [R]. |
| 04 §4 permanent password | Storage format hedged. The key `permanent-password-storage-and-salt` **[O]** suggests a newer combined storage scheme | **Partly resolved (update 3)**: 12 §2.1.1 documents the `01` encrypted-hash format, the salt binding and the IPC sync, from strings and file shapes [O]. The exact byte layout and encryption of the 87-byte blob are still unknown and need decompilation of `permanent_password.rs`. |
| `rendezvous.proto` field numbers | Entirely [R] | **Open**: validate against captured traffic (05 §8) |

## 5. Methodology gaps

- **No decompilation.** Every algorithm (QoS, punch timing, password-at-rest key derivation, login-failure thresholds, cliprdr state machine) is [R]. Highest-value targets once Ghidra is available: `video_qos.rs`, `login_failure_check.rs`, `password_security.rs`, `client.rs` punch logic, `connection.rs` login handler.
- **No runtime capture.** A packet capture of two official clients on a self-hosted, keyless hbbs/hbbr would confirm the rendezvous field numbers, framing and the handshake byte layout. This is the single most important validation for wire compatibility.
- **No host-side logs.** The `--server` and service logs (`C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RustDesk\log\server\`; confirmed to exist in update 3, but unreadable without elevation) were not read. Reading them elevated would confirm the incoming-connection sequence in 05 §4.2.
- **The UI was analysed only by class and file names.** Screens, dialogs and setting panes are not documented one by one. Taking screenshots of each settings tab would complete 15.

## 6. Recommended next steps (priority order)

1. ~~Write the missing docs **09, 11, 12, 15, 16** first: these hold most of the uncovered exports and proto messages, plus the blueprint the clone needs.~~ **Done (update 2)**: all docs 07–16 are written, and coverage is 354/354 exports and 119/119 proto types.
2. Add a **platform matrix** doc (Windows / macOS / Linux X11 / Wayland) if the clone will be cross-platform.
3. Add the missing features from §2 to `01-feature-inventory.md`.
4. Install Ghidra and decompile the five functions listed in §5; replace [R] values.
5. Do a keyless self-hosted traffic capture to lock down `rendezvous.proto` and the handshake.
6. Turn §3 into explicit design decisions in `16-clone-blueprint.md`: QUIC multiplexing, bandwidth-based pacing, GPU colour conversion, binary IPC.

## Status after update 2

| Review item | Status |
|---|---|
| §1 missing docs 07–16 | **Resolved** |
| §2 missing inventory features | **Resolved** (update 3): all 15 added to `01-feature-inventory.md`. Fingerprint derivation, the click-time check's purpose and the `/r` suffix handling stay [R]. |
| §3 bottlenecks | **Turned into design decisions** in `16-clone-blueprint.md` §4 |
| §4 accuracy issues | Still open (QoS constants, permanent-password blob layout, rendezvous field numbers). Fixed in 07: `allow_swap_key` is a Ctrl/Cmd swap, not a key-forwarding switch. |
| §5 methodology gaps | Unchanged: needs Ghidra + traffic capture |
| New: unverified [R] in 13 | Request/response JSON field names of the Pro API are [R]. Verify them against a Pro server or its public API docs. |
| New: whiteboard | Message format unknown (`10` §6) |
