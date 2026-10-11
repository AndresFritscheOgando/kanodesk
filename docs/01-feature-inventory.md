# 01: Feature Inventory

Every feature found in RustDesk 1.4.9 desktop. Each row gives its evidence (FFI export, proto message, module, or option key) and the doc that explains how it works internally.

Legend: **M** = main feature, **S** = secondary feature. Evidence tags are described in [README](README.md).

## A. Connectivity and identity

| | Feature | How it works (summary) | Evidence | Doc |
|---|---|---|---|---|
| M | **Numeric device ID** | Derived from a machine UID and registered with the ID server (hbbs). Stored encrypted as `enc_id`. | `RustDesk.toml: enc_id` [O], `wire_main_get_my_id` [O] | 03, 12 |
| M | **Connect by ID** | Client asks hbbs to broker a connection, then does TCP/UDP hole punching, with relay as fallback. | `src/rendezvous_mediator.rs`, `src/client.rs`, log "TCP Hole Punched" [O] | 03 |
| M | **Relay fallback** | Both peers connect to hbbr (port 21117), which pairs them by UUID. | `RequestRelay`, `RelayResponse` strings [O]; log `relay_server: ovh-east2.rustdesk.com` [O] | 03 |
| M | **Self-hosted server config** | ID server, relay server, API server and public key are configurable. | options `custom-rendezvous-server`, `relay-server`, `api-server`, `key` [O] | 03, 12 |
| S | Change ID | Re-registers a custom ID with hbbs (`RegisterPk.old_id`). | `wire_main_change_id` [O] | 03 |
| S | NAT type test | Probes two server ports and compares the mapped ports to classify the NAT as ASYMMETRIC or SYMMETRIC. | `TestNatRequest/Response` [O], `nat_type = 1` in config [O] | 03 |
| S | UDP hole punch + KCP | Optional reliable UDP transport (KCP) after a UDP punch. | `src/kcp_stream.rs`, `kcp-sys` crate, `enable-udp-punch` [O] | 03 |
| S | IPv6 punch | Uses an IPv6 address pair when both peers have one. | `socket_addr_v6`, `enable-ipv6-punch` [O] | 03 |
| S | WebSocket transport | Tunnels rendezvous/relay over WS(S) for restrictive firewalls. | `allow-websocket`, `tokio-tungstenite`, `RelayWebSocket` [O] | 03 |
| S | Proxy support | Connects through SOCKS5 or HTTP proxy. | `wire_main_set_socks`, `proxy-url`, `tokio-socks`, `libs/hbb_common/src/proxy.rs` [O] | 03 |
| S | Direct IP access | Host listens on TCP 21118 (configurable), and the client connects by `ip[:port]`. | `direct-server`, `direct-access-port` [O] | 03 |
| S | LAN discovery | UDP broadcast ping/pong, listed in the "Discovered" tab. | `src/lan.rs`, `PeerDiscovery{cmd,mac,misc}` [O], `wire_main_discover` [O] | 03 |
| S | Wake-on-LAN | Sends a magic packet to the MAC learned via discovery. | `wol-rs` crate, `wire_main_wol` [O] | 03 |
| S | Online status of peers | Batched `OnlineRequest` sent to hbbs. | `wire_query_onlines`, `OnlineRequest`/`OnlineResponse.states` [O] | 03 |
| S | Force relay per connection | Appending `/r` to the peer ID skips hole punching and goes straight to hbbr for that connection (sets `force_relay`). | `wire_main_handle_relay_id` [O]; `/r` suffix handling [R] | 03 |
| S | Disable UDP / TCP only | Forces TCP for everything. | `disable-udp` [O] | 03 |
| S | Insecure TLS fallback | Accepts self-signed HTTPS for the API. | `allow-insecure-tls-fallback` [O] | 13 |

## B. Security and access control

