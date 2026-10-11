# 09: Clipboard and File Transfer

Three separate mechanisms:
1. **Clipboard sync** for text, rich text and images (`Clipboard`, `MultiClipboards`).
2. **File copy/paste through the clipboard** (`Cliprdr*`): Ctrl+C a file on one side, Ctrl+V it on the other.
3. **File manager and transfer jobs** (`FileAction` / `FileResponse`), shown in a dedicated window.

---

## 1. Clipboard sync (non-file)

### 1.1 Messages [O]
```
message Clipboard {
  bool compress = 1;          // content is zstd-compressed
  bytes content = 2;
  int32 width = 3;            // for ImageRgba
  int32 height = 4;
  ClipboardFormat format = 5; // Text=0 Rtf=1 Html=2 ImageRgba=21 ImagePng=22 ImageSvg=23 Special=31
  string special_name = 6;    // for Special: an app-specific format name
}
message MultiClipboards { repeated Clipboard clipboards = 1; }
```

### 1.2 Watching the clipboard [O] modules/log, [R] logic
- `src/clipboard.rs` uses `clipboard-master` **[O]**, a clipboard change listener built on `AddClipboardFormatListener` and a hidden message-only window. One listener thread runs per process and fans changes out to subscribers. The log shows `Subscribe clipboard listener: client-clipboard`, `Start clipboard listener thread`, `Clipboard listener subscribed` **[O]**.
- Reading and writing go through `arboard` **[O]** (git fork with HTML/RTF/image support).
- **Host side**: `src/server/clipboard_service.rs` **[O]** is a `Service`. When the clipboard changes it reads every supported format, builds `MultiClipboards`, and publishes it to every connection with the **Clipboard** permission (`Permission.Clipboard = 2`) that has not set `disable_clipboard`.
- **Client side**: `Start client clipboard loop` **[O]**. When the local clipboard changes ("clipboard updated on client" **[O]**), the client sends `MultiClipboards` to the host.
- **Loop prevention [R]**: after writing peer content into the local clipboard, the receiver remembers a hash of it and ignores the change notification that its own write triggers. Without this, two sides would ping-pong forever.
- **Compression [R]**: content above a threshold of a few hundred bytes is zstd-compressed and `compress=true` is set.
- **Images**: PNG is preferred (`ImagePng`), and raw RGBA (`ImageRgba` plus width/height) is the fallback. SVG is passed through.
- **Initial sync**: `sync-init-clipboard` **[O]** (per peer). When set, the client sends its current clipboard as soon as the session starts.
- **Legacy peers**: a peer that does not understand `MultiClipboards` gets single `Clipboard` messages (text only) **[R]**. `Clipboard is enabled from client peer: type 1/2` in the CM log **[O]** shows per-type negotiation.

### 1.3 Permissions and toggles [O]
`enable-clipboard` (host global), `disable_clipboard` (client per-peer), `Permission.Clipboard`, `wire_cm_switch_permission(conn, "clipboard", bool)`.

---

## 2. File copy/paste via clipboard (`Cliprdr`)

