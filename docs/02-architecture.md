# 02: Architecture

## 1. Big picture

```
┌───────────────────────── RustDesk.exe (one binary, many roles) ───────────────────────────┐
│  Flutter runner (C++)  ──loads──►  flutter_windows.dll + data/app.so (Dart UI)              │
│          │                                                                                  │
│          └──FFI (flutter_rust_bridge 1.80.1)──► librustdesk.dll (all Rust logic)            │
│                                                   rustdesk_core_main_args() picks the role  │
└─────────────────────────────────────────────────────────────────────────────────────────────┘
```

`RustDesk.exe` is a thin Flutter launcher **[O]**. On start it calls the exported `rustdesk_core_main_args` **[O]** (`src/core_main.rs` **[O]**), which parses the CLI and decides which role the process plays. Several roles need no Flutter UI and return early: service, server, cm, tray, portable-service, install/uninstall.

### 1.1 Process roles (Windows)

| Role | Command line | Runs as | Responsibility |
|---|---|---|---|
| **Service** | `--service` **[O]** (`sc qc` output) | LocalSystem, session 0 | Watches the active console session (`WTSEnumerateSessions`, `WTSQuerySessionInformationW` **[O]**) and launches/kills the `--server` process in that session with `CreateProcessAsUser` **[O]**. Restarts it on session switch/logon/logoff. |
| **Server** | `--server` **[O]** (log string "start --server with user") | SYSTEM token *inside the user session* (so it can capture the secure desktop/UAC) | The controlled-side engine: rendezvous registration, accepts peer connections, capture/encode, input injection, clipboard, audio. Hosts the IPC server `\\.\pipe\RustDesk\query`. |
| **Main UI** | *(no args)* | User | Flutter home window: ID/password, peer lists, settings. Outgoing sessions run **inside this process** in extra Flutter windows (multi-window plugin). |
| **Connection Manager (CM)** | `--cm` **[O]** | User | Small window shown on the controlled side for each incoming connection: accept/deny, permission toggles, chat, file log, voice call. IPC server `\\.\pipe\RustDesk\query_cm` **[O]** (seen in CM log). |
| **Tray** | `--tray` **[O]** | User | Tray icon (`tray-icon` crate **[O]**, `Shell_NotifyIconW` import **[O]**). Separate log dir `log\tray` **[O]**. |
| **Portable service** | `--portable-service` **[O]** | Elevated SYSTEM helper when not installed | Captures and injects input on behalf of an unelevated portable instance. Data passes through shared memory (`--portable-service-shmem-name` **[O]**, `shared_memory` crate **[O]**). |
| **Terminal helper** | `--terminal-helper` **[O]** | Target user | Spawns the ConPTY shell under the correct user token. Pipes `\\.\pipe\rustdesk_term_in_*`, `rustdesk_term_out_*` **[O]**. |
| **hwcodec check** | `--check-hwcodec-config` **[O]** | User | Probes HW encoders/decoders in a throwaway process (so a driver crash cannot take down the app) and writes `RustDesk_hwcodec.toml` **[O]**. |
| **Whiteboard** | `--whiteboard` **[O]** | User | Overlay window for annotations. |
| **Outgoing shortcuts** | `--connect <id>`, `--play`, `--file-transfer <id>`, `--port-forward <id>`, `--view-camera <id>`, `--terminal <id>`, `--rdp` **[O]** (prefixes in strings) | User | Open a session window directly. |
| **Install/maintenance** | `--install`, `--silent-install`, `--uninstall`, `--install-service`, `--uninstall-service`, `--update`, `--install-remote-printer`, `--uninstall-amyuni-idd`, `--uninstall-cert` **[O]** | Admin | Installer actions. |
| **Config CLI** | `--get-id`, `--set-id`, `--password`, `--config`, `--option`, `--import-config`, `--deploy`, `--assign --token --user_name --strategy_name --address_book_name …`, `--version`, `--build-date` **[O]** | Admin | Headless provisioning. |

### 1.2 Why it is split this way
- **Session 0 isolation**: a Windows service cannot capture the user desktop. The service exists only to keep a SYSTEM-token `--server` alive in the active session.
- **The UI can crash or close without dropping incoming sessions**: the server is a separate process.
- **The CM runs as a separate user process**: the incoming-connection prompt must appear on the user's desktop even when the main window is closed.

## 2. IPC

`src/ipc.rs` and `src/ipc/auth.rs` **[O]** use `parity-tokio-ipc` **[O]** (named pipes on Windows, Unix sockets elsewhere). Pipe naming: `\\.\pipe\RustDesk\query<postfix>`. Postfixes are `""` (server), `_cm` **[O]**, plus others such as `_url` (URL-scheme handoff, `ipc_url_server` **[O]**) and `_service`.

Each message is a serde-JSON-encoded `ipc::Data` enum, sent with length-prefixed framing **[R]**. Variant names visible in the strings **[O]**:

```
Login, ChatMessage, SwitchPermission, SystemInfo, ClickTime, Authorize, Close, SAS,
OnlineStatus, Config, Options, NatType, ConfirmedKey, RawMessage, Socks, FS(CreateDir,
NewWrite, CancelWrite, WriteBlock, WriteDone, WriteError, WriteOffset, CheckDigest,
ReadFile, CancelRead, SendConfirmForRead…), Test, SyncConfig, ClipboardFile,
ClipboardNonFile, PrivacyModeState, TestRendezvousServer, Keyboard(Response…), Plugin,
VoiceCallIncoming/Response, CloseVoiceCall, FileTransferLog, CmErr, CheckHwcodec,
VideoConnCount, PortableServiceRunning, SwitchSidesRequest/Back, ControlPermissions,
RemoveTrustedDevices, HwCodecConfig, WaylandScreencastRestoreToken, Whiteboard, …
```

Usage patterns:
- **UI → server `Config`/`Options` get/set**: the server process owns `RustDesk.toml`/`RustDesk2.toml`, so the UI changes settings through it (`wire_main_set_option` → IPC). This is why `wire_option_synced` exists **[O]**.
- **Server → CM `Login`/`ChatMessage`/`FS`...** when a peer connects. **CM → server `Authorize`/`Close`/`SwitchPermission`**.
- **File transfers to the host** are written by the CM process (`Data::FS`), so files are created with the logged-in user's identity rather than SYSTEM.
- IPC auth (`ipc/auth.rs`) checks that the peer process is legitimate. The portable service uses an `ipc token` **[O]** ("Invalid portable service ipc token").

**[D]** For a clone, keep this split but use a typed binary format (bincode/postcard, or reuse protobuf), and check the pipe client's PID/SID with `GetNamedPipeClientProcessId`.

## 3. First-party module tree (from panic paths) [O]

```
src/
  core_main.rs            CLI dispatch / role selection
  flutter.rs              session registry, event streams to Dart, texture/RGBA handoff
  flutter_ffi.rs          all wire_* bridge functions
  ui_interface.rs         main-window API (options, peers, ID, passwords)
  ui_session_interface.rs Session<T>: per-outgoing-session state & actions
  ui_cm_interface.rs      Connection Manager logic
  client.rs               outgoing connection: rendezvous, punch, secure handshake, decoders, audio playback
  client/io_loop.rs       the outgoing session event loop (Remote<T>)
  client/file_trait.rs    file-transfer helper trait
  client/screenshot.rs
  server.rs               controlled-side bootstrap, service registry, accept loop
  server/connection.rs    one incoming peer connection (login, permissions, dispatch)
  server/service.rs       generic publish/subscribe service framework
  server/video_service.rs capture+encode per display
  server/video_qos.rs     adaptive fps/bitrate
  server/audio_service.rs
  server/clipboard_service.rs
  server/input_service.rs mouse/keyboard injection, cursor broadcasting
  server/printer_service.rs
  server/terminal_service.rs, terminal_helper.rs
  server/portable_service.rs
  server/login_failure_check.rs
  rendezvous_mediator.rs  keep-alive with hbbs, receive punch/relay requests
  kcp_stream.rs           KCP over UDP
  lan.rs                  LAN discovery + WOL
  port_forward.rs
  keyboard.rs             keyboard modes, key grab
  clipboard.rs            clipboard listeners/sync
  privacy_mode.rs + privacy_mode/{win_input,win_mag,win_topmost_window,win_virtual_display}.rs
  virtual_display_manager.rs
  whiteboard/{client,server,windows}.rs
  auth_2fa.rs
  hbbs_http/{account,downloader,http_client,record_upload,sync}.rs
  custom_server.rs        parse branding/server config from exe name
  updater.rs, tray.rs, common.rs, ipc.rs, ipc/auth.rs
  platform/{mod,windows,win_device}.rs
libs/
  hbb_common/   config, fs, tcp, udp, stream (framing+crypto), socket_client, proxy, tls,
                websocket, compress (zstd), password_security, fingerprint, verifier, protos
  scrap/        capture (dxgi, gdi, mag), codecs (vpx, aom, hwcodec, vram), convert, record, camera
  enigo/        input simulation
  clipboard/    cliprdr (file clipboard), context_send
  remote_printer/ printer driver setup
  virtual_display/ IDD control
```

## 4. Third-party crates (selected, versions observed) [O]