| | Feature | How it works | Evidence | Doc |
|---|---|---|---|---|
| M | **End-to-end encryption** | ed25519 identity, a Curve25519 sealed-box key exchange, then XSalsa20-Poly1305 per message. | "Connection secured", "Signature mismatch in key exchange" [O]; `PublicKey{asymmetric_value,symmetric_value}` [O] | 04 |
| M | **One-time (temporary) password** | Random password, regenerated on demand with the "Refresh random password" button, and rotated automatically after too many consecutive wrong attempts (threshold [R]). Automatic regeneration after each session is [R] (unverified). Length and charset are configurable. | `wire_main_get_temporary_password`, `wire_main_update_temporary_password`, UI string "Refresh random password", log string "Temporary password rotated after too many consecutive wrong attempts", `temporary-password-length`, `allow-numeric-one-time-password` [O] | 04 |
| M | **Permanent password** | Stored hashed/encrypted in `RustDesk.toml`. | `password`, `salt` keys [O], `permanent_password.rs` [O] | 04 |
| M | **Click-to-accept approval** | The Connection Manager window asks the local user. `approve-mode` = `password`, `click`, or both. | `approve-mode = 'password'` [O] | 04, 11 |
| S | Verification method | Temporary, permanent, or both passwords. | `verification-method`, `use-both-passwords` [O] | 04 |
| S | Two-factor auth (TOTP) | Host demands an `Auth2FA` code after the password. | `totp-rs`, `wire_main_generate2fa/verify2fa`, `Auth2FA` [O] | 04 |
| S | Telegram bot 2FA | Codes are delivered via a bot. | `wire_main_verify_bot`, `bot-*` strings [O] | 04 |
| S | Trusted devices | Skips 2FA for a known `hwid`. | `trusted_devices`, `LoginRequest.hwid`, `enable-trusted-devices` [O] | 04 |
| S | IP whitelist | Rejects connections from IPs that are not listed. | `whitelist` option string [O]; enforcement logic [R] | 11 |
| S | Brute-force protection | Bans by IP after repeated failures. | `src/server/login_failure_check.rs` [O] | 04 |
| S | Remember password per peer | Stores the hash in `peers/<id>.toml` with a "00" version prefix. | peer `password = "00…"` [O] | 04, 12 |
| S | OS-login credentials | Logs into a locked/logged-out machine. | `OSLogin{username,password}`, `wire_session_input_os_password` [O] | 04 |
| S | Unlock PIN for settings | Protects the settings UI. | `unlock_pin`, `wire_main_set_unlock_pin` [O] | 12 |
| S | Preset password (custom builds) | Built-in password. | `wire_is_preset_password` [O] | 12 |
| S | Server key pinning | Verifies the signature of the ID server. | `key` option, "invalid public key from rendezvous server" [O] | 03, 04 |
| S | Key fingerprint display | Shows a fingerprint of the device's public key so users can verify host identity out of band. | `wire_main_get_fingerprint`, `libs/hbb_common/src/fingerprint.rs`, config key `fingerprint` [O]; derivation [R] | 02, 15 |
| S | CM click-time anti-spoofing | The Connection Manager checks click timing on "Accept", probably so a remote controller who is already connected can't approve a new connection. | `wire_cm_check_click_time`, `wire_cm_get_click_time` [O]; purpose [R] | 11 |

## C. Remote desktop session

