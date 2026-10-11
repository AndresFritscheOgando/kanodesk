# 14: Windows Platform Integration

Modules: `src/platform/windows.rs`, `src/platform/win_device.rs`, `libs/hbb_common/src/platform/windows.rs`, `src/tray.rs`, `src/virtual_display_manager.rs`, `src/server/portable_service.rs` **[O]**.

## 1. Installed layout [O]

```
C:\Program Files\RustDesk\
  RustDesk.exe                     Flutter runner + role dispatcher
  librustdesk.dll                  core (Rust)
  flutter_windows.dll, data\app.so, data\flutter_assets\, data\icudtl.dat
  *_plugin.dll                     Flutter plugins: desktop_drop, desktop_multi_window, file_selector,
                                   flutter_custom_cursor, flutter_gpu_texture_renderer, screen_retriever,
                                   texture_rgba_renderer, uni_links_desktop, url_launcher, window_manager, window_size
  dylib_virtual_display.dll        virtual display control
  printer_driver_adapter.dll       client-side printing of received jobs
  WindowInjection.dll              privacy-mode helper
  drivers\RustDeskPrinterDriver\   v4 printer driver package
  usbmmidd_v2\                     Amyuni Indirect Display Driver (virtual monitors)
```

## 2. Windows service [O]

| Property | Value |
|---|---|
| Name / display | `RustDesk` / `RustDesk Service` |
| Type / start | `WIN32_OWN_PROCESS`, `AUTO_START` |
| Account | `LocalSystem` |
| Command | `"C:\Program Files\RustDesk\RustDesk.exe" --service` |

- Implemented with `windows-service 0.6` **[O]** (`StartServiceCtrlDispatcherW`, `RegisterServiceCtrlHandlerExW` imports **[O]**).
- **Session follower loop [R]**: every ~1 s, find the active console session (`WTSGetActiveConsoleSessionId`; `WTSEnumerateSessions`/`WTSQuerySessionInformationW` imports **[O]**) and compare it with the session of the running `--server` child. If it changed, or the child died, kill and relaunch: duplicate the SYSTEM token, set its `TokenSessionId` to the target session, build an environment block (`CreateEnvironmentBlock` **[O]**), then `CreateProcessAsUserW` on desktop `winsta0\default` **[O]** (import). Log string: "start --server with user" **[O]**.
- Service control: `wire_main_start_service`, `wire_main_stop_service`, `wire_main_is_installed_daemon` **[O]**. The UI's "Stop service" sets the `stop-service` option **[O]**.
- **Multiple Windows sessions** (RDP / fast user switching): the host advertises `PeerInfo.windows_sessions{sessions[{sid,name}], current_sid}` **[O]**. The client may pick one (`Misc.selected_sid`, `wire_session_send_selected_session_id` **[O]**). The service then runs the server in that session. "selected invalid sid" **[O]**. Support check: `wire_is_support_multi_ui_session` **[O]**.

## 3. Ctrl+Alt+Del (Secure Attention Sequence) [O]

- `SAS.dll!SendSAS` import **[O]**. It only works from a service or with UIAccess. Windows also requires the policy **`HKLM\Software\Microsoft\Windows\CurrentVersion\Policies\System\SoftwareSASGeneration`** to allow services.
- RustDesk reads the current value, sets it to 1 when needed, and restores the original value afterwards. Strings: "SoftwareSASGeneration is , setting to 1", "Deleted SoftwareSASGeneration (restored to original state)", "Restored SoftwareSASGeneration to", plus the manual fix hint `reg add HKEY_LOCAL_MACHINE\Software\Microsoft\Windows\CurrentVersion\Policies\System /f /v SoftwareSASGeneration /t REG_DWORD /d 1` **[O]**.
- `PeerInfo.sas_enabled` **[O]** tells the client whether the button will work.

## 4. Installer and install modes

| Mode | How | Notes |
|---|---|---|
| **MSI** | `rustdesk-1.4.9-x86_64.msi` (the one on this machine) | Standard install to Program Files and service registration |
| **Self-install from the EXE** | `--install` / `--silent-install` **[O]**; UI `InstallPage` **[O]**; FFI `wire_install_install_me(options, path)`, `wire_install_install_path`, `wire_install_install_options` **[O]** | Copies files, creates shortcuts (`wire_main_create_shortcut` **[O]**), registers the service, firewall rules ([11](11-host-server-side.md) §6), the URL scheme, and optionally the printer driver |
| **Portable** | Run the EXE without installing; `wire_install_run_without_install`, `wire_install_show_run_without_install` **[O]** | No service, so no access to UAC/secure desktop without elevation (§6) |
| **Upgrade** | `--update` **[O]**; `wire_main_is_installed_lower_version` **[O]** | |
| **Uninstall** | `--uninstall`, `--uninstall-service`, `--uninstall-cert`, `--uninstall-amyuni-idd`, `--uninstall-remote-printer` **[O]** | |

`wire_main_goto_install`, `wire_main_is_installed`, `wire_is_disable_installation` **[O]** gate the "Install" banner. `wire_main_check_super_user_permission` **[O]** prompts for UAC.

