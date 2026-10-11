# 10: Extra Session Types and Session Features

Covers: port forwarding/RDP, terminal, camera, chat, remote printing, whiteboard, switch sides, elevation, privacy mode, restart, and plugin messages.

---

## 1. TCP port forwarding (`src/port_forward.rs`) [O]

### 1.1 Concept
The client opens a **local TCP listener** (`localhost:<local_port>`). Every accepted local socket gets its own **new RustDesk connection** to the peer: rendezvous → punch/relay → secure → login, with `LoginRequest.port_forward = {host, port}` **[O]**. The host then dials `host:port` **from its own network** and pipes bytes in both directions.

```
local app ─► 127.0.0.1:L ─► [client] ══ E2E RustDesk conn (one per TCP socket) ══► [host] ─► host:port (e.g. 192.168.1.10:22)
```

### 1.2 Details [R], names [O]
- After a successful `LoginResponse`, the stream carries **raw application bytes**. Each read chunk is wrapped in the encrypted frame, but there are no protobuf `Message`s anymore.
- The UI manages a list per peer: `port_forwards = []` in `peers/<id>.toml` **[O]**, `wire_session_add_port_forward(local_port, remote_host, remote_port)`, `wire_session_remove_port_forward` **[O]**. Pages: `PortForwardPage`, `PortForwardTabPage` **[O]**.
- The host must allow it: `enable-tunnel` **[O]**, Pro permission `tunnel` **[R]**. `conn_type = PORT_FORWARD` in the rendezvous request **[R]**.
- Log strings: `Failed to listen on `, `port forward (:) exit` **[O]**.
- CLI: `--port-forward <id> <local> <host> <port>` **[O]** (prefix `--port-f`).

### 1.3 RDP shortcut
- `wire_session_new_rdp` **[O]** is a port forward to the host's `localhost:3389`, followed by launching `mstsc /v:localhost:<L>` (the string `mstsc` **[O]**). The rendezvous request uses `conn_type = RDP`.
- RDP credentials can be saved per peer (`rdp_port`, `rdp_username`, `rdp_password` in the peer config **[O]**).
- `wire_main_set_share_rdp` / `wire_main_is_share_rdp` **[O]**: whether the host lets RustDesk enable and share RDP.

---

## 2. Remote terminal (`src/server/terminal_service.rs`, `terminal_helper.rs`) [O]

### 2.1 Session setup
`LoginRequest.terminal = Terminal{service_id}` **[O]**. `service_id` lets a client re-attach to an existing **persistent** terminal service on the host.

### 2.2 Messages [O]
```
TerminalAction  oneof { OpenTerminal open=1 {terminal_id, rows, cols};
                        TerminalData data=2 {terminal_id, data, compressed};
                        ResizeTerminal resize=3 {terminal_id, rows, cols};
                        CloseTerminal close=4 {terminal_id} }
TerminalResponse oneof { TerminalOpened opened=1 {terminal_id, success, message, pid, service_id,
                                                  persistent_sessions[], replay_terminal_output};
                         TerminalData data=2; TerminalClosed closed=3 {terminal_id, exit_code};
                         TerminalError error=4 {terminal_id, message} }
```
Several tabs (`terminal_id`s) can share one connection.

### 2.3 Host implementation [O] names, [R] details
- PTY: the `wezterm` git dependency **[O]** provides `portable-pty`, which uses Windows **ConPTY** (`CreatePseudoConsole` string **[O]**). Default shell: `cmd.exe` or PowerShell; `$SHELL` on Unix.
- **User context**: `--server` runs as SYSTEM, so it starts a `--terminal-helper` **[O]** in the logged-in user's session with that user's token. The helper owns the ConPTY, and I/O flows through named pipes `\\.\pipe\rustdesk_term_in_<id>` / `rustdesk_term_out_<id>` **[O]**.
- **Admin terminal**: `terminal-admin`, `terminal-admin-login`, `terminal-admin-login-password` **[O]**. The client may ask for an elevated shell by giving admin credentials (`CreateProcessWithLogonW` import **[O]**).
- **Persistence**: `OptionMessage.terminal_persistent` / `terminal-persistent` **[O]**. When enabled, closing the client keeps the shell alive. On reconnect, `TerminalOpened.persistent_sessions` lists live IDs, and `replay_terminal_output=true` makes the host resend its buffered scrollback so the screen is rebuilt.
- A guard: "Terminal service user mismatch: … The service ID may have been manually changed…" **[O]**. A service ID only re-attaches when it belongs to the same user.
- Host toggle: `enable-terminal` **[O]**; `PeerInfo.features.terminal` **[O]** advertises support.