| | Feature | How it works | Evidence | Doc |
|---|---|---|---|---|
| M | **Screen streaming** | DXGI duplication (GDI fallback) → VP8/VP9/AV1/H.264/H.265 → `VideoFrame`. | `libs/scrap/src/common/{dxgi,vpxcodec,aom,hwcodec}.rs` [O] | 06 |
| M | **Hardware codec** | FFmpeg-based NVENC/AMF/QSV, with a RAM path and a zero-copy VRAM (D3D11) path. | `RustDesk_hwcodec.toml` [O], `vram.rs` [O] | 06 |
| M | **Remote mouse and keyboard** | `MouseEvent`/`KeyEvent` → host simulates them with SendInput. | `libs/enigo/src/win/win_impl.rs` [O] | 07 |
| M | **Keyboard modes** | Legacy, Map (scancode), Translate, Auto. | `KeyboardMode` enum [O] | 07 |
| M | **Multi-monitor** | Switch display, show all monitors at once, or open each display in its own window. | `SwitchDisplay`, `CaptureDisplays`, `wire_session_set_displays_as_individual_windows` [O] | 06 |
| S | Image quality presets | Best/Balanced/Low, plus custom % and custom FPS. | `ImageQuality`, `custom_image_quality`, `custom_fps` [O] | 06 |
| S | Codec preference | Auto, VP8, VP9, AV1, H.264, H.265; chroma 4:4:4. | `SupportedDecoding.PreferCodec`, `i444` [O] | 06 |
| S | Adaptive QoS | `TestDelay` RTT drives FPS and bitrate. | `src/server/video_qos.rs`, `TestDelay.target_bitrate` [O] | 06 |
| S | Quality monitor overlay | Shows FPS, delay, bitrate and codec. | `show-quality-monitor`, `QualityMonitorModel` [O] | 06 |
| S | Change remote resolution | `ChangeDisplaySettingsEx` on the host. | `wire_session_change_resolution`, `Misc.change_display_resolution` [O] | 06 |
| S | Virtual display (headless) | Amyuni IDD creates monitors on demand. | `usbmmidd_v2/`, `ToggleVirtualDisplay`, `virtual_display_manager.rs` [O] | 06, 14 |
| S | View styles | Original, adaptive/fit, custom scale. Scroll styles: auto, bar, edge scroll. | `view_style`, `scroll_style`, `edge_scroll_edge_thickness` [O] | 15 |
| S | Remote cursor | Cursor shape and position are streamed separately and cached by ID. | `CursorData`, `CursorPosition`, `cursor_id` [O] | 07 |
| S | Follow remote cursor/window | Client view auto-pans to follow. | `follow_remote_cursor`, `follow_remote_window` [O] | 07 |
| S | Show my cursor to the remote side | | `show_my_cursor` [O] | 07 |
| S | Relative mouse mode (games) | Pointer lock plus deltas. | `RelativeMouseModel` [O] | 07 |
| S | Clip / set local cursor | Confines and repositions the client's own cursor, used by relative mouse mode. | `wire_main_clip_cursor`, `wire_main_set_cursor_position` [O] | 15 |
| S | Swap mouse buttons, reverse wheel, trackpad speed | Client-side transforms. | `swap-left-right-mouse`, `wire_session_set_reverse_mouse_wheel`, `trackpad-speed` [O] | 07 |
| S | Ctrl+Alt+Del | `ControlKey.CtrlAltDel` → `SendSAS`. | `SAS.dll!SendSAS` import [O] | 07 |
| S | Lock remote screen | `ControlKey.LockScreen`. | `wire_session_lock_screen` [O] | 07 |
| S | Lock after session end | | `lock_after_session_end` [O] | 11 |
| S | Block user input | Host installs low-level hooks. | `privacy_mode_hook_keyboard/mouse` exports [O], `BlockInput` permission [O] | 07 |
| S | Privacy mode | Blanks the host screen for the local user. Three implementations. | `src/privacy_mode/{win_mag,win_topmost_window,win_virtual_display,win_input}.rs` [O] | 10 |
| S | Restart remote device | | `Misc.restart_remote_device` [O] | 11 |
| S | Session recording | Client- or host-side to WebM/MP4, with optional auto-record. | `libs/scrap/src/common/record.rs`, `allow-auto-record-*` [O] | 06 |
| S | Screenshot | Captures the remote screen to a file or the clipboard. | `ScreenshotRequest/Response`, `src/client/screenshot.rs` [O] | 06 |
| S | Remove wallpaper during session | | `allow-remove-wallpaper` [O] | 11 |
| S | Keep awake | Prevents sleep during incoming/outgoing sessions. | `keep-awake-during-*` [O] | 11 |
| S | View-only mode | | `view_only` [O] | 07 |
| S | Windows session picker / multi-UI-session (RDP, fast user switching) | Lists the host's Windows user sessions and lets the client choose one; the service then runs the server in that session. | `WindowsSessions`, `Misc.selected_sid`, `wire_is_support_multi_ui_session`, `wire_session_is_multi_ui_session` [O] | 14 |
| S | Elevation (UAC) for portable host | Direct, or with a logon. | `ElevationRequest`, `wire_session_elevate_*` [O] | 10 |
| S | Tabs and multi-window | Each session type gets its own window with tabs. | `desktop_multi_window_plugin.dll`, `DesktopTabController` [O] | 15 |
| S | Toolbar docking, pin/collapse | | `remote-menubar-*` options [O] | 15 |
| S | Reconnect and auto-reconnect | | `wire_session_reconnect` [O] | 05 |
| S | Audit note | User types a note that is sent to the audit server. | `wire_session_send_note`, `allow-ask-for-note` [O] | 13 |

