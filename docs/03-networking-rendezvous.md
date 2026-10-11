# 03: Networking, Rendezvous, NAT Traversal, Relay

## 1. Server components

| Component | Name in RustDesk | Default ports | Role |
|---|---|---|---|
| ID/Rendezvous server | **hbbs** | **21115/TCP** (NAT test), **21116/TCP+UDP** (register, punch broker), **21118/TCP** (WebSocket for hbbs) | Maps ID → current address/public key, brokers hole punching, tells peers which relay to use |
| Relay server | **hbbr** | **21117/TCP**, **21119/TCP** (WebSocket for hbbr) | Blindly pipes two TCP streams paired by UUID |
| API server (Pro) | hbbs-pro web | **21114/TCP** (HTTP), or HTTPS on 443 | Accounts, address book, audit ([13-account-api.md](13-account-api.md)) |

Evidence: the strings `UDP 21116`, `TCP 21116`, `:21117?key=`, `:21114`, `allow-https-21114` **[O]**. The defaults `rs-ny.rustdesk.com` and the public key `OeVuKk5nlHiXp+APNn0Y3pC1Iwpwn44JGqrQCsWqmBw=` are compiled in **[O]**. The log shows `rendezvous server: rs-ny.rustdesk.com:21116` and relay `ovh-east2.rustdesk.com` **[O]**. Ports 21115/21118/21119 are computed as offsets from 21116 (`port-1`, `port+2`, `port+3`) rather than stored as strings **[R]**.

> **Port reuse note.** Two port numbers have two unrelated meanings each, depending on which side and which protocol is involved. Both pairs are correct; they are just easy to mix up:
>
> | Port | Meaning 1 | Meaning 2 |
> |---|---|---|
> | **21118** | hbbs WebSocket listener (TCP, on the **server**) | Default `direct-access-port`: the **host** device's listener for direct IP connections (TCP, §8) |
> | **21119** | hbbr WebSocket listener (TCP, on the **server**) | LAN discovery broadcast (**UDP**, on every client/host, §9) |
>
> In normal deployments they never collide, because the server roles and the device roles run on different machines. 21119 can't collide even on one machine (TCP vs UDP). The one real conflict is **21118/TCP** when a single machine runs both hbbs (with WebSocket) and a RustDesk host with `direct-server = Y`. In that case, change `direct-access-port`. **[D]** for a clone: give direct access and LAN discovery their own port constants instead of deriving them from the rendezvous port.

Server address parsing **[R]**: `host[:port]`. Without a port the client uses 21116. The relay defaults to the same host on 21117. The API defaults to `http(s)://host:21114` when only a rendezvous host is configured. Several servers can be listed, and the client picks the fastest **[R]** (`rendezvous_servers` field in `ConfigUpdate` **[O]**).

Config import string format, seen in the translated help text **[O]**: `9123456234@192.168.16.1:21117?key=<base64 server pk>`. In other words `id@host:port?key=`, which the `--connect` URL also uses.

## 2. Transports

All logical channels share one framed byte stream: see [05-wire-protocol.md](05-wire-protocol.md) §1. The stream runs over one of:

1. **TCP** (`libs/hbb_common/src/tcp.rs` **[O]**), the default.
2. **UDP + KCP** (`src/kcp_stream.rs`, `kcp-sys` **[O]**), used after a successful UDP punch when `enable-udp-punch` is on **[O]**. KCP turns UDP into a reliable ordered stream, so the framing layer is the same as on TCP.
3. **WebSocket** (`libs/hbb_common/src/websocket.rs`, `tokio-tungstenite` **[O]**), used when `allow-websocket` = Y **[O]**. Each framed message becomes a binary WS message. This makes TLS-terminating reverse proxies on 443 possible.
4. **Through a proxy** (`libs/hbb_common/src/proxy.rs` **[O]**): SOCKS5 (`tokio-socks`) with optional auth, or an HTTP CONNECT proxy (`proxy-url`, `proxy-username`, `proxy-password` **[O]**). Proxies are TCP-only, so UDP punch is disabled while one is set.

`libs/hbb_common/src/socket_client.rs` **[O]** is the factory. It picks the transport from the config and resolves addresses (IPv4/IPv6, `test_if_valid_server` **[O]**).

## 3. Registration (controlled side, `rendezvous_mediator.rs`) [R], names [O]