### 2.4 Client [O]
`TerminalPage`, `TerminalTabPage`, `TerminalView`, `TerminalModel`, `TerminalConnectionManager` (one connection shared by tabs). API: `wire_session_open_terminal(id, terminal_id, rows, cols)`, `wire_session_send_terminal_input`, `wire_session_resize_terminal`, `wire_session_close_terminal`. Events: `terminal_response`.

[D] Client-side emulator: `alacritty_terminal` or `vt100` crate; render with your UI toolkit.

---

## 3. View camera [O]

- `LoginRequest.view_camera = ViewCamera{}` **[O]**. The host captures the **webcam** with `nokhwa` **[O]** (`libs/scrap/src/common/camera.rs`) and sends it through the normal video pipeline (`VideoFrame`) as "display 0".
- Pages: `ViewCameraPage`, `ViewCameraTabPage` **[O]**. Host toggle: `enable-camera` **[O]**. Client option: `disable_camera` **[O]**. Pro permission: `camera` **[R]**.
- Remote camera viewing is privacy-sensitive. The host's CM always shows the connection, so the local user knows.

---

## 4. Chat [O]

- `Misc.chat_message = ChatMessage{text}` **[O]**, in both directions.
- Client: `wire_session_send_chat` **[O]**. Host CM: `wire_cm_send_chat(conn_id, text)` **[O]**. UI: `ChatPage`, `ChatModel` (floating overlay or panel) **[O]**.
- Messages are not persisted.

---

## 5. Remote printing [O]

```
Host app prints ─► "RustDesk Printer" (v4 driver, drivers/RustDeskPrinterDriver) ─► render filter DLL
     ─► spool file (XPS/PDF) captured by printer_service.rs
     ─► sent to the client as FileTransferSendRequest{file_type = Printer}  (reuses file-transfer blocks)
Client receives job ─► event printer_request ─► user/auto choice
     ─► prints on a chosen local printer via printer_driver_adapter.dll (winspool: OpenPrinterW, …)
```
- Driver files shipped **[O]**: `RustDeskPrinterDriver.inf/.gpd/.cat`, `RustDeskPrinterDriverRenderFilter.dll`, `…-PipelineConfig.xml`. Installed via `UploadPrinterDriverPackageW`, `InstallPrinterDriverFromPackageW`, and ports via `XcvDataW` (winspool imports **[O]**). `libs/remote_printer/src/setup/{driver,setup}.rs` **[O]**.
- CLI: `--install-remote-printer`, `--uninstall-remote-printer` **[O]**. Option `enable-remote-printer` **[O]**.
- Client options **[O]**: `printer-incomming-job-action` (dialog / auto-print), `allow-printer-auto-print`, `allow-printer-dialog-save`, `printer-selected-name` (default vs a chosen printer). The local printer list comes from `wire_main_get_printer_names` **[O]**. The answer goes back through `wire_session_printer_response` **[O]**.
- `PrinterModel` (UI) **[O]**. Pro permission `remote_printer` **[R]**.

---

## 6. Whiteboard / annotation [O] modules, [R] behaviour

- Modules: `src/whiteboard/{client,server,windows}.rs`; CLI `--whiteboard`; `--whiteboard-gtk-sudo` on Linux **[O]**. The drawing stack is `tiny-skia`, `softbuffer`, `tao` (window) **[O]**.
- Behaviour **[R]**: a transparent, click-through, topmost overlay window on the **host** shows strokes or a pointer drawn by the controller, for presentations and support ("look here"). Strokes travel as small messages over the session and are rendered in the separate whiteboard process (IPC `Data::Whiteboard` **[O]**).
- Precise message format: **unknown** (not in `message.proto` as a named message, so it probably travels inside a plugin or `Misc` JSON payload). Verify with a decompiler.

---

## 7. Switch sides [O]

Lets the **controlled** user take control of the **controller's** machine using the same trust relationship.

```
Client ── Misc.switch_sides_request{uuid} ──► Host        (wire_session_switch_sides)
Host (CM) launches its own outgoing session to the client's ID, carrying the uuid
New conn: Host-as-client ── SwitchSidesResponse{uuid, lr: LoginRequest} ──► Client-as-host
Client-as-host verifies uuid == the one it generated → accepts without a password
The original session can be restored with Misc.switch_back{} (wire_cm_switch_back) [O]
```
The UUID is a one-time capability token, so no password is needed for the reverse direction **[R]**.

---

## 8. Elevation (portable/UAC) [O]

Problem: when RustDesk runs **portable** (not installed) as a normal user, it cannot capture UAC prompts or drive elevated windows.