## D. Data channels

| | Feature | How it works | Evidence | Doc |
|---|---|---|---|---|
| M | **Clipboard sync** | Text, RTF, HTML and images (PNG/RGBA/SVG), compressed with zstd. | `Clipboard`, `MultiClipboards`, `ClipboardFormat` [O] | 09 |
| M | **File copy/paste via clipboard** | A cliprdr protocol (modelled on MS-RDPECLIP). | `Cliprdr*` messages, `libs/clipboard` [O], `enable-file-copy-paste` [O] | 09 |
| M | **File manager / transfer** | Dual-pane browser with upload/download jobs, resume, and digest-based dedup. | `FileAction`/`FileResponse` [O] | 09 |
| M | **Audio streaming** | WASAPI loopback → Opus 48 kHz. | `src/server/audio_service.rs`, `magnum-opus`, log "recved audio format, sample rate=48000" [O] | 08 |
| S | Host audio input selection | Chooses what the host streams: system sound (loopback, the default) or a named microphone/line-in. | `audio-input`, `wire_main_get_sound_inputs`, `wire_main_audio_support_loopback` [O] | 08 |
| S | Voice call (two-way mic) | | `VoiceCallRequest/Response` [O] | 08 |
| S | Text chat | | `Misc.chat_message` [O] | 10 |
| S | Remote printing | Host virtual printer → job is sent as a file → printed on the client's printer. | `drivers/RustDeskPrinterDriver`, `FileTransferSendRequest.FileType.Printer` [O] | 10 |
| S | Persisted transfer jobs | Jobs are resumed after a reconnect. | `[transfer] read_jobs/write_jobs` in peer toml [O] | 09 |

## E. Other session types (separate windows)

| | Feature | How it works | Evidence | Doc |
|---|---|---|---|---|
| M | **TCP port forwarding** | Local listener → per-connection tunnel through the peer. | `LoginRequest.port_forward`, `src/port_forward.rs` [O] | 10 |
| S | RDP shortcut | Forwards to the remote `localhost:3389` and launches `mstsc`. | `wire_session_new_rdp`, `wire_main_set_share_rdp` [O] | 10 |
| M | **Remote terminal** | ConPTY shell on the host, with persistent sessions and replay. | `TerminalAction`, `src/server/terminal_service.rs`, `wezterm` portable-pty [O] | 10 |
| S | View remote camera | Webcam is streamed as video. | `LoginRequest.view_camera`, `nokhwa` crate, `ViewCameraPage` [O] | 10 |
| S | Switch sides | The controlled side becomes the controller. | `SwitchSidesRequest/Response`, `SwitchBack` [O] | 10 |
| S | Whiteboard/annotation | Overlay drawing. | `src/whiteboard/{client,server,windows}.rs`, `--whiteboard` [O] | 10 |

## F. Address book, accounts and management (Pro server)