```
loop every ~12 s (REG_INTERVAL) over UDP 21116, or TCP when disable-udp / websocket:
   host ──RegisterPeer{id, serial}──────────────► hbbs
   hbbs ──RegisterPeerResponse{request_pk}──────► host
   if request_pk:                         (first contact, or server lost our key)
      host ──RegisterPk{id, uuid, pk, old_id}───► hbbs
      hbbs ──RegisterPkResponse{result, keep_alive}► host
          result ∈ OK | UUID_MISMATCH | ID_EXISTS | TOO_FREQUENT | INVALID_ID_FORMAT | NOT_SUPPORT | SERVER_ERROR
   hbbs may also push: ConfigUpdate{serial, rendezvous_servers}, SoftwareUpdate{url}
```

- `id` is the device ID. `uuid` is a stable machine identifier (`wire_main_get_uuid` **[O]**). `pk` is the host's **ed25519 public key** (`key_pair` in `RustDesk.toml` **[O]**: a 64-byte secret plus a 32-byte public key).
- hbbs binds `id → (uuid, pk, last socket addr)`. A UUID mismatch for an existing ID means someone else owns that ID, so the server rejects it and prevents ID hijacking.
- `serial` versions the server-pushed config. When hbbs has a newer `serial` it sends `ConfigUpdate` **[O]** with the new server list.
- **Change ID** (`wire_main_change_id` **[O]**) sends `RegisterPk` with `old_id` set to the current ID and `id` set to the new one.
- **Key confirmation**: once hbbs accepts the key, `key_confirmed = true` and `[keys_confirmed] rs-ny = true` are saved **[O]**, one entry per server host.
- **Secure TCP to hbbs** (1.2+): on TCP connections to hbbs, the client first does `KeyExchange{keys}` **[O]**. hbbs sends its Curve25519 key signed with its ed25519 key. The client verifies it against the configured `key` ("Handshake failed: invalid public key from rendezvous server" **[O]**), and the rest of that rendezvous TCP connection is encrypted the same way as peer connections ([04](04-security-crypto.md)). `HealthCheck{token}` **[O]** keeps the connection alive.

The registration also learns the host's public address and reports it to the UI ("Ready" vs "Connecting to the RustDesk network…" **[O]**). `wire_main_get_connect_status` **[O]** polls the state: `-1` not connected, `0` connecting, `1` ready **[R]**.

## 4. NAT type detection [O] names, [R] logic

`TestNatRequest{serial}` is sent over TCP to **21115** and again to **21116** from the **same local port** (with `SO_REUSEADDR`/`SO_REUSEPORT`). Each `TestNatResponse{port, cu: ConfigUpdate}` returns the source port the server saw. If both ports are equal the NAT is `ASYMMETRIC` (cone-like, punchable); if they differ it is `SYMMETRIC`.

```
enum NatType { UNKNOWN_NAT = 0; ASYMMETRIC = 1; SYMMETRIC = 2; }
```
On this machine `nat_type = 1` is stored in `RustDesk2.toml` **[O]**, and the log prints `nat_type: ASYMMETRIC` **[O]**. That confirms value 1 = ASYMMETRIC.

Newer builds also query public STUN servers (`stun.l.google.com`, `stun.cloudflare.com`, `stun.nextcloud.com` **[O]**, using the `stunclient` and `stun_codec` crates) to learn the public UDP mapping for UDP punch and IPv6.

## 5. Connection establishment (client side, `client.rs`)

### 5.1 Observed timeline (from this machine's log) [O]

```
TCP connection establishment time used: 160ms          ← TCP to hbbs:21116
rendezvous server: rs-ny.rustdesk.com:21116
#1 TCP punch attempt with <peer-ip>:50074, id: 74318467
TCP Hole Punched 74318467 = <peer-ip>:33941
744 ms used to TCP punch hole, relay_server: ovh-east2.rustdesk.com, nat_type: ASYMMETRIC
peer address: <peer-ip>:33941, timeout: 18000
39ms used to establish TCP connection with TCP punch
TCP punch secure_connection ok                          ← E2E handshake done
handle_peer_info …                                      ← login succeeded
```
And for an offline peer: `Connection closed: Failed to connect: the target device is offline or does not exist.` **[O]**

### 5.2 Message flow [R], names [O]

