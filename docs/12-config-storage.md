# 12: Configuration and Storage

Code: `libs/hbb_common/src/config.rs` and `config/permanent_password.rs` **[O]**. Format: **TOML** via `toml 0.5.11` **[O]**, serialized from serde structs.

## 1. Locations [O]

| Who | Path |
|---|---|
| UI / CM / tray (user) | `%APPDATA%\RustDesk\config\` |
| Service and `--server` (LocalSystem) | `C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RustDesk\config\` **[O]**: the directory exists on this machine (unreadable without elevation), while `system32\config\systemprofile\…\RustDesk\config` does not. Both path fragments appear next to each other in `config.rs`'s strings, so the system-profile path is probably rewritten to LocalService **[R]**. The server process owns the main config files; the UI changes them through IPC ([02](02-architecture.md) §2). |
| Logs | User side: `%APPDATA%\RustDesk\log\` with subfolders `cm\`, `tray\`, `check-hwcodec-config\` **[O]**. Service and `--server`: `C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RustDesk\log\server\` **[O]** (exists; unreadable without elevation, so its file naming is not confirmed). Rotated files `RustDesk_r<date_time>.log`, the live file `RustDesk_rCURRENT.log`, and restart files `*.restart-NNNN.log` (`flexi_logger` **[O]**). |

Custom-named builds use their own app name for all of these (`get_rustdesk_app_name` **[O]**).

## 2. Files on this machine [O]

| File | Struct | Purpose |
|---|---|---|
| `RustDesk.toml` | `Config` | Device identity and access secrets |
| `RustDesk2.toml` | `Config2` | Server settings, NAT type, global options |
| `RustDesk_local.toml` | `LocalConfig` | UI-local state: last remote ID, window geometry, favourites, local options |
| `peers\<id>.toml` | `PeerConfig` | Per-remote-peer settings and cached peer info |
| `RustDesk_hwcodec.toml` | `HwCodecConfig` | Hardware codec probe results ([06](06-video-pipeline.md) §3.1) |
| `RustDesk_ab` | — | Address book cache (binary) ([13](13-account-api.md) §2.2) |
| `RustDesk_group` | — | Group/device list cache **[R]** |

### 2.1 `RustDesk.toml`: `Config` [O] keys
| Key | Meaning |
|---|---|
| `enc_id` | Device ID, stored protected |
| `password` | Permanent password as an **encrypted hash**, version prefix `01` (see §2.1.1) |
| `salt` | Per-install salt used in the login challenge and the password hash: 32 alphanumeric characters, stored in plain text |
| `key_pair` | Device identity key pair ([04](04-security-crypto.md) §1) |
| `key_confirmed` | ID server accepted our key |
| `[keys_confirmed]` | Same, per server host (e.g. `rs-ny = true`) |

All values in this file are sensitive. A clone should keep them in an OS keystore (see [04](04-security-crypto.md) §6 and [16](16-clone-blueprint.md) §5).

#### 2.1.1 Permanent password storage (1.4.x)

**Observed on this machine [O]** (shapes only; values not recorded):

| Value | Shape |
|---|---|
| `RustDesk.toml: password` | `"01"` + 116 base64 characters, no padding (87 bytes) |
| `RustDesk.toml: salt` | 32 alphanumeric characters, no prefix, not encrypted |
| `RustDesk.toml: enc_id`, `RustDesk2.toml: trusted_devices`, `peers/<id>.toml: password` | `"00"` + base64: the older at-rest scheme ([04](04-security-crypto.md) §6) |

So the permanent password is the only value stored with a **`01`** version prefix.

**Strings from `libs\hbb_common\src\config\permanent_password.rs` and `config.rs` [O]:**
- "Permanent password storage looks encrypted but cannot be decrypted"
- "Permanent password storage looks current but cannot be decoded as a hash"
- "Invalid permanent password encrypted hash storage"
- "Permanent password hash storage requires a non-empty salt"
- "Refusing to persist permanent password storage without salt"
- "Refusing to set salt because permanent password is hashed"
- "Salt is empty but permanent password is hashed and salt is empty"
- "Clearing invalid permanent password storage before storing config: "
- "Failed to compute permanent password storage; refusing update"
- "Rejecting non-current permanent password storage sync payload", "Invalid permanent password storage sync payload"

**IPC strings (UI ↔ daemon) [O]:** the IPC key `permanent-password-storage-and-salt` sits next to `permanent-password-set`, `permanent-password-is-preset` and `temporary-password`. Related messages: "Invalid permanent-password-storage-and-salt payload", "Failed to query permanent password state from daemon", "Changing permanent password was rejected by daemon", "Failed to sync permanent password storage from daemon: ", "Failed to set permanent password via IPC: ". Also "Permanent password accepted via logon-screen fallback" and the UI string "Permanent password is set (hidden)."

**Interpretation [R]:**
- **What is stored is a hash, encrypted, not the encrypted plaintext.** The most likely hash is `h1 = SHA256(password ‖ salt)`, which is exactly what the login challenge needs (`h2 = SHA256(h1 ‖ challenge)`, [04](04-security-crypto.md) §3). The host never needs the plaintext, which explains why the UI can only say the password "is set (hidden)".
- **The salt is bound to the hash.** Changing `salt` would invalidate the stored hash, so it is refused while a hashed password exists, and a hash can't be stored without a salt.
- **Versioning.** `00` = legacy (encrypted plaintext); `01` = current (encrypted hash). The "looks current" / "non-current" wording implies a migration: legacy values are re-hashed into `01`, and stale sync payloads are rejected.
- **The daemon is the authority.** The service/`--server` process owns the storage. UI processes query its state and receive the storage plus salt over IPC (`permanent-password-storage-and-salt`), never the plaintext.
- **The byte layout of the 87-byte blob is unknown.** It matches neither the `00` scheme applied to a 32-byte hash (secretbox with zero nonce = 48 bytes) nor that scheme with a prepended 24-byte nonce (72 bytes). The `01` encryption therefore differs, or the plaintext isn't a raw 32-byte hash. Decompiling `permanent_password.rs` is the way to settle this.

**[D] for a clone:** storing `h1` instead of the password is an improvement, but in this protocol `h1` is still *password-equivalent*: anyone who holds it can answer the challenge. Protect it at rest with the OS keystore, or remove the problem with the SPAKE2 design in [16](16-clone-blueprint.md).

### 2.2 `RustDesk2.toml`: `Config2` [O] keys
| Key | Meaning |
|---|---|
| `rendezvous_server` | Last working ID server `host:port` |
| `nat_type` | 0 unknown / 1 asymmetric / 2 symmetric |
| `serial` | Version of the server-pushed config |
| `unlock_pin` | Settings unlock PIN (protected) |
| `trusted_devices` | 2FA trusted devices (protected) |
| `socks` | Proxy settings `{proxy, username, password}` **[R]** |
| `[options]` | Global option map, see §3 (seen here: `local-ip-addr`, `av1-test`, `verification-method`, `approve-mode`) |

### 2.3 `RustDesk_local.toml`: `LocalConfig` [O]
```toml
remote_id = '74318467'        # last ID typed in the main window
kb_layout_type = ''           # ISO/ANSI for Map keyboard mode
size = [0, 0, 0, 0]           # main window geometry
fav = []                      # favourite peer IDs
[options]                     # local-only options, e.g. remote-menubar-drag-left/right
[ui_flutter]                  # Flutter-side UI state: wm_Main, wm_RemoteDesktop (window positions), peer-sorting
```

### 2.4 `peers\<id>.toml`: `PeerConfig` [O]
Observed keys (this machine):
```toml
password = <remembered login digest, protected>   # see 04 §3
size = [0,0,0,0]          # remote window geometry
size_ft = [0,0,0,0]       # file-transfer window geometry
size_pf = [0,0,0,0]       # port-forward window geometry
view_style = 'original'   # original | adaptive | custom
scroll_style = 'scrollauto'
edge_scroll_edge_thickness = 100
image_quality = 'balanced'
custom_image_quality = [50]
show_remote_cursor = false
lock_after_session_end = false
terminal-persistent = false
privacy_mode = false
allow_swap_key = false
port_forwards = []
direct_failures = 0
disable_audio = false
disable_clipboard = false
enable-file-copy-paste = true
show_quality_monitor = false
follow_remote_cursor = false
follow_remote_window = false
keyboard_mode = 'map'
view_only = false
show_my_cursor = false
sync-init-clipboard = false
trackpad-speed = 100

