# KanoDesk

KanoDesk is a remote-desktop app written in Rust, inspired by RustDesk. It uses its own protocol on QUIC and is **not** wire-compatible with RustDesk.

Status: early scaffold. See [PROGRESS.md](PROGRESS.md) for what works and what's next.

## Spec

[`docs/`](docs/README.md) holds reverse-engineering notes on RustDesk 1.4.9 plus the design blueprint for this project. Start with [docs/16-clone-blueprint.md](docs/16-clone-blueprint.md) (roadmap and crate layout) and [docs/02-architecture.md](docs/02-architecture.md). The notes are a **behaviour spec only**: no RustDesk source code (AGPL-3.0) is used here.

## Layout

| Path | Purpose |
|---|---|
| `crates/proto` | Wire messages and stream framing |
| `crates/crypto` | Device identity, Noise IK handshake, SPAKE2 login |
| `crates/transport` | QUIC transport, NAT traversal, relay client |
| `crates/rendezvous` | ID server client |
| `crates/capture` | Screen capture trait and per-OS backends |
| `crates/codec` | Video encoder/decoder traits and backends |
| `crates/input` | Input event model and injection backends |
| `crates/host` | Controlled-side engine |
| `crates/client` | Controller-side engine |
| `crates/config` | Typed configuration |
| `crates/platform` | OS integration (service, sessions, autostart) |
| `apps/desktop` | Desktop UI (`kanodesk`) |
| `apps/service` | Windows service wrapper (`kanodesk-service`) |
| `crates/ipc` | Local IPC (named pipes, postcard) between the host process and the UI |
| `apps/cli` | Provisioning CLI (`kanodesk-cli`); for now the `p2p-host` / `p2p-connect` network diagnostics |
| `apps/spike` | Phase 0 spike: `spike-host` and `spike-viewer` |
| `server/idserver` | ID/rendezvous server (`kanodesk-idserver`): registration, lookups, online status, connect broker |
| `server/relay` | Relay server (`kanodesk-relay`): blind UDP relay for QUIC with tickets and bandwidth limits |

## Build

Windows 10/11 x64 is the first target. The toolchain is pinned to stable by `rust-toolchain.toml`.

Prerequisites:

- Visual Studio Build Tools with the C++ workload (MSVC).
- [vcpkg](https://github.com/microsoft/vcpkg) for native libraries (currently libvpx). `vcpkg.json` pins the versions; `.cargo/config.toml` points the build at `./vcpkg_installed/`.

Install the native libraries once from the repo root (about 3 minutes the first time):

```bash
vcpkg install --triplet x64-windows-static-md
```

Then build:

```bash
cargo build --workspace
```

```bash
cargo test --workspace
```

Before you send changes, check that these pass:

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Tests that need a real interactive desktop (DXGI capture) are `#[ignore]`d because CI runners have none. Run them locally with:

```bash
cargo test -p kanodesk-capture -- --ignored
```

### ID server

`server/idserver` registers devices (9-digit IDs bound to their ed25519 keys), answers lookups and online queries, lets devices change ID, and brokers connect requests (with a relay assignment) over QUIC on UDP 21116. It keeps a SQLite database and its signing key (DPAPI, machine scope, on Windows; an owner-only file elsewhere) in the data directory, and prints the server key that clients must pin.

```bash
cargo run --release -p kanodesk-idserver -- serve --listen 0.0.0.0:21116 --data-dir ./idserver-data --relay relay.example.com:21117
```

Rate limits are flags of the form `BURST/SECONDS` (a burst, then one request every SECONDS): `--per-ip`, `--per-device`, `--new-ids-per-ip`, `--per-target`, `--connects-per-target`. `--trusted-network CIDR` (repeatable) exempts a network, such as a large office NAT, from the per-IP limits.

Operator commands work whether the server is running or not:

```bash
cargo run --release -p kanodesk-idserver -- list --data-dir ./idserver-data
```

`show ID` prints one device; `remove ID` deletes it, frees the ID and disconnects it if online, which is how a device that lost its key gets its ID back (it registers again with a new key and asks for that ID); `set-relays HOST:PORT...` replaces the relay list and pushes it to connected devices; `relays` prints the saved list (server stopped). With the server stopped they edit the database directly; while it runs they use its admin API (`--server ADDR`, default `127.0.0.1:21116`), authenticated with the server key from the data directory, so only the server machine's administrators can use them.

Clients use `kanodesk_rendezvous::RendezvousClient` with the server address and that key. The end-to-end tests in `server/idserver/tests/rendezvous.rs` show the full flow.

### Relay server

`server/relay` forwards whole QUIC packets between two devices when no direct path works. It never sees keys or content (TLS and Noise stay end to end), only serves sessions the ID server issued (tickets signed with a secret both share), and limits each session's bandwidth.

```bash
cargo run --release -p kanodesk-relay -- new-secret relay.secret
```

```bash
cargo run --release -p kanodesk-relay -- --listen 0.0.0.0:21117 --secret-file relay.secret --port-range 30000-30099
```

Give the ID server the same file and the relay's public address: `--relay-secret-file relay.secret --relay PUBLIC_IP:21117`. The ID server also answers NAT probes on its port minus one (21115 by default).

### Testing connections between two networks

You need one machine reachable from both networks for the servers (a small VPS, or a router with port forwarding), and two machines on different networks. Neither server needs libvpx, so they build on Linux too.

1. **Server machine.** Open UDP 21115, 21116, 21117 and 30000–30099 in the firewall (and forward them if behind a router). Then:

   ```bash
   cargo build --release -p kanodesk-idserver -p kanodesk-relay
   ```

   ```bash
   ./target/release/kanodesk-relay new-secret relay.secret
   ```

   ```bash
   ./target/release/kanodesk-relay --listen 0.0.0.0:21117 --secret-file relay.secret --port-range 30000-30099
   ```

   In a second terminal (replace `PUBLIC_IP` with the address the two machines will use to reach this server):

   ```bash
   ./target/release/kanodesk-idserver --listen 0.0.0.0:21116 --data-dir ./ids --relay PUBLIC_IP:21117 --relay-secret-file relay.secret
   ```

   Note the `server key` it prints.

2. **Machine A (host).** Build `cargo build --release -p kanodesk-cli` and run (allow the firewall prompt):

   ```bash
   ./target/release/kanodesk-cli p2p-host --server PUBLIC_IP:21116 --server-key SERVER_KEY
   ```

   It prints its device ID, NAT type and a **temporary password**, tries a UPnP port mapping on the router (`--no-upnp` to skip; removed again on Ctrl+C), and waits. Every connection must log in with that password before anything is echoed. Each temporary password works for one login only (and changes after repeated wrong guesses); the host prints the new one. Failed logins are rate-limited per source IP and remembered across restarts; `--reset-login-limits` forgets them.

3. **Machine B (controller), on another network:**

   ```bash
   ./target/release/kanodesk-cli p2p-connect HOST_ID --server PUBLIC_IP:21116 --server-key SERVER_KEY
   ```

   It asks for the host's password (hidden input; or pass `--password PASSWORD`). After connecting it checks that it reached the device whose key the ID server signed, then logs in. It prints both NAT types, which path won (`Lan`, `Ipv6`, `Direct` or `Relay`), the connect time, round-trip times and throughput. Run it again with `--relay-only` to test the relay, and with `--no-relay` to see whether a direct punch works on its own. Add `RUST_LOG=debug` for more detail.

### Fuzzing the protocol decoder

`crates/proto/fuzz` is a cargo-fuzz project (its own workspace, nightly only). The same check also runs on stable as a proptest. One-time setup:

```bash
rustup toolchain install nightly --profile minimal
```

```bash
cargo install cargo-fuzz --locked
```

On Windows, AddressSanitizer needs the MSVC runtime DLL on `PATH`: add `VC\Tools\MSVC\<version>\bin\Hostx64\x64` from your Visual Studio Build Tools install. Then, from `crates/proto`:

```bash
cargo +nightly fuzz run decode_frames -- -max_total_time=60
```

### The desktop app

`kanodesk` with no arguments opens the main window. It needs an ID server (and, for connections that cannot go direct, a relay); see above. On one machine for a quick try:

```bash
cargo run --release -p kanodesk-relay -- --listen 127.0.0.1:21117 --open
```

```bash
cargo run --release -p kanodesk-idserver -- --listen 127.0.0.1:21116 --nat-test-listen 127.0.0.1:21115 --relay 127.0.0.1:21117
```

```bash
cargo run --release -p kanodesk-desktop
```

1. **Settings** → ID server `127.0.0.1:21116` (port 21116 if omitted) and the server key the ID server printed → **Save**. The host re-reads the settings at once; nothing restarts.
2. **Home** shows **your ID** ("Ready: reachable by ID") and the **one-time password** (Copy, **New password**). The window starts the host process (`kanodesk --server`, no console, log in `%LOCALAPPDATA%\KanoDesk\data\logs\host.log`) when none runs, and stops it when you close the window.
3. On the controlling machine (the same machine works too, for a try), type the other device's ID and **Connect** (or press Enter). A viewer window opens (`kanodesk --connect ID`, its own process; log in `logsiewer.log`): *Connecting* with the current step, then the *password* dialog if the host wants one (again after a wrong one), or *Waiting for the remote user to accept* in click mode. A relayed connection is pointed out once.
4. The viewer's toolbar: **display** picker (when the host has several), **Fit to window / Original size** (original size scrolls with the pointer), **quality** (Balanced, Best, Low bandwidth; shared by everyone viewing that display), **Ctrl+Alt+Del** (sent; performed once the service of prompt 11 exists), the path (LAN, direct, relayed) and fps, and **Disconnect**. Errors (unknown ID, device offline, wrong ID server key, refused, no answer, ended by the remote side, …) come as dialogs with **Try again** where it makes sense.
5. Successful sessions appear under **Recent sessions** in the main window (Connect, ✕ to forget); they are kept in `peers.toml` next to the config, without passwords.

Settings also cover the approve mode, the one-time password length, a permanent password, the default keyboard/mouse permissions for remote users, and the viewer's defaults (`view-style`, `image-quality`). The viewer registers with the ID server under its own device key (`%LOCALAPPDATA%\KanoDesk\data\client`), so it never takes over the host's ID.

### Host and connection manager

`kanodesk --server` is the host process: it reads the config, keeps its device key in the key store, and accepts direct connections (UDP 21118 by default). When someone connects, it opens the connection-manager window (`kanodesk --cm`): who is connecting, **Accept**/**Deny**, keyboard and mouse toggles that apply at once, and **Disconnect**. Approval modes (`approve-mode`): `password` (the viewer needs the password; the window just shows the session), `click` (no password; you accept), `both`.

```bash
cargo run --release -p kanodesk-desktop -- --server --approve-mode click --listen 127.0.0.1:9200
```

It prints its device key (and the temporary password in `password`/`both` mode). Connect with the spike viewer; `--ask-host` asks for a click instead of sending a password:

```bash
cargo run --release -p kanodesk-spike --bin spike-viewer -- --connect 127.0.0.1:9200 --host-key <device key> --ask-host
```

The window comes to the front; Accept is disabled for the first second (so a remote viewer cannot click it). Denying turns the viewer away before any video; Disconnect ends the session and its video at once. The window closes itself a few seconds after the last session ends. The same actions from a terminal:

```bash
cargo run --release -p kanodesk-desktop -- --cm-cmd list
```

(`accept ID`, `deny ID`, `disconnect ID`, `perms ID keyboard=off mouse=on`). Settings live in `%APPDATA%\KanoDesk\config\kanodesk.toml` (option table in `crates/config/src/options.rs`: `approve-mode`, `enable-keyboard`, `enable-mouse`, `direct-access-listen`, `custom-fps`, `video-bitrate-kbps`, `hide_cm`, …); command-line values override it for one run. Secrets stay in the key store under `%LOCALAPPDATA%\KanoDesk\data\host`.

### Phase 0 spike: stream your screen

Two binaries in `apps/spike`. Start the host; it prints its device key, a temporary password and the exact viewer command:

```bash
cargo run --release -p kanodesk-spike --bin spike-host -- --listen 127.0.0.1:9000 --fps 30
```

Then, in a second terminal, run the viewer with the device key the host printed, and type the password when asked (input is hidden; or pass `--password`):

```bash
cargo run --release -p kanodesk-spike --bin spike-viewer -- --connect 127.0.0.1:9000 --host-key <device key>
```

The viewer logs in before any video flows: a Noise handshake bound to the TLS connection proves it reached the device with that key, then SPAKE2 proves it knows the password without sending it. A wrong password gets no video; repeated failures are rate-limited (remembered across host restarts; `spike-host --reset-login-limits` forgets them) and change the temporary password. Each temporary password works for one login; the host prints the next one. `spike-host --set-permanent PASSWORD` saves a permanent password (at least 8 characters, DPAPI-protected) that also works; `--clear-permanent` removes it. Both binaries keep their device keys DPAPI-protected under `%LOCALAPPDATA%\KanoDesk\` (`--data-dir` to change). `--pin <fingerprint>` additionally pins the host's TLS certificate.

The viewer window shows the host's primary display with a frame counter and latency overlay, and prints per-second stats. Add `--seconds N` to stop after N seconds with a summary, or `--vsync` for Fifo presentation. The latency figure is only meaningful when both run on the same machine. The spike is still a test tool (one viewer at a time, no access control beyond the password); keep it on localhost or a trusted LAN.

#### Controlling the host

Once logged in, the viewer controls the host's mouse and keyboard while its window has focus: clicks, drags, the wheel and keys (by physical position, so the host's keyboard layout decides the characters). Everything held is released when the viewer loses focus or closes, and the host also releases a viewer's keys if it goes quiet for 3 s or disconnects. On the host, `--view-only` starts with input refused; pressing Enter in the host's console toggles between view only and full control (the viewer's overlay shows `VIEW ONLY`).

The viewer's own shortcuts (not sent to the host): **Ctrl+Alt+End** sends Ctrl+Alt+Del (the host performs it once the Windows service exists; until then it reports that it cannot); **Ctrl+Alt+Shift+M** toggles relative mouse (the pointer is locked to the window and raw motion is sent, for games); **Ctrl+Alt+Shift+K** toggles Translate mode, which types characters as your own keyboard layout produces them (useful when the two machines use different layouts; system keys then stay local). In the default Map mode the viewer grabs the keyboard while it has focus, so Win, Alt+Tab, Alt+F4 and Ctrl+Esc go to the host; Ctrl+Alt+Del and Win+L always stay local (Windows handles them below any program). Caps/Num/Scroll Lock act on your machine and the host follows their states. Media keys (volume, play/pause, browser keys) are forwarded.

The host's pointer is shown: while you control, your pointer over the window takes the host's shape; in view-only and relative mode the host's pointer is drawn into the picture. (Windows reports the pointer when it moves, so it appears after the first movement on the host.)

To try it on one machine with two displays, stream the display the viewer is *not* on, for example `spike-host --display 1` and the viewer window on display 0; otherwise the pointer chases itself.

### Capture benchmark

Captures the primary display, converts to I420 and encodes VP9, then prints fps, per-stage times and bytes per frame. Put something moving on screen first. The flags are listed at the top of `crates/codec/examples/capture_bench.rs`.

```bash
cargo run --release -p kanodesk-codec --example capture_bench -- --seconds 10 --fps 30
```

## Licence

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT licence ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this work, as defined in the Apache-2.0 licence, is dual-licensed as above, without any additional terms or conditions.
