# 04: Security and Cryptography

## 1. Identity keys

| Key | Where | Format | Purpose |
|---|---|---|---|
| Host identity key | `RustDesk.toml: key_pair = [[64 bytes],[32 bytes]]` **[O]** | libsodium `crypto_sign` ed25519: a 64-byte secret key (seed‖pk) and a 32-byte public key. The last 32 bytes of the secret equal the public key, which the local file confirms **[O]**. | Signs the per-connection ephemeral box key, and is registered with hbbs |
| Server key | option `key` (base64 32 bytes); default public server key compiled in **[O]** | ed25519 public key of hbbs | Verifies the `IdPk` signed by hbbs, and the KeyExchange with hbbs |
| Ephemeral box keys | memory only | Curve25519 (`crypto_box`) | Encrypt the session key in transit |
| Session key | memory only | 32-byte `crypto_secretbox` key | Encrypts every frame (XSalsa20-Poly1305) |

The key pair is generated on first run and kept for the lifetime of the install. The `key_confirmed` flag is set once hbbs has accepted the key **[O]**.

## 2. Secure handshake (peer ↔ peer) [R], messages and errors [O]

This runs right after the TCP/KCP/relay stream is established (log: `TCP punch secure_connection ok` **[O]**).

```
Host (controlled)                                        Client (controller)
-----------------                                        -------------------
generate ephemeral box keypair (b_pk, b_sk)
signed = crypto_sign( IdPk{ id: my_id, pk: b_pk }.encode(), host_ed25519_sk )
──── Message{ signed_id: SignedId{ id: signed } } ─────►
                                                         know host_ed25519_pk from hbbs-signed PunchHoleResponse.pk
                                                         (or from the config cache / TOFU)
                                                         idpk = crypto_sign_open(signed, host_ed25519_pk)
                                                            fail → "Signature mismatch" / "pk mismatch, fall back to non-secure"
                                                         check idpk.id == requested id
                                                         generate ephemeral (a_pk, a_sk), random key K (32 bytes)
                                                         sealed = crypto_box(K, nonce=0^24, b_pk, a_sk)
◄─── Message{ public_key: PublicKey{ asymmetric_value: a_pk, symmetric_value: sealed } } ──
K = crypto_box_open(sealed, nonce=0^24, a_pk, b_sk)
   fail → "Handshake failed: invalid key exchange message"
both sides: stream.set_key(K)  →  "Connection secured"
```

Observed error strings that pin down this logic **[O]**: `Handshake failed: invalid public sign key length from peer`, `Handshake failed: invalid message type`, `Handshake failed: invalid message format`, `Wrong their public length in key exchange`, `Signature mismatch in key exchange`, `Connection secured`, `pk mismatch, fall back to non-secure`, `insecure-connection` and `conn-e2ee-unavailable-tip` (the UI warning shown when the connection is **not** E2E).

**Insecure fallback**: when the client has no trusted host key (no server key configured, or a direct IP connection), it may still proceed with `PublicKey{}` empty, so there is no encryption. The UI shows an "Insecure Connection" warning **[O]**.

### 2.1 Per-frame encryption [R]

- After `set_key`, every framed payload is `crypto_secretbox_easy(plaintext, nonce, K)`, which adds a 16-byte Poly1305 tag.
- Nonce: 24 bytes, the first 8 bytes being a little-endian **u64 sequence number**, the rest zero. Each direction keeps its own counter (`send_seqnum`, `recv_seqnum`), starting at 1 and incremented per frame. Because frames arrive in order over a reliable stream, no nonce travels on the wire.
- A decryption failure closes the connection.

**[D]** In Rust use `crypto_box` / `xsalsa20poly1305` (RustCrypto) and `ed25519-dalek`. They are byte-compatible with libsodium: `crypto_box` = X25519 + HSalsa20 + XSalsa20Poly1305. A libsodium `crypto_sign` signed message is `sig(64) ‖ msg`, and `crypto_sign_open` checks and strips the signature.

## 3. Password authentication [R], messages [O]

Once the connection is secured, the host sends a challenge:

```
Host ── Message{ hash: Hash{ salt, challenge } } ──► Client
```
- `salt` is the host's stored per-install salt (`RustDesk.toml: salt` **[O]**). `challenge` is random per connection.
- The client computes:
  ```
  h1 = SHA256( password_utf8 ‖ salt )          // what "remember password" stores
  h2 = SHA256( h1 ‖ challenge )
  ```