## 5. Drivers [O]

### 5.1 Amyuni Indirect Display Driver (virtual monitors)
- Files: `usbmmidd_v2\usbmmIdd.inf`, `usbmmidd.cat`, `x64\usbmmIdd.dll`, `idd_instructions.txt`, `License.txt` **[O]**.
- Installed with SetupAPI (`SetupDiCreateDeviceInfoList`, `SetupDiCreateDeviceInfoW`, `SetupDiSetDeviceRegistryPropertyW`, `SetupDiCallClassInstaller`) and `UpdateDriverForPlugAndPlayDevicesW` (newdev.dll); status via `CM_Get_DevNode_Status` **[O]** imports.
- Monitors are plugged and unplugged with `DeviceIoControl` on the driver's interface **[R]**. Used for headless hosts, the "virtual display" toggle, and privacy mode's virtual-display implementation ([06](06-video-pipeline.md) §6, [10](10-extra-session-types.md) §9).

### 5.2 RustDesk Printer Driver
v4 driver package (`.inf`, `.gpd`, `.cat`, render filter DLL, pipeline config) **[O]**. Installed through winspool (`UploadPrinterDriverPackageW`, `InstallPrinterDriverFromPackageW`, `XcvDataW`, `AddPrinter…`) **[O]**. See [10](10-extra-session-types.md) §5.

## 6. Portable service (elevated helper) [O]

When running portable and the user grants elevation ([10](10-extra-session-types.md) §8):
- `RustDesk.exe --portable-service --portable-service-shmem-name <name>` **[O]** runs as SYSTEM in the user's session.
- **Shared memory** (`shared_memory 0.12.4` **[O]**) carries captured frames, and an IPC channel carries input commands. The many strings show:
  - A per-run random name; the "flink" backing file is created and cleaned up ("removed portable service shared-memory flink artifact" **[O]**).
  - An **ACL hardening** check: rejects reparse points and unexpected directory/file targets ("ACL target is a reparse point and is rejected" **[O]**). This protects against symlink attacks on the shared-memory file.
  - An **IPC token** stored in shared memory and verified at handshake ("Invalid portable service ipc token", "Missing portable service ipc token in shared memory, aborting startup" **[O]**).
  - Capacity checks ("capture frame exceeds shared memory capacity: frame_len=, capacity=" **[O]**).
- `Misc.portable_service_running` **[O]** tells the client that elevated capture is active.

## 7. Tray (`src/tray.rs`) [O]

`RustDesk.exe --tray` **[O]**. It uses `tray-icon` + `muda` menus (`Shell_NotifyIconW` import **[O]**). The menu offers open the main window, stop/start service, and quit. Log dir `log\tray` **[O]**.

## 8. URL scheme and single-instance handoff [O]

- `rustdesk://` (or `<appname>://` for custom builds; `wire_main_uri_prefix_sync` **[O]**) is registered in `HKCR` at install time **[R]**.
- `uni_links_desktop_plugin.dll` **[O]** delivers the URL to the running app. A second process started by the browser passes the URL to the first instance over the IPC pipe `…_url` (`wire_main_start_ipc_url_server`, `ipc_url_server` **[O]**), and the app raises event `on_url_scheme_received` **[O]**.
- Formats **[R]**: `rustdesk://<id>`, `rustdesk://connection/new/<id>?password=…`, `rustdesk://config/<base64 server config>`, `rustdesk://file-transfer/<id>`, `rustdesk://port-forward/<id>`. FFI `wire_send_url_scheme` **[O]**.

## 9. Misc Windows behaviours [O] API, [R] details
| Feature | Mechanism |
|---|---|
| DPI awareness | Per-monitor v2 (`src/platform_impl/windows/dpi.rs` **[O]**) |
| Drag & drop of files onto the session window | `desktop_drop_plugin.dll`, `drop_handler.rs` (OLE `RegisterDragDrop` **[O]**) → upload job |
| Window state memory | `wm_Main`, `wm_RemoteDesktop` in `RustDesk_local.toml [ui_flutter]` **[O]** |
| Double-click time sync | `wire_get_double_click_time` **[O]** (so the remote double-click threshold matches) |
| CPU usage reporting | PDH counters (`PdhAddEnglishCounter*` **[O]**). Log: "cpu usage synced: Some(5.69)" **[O]**. Used to choose decoder threads |
| Recent documents | `SHAddToRecentDocs` **[O]** for received files |
| Power status | `CallNtPowerInformation` **[O]** |

## 10. Clone notes [D]
- Write the service with `windows-service`, and the session-follow logic with `windows` crate WTS + token APIs. Test carefully on fast user switching, RDP sessions, and lock/unlock.
- Prefer the Microsoft **IddCx sample** or a licensed IDD for virtual displays. Check the Amyuni licence before redistributing (`usbmmidd_v2\License.txt`).
- Use an MSI built with `cargo-wix` for enterprise deployment, with silent-install switches mirroring `--silent-install`.
