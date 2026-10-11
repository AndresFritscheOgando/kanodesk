# 05: Wire Protocol

## 1. Framing (`libs/hbb_common/src/bytes_codec.rs`, `stream.rs`) [R]

Every message on a peer or rendezvous TCP/KCP stream is one **protobuf message inside a variable-length frame**:

```
header (1–4 bytes, little-endian)  │  payload (len bytes)
low 2 bits of byte0 = header_len-1 │
len = header_value >> 2            │
```

| len range | header bytes | encoding |
|---|---|---|
| ≤ 0x3F | 1 | `(len << 2) | 0` as u8 |
| ≤ 0x3FFF | 2 | `(len << 2) | 1` as u16 LE |
| ≤ 0x3FFFFF | 3 | `(len << 2) | 2` as u24 LE |
| ≤ 0x3FFFFFFF | 4 | `(len << 2) | 3` as u32 LE |

- Decoding reads byte0, gets `n = (b0 & 3) + 1`, reads `n` header bytes LE, `len = v >> 2`, then reads `len` payload bytes.
- After the secure handshake, `payload = secretbox(protobuf_bytes)`. The header is **not** encrypted and covers the ciphertext length.
- UDP rendezvous datagrams carry a raw `RendezvousMessage` with **no** header.
- WebSocket: one binary WS message per payload, with no header **[R]**.

```rust
// [D] tokio_util codec sketch
impl Decoder for BytesCodec {
    type Item = BytesMut; type Error = io::Error;
    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        if src.is_empty() { return Ok(None) }
        let n = (src[0] & 3) as usize + 1;
        if src.len() < n { return Ok(None) }
        let mut v = 0u32; for i in 0..n { v |= (src[i] as u32) << (8 * i) }
        let len = (v >> 2) as usize;
        if src.len() < n + len { src.reserve(n + len - src.len()); return Ok(None) }
        src.advance(n); Ok(Some(src.split_to(len)))
    }
}
```

## 2. Schemas

- **Peer protocol**: [`proto/message.proto`](proto/message.proto). It was **recovered exactly** from the descriptor embedded in `librustdesk.dll` **[O]**: package `hbb`, 119 messages and enums. The top-level envelope is `Message { oneof union { … 30 arms … } }`.
- **Rendezvous protocol**: [`proto/rendezvous.proto`](proto/rendezvous.proto), reconstructed **[R]**. Names were confirmed in the strings **[O]**.

RustDesk uses `rust-protobuf 3.7.2` **[O]**. **[D]** In a clone you can use `prost` with the same `.proto` and stay wire-compatible, because protobuf is codegen-agnostic.

## 3. `Message` envelope arms (exact field numbers) [O]

| # | Arm | Direction | Purpose |
|---|---|---|---|
| 3 | `signed_id: SignedId` | H→C | First message: signed ephemeral key ([04](04-security-crypto.md)) |
| 4 | `public_key: PublicKey` | C→H | Sealed session key |
| 5 | `test_delay: TestDelay` | both | RTT probe, QoS ([06](06-video-pipeline.md)) |
| 6 | `video_frame: VideoFrame` | H→C | Encoded frames for one display |
| 7 | `login_request: LoginRequest` | C→H | Auth + session type + options |
| 8 | `login_response: LoginResponse` | H→C | Error or `PeerInfo` |
| 9 | `hash: Hash` | H→C | Password salt + challenge |
| 10 | `mouse_event: MouseEvent` | C→H | Mouse |
| 11 | `audio_frame: AudioFrame` | both | Opus packet |
| 12 | `cursor_data: CursorData` | H→C | Cursor bitmap |
| 13 | `cursor_position: CursorPosition` | H→C | Cursor position |
| 14 | `cursor_id: uint64` | H→C | Switch to a cached cursor |
| 15 | `key_event: KeyEvent` | C→H | Keyboard |
| 16 | `clipboard: Clipboard` | both | Single clipboard item (legacy) |
| 17 | `file_action: FileAction` | both | File-system command |
| 18 | `file_response: FileResponse` | both | File-system reply / data block |
| 19 | `misc: Misc` | both | 33 control sub-messages |
| 20 | `cliprdr: Cliprdr` | both | File clipboard protocol |
| 21 | `message_box: MessageBox` | H→C | Remote message box |
| 22 | `switch_sides_response` | C→H | Switch sides |
| 23/24 | `voice_call_request/response` | both | Voice call |
| 25 | `peer_info: PeerInfo` | H→C | Updated peer info mid-session (displays changed) |
| 26 | `pointer_device_event` | C→H | Touch gestures |
| 27 | `auth_2fa: Auth2FA` | C→H | TOTP code |
| 28 | `multi_clipboards` | both | Several formats at once (text+html+rtf+image) |
| 29/30 | `screenshot_request/response` | C→H / H→C | Remote screenshot |
| 31/32 | `terminal_action/response` | C→H / H→C | Terminal |