- The client sends `LoginRequest{ username: <target id>, password: h2, my_id, my_name, option, union{…}, video_ack_required, session_id, version, os_login, my_platform, hwid, avatar }` **[O]** fields.
- The host verifies by computing the same `h2` from each currently valid password (temporary and/or permanent, per `verification-method`) and comparing.
- **Remember password**: the client stores `h1`, not the plaintext, in `peers/<id>.toml: password`. It is encrypted at rest (see §6). Later logins reuse `h1` directly, which is why `main_peer_has_password` exists **[O]**.
- **Address-book password sync**: `sync_peer_hash_password_to_personal_ab` **[O]** uploads `h1` to the Pro address book, so a user's other devices can connect without retyping.

`LoginResponse{ union { error: string | peer_info: PeerInfo }, enable_trusted_devices }` **[O]**. Error strings drive client behaviour **[R]**: `"Wrong Password"` → re-prompt; `"Empty Password"` → prompt (`Empty Password` **[O]**); `"No Password Access"`; `"Offline"`; `"2FA Required"` → prompt for TOTP; `"Wrong 2FA Code"`.

### 3.1 Click-approve flow
If `approve-mode` = `click` (or both, and no password was given), the host forwards the login to the CM via IPC (`Data::Login`). The CM shows **Accept / Dismiss**, and the server waits for `Data::Authorize` or `Data::Close`. The client sees "Please wait for the remote side to accept" **[R]** (strings present in the translations **[O]**).

`approve-mode = 'password'` is set on this machine **[O]**.

## 4. Password types

| Type | Storage | Notes |
|---|---|---|
| **Temporary (one-time)** | Memory of the `--server` process | Regenerated on demand: the "Refresh random password" button calls `wire_main_update_temporary_password` **[O]**. It is also **rotated automatically after too many consecutive wrong password attempts** (log string "Temporary password rotated after too many consecutive wrong attempts: failures=, ip=" **[O]**; see §7). Automatic regeneration after a session ends (see the teardown sequence in [11](11-host-server-side.md) §3) is still **[R]**: no string confirms it, so that policy needs a decompiler. Length from `temporary-password-length` (6/8/10). Charset: alphanumeric, or digits only with `allow-numeric-one-time-password` **[O]**. |
| **Permanent** | `RustDesk.toml: password` (+ `salt`) **[O]** | Set via `wire_main_set_permanent_password_with_result` **[O]**. Since 1.4.x, `libs/hbb_common/src/config/permanent_password.rs` **[O]** stores an **encrypted hash** with version prefix `01` **[O]** (most likely of `h1`, §3 **[R]**), tied to `salt`. The `--server`/service process owns it and syncs it to UI processes over IPC. Details in [12](12-config-storage.md) §2.1.1. `local-permanent-password-set` **[O]** indicates whether one exists without exposing it. |
| **Preset (custom builds)** | Compiled in | `wire_is_preset_password` **[O]**, `preset-password-warning` **[O]**. |
| **Default connect password (client side)** | option `default-connect-password` **[O]** | Tried automatically for every outgoing connection. |

## 5. Two-factor authentication (`src/auth_2fa.rs`) [O] names, [R] flow

- Setup: `wire_main_generate2fa()` **[O]** creates a TOTP secret (`totp-rs`, base32) and returns an `otpauth://` URI for a QR code (`QrImageView` **[O]** in the UI). `wire_main_verify2fa(code)` **[O]** confirms the setup and stores the secret encrypted in options (`2fa`).
- Login: after a correct password, the host replies `LoginResponse.error = "2FA Required"`. The client UI asks for a code and sends `Message{auth_2fa: Auth2FA{code, hwid}}` **[O]**.
- **Telegram bot**: `wire_main_verify_bot(token)`, `wire_main_has_valid_bot_sync` **[O]**. The host sends the current code or login notices to a Telegram chat through the bot API, so the remote user can read it.
- **Trusted devices**: `LoginResponse.enable_trusted_devices` **[O]**. If the client ticks "trust this device", the host stores the client `hwid` (`LoginRequest.hwid` **[O]**) with an expiry in `RustDesk2.toml: trusted_devices` **[O]**, and 2FA is skipped next time. Managed through `wire_main_get_trusted_devices`, `wire_main_remove_trusted_devices`, `wire_main_clear_trusted_devices` **[O]**.