[options]                 # per-peer string options
custom-fps = '30'
collapse_toolbar = 'Y'
zoom-cursor = ''
codec-preference = 'auto'
remote-menubar-drag-x = '0.5'
remote-menubar-frac = '0.5'
i444 = ''
remote-menubar-edge = 'top'
swap-left-right-mouse = ''

[ui_flutter]              # wm_RemoteDesktop …

[info]                    # cached PeerInfo for the peer list
username = '…'
hostname = '…'
platform = 'Linux'

[transfer]                # unfinished file jobs (09 §3.3)
write_jobs = []
read_jobs = []
```
Further keys from the string table **[O]**: `rdp_port`, `rdp_username`, `rdp_password`, `login_name`, `display_name`, `alias` (via `wire_main_set_peer_alias`). Preloading: "Preload peers done in 1.9ms, batch_count: 100, total: 1" **[O]**. Peers load in batches so the main window opens quickly with hundreds of peers.

## 3. Option catalogue

Options are a flat `String → String` map, with `"Y"`/`"N"`/`""` for booleans. They live in three scopes:
- **Global** (`Config2.options`): read/written through `wire_main_get_option` / `wire_main_set_option` **[O]**, via IPC to the server.
- **Local** (`LocalConfig.options`): `wire_main_get_local_option` / `wire_main_set_local_option` **[O]**.
- **Per peer** (`PeerConfig.options`): `wire_main_get/set_peer_option`, `wire_session_*option*` **[O]**.

**User defaults** for new peers come from `wire_main_get/set_user_default_option` **[O]**.

All keys below are **[O]** (from the binary's strings). Meanings are **[R]** where they aren't obvious from the name.

### 3.1 Server and network
`custom-rendezvous-server`, `relay-server`, `api-server`, `key`, `proxy-url`, `proxy-username`, `proxy-password`, `allow-websocket`, `disable-udp`, `enable-udp-punch`, `enable-ipv6-punch`, `direct-server`, `direct-access-port`, `allow-hostname-as-id`, `use-raw-tcp-for-api`, `allow-insecure-tls-fallback`, `allow-https-21114`, `local-ip-addr`, `enable-lan-discovery`, `disable-discovery-panel`

### 3.2 Security and access
`approve-mode`, `verification-method`, `use-temporary-password`, `use-permanent-password`, `use-both-passwords`, `temporary-password-length`, `allow-numeric-one-time-password`, `whitelist`, `enable-2fa` (title/desc keys), `enable-trusted-devices`, `allow-only-conn-window-open`, `allow-remote-config-modification`, `allow-remote-cm-modification`, `disable-change-permanent-password`, `disable-change-id`, `disable-unlock-pin`, `allow-scope-violation-alarm`, `hide_cm`, `default-connect-password`, `custom-os-password`

### 3.3 Host permissions (defaults for incoming sessions)
`enable-keyboard`, `enable-clipboard`, `enable-file-transfer`, `one-way-file-transfer`, `enable-audio`, `enable-camera`, `enable-terminal`, `enable-tunnel`, `enable-remote-restart`, `enable-record-session`, `enable-block-input`, `enable-privacy-mode`, `enable-remote-printer`, `enable-file-copy-paste`, `enable-perm-change-in-accept-window`, `allow-auto-disconnect`, `auto-disconnect-timeout`, `allow-remove-wallpaper`, `keep-awake-during-incoming-sessions`, `keep-awake-during-outgoing-sessions`, `allow-auto-record-incoming`, `allow-auto-record-outgoing`, `allow-linux-headless`, `stop-service`, `terminal-admin`

### 3.4 Video and display
`codec-preference`, `enable-hwcodec`, `enable-directx-capture`, `i444`, `custom-fps`, `image-rendering`, `use-texture-render`, `allow-d3d-render`, `allow-always-software-render`, `av1-test`, `video-save-directory`, `show-quality-monitor`, `view-only`, `zoom-cursor`, `follow-remote-cursor`, `follow-remote-window`, `show-remote-cursor`, `show-my-cursor`, `virtual-display`, `privacy-mode-impl-key`

### 3.5 Input
`swap-left-right-mouse`, `trackpad-speed`, `allow-wayland-keyboard`, `show-virtual-mouse`, `show-virtual-joystick`, `keyboard_mode` (per peer)

### 3.6 Audio
`audio-input`, `disable-audio`

### 3.7 UI and behaviour
`enable-confirm-closing-tabs`, `enable-open-new-connections-in-tabs`, `allow-monitor-switch-main-toolbar`, `allow-multi-edge-toolbar-dock`, `allow-remote-toolbar-docking-any-edge`, `hide-toolbar`, `collapse_toolbar`, `remote-menubar-*`, `peer-sorting`, `sync-ab-with-recent-sessions`, `sync-ab-tags`, `allow-ask-for-note`, `enable-check-update`, `allow-auto-update`, `enable-flutter-http-on-rust`, `disable-warnings`

### 3.8 Printer
`printer-incomming-job-action`, `allow-printer-auto-print`, `allow-printer-dialog-save`, `printer-selected-name`

### 3.9 Custom-client "hide" switches
`hide-help-cards`, `hide-network-settings`, `hide-proxy-settings`, `hide-websocket-settings`, `hide-security-settings`, `hide-server-settings`, `hide-remote-printer-settings`, `hide-stop-service`, `hide-username-on-card`, `hide-powered-by-me`

### 3.10 Deployment presets
`preset-user-name`, `preset-strategy-name`, `preset-address-book-name`, `preset-address-book-tag`, `preset-address-book-alias`, `preset-address-book-password`, `preset-address-book-note`, `preset-device-group-name`, `preset-device-username`, `preset-device-name`, `preset-note` **[R]** (the strings show the `preset-…-name` prefixes)

### 3.11 Option override layers [O] API, [R] semantics
1. **Built-in / hard options**, compiled into custom clients: `wire_main_get_hard_option`, `wire_main_get_buildin_option`. Highest priority.
2. **Strategy / override options** pushed by the Pro server (`override-settings`, `default-settings` strings **[O]**). Overrides are **fixed** (`wire_main_is_option_fixed`). Defaults only seed values.
3. User-set values.
4. Code defaults.

## 4. Custom clients (`src/custom_server.rs`) [O]

A downloaded EXE can carry server settings **in its file name** **[R]**: for example `rustdesk-host=<server>,key=<base64>.exe` (also `relay=`, `api=`). On first run these are parsed into options. RustDesk's web console (Pro) can also build a fully custom client with a different app name, icon, hard options and preset password. Related flags: `wire_is_custom_client`, `wire_is_incoming_only`, `wire_is_outgoing_only`, `wire_is_disable_settings`, `wire_is_disable_installation` **[O]**.

## 5. Import / export [O]

- CLI: `--config <string>` / `--import-config <file>` **[O]** imports server settings. `--option <key> [value]` reads or writes one option. `--get-id`, `--set-id` **[O]**.
- UI: the Network settings page offers copy/paste of a server config string (base64 JSON `{host, relay, api, key}` **[R]**). The URL scheme `rustdesk://config/<string>` does the same ([14](14-windows-platform.md) §8).

## 6. Clone notes [D]

- Split **non-secret config** (TOML, versioned with a `schema_version` key and migrations) from **secrets** (OS keystore).
- Give every option a typed definition in one table: key, scope, type, default, UI section, whether policy can fix it. Generate the settings UI and validation from that table.
- Write files atomically (temp file + rename) and keep one owner process per file to avoid races. RustDesk does this through the server process.
