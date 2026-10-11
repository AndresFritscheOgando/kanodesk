# 13: Account and Management API (RustDesk Server Pro)

The open-source server (hbbs/hbbr) has **no** HTTP API. These endpoints are served by **RustDesk Server Pro** (or compatible third-party servers) on the configured `api-server`, defaulting to `http://<rendezvous-host>:21114` (`:21114`, `allow-https-21114` **[O]**). Client modules: `src/hbbs_http/{account,http_client,sync,record_upload,downloader}.rs` **[O]**. UI models: `UserModel`, `AbModel`, `GroupModel` **[O]**.

## 1. HTTP client (`hbbs_http/http_client.rs`) [O]

- Built on `reqwest 0.12` **[O]**. TLS is tried in order: **rustls** (with native roots), then **native-tls** (schannel), then **accept invalid cert**, but only if `allow-insecure-tls-fallback` is on. Log strings: "Failed to connect to server … with native-tls: …, trying accept invalid cert", "… with rustls-tls: …, trying native-tls", "Successfully connected to server" **[O]**.
- Honours the configured proxy (`socks5://…` / HTTP proxy) **[O]**.
- `use-raw-tcp-for-api` **[O]**: an alternative transport for API calls **[R]**.
- The UI can route its own HTTP through Rust (`wire_main_http_request`, `enable-flutter-http-on-rust` **[O]**), so proxy and TLS settings apply to every request.
- Auth header: `Authorization: Bearer <access_token>` **[O]**.

## 2. Endpoint catalogue

All routes below are **[O]** (seen as strings). Request and response field names are **[R]**, partly confirmed by the strings `access_token`, `tfa_type`, `strategy`, `sysinfo`, `modified_at`, `conns`, `guid`, `note`, `hash`, `ab_entries`, `device_groups` **[O]**.

### 2.1 Authentication
| Route | Method | Purpose |
|---|---|---|
| `/api/login-options` | GET | Which login methods the server offers (password, OIDC providers such as `oidc/<name>`, `common-oidc/…`) |
| `/api/login` | POST | `{username, password, id, uuid, autoLogin, type:"account", deviceInfo{os,type,name}, verificationCode?, tfaCode?, secret?}` → `{access_token, type:"access_token", user{name, email, note, status, is_admin, info…}}`, or `{type:"email_check"|"tfa_check", tfa_type, secret}` for a second step |
| `/api/oidc/auth` | POST | `{op:<provider>, id, uuid, deviceInfo}` → `{code, url}`. The client opens `url` in the browser. |
| `/api/oidc/auth-query` | GET `?code=&id=&uuid=` | Polled until the browser login finishes → same payload as `/api/login`. Log strings: "Requesting account auth", "Waiting account auth", "No authed oidc is found", "Login account auth", "Failed query oidc", "Invalid auth response" **[O]**. FFI: `wire_main_account_auth(op, remember)`, `wire_main_account_auth_cancel`, `wire_main_account_auth_result` **[O]**. |
| `/api/logout` | POST | `{id, uuid}` |
| `/api/currentUser` | POST | Refresh the user profile and check the token is still valid |

The token and user info are kept in local options (`access_token`, `user_info` **[R]**).

### 2.2 Address book (AB)
Legacy single-AB API:
| Route | Method | Purpose |
|---|---|---|
| `/api/ab` | GET/POST | Whole address book as one JSON blob `{data: "<json {tags, peers, tag_colors}>"}` |

Newer multi-AB API (personal + shared profiles):
| Route | Method | Purpose |
|---|---|---|
| `/api/ab/settings` | POST | Server limits, e.g. max peers per AB |
| `/api/ab/personal` | POST | The guid of the user's personal AB |
| `/api/ab/shared/profiles` | POST (paged) | Shared ABs visible to the user, with their rules (read / read-write / full) |
| `/api/ab/peers` | POST (paged, `?ab=<guid>`) | Peers in an AB: `{id, hash, username, hostname, platform, alias, tags[], note, password?}` |
| `/api/ab/peer/add/<guid>` | POST | Add a peer |
| `/api/ab/peer/update/<guid>` | PUT | Update a peer (alias, tags, note, password hash) |
| `/api/ab/peer/<guid>` | DELETE | Remove peers (body: an id list) |
| `/api/ab/tags/<guid>` | POST | List tags with colours |
| `/api/ab/tag/add/<guid>`, `/api/ab/tag/rename/<guid>`, `/api/ab/tag/update/<guid>` | POST/PUT | Tag CRUD; colours in `tag_colors` **[O]** |

The AB is cached locally in `RustDesk_ab` **[O]** for offline display. FFI: `wire_main_load_ab`, `wire_main_save_ab`, `wire_main_clear_ab`, `wire_main_load_recent_peers_for_ab` **[O]**. Options: `sync-ab-with-recent-sessions`, `sync-ab-tags` **[O]**.