### `Misc` sub-messages [O]

`chat_message(4)`, `switch_display(5)`, `permission_info(6)`, `option(7)`, `audio_format(8)`, `close_reason(9)`, `refresh_video(10)`, `video_received(12)`, `back_notification(13)`, `restart_remote_device(14)`, `uac(15)`, `foreground_window_elevated(16)`, `stop_service(17)`, `elevation_request(18)`, `elevation_response(19)`, `portable_service_running(20)`, `switch_sides_request(21)`, `switch_back(22)`, `change_resolution(24)`, `plugin_request(25)`, `plugin_failure(26)`, `full_speed_fps(27)`, `auto_adjust_fps(28)`, `client_record_status(29)`, `capture_displays(30)`, `refresh_video_display(31)`, `toggle_virtual_display(32)`, `toggle_privacy_mode(33)`, `supported_encoding(34)`, `selected_sid(35)`, `change_display_resolution(36)`, `message_query(37)`, `follow_current_display(38)`.

## 4. Session state machine

### 4.1 Client side (`client.rs` + `client/io_loop.rs`) [R], log lines [O]

```
[Idle]
  │ session_start(id)  (wire_session_add_sync + wire_session_start)
  ▼
[Rendezvous]──offline/not-exist──► msgbox error, end
  │ punch ok | relay ok
  ▼
[Securing]  recv SignedId → send PublicKey           "secure_connection ok"
  ▼
[WaitHash]  recv Hash{salt,challenge}
  │ have stored h1? → send LoginRequest
  │ else           → UI password prompt (msgbox "input-password") → send LoginRequest
  ▼
[WaitLogin] recv LoginResponse
  ├─ error "Wrong Password"  → re-prompt (attempt counter)
  ├─ error "2FA Required"    → prompt code → send Auth2FA → back to WaitLogin
  ├─ (click-approve)         → "waiting for accept" until LoginResponse arrives
  └─ peer_info               → "handle_peer_info"                     [O]
  ▼
[Active]
   on peer_info: store [info] in peers/<id>.toml, remember password (log "remember password of …") [O]
   pick decoder per display from SupportedEncoding (log "new video handler for display #0, format: VP9") [O]
   start clipboard listener (log "Start client clipboard loop") [O]
   on audio_format: open cpal output (log "Using default output device") [O]
   periodic: TestDelay echo, video_received acks, clipboard
  ▼
[Closed] reason from Misc.close_reason / socket error → "Connection closed: …" [O]
         offer Reconnect (wire_session_reconnect) [O]
```

### 4.2 Host side (`server/connection.rs`) [R]

```
accept stream (punch / relay / direct)
 → whitelist check → send SignedId → recv PublicKey → secure
 → send Hash
 → recv LoginRequest
     • check session type allowed (file transfer / port-forward / terminal / camera enabled?)
     • check login_failure_check (IP ban)
     • verify password against temp/permanent (per verification-method); or forward to CM for click-approve
     • if 2FA enabled and hwid not trusted → "2FA Required" → wait Auth2FA
 → send LoginResponse{peer_info}  (displays, encodings, resolutions, windows sessions, features)
 → notify CM over IPC (Data::Login) → CM window shows this connection
 → subscribe to services according to permissions:
      video(display N), audio, clipboard, cursor/input; file transfer handled inline
 → main loop: dispatch incoming Messages (input → input_service, file_action → fs jobs, misc → …)
 → on close: unsubscribe, maybe lock screen (lock_after_session_end), maybe restore privacy mode, audit
```

