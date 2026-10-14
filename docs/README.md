# RustDesk Desktop: Reverse-Engineering Notes and Clone Blueprint

This folder documents how the **RustDesk 1.4.9 desktop app for Windows x64** works internally: its features, how they are put together, the wire protocol, and storage. The goal is to give you enough detail to build a **compatible or independent clone in Rust**.

## Target that was analysed

| Item | Value |
|---|---|
| Install dir | `C:\Program Files\RustDesk\` |
| Installer | `rustdesk-1.4.9-x86_64.msi` |
| Launcher | `RustDesk.exe` (367 KB, Flutter runner; SHA-256 `3ccd9d5e…bfbdd`) |
| Core | `librustdesk.dll` (36 MB, Rust; all logic lives here) |
| UI | `data/app.so` (13 MB, Dart AOT snapshot of the `flutter_hbb` package) + `flutter_windows.dll` |
| Drivers | `usbmmidd_v2/` (Amyuni Indirect Display Driver = virtual monitor), `drivers/RustDeskPrinterDriver/` (v4 printer driver) |
| Helpers | `WindowInjection.dll` (privacy mode), `printer_driver_adapter.dll`, `dylib_virtual_display.dll`, Flutter plugins |
| User data | `%APPDATA%\RustDesk\config\*.toml`, `%APPDATA%\RustDesk\log\{,cm,tray,check-hwcodec-config}\` |
| Service | `RustDesk` service, `LocalSystem`, `AUTO_START`, cmdline `"RustDesk.exe" --service` |

## Method and evidence levels

The analysis used the **REA** reverse-engineering MCP. No disassembler/decompiler backend (Ghidra/IDA) was configured on this machine, so instead of pseudocode the analysis relied on these static and passive sources:

1. **PE export table** of `librustdesk.dll` → 354 exported symbols, including **327 `wire_*` flutter_rust_bridge functions**. Together these are the complete Rust↔UI API.
2. **Embedded protobuf `FileDescriptorProto`** for `message.proto`, decoded byte for byte → the complete peer-to-peer wire schema ([`proto/message.proto`](proto/message.proto)).
3. **Rust panic/log metadata** → the complete first-party module tree (`src/server/video_service.rs`, `libs/scrap/...`) and all 170 third-party crates with versions.
4. **String tables** of `librustdesk.dll` and `app.so` → option keys, HTTP API routes, CLI flags, IPC pipe names, Dart source files/classes, rendezvous message and field names.
5. **PE import table** → the Windows APIs used (DXGI/D3D11, SendSAS, WTS*, CreateProcessAsUser, SetupDi*, winspool, Magnification...).
6. **Real config files and logs** on this machine (secrets redacted) → on-disk formats and the observed runtime sequence (punch → secure → login → codec negotiation...).

Every statement in these docs carries one of these tags:

- **[O] Observed**: directly seen in the shipped binary, config or log.
- **[R] Reconstructed**: comes from knowledge of the public open-source RustDesk code base (AGPL-3.0), consistent with the names/strings found in the binary, but the exact logic was not decompiled here.
- **[D] Design**: a recommendation for your clone, not a claim about RustDesk.

> **Licensing note.** RustDesk is AGPL-3.0. Implementing a *compatible protocol* from these notes is fine. Copying RustDesk source code into your clone makes the clone AGPL as well. Decide early whether you want wire compatibility with official RustDesk servers and clients (see `16-clone-blueprint.md`).

## Document map

| # | File | Contents |
|---|---|---|
| 00 | [00-review.md](00-review.md) | Review: gaps, bottlenecks, missing coverage, open accuracy issues |
| — | [next-session-prompts.md](next-session-prompts.md) | Copy-paste prompts for building the clone (phase 0 spike → phase 1 MVP), one per message |
| — | [ui-refinement-plan.md](ui-refinement-plan.md) | Plan for the 2026-10-13 UI audit: theme, truthful status and errors, Home, Settings, connection manager, keyboard and screen readers, streaming view |
| 01 | [01-feature-inventory.md](01-feature-inventory.md) | Every main and secondary feature, with links to how it works |
| 02 | [02-architecture.md](02-architecture.md) | Processes, module tree, crates, threading, the Flutter↔Rust bridge |
| 03 | [03-networking-rendezvous.md](03-networking-rendezvous.md) | ID server, NAT detection, TCP/UDP hole punching, relay, WebSocket, KCP, LAN discovery, direct IP |
| 04 | [04-security-crypto.md](04-security-crypto.md) | Keys, E2E handshake, password challenge, 2FA, trusted devices, approval modes, brute-force protection |
| 05 | [05-wire-protocol.md](05-wire-protocol.md) | Framing, message catalogue, session state machine, rendezvous protocol |
| 06 | [06-video-pipeline.md](06-video-pipeline.md) | Capture, codecs, HW/VRAM encoding, QoS, multi-monitor, virtual displays, recording, screenshots |
| 07 | [07-input-control.md](07-input-control.md) | Mouse, keyboard modes, key grabbing, touch, cursors, Ctrl+Alt+Del, block input |
| 08 | [08-audio-voice.md](08-audio-voice.md) | Audio streaming and voice calls |
| 09 | [09-clipboard-file-transfer.md](09-clipboard-file-transfer.md) | Clipboard sync, file copy/paste (cliprdr), file manager and transfer jobs |
| 10 | [10-extra-session-types.md](10-extra-session-types.md) | Port forwarding/RDP, terminal, camera, chat, printer, whiteboard, switch sides, elevation, privacy mode |
| 11 | [11-host-server-side.md](11-host-server-side.md) | Controlled-side services, Connection Manager, permissions, auto-disconnect |
| 12 | [12-config-storage.md](12-config-storage.md) | Config files, TOML schemas, the full option-key catalogue, custom clients |
| 13 | [13-account-api.md](13-account-api.md) | Login/OIDC, address book, groups, heartbeat, audit, recording upload, deploy |
| 14 | [14-windows-platform.md](14-windows-platform.md) | Service, session handling, installer, drivers, portable service, tray, URL scheme |
| 15 | [15-ui-and-ffi.md](15-ui-and-ffi.md) | UI structure, windows and pages, event stream, the full FFI API catalogue |
| 16 | [16-clone-blueprint.md](16-clone-blueprint.md) | Recommended Rust workspace, crate choices, phased roadmap, test plan |
| — | [proto/message.proto](proto/message.proto) | Recovered peer protocol schema (exact) |
| — | [proto/rendezvous.proto](proto/rendezvous.proto) | Reconstructed rendezvous/relay schema |

## Known gaps

- Function bodies were **not decompiled** (no Ghidra/IDA available). Algorithms such as QoS tuning constants, exact punch timing, and password-encryption key derivation are **[R]**. Install Ghidra 12.1.x and set `GHIDRA_INSTALL_DIR` to verify specific functions with REA.
- `rendezvous.proto` has no descriptor embedded in the binary. Its message and field names were confirmed from strings, but its field numbers are **[R]**.
- Logs from the service and `--server` processes were not read, because they need elevation. They live under `C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RustDesk\log\` (a `server\` subfolder exists there), not under the LocalSystem profile `system32\config\systemprofile`, which has no RustDesk folder on this machine. See [12](12-config-storage.md) §1. Host-side runtime behaviour is inferred from the CM log and the code structure.