| | Feature | How it works | Evidence | Doc |
|---|---|---|---|---|
| M | **Recent / Favorites / Discovered / Address book / Groups tabs** | | `PeerTabPage`, `RecentPeersView`, `AddressBookPeersView`, `MyGroupPeerView` [O] | 13, 15 |
| S | Peer management | Rename (alias), forget the saved password, remove a peer, sort the list. | `wire_main_set_peer_alias`, `wire_main_forget_password`, `wire_main_remove_peer`, `peer-sorting` [O] | 12, 15 |
| S | Account login (password, OIDC/SSO) | | `/api/login`, `/api/oidc/auth`, `/api/oidc/auth-query` [O] | 13 |
| S | Login device info | Sends `{os, type, name}` with the account login. | `wire_main_get_login_device_info` [O] | 13 |
| S | HTTP routed through Rust | UI HTTP requests go through the Rust core, so proxy and TLS settings apply to them. | `wire_main_http_request`, `enable-flutter-http-on-rust` [O] | 13 |
| S | Shared address books, tags, aliases, peer passwords | | `/api/ab/*` [O] | 13 |
| S | Device groups | | `/api/device-group/accessible` [O] | 13 |
| S | Heartbeat and sysinfo upload, remote strategy push | | `/api/heartbeat`, `src/hbbs_http/sync.rs` [O] | 13 |
| S | Connection, file and alarm audit | | `/api/audit/*` [O] | 13 |
| S | Conn token / audit GUID | Links a session to its Pro audit record. | `wire_session_get_conn_token`, `wire_session_get/set_audit_guid` [O] | 13 |
| S | Recording upload | | `/api/record`, `record_upload.rs` [O] | 13 |
| S | CLI deploy/assign | Assigns the device to a user/group/strategy. | `--deploy`, `--assign`, `/api/devices/cli` [O] | 13 |

## G. Desktop/OS integration

| | Feature | How it works | Evidence | Doc |
|---|---|---|---|---|
| M | **Install as service** | Unattended access. Service runs `--server` in the active session. | `--install-service`, `RustDesk` service [O] | 14 |
| M | **Portable mode** | Runs without installing. Optional elevated "portable service" uses shared memory. | `--portable-service`, `portable_service.rs` [O] | 14 |
| S | System tray | | `--tray`, `tray-icon` crate [O] | 14 |
| S | `rustdesk://` URL scheme | | `uni_links_desktop_plugin.dll`, `wire_send_url_scheme` [O] | 14 |
| S | Auto-update | Checks for and downloads the MSI. | `src/updater.rs`, `hbbs_http/downloader.rs`, `allow-auto-update` [O] | 14 |
| S | Install options | Install path, shortcuts and printer driver chosen on the install page. | `wire_install_install_options`, `wire_install_install_path` [O] | 14 |
| S | Custom client / branding | Hard-coded options and incoming-only/outgoing-only builds. | `src/custom_server.rs`, `wire_is_incoming_only`, `wire_main_get_hard_option` [O] | 12 |
| S | Enforced (locked) options | Options fixed by a custom build or a Pro strategy are shown read-only in settings. | `wire_main_is_option_fixed`, `wire_main_get_buildin_option` [O] | 12, 13 |
| S | Default settings for new peers | User-chosen defaults (view style, quality, codec…) applied to peers that have no saved settings. | `wire_main_get/set_user_default_option` [O] | 12 |
| S | Import/export server config | | `--import-config`, `--config` [O] | 12 |
| S | Multi-language UI | 40+ languages, compiled in. | `wire_main_get_langs`, `wire_translate` [O] | 15 |
| S | Theme (light/dark/system) | | `wire_main_change_theme` [O] | 15 |
| S | Plugins | Signed native plugins with UI descriptors. | `wire_plugin_*` [O] | 15 |
| S | Create desktop shortcut | | `wire_main_create_shortcut` [O] | 14 |
| S | Non-Windows platform hooks | macOS: screen-recording and input-monitoring permission checks, hide dock icon. Linux: Wayland detection and screencast restore token, D-Bus server, SELinux check. Exported but not analysed (Windows-only target). | `wire_main_is_can_screen_recording`, `wire_main_is_can_input_monitoring`, `wire_main_hide_dock`, `wire_main_current_is_wayland`, `wire_main_handle_wayland_screencast_restore_token`, `wire_main_start_dbus_server`, `wire_is_selinux_enforcing` [O] | 15 |
