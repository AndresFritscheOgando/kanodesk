# 06: Video Pipeline

```
HOST                                                                 CLIENT
┌────────────┐  BGRA  ┌──────────┐ I420/I444 ┌─────────┐ VideoFrame  ┌─────────┐ RGBA/texture ┌────────┐
│ Capturer   │───────►│ Convert  │──────────►│ Encoder │────────────►│ Decoder │─────────────►│ Render │
│ DXGI/GDI/  │        │ (libyuv- │           │ VPx/AOM/│  (per disp) │ VPx/AOM/│              │ Flutter│
│ Mag/camera │        │  style)  │           │ HW/VRAM │             │ HW/VRAM │              │ texture│
└────────────┘        └──────────┘           └─────────┘             └─────────┘              └────────┘
   ▲   video_service.rs (one thread per captured display)                ▲ io_loop + per-display thread
   └── video_qos.rs  ◄──── TestDelay / video_received acks ──────────────┘
```

## 1. Capture (`libs/scrap/src/dxgi/*`, `libs/scrap/src/common/dxgi.rs`) [O]

| Backend | Evidence | Use |
|---|---|---|
| **DXGI Desktop Duplication** | `D3D11CreateDevice`, `CreateDXGIFactory1` imports **[O]**; `scrap/common/dxgi.rs` **[O]**; option `enable-directx-capture` **[O]** | Default on Win8+. `IDXGIOutput1::DuplicateOutput` → `AcquireNextFrame` returns a D3D11 texture plus dirty rects and the pointer shape. |
| **GDI** | `scrap/dxgi/gdi.rs`; `BitBlt`, `CreateDIBSection`, `GetDIBits` imports **[O]** | Fallback when DXGI fails: secure desktop, some RDP sessions, Win7. |
| **Magnifier** | `scrap/dxgi/mag.rs`; `MagSetWindowFilterList` string **[O]** | Captures the screen **while excluding chosen windows**. Used by privacy mode (`win_mag`) so the black privacy overlay is not captured. |
| **Camera** | `scrap/common/camera.rs`, `nokhwa` **[O]** | "View camera" sessions. |

Behaviour **[R]**:
- Each `DisplayInfo` maps to one capturer. `wire_main_get_displays`, `wire_main_get_main_display` **[O]**.
- A capture timeout of about 100 ms means "no change", in which case nothing is sent (static screens use no bandwidth).
- When the desktop switches (UAC/secure desktop/lock screen), DXGI gets `ACCESS_LOST`. The service thread re-creates the capturer and may switch to GDI. Running as SYSTEM in the user session is what makes capturing the secure desktop possible.
- `cursor_embedded`: when true, the cursor is drawn into the frame (for example on GDI or some platforms). Otherwise the cursor is sent separately ([07](07-input-control.md)).
- Monitor scale/DPI: `DisplayInfo.scale` and `original_resolution` **[O]**.

## 2. Pixel conversion (`libs/scrap/src/common/convert.rs`) [O]

- BGRA → **I420** (4:2:0, default) or **I444** (4:4:4, `Chroma.I444`, toggled per peer by the `i444` option **[O]**). I444 keeps sharp text in colour at higher bitrate.
- Conversion uses libyuv-equivalent SIMD routines statically linked in **[R]**.
- The VRAM path skips the CPU conversion: a D3D11 video processor converts BGRA→NV12 on the GPU **[R]**.

## 3. Encoders (`libs/scrap/src/common/{codec,vpxcodec,aom,hwcodec,vram}.rs`) [O]

| Codec | Implementation | Proto arm | Notes |
|---|---|---|---|
| VP8 | libvpx | `vp8s` (12) | Lightweight, used by low-end peers |
| **VP9** | libvpx | `vp9s` (6) | **Default software codec.** Log: `new video handler for display #0, format: VP9` **[O]** |
| AV1 | libaom (realtime, `cpu-used` high) | `av1s` (13) | `av1-test = 'Y'` in config **[O]** means the AV1 encoder passed a startup speed test. AOM tuning keys appear in the strings (`enable-cdef`, `enable-tpl-model`, `--end-usage`, `--cq-level`… **[O]**). |
| H.264 | FFmpeg via `hwcodec` (nvenc/amf/qsv, or mediacodec on Android) | `h264s` (10) | Hardware only on desktop |
| H.265 | same | `h265s` (11) | |
| raw | `RGB`/`YUV` + zstd | `rgb`/`yuv` | Debug and LAN |

