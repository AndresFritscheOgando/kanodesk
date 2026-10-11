# 07: Input and Control (wire semantics)

This doc describes the **message formats and semantics** needed to interoperate. Host-side injection is just "simulate this event with the OS input API"; see [16](16-clone-blueprint.md) §3 for crate choices.

## 1. Mouse: `MouseEvent{mask, x, y, modifiers}` [O] fields

`mask = (buttons << 3) | kind` **[R]**

| kind | value | | button | bit |
|---|---|---|---|---|
| move | 0 | | left | 0x01 |
| down | 1 | | right | 0x02 |
| up | 2 | | middle/wheel | 0x04 |
| wheel | 3 | | back | 0x08 |
| trackpad (smooth scroll) | 4 | | forward | 0x10 |
| move relative | 5 | | | |

- move/down/up: `x, y` are **absolute coordinates in the host's virtual-desktop space** (`sint32`, so monitors left of or above the primary have negative values). The client maps widget coordinates through the canvas zoom/offset and the selected `DisplayInfo.x/y`.
- wheel/trackpad: `x, y` are the horizontal/vertical deltas.
- relative: deltas for pointer-locked mode (`RelativeMouseModel` **[O]**, games/3D apps).
- `modifiers`: `ControlKey`s held at that moment (e.g. Ctrl+click).
- Client-side transforms before sending **[O]** options: `swap-left-right-mouse`, reverse wheel (`wire_session_set_reverse_mouse_wheel`), `trackpad-speed` (%).
- Dart sends JSON to `wire_session_send_mouse` **[O]** (e.g. `{"type":"down","buttons":"left"}` **[R]**), and Rust builds the protobuf.

## 2. Keyboard: `KeyEvent` [O]

```
bool down = 1; bool press = 2;           // press = down+up in one message
oneof union { ControlKey control_key = 3; uint32 chr = 4; uint32 unicode = 5; string seq = 6; uint32 win2win_hotkey = 7; }
repeated ControlKey modifiers = 8;
KeyboardMode mode = 9;                   // Legacy=0 Map=1 Translate=2 Auto=3
```

### 2.1 Modes [O] enum, [R] semantics
| Mode | Client sends | Host interprets |
|---|---|---|
| **Map** (default; `keyboard_mode = 'map'` on this machine **[O]**) | `chr` = position/scancode of the physical key, plus `down` | Presses the key at the **same physical position**, so the host's layout decides the character. Best for shortcuts and games. |
| **Translate** | Printable text as `seq`/`unicode` (already resolved by the client's layout, dead keys and IME); non-printables as `control_key` | Types the text as Unicode; presses control keys |
| **Legacy** | `control_key` for named keys, `chr` = character | Maps the character to a key in the host layout |
| **Auto** | Map or Translate, chosen by platform pair | |

- `win2win_hotkey`: Windows↔Windows packed virtual-key + scancode for exact hotkey reproduction **[R]**.
- Map mode needs the client's physical layout class (ISO/ANSI): `wire_get/set_local_kb_layout_type` **[O]**, UI `kb_layout_type_chooser.dart` **[O]**.
- Mode support depends on the peer's version and platform: `wire_session_is_keyboard_mode_supported` **[O]**.
- Text paste as keystrokes: `wire_session_input_string` **[O]** sends a `seq`.

### 2.2 `ControlKey` [O]
About 80 named keys (Alt, Backspace, CapsLock, Control, Delete, arrows, F1–F12, Home/End, PageUp/Down, Return, Shift, Space, Tab, Numpad0–9, NumLock, Meta, RWin, RShift/RControl/RAlt, media/volume…) plus two **actions**: `CtrlAltDel = 100`, `LockScreen = 101`. Full list in [`proto/message.proto`](proto/message.proto).

### 2.3 Client keyboard focus [O] log
Key events are only sent while the remote session window has focus. While it does, the client also forwards OS shortcuts (Win key, Alt+Tab) to the remote side instead of acting on them locally. The log shows the client taking and releasing this per-session focus as the window is activated (`[grab] Run(...) mode=map` / `releasing grab` **[O]**). API: `wire_session_enter_or_leave` **[O]** (pointer entered or left the canvas). Related per-peer key option: `allow_swap_key` **[O]** (swap Ctrl/Cmd when controlling a Mac **[R]**).

## 3. Touch / pointer devices [O]
```
PointerDeviceEvent { oneof union { TouchEvent touch_event = 1; } repeated ControlKey modifiers = 2; }
TouchEvent { oneof union { TouchScaleUpdate scale_update{scale}; TouchPanStart pan_start{x,y};
                           TouchPanUpdate pan_update{x,y}; TouchPanEnd pan_end{x,y}; } }
```
Used by touch clients (pinch-zoom and pan gestures, forwarded to the host as gestures). Desktop API: `wire_session_send_pointer` **[O]**. Log: "unknown touch pan event" **[O]**.

## 4. Cursor [O]
| Message | Meaning |
|---|---|
| `CursorData{id, hotx, hoty, width, height, colors}` | New cursor bitmap (RGBA, possibly compressed), identified by `id` |
| `cursor_id` (Message arm 14) | Switch to an already-sent cursor (client caches by id) |
| `CursorPosition{x, y}` | Host cursor moved (sent when the host user moves it or it jumps) |

- Client options: `show_remote_cursor`, `zoom-cursor`, `follow_remote_cursor`, `follow_remote_window`, `show_my_cursor` **[O]**. The client renders the cursor with the `flutter_custom_cursor` plugin **[O]** (`CursorModel`, `CursorManager` **[O]**).
- When `DisplayInfo.cursor_embedded` is true, the cursor is part of the video and no cursor messages are sent.

## 5. Control actions summary
| Action | Wire | API |
|---|---|---|
| Ctrl+Alt+Del | `KeyEvent{control_key=CtrlAltDel}` | `wire_session_ctrl_alt_del` (host side: see [14](14-windows-platform.md) §3) |
| Lock screen | `KeyEvent{control_key=LockScreen}` | `wire_session_lock_screen` |
| Block local user input | `OptionMessage.block_input` → `BackNotification.block_input_state` | toggle option ([10](10-extra-session-types.md) §9) |
| View only | client stops sending input (`view_only` **[O]**) | toggle option |
| Keyboard permission | host `Permission.Keyboard = 0`; when off, the host ignores input | CM toggle |
| Local/remote input arbitration | host tracks the last local vs remote input time (`wire_main_get_mouse_time`, `wire_main_check_mouse_time` **[O]**) | — |
| Double-click threshold | `wire_get_double_click_time` **[O]** | — |

## 6. Clone notes [D]
- Model input as a small enum independent of protobuf. Send input on a low-latency channel (QUIC datagram or high-priority stream), with sequence numbers. Coalesce mouse moves (send only the latest position per frame interval).
- Start with **Map mode only** (scancodes): it is the most predictable. Add Translate for cross-layout users later.
- Always release all pressed keys and buttons on disconnect or focus loss (send key-up for anything still down), so keys don't get stuck on the host.
