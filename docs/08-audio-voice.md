# 08: Audio Streaming and Voice Calls

## 1. One-way audio: host → client

```
HOST (audio_service.rs)                                       CLIENT (client.rs)
WASAPI loopback / selected input ──► resample/downmix ──► Opus encode ──► AudioFrame ──► Opus decode ──► ring buffer ──► cpal output
           (cpal)                       (dasp)          (magnum-opus)                    (magnum-opus)    (ringbuf)      (WASAPI)
```

### 1.1 Host side (`src/server/audio_service.rs`) [O] module, [R] details
- Capture uses `cpal` **[O]** (RustDesk's git fork). By default it opens **WASAPI loopback** on the default render device ("what you hear"). Whether loopback is available is reported to the UI by `wire_main_audio_support_loopback` **[O]**.
- The source can be changed with the `audio-input` option **[O]**. Devices are listed via `wire_main_get_sound_inputs` **[O]**. An empty value means system sound (loopback); a device name means a microphone or line-in.
- The service is a normal publish/subscribe `Service`. It only captures while at least one subscribed connection has the **Audio** permission (`PermissionInfo.Permission.Audio = 3` **[O]**) and has not set `OptionMessage.disable_audio` **[O]**.
- Format: **48 000 Hz, stereo**. On this machine the client log shows `recved audio format, sample rate=48000` and `AudioFormat { sample_rate: 48000, channels: 2 }` **[O]**. The host first sends `Misc.audio_format{sample_rate, channels}` **[O]**, then a stream of `Message.audio_frame{data}` **[O]**, where each `data` is one Opus packet.
- Opus is in low-delay/VoIP or audio mode with 10–20 ms frames **[R]**. Samples are converted to f32 and downmixed or upmixed to the advertised channel count (`dasp_signal`, `fon` crates **[O]**).
- Silence detection: when the captured buffer is all zeros, the host skips sending, so idle desktops use no audio bandwidth **[R]**.

### 1.2 Client side [O] log lines, [R] details
- On `audio_format`, the client opens the default output device: `Using default output device: "Speakers (Realtek(R) Audio)"`, `Default output format: SupportedStreamConfig { channels: 2, sample_rate: 48000, … sample_format: F32 }` **[O]**.
- A dedicated decoder loop ("Audio decoder loop exits" on close **[O]**) decodes Opus into f32 PCM and resamples if the device rate differs. It pushes the PCM into a `ringbuf` **[O]** ring buffer that the cpal output callback drains.
- **Jitter/latency policy [R]**: when the ring buffer grows beyond a threshold (around 100–200 ms), old samples are dropped so latency doesn't build up. When it runs dry, the callback outputs silence.
- The client can mute with `disable_audio`, or by toggling the session option (`wire_session_toggle_option("disable-audio")` **[O]**).

## 2. Voice call (two-way microphone)

Messages **[O]**:
```
VoiceCallRequest  { int64 req_timestamp = 1; bool is_connect = 2; }
VoiceCallResponse { bool accepted = 1; int64 req_timestamp = 2; int64 ack_timestamp = 3; }
```

Flow **[R]**, API names **[O]**:
```
Client UI: wire_session_request_voice_call ──► VoiceCallRequest{req_ts, is_connect=true} ──► Host
Host forwards to CM via IPC (Data::VoiceCallIncoming) ──► CM shows "incoming voice call" (event on_voice_call_incoming)
CM user accepts: wire_cm_handle_incoming_voice_call(id, accept) ──► VoiceCallResponse{accepted, req_ts, ack_ts} ──► Client
Both sides now:
   - capture their microphone (device chosen via wire_set_voice_call_input_device / wire_get_voice_call_input_device)
   - send AudioFrame in both directions over the same connection
   - the host switches its audio source from loopback to (loopback + mic mix) or mic only for the call [R]
Hang up: wire_session_close_voice_call (client) / wire_cm_close_voice_call (host) ──► VoiceCallRequest{is_connect=false}
```
UI events (`app.so`) **[O]**: `on_voice_call_waiting`, `on_voice_call_started`, `on_voice_call_closed`, `on_voice_call_incoming`, `update_voice_call_state`.

Edge cases **[R]**:
- Only one voice call per host at a time. A second request is refused (`accepted=false`).
- If the host is muted (`disable_audio`), the call still works for the microphone direction.
- `req_timestamp` matches responses to requests, so a late response to a cancelled request is ignored.

## 3. Bandwidth figures [D] (for design)

| Content | Opus bitrate (typical) |
|---|---|
| Voice (mono 48 kHz) | 24–32 kbps |
| Desktop audio (stereo music) | 64–128 kbps |

Audio is tiny compared to video, but it shares the ordered stream (see review §3.1). Any video backlog delays audio, causing lip-sync drift.

## 4. Clone recommendations [D]

| Need | Crate |
|---|---|
| Capture/playback | `cpal` (WASAPI loopback is supported on Windows through the input stream of an output device) |
| Codec | `audiopus` or `opus` (libopus bindings); `magnum-opus` is what RustDesk uses |
| Resampling | `rubato` |
| Buffering | `ringbuf` |
| Echo cancellation for voice calls | `webrtc-audio-processing` (RustDesk does not appear to do AEC; this would be an improvement) |

- Put audio on its own **unreliable or low-priority-latency channel** (a QUIC datagram or a separate stream) so loss or backlog on the video path doesn't stall it.
- Include a 32-bit sequence number and a timestamp in each audio packet. That lets a jitter buffer reorder packets and lets you sync audio against video `pts`.