| Area | Crates |
|---|---|
| Async/runtime | `tokio 1.44.2`, `tokio-util 0.7.15`, `futures-* 0.3.31`, `mio 1.0.3`, `crossbeam-* `, `flume 0.11`, `parking_lot 0.12`, `rayon 1.10`, `dashmap 6.1` |
| Protocol | `protobuf 3.7.2` (rust-protobuf, not prost), `bytes 1.10`, `serde 1.0.228`, `serde_json`, `toml 0.5.11` |
| Network | `socket2`, `tokio-socks`, `tokio-tungstenite 0.26.2`, `stunclient 0.4.1`, `stun_codec 0.3.5`, `kcp-sys` (git), `default-net 0.14.1`, `mac_address`, `wol-rs 1.0.1`, `cidr-utils`, `ipnet` |
| HTTP/TLS | `reqwest 0.12.24`, `hyper 1.7`, `rustls 0.23.28`, `rustls-native-certs`, `hyper-tls`/`schannel` (native TLS fallback), `ring 0.17.14` |
| Crypto | libsodium (sodiumoxide, statically linked: "sodium" string), `sha2 0.10.8`, `totp-rs 5.5.1`, `base32`, `base64 0.22`, `zeroize` |
| Compression | `zstd-safe 7.1`, `flate2` |
| Video | `hwcodec` (git, FFmpeg wrapper), libvpx, libaom (C, static), `rust-webm` (git), `mozjpeg`, `image 0.24`, `png`, `repng` |
| Camera | `nokhwa` (git) |
| Audio | `cpal` (git fork), `magnum-opus` (git), `dasp_signal`, `ringbuf 0.3` |
| Input | `rdev` (git fork, hooks and simulation), `enigo` (vendored lib) |
| Clipboard | `arboard` (git fork), `clipboard-master` (git) |
| Windows | `windows 0.43/0.52/0.54/0.61`, `winreg`, `windows-service 0.6`, `impersonate-system`, `shared_memory 0.12.4`, `widestring` |
| Terminal | `wezterm` (git; `portable-pty`) |
| UI glue | `flutter_rust_bridge 1.80.1`, `allo-isolate`, `tao`, `muda 0.17` (menus), `tray-icon`, `softbuffer`, `tiny-skia` (whiteboard drawing), `fontdb`, `ttf-parser` |
| Misc | `flexi_logger 0.27`, `sysinfo` (git), `whoami`, `uuid 1.16`, `chrono`, `regex`, `lazy_static`, `once_cell` |

## 5. Concurrency model [R]

- One global multi-threaded **tokio runtime** per process.
- **Outgoing session** = `Session<FlutterHandler>` (UI-facing state) + `Remote<T>` io_loop task. The loop runs `tokio::select!` over the peer stream, a UI command channel (`mpsc::UnboundedReceiver<Data>`), timers (test-delay, clipboard polling), and file-job progress.
- Video decoding runs on a **dedicated thread per display** (`new video handler for display #0` **[O]**). Frames go to Flutter by one of two routes. **Texture render** (`use texture render: true` **[O]**, `texture_rgba_renderer_plugin`, `flutter_gpu_texture_renderer_plugin`) pushes RGBA, or a D3D11 texture handle, straight to a Flutter texture without crossing Dart. The **fallback** is an RGBA buffer pulled by Dart (`session_get_rgba` / `wire_session_next_rgba` **[O]**).
- Audio playback runs on a cpal callback thread fed by a ring buffer. A separate decoder loop exists ("Audio decoder loop exits" **[O]**).
- **Host side**: each `Service` (video per display, audio, clipboard, input/cursor) runs its own thread or task and **publishes to subscribed connections**. A connection subscribes on login according to its permissions and unsubscribes when they are revoked.

## 6. Flutter↔Rust bridge

- `flutter_rust_bridge 1.80.1` **[O]** generates the C ABI `wire_<fn>(port, args…)` exports. Async calls return results on a Dart `SendPort` (`store_dart_post_cobject` **[O]**). `*_sync` variants return a `WireSyncReturn` immediately (`free_WireSyncReturn` **[O]**).
- **Event streams** go from Rust to Dart (`wire_start_global_event_stream`, `wire_session_start` returns a stream, `wire_plugin_register_event_stream` **[O]**). Rust pushes JSON maps such as `{"name":"msgbox","type":…,"title":…,"text":…}`. Event names seen in `app.so` **[O]**: `peer_info`, `switch_display`, `cursor_data`, `cursor_id`, `cursor_position`, `msgbox`, `job_progress`, `job_done`, `job_error`, `file_dir`, `update_folder_files`, `override_file_confirm`, `update_quality_status`, `update_block_input_state`, `update_privacy_mode`, `chat_client_mode`, `clipboard`, `new_connection`, `add_connection`, `on_voice_call_*`, `update_voice_call_state`, `record_status`, `screenshot`, `printer_request`, `terminal_response`, `sync_peer_info`, `sync_platform_additions`, `load_recent_peers`, `load_fav_peers`, `load_lan_peers`, `load_address_book_peers`, `on_url_scheme_received`, `show_elevation`, `switch_back`, `plugin_*`.
- See [15-ui-and-ffi.md](15-ui-and-ffi.md) for the full API catalogue.

**[D]** Clone UI options: (a) keep Flutter plus `flutter_rust_bridge` v2 (closest to the original); (b) a pure-Rust UI such as **Slint** or **egui/eframe** with a `wgpu` texture for video; (c) Tauri. For a remote-desktop viewer the video path decides performance. Whatever UI you pick must accept a GPU texture or a shared RGBA buffer without per-frame copies through a scripting layer.