### 3.1 HW codec discovery (`hwcodec.rs`) [O]
1. At startup the UI spawns `RustDesk.exe --check-hwcodec-config` **[O]** (separate log dir `log\check-hwcodec-config` **[O]**).
2. That process tries to open each FFmpeg encoder/decoder (`h264_nvenc`, `hevc_nvenc`, `h264_amf`, `h264_qsv`, `d3d11va` decoders…) and each **VRAM** pair (driver `FFMPEG` or vendor SDK, `vendor: NV|AMF|INTEL`, adapter `luid`).
3. It writes the results to `RustDesk_hwcodec.toml` as JSON with a `signature` (a hash of GPU/driver identity, so the check reruns when drivers change) **[O]**:
   ```json
   { "signature": 18014403062830980,
     "ram_encode":  [{"name":"h264_nvenc","format":"H264","priority":0,"hwdevice":"AV_HWDEVICE_TYPE_NONE"}, …],
     "ram_decode":  [{"name":"h264","format":"H264","priority":0,"hwdevice":"AV_HWDEVICE_TYPE_D3D11VA"}, …],
     "vram_encode": [{"driver":"FFMPEG","vendor":"NV","luid":87558,"data_format":"H264"}, …],
     "vram_decode": [ … ] }
   ```
4. The main process loads this file ("set hwcodec config" **[O]**) and advertises abilities in `PeerInfo.encoding` / `SupportedDecoding`.

- **RAM path**: CPU frame → upload → HW encode → bitstream.
- **VRAM path** (`vram.rs`, `option enable-hwcodec`, `wire_main_has_vram` **[O]**): the DXGI texture stays on the GPU, and the encoder takes the D3D11 texture directly. On the decode side, D3D11VA output goes to `flutter_gpu_texture_renderer_plugin` **[O]** with no CPU copy. The adapter `luid` must match between capture and encode, and between decode and render (log: `luid: Some(86699)` **[O]**).

### 3.2 Codec negotiation [R], fields [O]
- The client sends `SupportedDecoding{ability_vp9, ability_h264, ability_h265, ability_vp8, ability_av1, prefer, i444, prefer_chroma}` in `LoginRequest.option.supported_decoding`.
- The host intersects that with its own encoders. If several clients are connected to one host, the host picks a codec **all of them** can decode (one encoder is shared per display). Order when `prefer = Auto`: a HW codec if both sides support it and the GPU is not overloaded; otherwise VP9; AV1 only when chosen explicitly, or when Auto heuristics pick it on fast CPUs (`av1-test`).
- `wire_session_change_prefer_codec`, `wire_session_alternative_codecs`, `wire_main_supported_hwdecodings` **[O]**.
- The host announces encoder changes with `Misc.supported_encoding` **[O]**.

### 3.3 Frames
- `VideoFrame{ union{vp9s|h264s|…}, display }` **[O]**. `EncodedVideoFrames.frames[]`, each `{data, key, pts}` **[O]**. Several frames can be batched.
- Keyframes: sent at start, on `Misc.refresh_video` / `refresh_video_display(n)` (client request after a decode error or a window reopen), and periodically for HW codecs **[R]**.
- The decoder thread count depends on CPU, available memory, and codec (log: `cpu num: 24, cpu usage: 5.69, available memory: 6G, codec thread: 2` **[O]**).

## 4. QoS / adaptive bitrate (`src/server/video_qos.rs`) [O] module, [R] algorithm

Inputs:
- **RTT**: the host sends `TestDelay{time, from_client=false, last_delay, target_bitrate}` about once a second. The client echoes it back immediately, and the host measures the delay. The client also displays `last_delay` and `target_bitrate` in the quality monitor.
- **Acks**: if the client sets `LoginRequest.video_ack_required` **[O]**, it sends `Misc.video_received=true` **[O]** after each decoded frame. The host limits frames in flight, typically 1-2 unacked frames per display, so it never floods a slow link.
- **User settings**: `ImageQuality{Best=4, Balanced=3, Low=2}` **[O]** and `custom_image_quality` (10-2000 %, default 50 per the peer config **[O]**). `custom_fps` (default 30 **[O]**; log "Set fps to 30" **[O]**). `Misc.full_speed_fps` and `Misc.auto_adjust_fps` **[O]**.

Algorithm sketch **[R]**:
```
base_bitrate(width,height,codec) ≈ k_codec * sqrt(w*h) * ...   // per-codec constant
ratio = {Best:1.5?, Balanced:1.0?, Low:0.5?} or custom/100
target_bitrate = base_bitrate * ratio
every second:
   if delay rising or frames unacked → fps = max(fps*0.8, min_fps); maybe lower ratio
   if delay low & stable            → fps = min(fps+step, user_fps)
encoder.set_bitrate(target_bitrate); capture interval = 1000/fps
```
With several viewers, the host takes the **minimum** fps across connections and the quality from the user who set it most recently. Debug is visible via `show_quality_monitor` (FPS / delay / target bitrate / codec / chroma).

## 5. Multi-monitor [O]