**Password hash in the AB**: `sync_peer_hash_password_to_personal_ab` **[O]** uploads the remembered per-peer password digest ([04](04-security-crypto.md) §3), so the same account can connect from another device without retyping.

### 2.3 Groups and devices
| Route | Purpose |
|---|---|
| `/api/users` | (paged) Users in the user's group, for the "Accessible devices" tab |
| `/api/peers` | (paged) Devices visible to the user, with online status |
| `/api/device-group/accessible` | Device groups the user can access |

Cached in `RustDesk_group` **[R]**. FFI: `wire_main_load_group`, `wire_main_save_group`, `wire_main_clear_group` **[O]**. UI: `MyGroupPeerView`, `MyGroupPeerCard` **[O]**. Option: `preset-device-group-name` **[O]**.

### 2.4 Heartbeat and sync (`hbbs_http/sync.rs`) [O]
| Route | Purpose |
|---|---|
| `/api/heartbeat` | Sent by the **host** about every 15–30 s **[R]**: `{id, uuid, ver, conns:[active conn ids], modified_at}`. The reply can carry `sysinfo` (please upload system info), `disconnect:[conn ids]` (server-side kick), `modified_at` and `strategy` (pushed settings policy) |
| `/api/sysinfo` | Upload `{cpu, memory, os, hostname, username, version, id, uuid, …}` when asked |

**Strategy**: a server-managed set of options pushed to the device. Applied options become **fixed** (`wire_main_is_option_fixed` **[O]**), and the settings UI shows them read-only.

### 2.5 Audit
| Route | Purpose |
|---|---|
| `/api/audit/conn` | Connection events: `{action:"new"|"close", id, uuid, conn_id, session_id, ip, type, peer:[id,name]}`. Also a **note** typed by the controller (`wire_session_send_note`, `allow-ask-for-note`, `wire_session_get_last_audit_note` **[O]**) |
| `/api/audit/file` | File-transfer events `{id, peer_id, path, is_file, type: upload/download, info{files[]}}` |
| `/api/audit/alarm` | Alarms, e.g. too many failed logins, scope violation (`allow-scope-violation-alarm` **[O]**) |

The connection is linked to its audit record through `wire_session_get_audit_guid`, `wire_session_set_audit_guid`, `wire_session_get_audit_server_sync`, and `wire_session_get_conn_token` **[O]**.

### 2.6 Recording upload (`hbbs_http/record_upload.rs`) [O]
`/api/record` **[O]**. Uploads session recordings in parts. Fields seen in the strings: `part`, `offset`, `length`, `tail` **[O]** (a chunked upload with a final "tail" header rewrite for the container). Log strings: "upload success, file:", "upload stop:" **[O]**.

### 2.7 Deployment / CLI
| Route | Purpose |
|---|---|
| `/api/devices/deploy` | `--deploy` **[O]**: register this device with the server using a token. Strings: "token is required!", "Failed to persist deployed id locally", "Cannot deploy an unregistrable device!" **[O]**. FFI `wire_main_deploy_device` **[O]**. |
| `/api/devices/cli` | `--assign --token <t> [--user_name] [--strategy_name] [--address_book_name --address_book_tag --address_book_alias --address_book_password --address_book_note] [--device_group_name] [--note] [--device_username] [--device_name]` **[O]** |

### 2.8 Misc
- `wire_main_resolve_avatar_url` **[O]**: user avatar from the server.
- `wire_main_get_login_device_info` **[O]**: `{os, type, name}` sent at login.
- `wire_main_is_using_public_server` **[O]**: hides account features on the public server.
- `wire_is_disable_account`, `wire_is_disable_ab`, `wire_is_disable_group_panel` **[O]**: custom-client switches.

## 3. Software update (`src/updater.rs`, `hbbs_http/downloader.rs`) [O]

- Version check: `wire_main_get_new_version`, `wire_main_get_software_update_url` **[O]**, plus `SoftwareUpdate{url}` pushed by hbbs **[O]**.
- Download: the downloader writes to a temp file with progress events ("Download … end, … %", "Failed to remove stale download file" **[O]**). FFI `wire_main_update_me` **[O]**. UI `update_progress.dart` **[O]**.
- Install: the downloaded MSI/EXE is run with `--update` **[O]**. Options: `enable-check-update`, `allow-auto-update` **[O]**.

## 4. Clone notes [D]
- Your own server can implement a compatible subset: login + `/api/ab` (legacy) + heartbeat is enough for the official client's address book to work.
- Use short-lived access tokens plus refresh tokens. The observed client only stores a bearer token.
- Paginate everything, and use `modified_at`-style ETags so heartbeats stay cheap at scale.