```
Client (controller)                 hbbs                         Host (controlled)
  │ TCP connect :21116                │                                │
  │──PunchHoleRequest────────────────►│                                │
  │  {id, nat_type, licence_key,      │                                │
  │   conn_type, token, version,      │                                │
  │   udp_port, force_relay,          │                                │
  │   socket_addr_v6, upnp_port}      │                                │
  │                                   │ lookup id → host addr (UDP)    │
  │                                   │──PunchHole{socket_addr=client, │
  │                                   │   relay_server, nat_type,      │
  │                                   │   udp_port, force_relay,       │
  │                                   │   socket_addr_v6,              │
  │                                   │   control_permissions}────────►│
  │                                   │                                │ host opens TCP from its
  │                                   │                                │ registered local port toward
  │                                   │                                │ client addr (creates NAT
  │                                   │                                │ mapping) and listens
  │                                   │◄──PunchHoleSent{socket_addr,   │
  │                                   │   id, relay_server, nat_type,  │
  │                                   │   version, upnp_port, v6}──────│
  │◄──PunchHoleResponse───────────────│                                │
  │  {socket_addr=host, pk=signed     │                                │
  │   IdPk, relay_server, nat_type /  │                                │
  │   is_local, failure, other_failure│                                │
  │   , feedback, upnp_port, v6}      │                                │
  │                                                                    │
  │ simultaneous TCP connect from the same local port to host addr ───►│ (TCP simultaneous open)
  │ on failure/timeout → relay path (§6)                               │
```

Details:
- **`failure`** enum: `ID_NOT_EXIST`, `OFFLINE`, `LICENSE_MISMATCH`, `LICENSE_OVERUSE` **[R]**. These map to the UI messages "the target device is offline or does not exist" **[O]**.
- **`licence_key`** is the server public key (`key` option). hbbs rejects clients whose key does not match its own (`LICENSE_MISMATCH`). This is what stops strangers using a private server.
- **`conn_type`** enum: `DEFAULT_CONN`, `FILE_TRANSFER`, `PORT_FORWARD`, `RDP`, `VIEW_CAMERA`, `TERMINAL` **[R]** (the session kinds are confirmed by `LoginRequest` union arms **[O]**).
- **`control_permissions`** (`ControlPermissions{permissions: u64 bitmask}`, names **[O]**) is added by a Pro hbbs from the strategy that applies to this client and connection. The client's `PunchHoleRequest` doesn't carry it; the client only sends its `token` **[R]**. The host uses it to **narrow** the permissions it grants for the session (keyboard, clipboard, file, audio, camera, terminal, tunnel, restart, recording, block_input, remote_printer, remote_modify). OSS hbbs leaves it unset, and then nothing is restricted. See [11](11-host-server-side.md) §3. Field numbers and bit positions are **[R]**.
- **`pk`** in the response is `sign(IdPk{id, pk=host_ed25519_pk}, hbbs_sk)`. The client verifies it with the configured server key and so learns the host's identity key **through a trusted third party**. If `key` is empty (the public server is used without a key), the client falls back and trusts the key the peer presents itself ("pk mismatch, fall back to non-secure" **[O]**). See [04](04-security-crypto.md).
- **Address mangling** **[R]**: `socket_addr` bytes are not raw. `AddrMangle::encode` XORs the IPv4 address and port with a timestamp-derived value and packs them into 16-18 bytes, so middleboxes cannot rewrite them (anti-ALG).
- **Same LAN**: if hbbs sees both peers behind the same public IP, it sends `FetchLocalAddr{socket_addr, relay_server}` to the host. The host replies `LocalAddr{socket_addr, local_addr, relay_server, id, version}`, and the client gets `PunchHoleResponse{is_local=true}` with the LAN address **[O]** (names `FetchLocalAddr`, `LocalAddr`, `local_addr` present).
- **Timeouts**: the punch connect timeout is about 18 s (`timeout: 18000` **[O]**). There are up to N attempts ("#1 TCP punch attempt") **[O]**. `direct_failures` in the peer config **[O]** counts how often direct connection failed. Once it passes a threshold the client goes **straight to relay** next time **[R]**.
- **UDP punch / KCP**: when both sides advertise `udp_port`, they punch UDP in parallel with TCP and use whichever wins. The UDP path is wrapped in KCP **[R]**.
- **IPv6**: if both have global IPv6 (`socket_addr_v6`), they try a direct v6 connection first, since no NAT is involved (`enable-ipv6-punch` **[O]**).
- **UPnP**: `upnp_port` lets a host advertise a mapped port **[R]**.

