# 11: Host (Controlled) Side: Server, Services, Connection Manager, Permissions

## 1. Server bootstrap (`src/server.rs`) [O] module, [R] flow

The `--server` process (started by the Windows service in the active session; log string "start --server with user" **[O]**) does the following:

1. Loads config and the identity key.
2. Starts `RendezvousMediator` for each configured rendezvous server: registration loop, punch/relay request handling ([03](03-networking-rendezvous.md)).
3. Optionally starts the **direct-access listener** (`direct-server`, port `direct-access-port`) and the **LAN discovery** responder.
4. Starts the IPC server (`\\.\pipe\RustDesk\query`) for the UI, CM and tray.
5. Creates the **service registry**: one `Server` object holding named services. They start lazily when the first subscriber arrives and stop when the last one leaves.
6. For each accepted peer stream, spawns `Connection::start` (`server/connection.rs` **[O]**).

## 2. Service framework (`server/service.rs`) [O] module, [R] design

```rust
// conceptual shape [R]
trait Service { fn name(&self) -> String; fn on_subscribe(&self, sub: ConnInner); fn on_unsubscribe(&self, id: i32); fn is_subed(&self, id: i32) -> bool; fn join(&self); }
struct GenericService { sp: Arc<RwLock<ServiceInner>> } // ServiceInner { subscribes: HashMap<conn_id, ConnInner>, active: bool, ... }
// Each service runs its own thread/task: loop { produce Message; for each subscriber: sub.send(msg.clone()) }
```

| Service | Module | Produces | Subscribed when |
|---|---|---|---|
| `video_service_<display>` | `video_service.rs` **[O]** | `VideoFrame` | session type desktop/camera, per captured display |
| `audio_service` | `audio_service.rs` **[O]** | `AudioFrame`, `Misc.audio_format` | Audio permission on and audio not disabled |
| `clipboard_service` | `clipboard_service.rs` **[O]** | `MultiClipboards` | Clipboard permission on |
| `input_service` (mouse/cursor) | `input_service.rs` **[O]** | `CursorData`, `CursorPosition`, `cursor_id` | desktop sessions |
| `printer_service` | `printer_service.rs` **[O]** | print jobs as file-transfer messages | remote-printer enabled |
| `terminal_service` | `terminal_service.rs` **[O]** | `TerminalResponse` | terminal sessions |
| portable service bridge | `portable_service.rs` **[O]** | replaces capture/input when running elevated | portable plus elevation |

**Multiple viewers**: services are shared. One capture and encode per display feeds every connection that subscribed to it. That is efficient, but it forces the lowest common codec and FPS ([06](06-video-pipeline.md) §3.2, review §3.3).

## 3. Connection (`server/connection.rs`) [O] module

### 3.1 Per-connection state [R]
`Connection { inner: ConnInner{id, tx}, stream, server: Weak<Server>, peer_info, authorized, keyboard, clipboard, audio, file, restart, recording, block_input, privacy_mode, show_remote_cursor, lock_after_session_end, last_test_delay, video_ack_required, file_transfer: Option<(dir, show_hidden)>, port_forward_socket, terminal..., disable_*..., tx_to_cm, session_key, ... }`

### 3.2 Login pipeline
See [05](05-wire-protocol.md) §4.2 and [04](04-security-crypto.md). Host-side checks in order **[R]**:
1. IP whitelist (`whitelist` **[O]**).
2. `allow-only-conn-window-open` **[O]**: refuse if the main window isn't open.
3. Session-type enabled? `enable-file-transfer`, `enable-tunnel`, `enable-terminal`, `enable-camera` **[O]**.
4. Login-failure throttle (`login_failure_check.rs` **[O]**).
5. Password, click approval, or both (`approve-mode`, `verification-method` **[O]**).
6. 2FA / trusted device.
7. Pro access control: `ControlPermissions` from rendezvous **[R]** can **narrow** the permissions granted.
8. Send `LoginResponse{peer_info}`. Notify the CM (`Data::Login`). Audit (`/api/audit/conn`). Start the video/audio/clipboard subscriptions.

### 3.3 Runtime permissions [O]
```
enum PermissionInfo.Permission { Keyboard=0; Clipboard=2; Audio=3; File=4; Restart=5; Recording=6; BlockInput=7; PrivacyMode=8; }
```
- Initial values come from host options (`enable-keyboard`, `enable-clipboard`, `enable-audio`, `enable-file-transfer`, `enable-remote-restart`, `enable-record-session`, `enable-block-input`, `enable-privacy-mode` **[O]**).
- The CM user can flip them **live**: `wire_cm_switch_permission(conn_id, name, enabled)` **[O]** → IPC `SwitchPermission` **[O]** → server updates the connection, (un)subscribes services, and tells the client `Misc.permission_info{permission, enabled}` **[O]**. The client UI greys out the matching toolbar items.
- `enable-perm-change-in-accept-window` **[O]**: whether the toggles appear in the accept dialog.
- Pro `ControlPermissions` bits **[R]**: keyboard, remote_printer, clipboard, file, audio, camera, terminal, tunnel, restart, recording, block_input, remote_modify.