- The host tells the client `Misc.uac = true` (a UAC prompt is showing) or `Misc.foreground_window_elevated = true` **[O]**. The client then shows an "elevation required" banner (`ElevationModel`, event `show_elevation` **[O]**).
- The client requests `Misc.elevation_request = ElevationRequest{ direct = true | logon = {username, password} }` **[O]** via `wire_session_elevate_direct` / `wire_session_elevate_with_logon` **[O]**.
  - `direct`: the host user sees a UAC consent prompt that **they** must accept (the CM can trigger it: `wire_cm_elevate_portable`, `wire_cm_can_elevate` **[O]**).
  - `logon`: the controller supplies admin credentials, and the host starts the elevated portable service with `CreateProcessWithLogonW` **[O]**.
- The result is `Misc.elevation_response` (an error string, or empty for OK) and `Misc.portable_service_running = true` **[O]**.
- Then the elevated **portable service** (`--portable-service`, running as SYSTEM) takes over capture and input. It shares frames with the unelevated process through **shared memory**, with an ACL and an IPC token check (many `portable service shared-memory` strings **[O]**). See 14.

---

## 9. Privacy mode [O] modules, [R] mechanics

Purpose: during support, the **host screen looks blank to anyone physically present** while the remote controller still sees the desktop.

| Implementation key [O] | Module | Mechanism [R] |
|---|---|---|
| `privacy_mode_impl_mag` | `privacy_mode/win_mag.rs` | A full-screen black topmost window is shown locally. Capture uses the **Magnifier API** with the black window excluded (`MagSetWindowFilterList` **[O]**), so the remote side sees through it. |
| `privacy_mode_impl_exclude_from_capture` | `privacy_mode/win_topmost_window.rs` + `WindowInjection.dll` **[O]** | A black overlay window marked with `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` (import **[O]**) so DXGI capture ignores it. The helper DLL makes the overlay robust. |
| `privacy_mode_impl_virtual_display` | `privacy_mode/win_virtual_display.rs` | Turns the physical monitors off and moves the session to an **Amyuni virtual display** that only the remote side sees. |
| (input blocking part) | `privacy_mode/win_input.rs` | While privacy mode is on, local keyboard and mouse input are blocked so the person at the machine can't interfere. Uses the exported hook callbacks (`privacy_mode_hook_keyboard/mouse` **[O]**); remote-injected input is still allowed through. |

- The client toggles with `wire_session_toggle_privacy_mode(impl_key, on)` → `Misc.toggle_privacy_mode{impl_key, on}` **[O]**. Supported implementations come from `wire_main_supported_privacy_mode_impls`, and the default from `wire_main_default_privacy_mode_impl` **[O]**.
- The host answers with `BackNotification{privacy_mode_state, details, impl_key}` **[O]**: `PrvOnSucceeded`, `PrvOnFailedDenied`, `PrvOnFailedPlugin`, `PrvOnByOther` (another controller already has it), `PrvNotSupported`, `PrvOffSucceeded`, `PrvOffByPeer`, … **[O]**.
- Permission: `Permission.PrivacyMode = 8` **[O]**, host option `enable-privacy-mode` **[O]**. Only one controller can hold privacy mode at a time. It is released automatically when that connection closes.
- UI strings **[O]**: "Enter privacy mode", "Exit privacy mode", "Peer denied", "Please install plugins", "Failed to turn off", "Turned off".

**Block input** (separate from privacy mode): `Permission.BlockInput = 7` **[O]**, `OptionMessage.block_input` **[O]**. The result is reported with `BackNotification.block_input_state` = `BlkOnSucceeded/BlkOnFailed/BlkOffSucceeded/BlkOffFailed` **[O]**. It only blocks the local user's input; there is no visual blanking.

---

## 10. Restart and lock

| Action | Message | Notes |
|---|---|---|
| Restart remote device | `Misc.restart_remote_device = true` **[O]** (`wire_session_restart_remote_device`) | Needs `Permission.Restart = 5` / `enable-remote-restart` **[O]**. Host calls `ExitWindowsEx` / `InitiateSystemShutdownEx` **[R]**. The client offers auto-reconnect after the reboot. |
| Lock screen | `KeyEvent{control_key = LockScreen(101)}` **[O]** (`wire_session_lock_screen`) | Host calls `LockWorkStation` **[R]** |
| Lock after session end | `OptionMessage.lock_after_session_end` **[O]** | Host locks when the last controlling connection closes |
| Ctrl+Alt+Del | `KeyEvent{control_key = CtrlAltDel(100)}` **[O]** (`wire_session_ctrl_alt_del`) | See 07 and 14 |
| Stop host service | `Misc.stop_service` **[O]** | Admin-only remote stop |

---

## 11. Plugin messages [O]

`Misc.plugin_request{id, content}` / `Misc.plugin_failure{id, name, msg}` **[O]** carry opaque payloads between a client plugin and a host plugin with the same `id`. See 15 §6.