## 6. Relay [R], names [O]

Used when punching fails, either NAT is SYMMETRIC, `force_relay` is set, or a proxy/WebSocket is in use.

```
Client ──RequestRelay{id, uuid(random), socket_addr, relay_server, secure, licence_key,
                       conn_type, token}──► hbbs ──(UDP/TCP) RequestRelay{…, control_permissions}──► Host
Host  ── TCP connect relay_server:21117 ──► hbbr : RequestRelay{uuid, licence_key}
Host  ──RelayResponse{socket_addr, uuid, relay_server, id | pk, refuse_reason, version, feedback}──► hbbs ──► Client
Client── TCP connect relay_server:21117 ──► hbbr : RequestRelay{uuid, licence_key}
hbbr pairs the two sockets with equal uuid and copies bytes both ways (it can rate-limit, but cannot read E2E data).
```

- Log line seen: `relay requested from peer, time used: …, relay_server: …` **[O]**, and `request relay attempt, id: , uuid: , secure: ` **[O]**.
- The relayed stream is then identical to a direct one, and the E2E handshake runs on top.
- hbbr supports bandwidth limits and blacklists in the OSS server **[R]**.

## 7. Online status query

`wire_query_onlines([ids])` **[O]** → `OnlineRequest{id: my_id, peers: [ids]}` on TCP to hbbs → `OnlineResponse{states: bytes}`. `states` is a **bitmap**, one bit per requested peer in order **[R]**. The peer list UI shows green/grey dots (`OnlineStatusWidget` **[O]**).

## 8. Direct IP access [O] options, [R] logic

- Option `direct-server = Y` makes the host listen on TCP `direct-access-port` (default **21118**) **[R]**.
- The client accepts an "ID" that is an IP or `ip:port`, or a hostname if `allow-hostname-as-id` is set **[O]**. It skips rendezvous entirely and starts with the E2E handshake. There is no server-signed key in this case, so the client trusts the presented key on first use.
- **Security implication**: no `licence_key` check, so a strong password is mandatory.

## 9. LAN discovery and WOL (`src/lan.rs`) [O] names, [R] logic

- UDP broadcast on port **21119** (`RENDEZVOUS_PORT + 3`) to `255.255.255.255` and per-interface broadcast addresses (`default-net`, `GetAdaptersAddresses` **[O]**).
- Message: `PeerDiscovery{cmd: "ping" | "pong", mac, id, username, hostname, platform, misc}` **[O]**, carried in a `RendezvousMessage`.
- A host with `enable-lan-discovery` **[O]** answers pings with `pong` plus its info. The client stores results in `LanPeers` (`wire_main_load_lan_peers`, `wire_main_remove_discovered` **[O]**) for the "Discovered" tab.
- **WOL**: `wire_main_wol(id)` **[O]** looks up the stored MAC (`mac_address`, `SendARP` import **[O]**) and sends a magic packet with `wol-rs` to the broadcast address on port 9.

## 10. Rendezvous message envelope

All hbbs/hbbr traffic uses a single `RendezvousMessage { oneof union { … } }` (the name `RendezvousMessage` is seen in the strings **[O]**). Over TCP it is framed exactly like peer messages. Over UDP it is one datagram per message with no length header. See [proto/rendezvous.proto](proto/rendezvous.proto).

## 11. Clone notes [D]

1. Implement **your own hbbs/hbbr first** (≈1500 LoC): a UDP/TCP listener, an ID table in SQLite (`id, uuid, pk, addr, last_seen`), a punch broker, and a relay pairing map with UUID → oneshot channel. The open-source `rustdesk-server` is a useful reference, but it is AGPL.
2. A punch needs `SO_REUSEADDR` plus `SO_REUSEPORT` (Unix) on the **same local port** for the hbbs connection, the listen socket, and the outgoing connect. Use `socket2` to build the sockets, then convert them into tokio sockets.
3. Run the punch race with `tokio::select!`: direct TCP vs UDP/KCP vs IPv6 vs a relay fallback after a timeout. Cancel the losers.
4. Keep the transport abstract: `trait FramedStream: Sink<Bytes> + Stream<Item=Bytes>`, so TCP, KCP, WS and relay are interchangeable.
5. For modern alternatives, consider **QUIC** (`quinn`) for both punching and transport. You get congestion control and multiplexed channels, which avoid video blocking file transfer. The cost is losing wire compatibility.