### 3.4 Inactivity / auto-disconnect [O]
- `allow-auto-disconnect` + `auto-disconnect-timeout` (minutes) **[O]**. With no input from the controller for that long, the host closes the connection with reason "Connection failed due to inactivity" / "auto disconnect" **[O]**.

### 3.5 Session end [R]
Unsubscribe from all services → release privacy mode / block input if held → restore resolution and wallpaper → `lock_after_session_end` → tell the CM (`Data::Close`) → audit close → if this was the last connection, regenerate the temporary password **if configured** (policy unverified, see review §4).

## 4. Connection Manager (CM) [O]

The CM is a user-session process (`--cm`) with its own window. The server talks to it through `\\.\pipe\RustDesk\query_cm` **[O]** (the CM log shows `Started ipc_cm server at path: \\.\pipe\RustDesk\query_cm`, `Got new connection`, `ipc task begin`, `conn_id: 969`, `cm ipc connection closed from connection request`, `ipc task end` **[O]**).

### 4.1 What the CM shows (per incoming connection) [R], from `ServerModel`/`DesktopServerPage` **[O]**
- Peer name, ID, avatar (`LoginRequest.avatar` **[O]**), connection type (desktop / file / port forward / terminal / camera), and the time connected.
- **Accept / Dismiss** buttons (click approval), with the anti-spoof click-timing check (`wire_cm_check_click_time`, `wire_cm_get_click_time` **[O]**; purpose inferred in review §2).
- Permission toggles (§3.3).
- Chat tab, file-transfer log (`CmFileModel` **[O]**), voice call accept/hang-up.
- "Disconnect" and "Switch back" (`wire_cm_switch_back` **[O]**).
- Elevate button for portable mode (`wire_cm_elevate_portable` **[O]**).

### 4.2 CM FFI API [O]
| Function | Purpose |
|---|---|
| `wire_cm_init` | Start the CM IPC server and event stream |
| `wire_cm_login_res(conn_id, res)` | Accept/deny a pending connection |
| `wire_cm_close_connection(conn_id)` | Kick a peer |
| `wire_cm_remove_disconnected_connection` | Remove a closed entry from the list |
| `wire_cm_switch_permission(conn_id, name, enabled)` | Live permission toggle |
| `wire_cm_send_chat(conn_id, text)` | Chat |
| `wire_cm_handle_incoming_voice_call(id, accept)`, `wire_cm_close_voice_call` | Voice |
| `wire_cm_get_clients_state`, `wire_cm_get_clients_length`, `wire_cm_check_clients_length` | List and refresh |
| `wire_cm_can_elevate`, `wire_cm_elevate_portable` | Portable elevation |
| `wire_cm_switch_back` | Switch sides back |
| `wire_cm_get_config` | Read options from the CM |
| `wire_cm_check_click_time`, `wire_cm_get_click_time` | Click-timing check |

### 4.3 CM-only options [O]
`hide_cm` (config key **[O]**) hides the CM window for unattended setups (only allowed with a permanent password / custom client **[R]**). `allow-remote-cm-modification` **[O]**.

## 5. Host-side session behaviours [O] options

| Option | Effect |
|---|---|
| `allow-remove-wallpaper` / `wire_main_support_remove_wallpaper`, `wire_main_test_wallpaper` | Hide the wallpaper while a session is active (less bandwidth). Restored afterwards. |
| `keep-awake-during-incoming-sessions` | `SetThreadExecutionState` keeps the display and system awake **[R]** |
| `allow-auto-record-incoming` | Record every incoming session (default dir under `ProgramData\…\recording` when running as a service **[R]**; strings `\ProgramData\`, `\recording` **[O]**) |
| `enable-lan-discovery` | Answer LAN pings |
| `stop-service` | Stop accepting connections (the UI's "Stop service" button) |
| `allow-linux-headless` | Allow sessions without a logged-in display (Linux) |

## 6. Firewall [O]

During install, RustDesk adds inbound and outbound firewall rules for its program path (strings: `netsh advfirewall firewall add rule name=" Service" dir=in action=allow program="" enable=yes` and the `delete rule` counterpart **[O]**). This is needed for direct-access and LAN connections.

## 7. Clone notes [D]
- Model services as `tokio::sync::broadcast` channels per stream type. Each connection task holds receivers for what it is allowed to see; toggling a permission just drops or creates a receiver.
- Keep one "authority" for permissions (connection state), push changes to both the CM and the client, and audit every change.
- Put the CM accept dialog in its own process on the interactive desktop, and add a short delay before "Accept" can be clicked. That protects against a remote controller accepting its own second connection.