### 4.3 `LoginRequest.union` session types [O]

| Arm | Window opened | Host behaviour |
|---|---|---|
| *(none)* | Remote desktop | Normal session |
| `file_transfer{dir, show_hidden}` | File manager | No video/input, only `FileAction` |
| `port_forward{host, port}` | Port forward | After login, the stream carries **raw TCP bytes** to `host:port` on the host's network **[R]** |
| `view_camera{}` | Camera viewer | Video comes from a webcam instead of the screen |
| `terminal{service_id}` | Terminal | Only `TerminalAction` |

`OptionMessage` (sent inside `LoginRequest.option` and later as `Misc.option`) carries the client's per-peer preferences **[O]**: image quality, custom quality and fps, lock_after_session_end, show_remote_cursor, privacy_mode, block_input, disable_audio/clipboard/keyboard/camera, enable_file_transfer, supported_decoding, follow_remote_cursor/window, terminal_persistent, show_my_cursor. `BoolOption` is three-valued (`NotSet`/`No`/`Yes`), so only changes are sent.

## 5. `PeerInfo` (what the host advertises) [O]

```
username, hostname, platform ("Windows"/"Linux"/"Mac OS"/"Android")
displays[]: DisplayInfo{x, y, width, height, name, online, cursor_embedded, original_resolution, scale}
current_display, sas_enabled, version
features{privacy_mode, terminal}
encoding: SupportedEncoding{h264, h265, vp8, av1, i444: CodecAbility}
resolutions: SupportedResolutions
platform_additions: JSON string (e.g. {"is_installed":true,"virtual_displays":[…],"supported_privacy_mode_impl":[…],"idd_impl":"amyuni", "headless":…}) [R]
windows_sessions: WindowsSessions{sessions[{sid,name}], current_sid}
```
Seen on this machine: `PeerInfo { username: "andres", hostname: "andres", platform: "Linux", displays: [DisplayInfo { x: 0, y: 0, width: 1920, … }]}` and `SupportedEncoding { h264: false, h265: false, vp8: true, av1: true, i444: … }` **[O]**.

## 6. Rendezvous protocol summary

See [proto/rendezvous.proto](proto/rendezvous.proto) and [03](03-networking-rendezvous.md). The envelope arms are `RegisterPeer`, `RegisterPeerResponse`, `PunchHoleRequest`, `PunchHole`, `PunchHoleSent`, `PunchHoleResponse`, `FetchLocalAddr`, `LocalAddr`, `ConfigUpdate`, `RegisterPk`, `RegisterPkResponse`, `SoftwareUpdate`, `RequestRelay`, `RelayResponse`, `TestNatRequest`, `TestNatResponse`, `PeerDiscovery`, `OnlineRequest`, `OnlineResponse`, `KeyExchange`, `HealthCheck` (all names **[O]**).

## 7. Compression

`libs/hbb_common/src/compress.rs` **[O]** uses **zstd** (`zstd-safe` **[O]**). It applies to clipboard content (`Clipboard.compress`), file blocks (`FileTransferBlock.compressed`), terminal data (`TerminalData.compressed`), raw RGB/YUV frames (`RGB.compress`, `YUV.compress`) and cursor colors **[R]**. The compression level is low (around 3) for speed **[R]**.

## 8. Compatibility checklist [D]

To interoperate with official RustDesk 1.4.x:
1. Use `message.proto` verbatim and `rendezvous.proto` as reconstructed. Validate field numbers by capturing traffic between two official clients on a self-hosted hbbs **with no key**, so the session falls back to non-secure and the protobuf is visible.
2. Implement framing (§1), the handshake ([04](04-security-crypto.md) §2), the password hash (§3 there), VP9 or AV1 decoding, and the `MouseEvent`/`KeyEvent` semantics ([07](07-input-control.md)).
3. Send `LoginRequest.version` ≥ "1.4.0". Hosts gate features on the peer version **[R]** (`wire_version_to_number` **[O]**).
