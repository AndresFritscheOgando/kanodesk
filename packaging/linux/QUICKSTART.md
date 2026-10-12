# KanoDesk for Linux (x86_64): quick start

This archive has three programs:

| File | What it is |
|---|---|
| `kanodesk` | The desktop app. On Linux it is a **viewer only**: it controls other desks (Windows hosts) by ID. Others cannot connect to a Linux machine yet. |
| `kanodesk-idserver` | The ID server (devices register here and find each other). |
| `kanodesk-relay` | The relay server (carries sessions that cannot go direct). |

You only need the servers if you run them on this machine; a viewer just needs to reach an existing ID server.

Built on Debian 12 (glibc 2.36): it runs on Debian 12 and 13, Ubuntu 22.04 and newer, and similar distributions. libvpx is built in, nothing to install for video.

## 1. Packages

A desktop install (GNOME, KDE, Xfce, …) normally has everything. On a minimal one:

```bash
sudo apt install libxkbcommon-x11-0 libxcursor1 libxrandr2 libxi6 libwayland-client0 libwayland-egl1 libgl1 libegl1 mesa-vulkan-drivers
```

X11 and Wayland both work. The video is drawn with Vulkan (or OpenGL ES if Vulkan is missing); the main window uses OpenGL.

## 2. Unpack and start

```bash
tar -xzf kanodesk-*-linux-x86_64.tar.gz
cd kanodesk-*-linux-x86_64
./kanodesk
```

## 3. Point it at your ID server

In the main window: **Settings** → **ID server**: the server's address, for example `10.0.0.31` (port 21116 if omitted), and **Server key**: the 64 hex characters `kanodesk-idserver` printed when it started (the Windows machine's KanoDesk has the same key under Settings → Server key; copy it from there). → **Save**.

Settings are saved in `~/.config/kanodesk/kanodesk.toml`.

## 4. Connect

On **Home**, type the remote desk's 9-digit ID (shown in KanoDesk on the Windows machine) and press **Connect**. A viewer window opens; type the one-time password shown on the Windows machine when asked, or wait for the person there to accept.

The same from a terminal:

```bash
./kanodesk --connect 123456789
```

Logs: `~/.local/share/kanodesk/logs/viewer.log`. Recent sessions: `~/.config/kanodesk/peers.toml`. The viewer's own device key: `~/.local/share/kanodesk/client/` (owner-only files).

## Good to know

- **System keys stay local on Linux.** The viewer does not grab the keyboard here, so Super, Alt+Tab, Alt+F4 and similar shortcuts act on this machine, not on the remote one. Everything else (letters, Ctrl/Alt/Shift shortcuts, function keys, media keys) is sent. **Ctrl+Alt+End** sends Ctrl+Alt+Del.
- Viewer shortcuts: **Ctrl+Alt+Shift+M** relative mouse (games), **Ctrl+Alt+Shift+K** Translate mode (types characters as your keyboard layout produces them; useful when the two machines use different layouts).
- A firewall on this machine does not need any opening for the viewer: it only makes outgoing connections (UDP to the ID server on 21116 and 21115, to the host, and to the relay).
- If the window does not open, run `RUST_LOG=info ./kanodesk --connect ID` in a terminal and look at the messages; `WGPU_BACKEND=gl` forces OpenGL for the video if Vulkan misbehaves.

## Running the servers on Linux

```bash
./kanodesk-relay new-secret relay.secret
./kanodesk-relay --listen 0.0.0.0:21117 --secret-file relay.secret --port-range 30000-30099
./kanodesk-idserver --listen 0.0.0.0:21116 --data-dir ./ids --relay PUBLIC_IP:21117 --relay-secret-file relay.secret
```

Open UDP 21115–21117 and 30000–30099. The ID server prints the `server key` clients need. See the project README for details.