## 6. Secret storage at rest [R], format [O]

- `libs/hbb_common/src/password_security.rs` **[O]** provides `encrypt_str_or_original` / `decrypt_str_or_original` and `encrypt_vec_or_original` **[R]**.
- Format: the version prefix `"00"` followed by base64 of `secretbox(plaintext, nonce, key)`. **Observed**: a stored peer password decodes to the ASCII string `00AY5…g==` **[O]** (shown here truncated). The `enc_id` in `RustDesk.toml` uses the same scheme.
- The key is derived from the **machine UID** (`machine_uid` / `MachineGuid` **[O]**), so files copied to another machine cannot be decrypted. Treat this as obfuscation, not real protection, because any local process can derive the same key.
- `max_encrypt_len` (`wire_main_max_encrypt_len` **[O]**) caps the plaintext length.
- The address book cache `RustDesk_ab` is a binary blob (encrypted/compressed) **[O]**.

**[D]** Use the OS keystore (Windows DPAPI `CryptProtectData` with machine or user scope; `keyring` crate) for secrets, and keep the TOML for non-secrets.

## 7. Brute-force protection (`src/server/login_failure_check.rs`) [O] module, [R] logic

- Failures are tracked per **source IP** (and per ID for relayed connections).
- Rough policy **[R]**: after about 6 failures in 1 minute the client gets "Too many wrong password attempts, please try again later"; after about 30 cumulative failures the IP is blocked until restart.
- **Temporary password rotation** **[O]** string, **[R]** threshold: after too many *consecutive* wrong attempts the host replaces the temporary password and logs "Temporary password rotated after too many consecutive wrong attempts: failures=, ip=". This stops an attacker from continuing to guess against the same short one-time password; the permanent password is not rotated. The string sits in the connection/login code, next to "Permanent password accepted via logon-screen fallback" and "2FA bypassed by trusted devices". Unknown: the threshold, and whether "consecutive" is counted per IP or across all peers (the log line carries both a count and an IP). Add this to the decompile targets `connection.rs` / `login_failure_check.rs` (review §5).
- **[D]** for a clone: rotate the one-time password after N consecutive failures, independent of the per-IP ban, and notify the local user (tray/CM) that it changed.
- Also present: the `whitelist` option **[O]**, a comma/newline-separated list of IPs or CIDRs (`cidr-utils`, `ipnet` crates **[O]**). Non-matching peers are refused before the password step.

## 8. Other protections

| Feature | Evidence | Behaviour |
|---|---|---|
| Message box link sanitising | log: `Message box ignore link  for security` **[O]** | Links in peer-sent `MessageBox` are not shown unless they come from a trusted source |
| `allow-only-conn-window-open` **[O]** | option | Accept connections only while the main window is open |
| `allow-remote-config-modification` **[O]** | option | Whether a remote controller may change this host's settings |
| `allow-remote-cm-modification` **[O]** | option | Whether permissions can be toggled from the controller side |
| `disable-change-permanent-password`, `disable-change-id`, `disable-unlock-pin` **[O]** | options | Lock settings in managed deployments |
| `hide-security-settings`, `hide-server-settings`… **[O]** | options | Hide UI panes |
| Unlock PIN | `unlock_pin` **[O]** | Required to open settings |
| Scope-violation alarm | `allow-scope-violation-alarm` **[O]** | Sends an audit alarm when a connection exceeds its allowed scope |
| TLS for API | `rustls` + native roots, `allow-insecure-tls-fallback` **[O]** | Verifies HTTPS for the API, with an optional insecure fallback |

## 9. Threat-model notes for the clone [D]

1. **Pin the server key.** Without a configured `key`, the first key you see is trusted on first use (TOFU). Ship the key in the build or pass it at deployment.
2. The password scheme `SHA256(SHA256(pw‖salt)‖challenge)` stops passive replay. However, `h1` is a password-equivalent. Prefer a **PAKE** (SPAKE2 or OPAQUE) in a non-compatible clone, or at least use Argon2 for `h1`.
3. Bind authentication to the channel: include the handshake transcript hash in the login proof, so a relay/MITM cannot forward a login.
4. Use separate keys per direction (derived with HKDF from K) instead of one key with direction-specific counters.
5. Rate-limit at both hbbs and the host.