| Action | Client API | Message |
|---|---|---|
| Switch the displayed monitor | `wire_session_switch_display` | `Misc.switch_display` → host responds `SwitchDisplay{display,x,y,width,height,cursor_embedded,resolutions,original_resolution}` |
| View several monitors at once (merged canvas) | `wire_session_set_use_all_my_displays_for_the_remote_session`, `CaptureDisplays{add,sub,set}` | Host starts one video_service per display. Each `VideoFrame.display` routes to the right decoder. |
| Each display in a separate window | `wire_session_set_displays_as_individual_windows`, `wire_session_start_with_displays` | Same, but the UI opens extra windows |
| Follow the active monitor | `Misc.follow_current_display` | Host tells the client which display holds the active window |
| Monitor toolbar | options `allow-monitor-switch-*`, `show_monitors_toolbar` **[O]** | UI only |

## 6. Remote resolution and virtual displays

- `wire_session_change_resolution(display, w, h)` → `Misc.change_display_resolution{display, resolution}` **[O]**. The host calls `ChangeDisplaySettingsExW` (`EnumDisplaySettingsExW` import **[O]**) and restores the original resolution when the session ends.
- **Virtual display** (headless servers, or extra monitors):
  - Driver: **Amyuni `usbmmidd_v2`** IDD (`usbmmIdd.inf`, `usbmmIdd.dll` shipped **[O]**), installed with `SetupDi*` + `UpdateDriverForPlugAndPlayDevicesW` (imports **[O]**). It is controlled through `DeviceIoControl` on the device interface.
  - `src/virtual_display_manager.rs`, `libs/virtual_display`, `dylib_virtual_display.dll` **[O]**.
  - `Misc.toggle_virtual_display{display, on}` **[O]**. Virtual displays are listed in `platform_additions`.
  - If no physical monitor is attached and `allow-linux-headless`/Windows equivalent is set, a virtual display is created automatically **[R]**.
  - `--uninstall-amyuni-idd` **[O]**.

## 7. Client rendering [O]

- **Texture render** (`use texture render: true` in log **[O]**, `wire_main_get_use_texture_render`, `use-texture-render` option **[O]**): Rust writes the RGBA buffer into a Flutter external texture (`texture_rgba_renderer_plugin.dll`) or shares the D3D11 texture (`flutter_gpu_texture_renderer_plugin.dll`). The keys come from `wire_session_register_pixelbuffer_texture` / `register_gpu_texture` and `wire_get_next_texture_key` **[O]**.
- **Fallback**: Dart calls `session_get_rgba` (exported **[O]**) and `wire_session_next_rgba` to pull frames.
- Canvas: `CanvasModel` (zoom, pan, scale), `view_style` original/adaptive/custom, `scroll_style` scrollauto/scrollbar/edge **[O]**.
- Optional D3D rendering for Flutter: `allow-d3d-render` **[O]**.

## 8. Recording (`libs/scrap/src/common/record.rs`) [O]

- Client side: `wire_session_record_screen(start)` **[O]** writes the **already-encoded** stream to a file without re-encoding: VP8/VP9/AV1 into **WebM** (`rust-webm` **[O]**), H.264/H.265 into **MP4** (FFmpeg muxer in hwcodec) **[R]**. The client tells the host with `Misc.client_record_status` **[O]** (the host shows a "recording" indicator).
- Host side (incoming sessions): `allow-auto-record-incoming` **[O]**. The host records the stream it sends.
- `allow-auto-record-outgoing` **[O]** does the same on the client side.
- Directory: `video-save-directory` / `wire_main_video_save_directory` **[O]**.
- Pro: recordings can be uploaded to `/api/record` (`hbbs_http/record_upload.rs` **[O]**).
- On a codec switch mid-session, a new file is started **[R]**.

## 9. Screenshot [O]

`wire_session_take_screenshot(display)` → `ScreenshotRequest{display, sid}` → host captures one frame, encodes PNG, and replies `ScreenshotResponse{sid, msg, data}`. The client saves the file or copies it to the clipboard (`wire_session_handle_screenshot` **[O]**). The host must support it (`is_screenshot_supported` **[O]**).

## 10. Clone recommendations [D]

| Need | Crate |
|---|---|
| Windows capture | `windows` crate DXGI duplication directly, or `windows-capture` (WGC). WGC is simpler and supports per-window capture, but shows a yellow border on older builds. |
| Cross-platform capture | `xcap`, `scap` |
| Colour conversion | `yuvutils-rs` (pure Rust SIMD) or `dcv-color-primitives` |
| VP8/VP9 | `vpx-encode` / `env-libvpx-sys`. **AV1**: `rav1e` (encoder; slower realtime) + `dav1d` (decoder) or libaom-sys. **H.264**: `openh264` (Cisco, BSD) for software; `ffmpeg-next` for HW. |
| HW on Windows | Media Foundation transforms through the `windows` crate (no FFmpeg licensing), or NVENC/AMF SDK bindings |
| Rendering | `wgpu` texture upload, or share a D3D11 texture via DXGI shared handles |

MVP: DXGI → I420 → VP9 (libvpx realtime, `cpu-used=6`, `lag_in_frames=0`, `error_resilient`, a keyframe on request) → client libvpx decode → `wgpu`. Add dirty-rect skipping, then QoS, then HW encode.