`libs/clipboard/` **[O]** is a port of the **MS-RDPECLIP** virtual channel (FreeRDP's cliprdr). It lets Explorer paste files that live on the remote machine, streaming them on demand without copying them first.

### 2.1 Messages [O] (all inside `Message.cliprdr`)
| Arm | Message | Fields |
|---|---|---|
| 1 | `CliprdrMonitorReady` | — |
| 2 | `CliprdrServerFormatList` | `formats[]: {id, format(name)}` |
| 3 | `CliprdrServerFormatListResponse` | `msg_flags` (OK/FAIL) |
| 4 | `CliprdrServerFormatDataRequest` | `requested_format_id` |
| 5 | `CliprdrServerFormatDataResponse` | `msg_flags`, `format_data` |
| 6 | `CliprdrFileContentsRequest` | `stream_id, list_index, dw_flags (SIZE=1 / RANGE=2), n_position_low, n_position_high, cb_requested, have_clip_data_id, clip_data_id` |
| 7 | `CliprdrFileContentsResponse` | `msg_flags, stream_id, requested_data` |
| 8 | `CliprdrTryEmpty` | — (ask the peer to clear stale file clipboard) |
| 9 | `CliprdrFiles` | `files[]: {name, size}` (simplified list for non-Windows peers) |

All `conn_id` routing is done locally. The proto field numbers start at 2 because field 1 was the old `conn_id` **[R]**.

### 2.2 Flow (Windows → Windows) [R]
```
Side A: user copies files in Explorer
   A ── FormatList{ "FileGroupDescriptorW", "FileContents", … } ──► B
   B ── FormatListResponse{OK} ──► A
   B: places a *delayed-rendering* IDataObject on its clipboard (libs/clipboard/src/platform/windows.rs)
Side B: user presses Ctrl+V in Explorer
   Explorer asks B's IDataObject for FileGroupDescriptorW
   B ── FormatDataRequest{id(FileGroupDescriptorW)} ──► A
   A ── FormatDataResponse{FILEDESCRIPTORW array: names, sizes, attrs} ──► B
   for each file i, Explorer reads B's IStream:
       B ── FileContentsRequest{stream_id, list_index=i, dw_flags=SIZE} ──► A ── Response{8-byte size}
       loop: B ── FileContentsRequest{RANGE, position, cb_requested (≤ 64 KiB…)} ──► A ── Response{bytes}
```
- `src/client/io_loop.rs` logs `get cliprdr client for conn_id 1` **[O]**. When a session ends: `try to empty client cliprdr for conn_id 0` / `try to empty host cliprdr for conn_id 969` **[O]**, which removes stale delayed-render data so a later paste can't hang.
- `libs/clipboard/src/context_send.rs` **[O]** routes cliprdr messages from the clipboard thread to the right connection ("clipboard context for file transfer created" **[O]**).
- Option: `enable-file-copy-paste` **[O]** (peer config value `true` **[O]**). Ability check: `wire_main_has_file_clipboard` **[O]**.
- On the host, the CM process owns the cliprdr context (it runs in the user's desktop), and `Data::ClipboardFile` IPC relays the messages **[O]**.
- Non-Windows peers use `CliprdrFiles` plus a FUSE filesystem (Linux) or a file promise (macOS) **[R]**.

---

## 3. File manager and transfer jobs

### 3.1 Messages [O]
`Message.file_action` (requests) and `Message.file_response` (replies and data). Both sides can send either, because uploads reverse the roles.

```
FileAction  oneof: read_dir(1) send(2) receive(3) create(4) remove_dir(5) remove_file(6)
                   all_files(7) cancel(8) send_confirm(9) rename(10) read_empty_dirs(11)
FileResponse oneof: dir(1) block(2) error(3) done(4) digest(5) empty_dirs(6)

FileEntry { FileType entry_type; string name; bool is_hidden; uint64 size; uint64 modified_time; }
FileType  { Dir=0, DirLink=2, DirDrive=3, File=4, FileLink=5 }
FileDirectory { int32 id; string path; repeated FileEntry entries; }
```

| Request | Purpose | Reply |
|---|---|---|
| `ReadDir{path, include_hidden}` | List a folder. An empty path on Windows lists drives (`DirDrive`) | `FileResponse.dir` |
| `ReadAllFiles{id, path, include_hidden}` | Recursive listing for a job | `dir` with a flattened relative-path list |
| `ReadEmptyDirs{path, include_hidden}` | Find empty dirs (so they are recreated after transfer) | `empty_dirs{path, empty_dirs[]}` |
| `FileTransferSendRequest{id, path, include_hidden, file_num, file_type}` | **"Peer, send me this"** = download. `file_type=Printer` is used for print jobs (see 10) | stream of `digest` → `block`… → `done`, or `error` |
| `FileTransferReceiveRequest{id, path, files[], file_num, total_size}` | **"Peer, receive these"** = upload. The sender then streams `block`s | `digest` / `error` / `done` from the receiver |
| `FileTransferSendConfirmRequest{id, file_num, union{skip | offset_blk}}` | Answer to a digest: skip the file, or start at block N (resume) | — |
| `FileDirCreate{id, path}` | mkdir | `done`/`error` |
| `FileRemoveDir{id, path, recursive}` / `FileRemoveFile{id, path, file_num}` | Delete | `done`/`error` |
| `FileRename{id, path, new_name}` | Rename | `done`/`error` |
| `FileTransferCancel{id}` | Abort a job | — |

### 3.2 Transfer protocol per job [R], fields [O]

```
Downloader (D)                                   Sender (S)
D ── send{id, path, include_hidden} ──────────►  S enumerates files (ReadAllFiles semantics)
                                                 S ── dir{id, path, entries[]} ── (job file list)
for each file_num:
    S ── digest{id, file_num, last_modified, file_size, is_upload=false} ──► D
    D compares with local target:
        • not exists             → send_confirm{offset_blk = 0}
        • identical size+mtime   → is_identical → UI asks or auto-skip → send_confirm{skip=true}
        • partial (resume job)   → send_confirm{offset_blk = transferred_size / BLK}
        • exists & different     → UI "override?" (event override_file_confirm;
                                   wire_session_set_confirm_override_file(id, file_num, need_override, remember, is_upload))
    S ── block{id, file_num, data, compressed, blk_id} ──► D   (repeated)
    S ── done{id, file_num} ──► D
error{id, error, file_num} can arrive at any time (permission denied, disk full …)
```
- **Block size**: 128 KiB **[R]**. Blocks are zstd-compressed only when it shrinks them (`compressed` flag), so already-compressed files are sent raw.
- **Progress** goes to the UI as `job_progress{id, file_num, speed, finished_size}`, then `job_done` / `job_error` **[O]** event names.
- **Writing on the host**: the `--server` process does **not** write files itself. It forwards blocks over IPC as `Data::FS(NewWrite | WriteBlock | WriteDone | WriteError | WriteOffset | CheckDigest | CancelWrite …)` **[O]**, and the **CM process**, running as the logged-in user, writes them. Files therefore end up owned by the user, not SYSTEM, and respect that user's ACLs. Reads also go through `ReadFile` / `SendConfirmForRead` **[O]**.
- `wire_session_get_conn_session_id` / per-job `id` keep simultaneous jobs apart.

### 3.3 Job persistence and resume [O]
- Peer config holds `[transfer] write_jobs = [] read_jobs = []` **[O]**. Unfinished jobs (path, file list, `file_num`, `transferred_size`) are saved there.
- On reconnect: `wire_session_load_last_transfer_jobs` **[O]**. Log strings `restore write_job:` / `restore read_job:` **[O]**. `wire_session_resume_job` restarts a job, and the digest/offset handshake skips blocks already done.

### 3.4 Client-side API (file manager window) [O]
| Group | Functions |
|---|---|
| Browse | `wire_session_read_remote_dir`, `wire_session_read_local_dir_sync`, `wire_session_read_remote_empty_dirs_recursive_sync`, `wire_session_read_local_empty_dirs_recursive_sync` |
| Jobs | `wire_session_send_files` (start), `wire_session_add_job` (queue), `wire_session_resume_job`, `wire_session_cancel_job`, `wire_session_load_last_transfer_jobs` |
| Edits | `wire_session_create_dir`, `wire_session_remove_file`, `wire_session_rename_file`, `wire_session_read_dir_to_remove_recursive`, `wire_session_remove_all_empty_dirs` |
| Conflicts | `wire_session_set_confirm_override_file` |
| UI | `FileManagerPage`, `FileManagerView`, `FileController`, `JobController`, `CmFileModel` (host-side log) **[O]** |

Permissions: `enable-file-transfer` (host global) **[O]**, `Permission.File = 4` **[O]**, and the session type `LoginRequest.file_transfer{dir, show_hidden}` **[O]**. A one-way mode (`one-way-file-transfer` **[O]**) allows downloads from the host but no uploads to it. File events go to the Pro audit API (`/api/audit/file`, see 13) **[R]**.

---

## 4. Clone recommendations [D]

1. **Don't run file blocks on the interactive stream.** Use a separate QUIC stream or connection per job; this is the biggest latency win (review §3.1).
2. Use content hashing (BLAKE3 per 1 MiB chunk) instead of size+mtime for the "identical" check and for resume. Then you can resume safely even when a file changed.
3. Write host-side files through a user-context helper, as RustDesk does with the CM. Never write as SYSTEM.
4. For clipboard-file paste on Windows, implement `IDataObject` + `IStream` with delayed rendering (`windows` crate COM). It's complex; ship the file manager first and add cliprdr later.
5. Clipboard: `arboard` + `clipboard-master` are enough for an MVP. Add a "last written hash" guard against loops.
