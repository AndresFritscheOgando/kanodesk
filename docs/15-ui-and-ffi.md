# 15: UI Structure and the Flutter↔Rust FFI API

## 1. UI technology [O]

- Flutter desktop. The Dart package is **`flutter_hbb`** (source paths in `app.so`). State management uses **GetX** (`GetxController`, `ObxWidget`, `SuperController` **[O]**).
- Rust bridge: `flutter_rust_bridge 1.80.1` **[O]**. Dart side: `generated_bridge.dart`, `models/native_model.dart`, `models/platform_model.dart` **[O]**.
- Multi-window: `desktop_multi_window` plugin + `utils/multi_window_manager.dart` (`RustDeskMultiWindowManager` **[O]**). Each session *type* has its own window, which holds tabs (`DesktopTabController`, `DesktopTab` **[O]**). Option `enable-open-new-connections-in-tabs` **[O]**.

## 2. Windows and pages [O]

| Window | Dart files | Key widgets/models |
|---|---|---|
| **Main** | `desktop/pages/desktop_home_page.dart`, `connection_page.dart`, `desktop_setting_page.dart`, `desktop_tab_page.dart` | Left panel: your ID, one-time password (refresh/copy), status. Right: ID input with autocomplete + connect / file-transfer buttons; peer tabs. `ServerModel`, `PeerTabModel`, `UserModel` |
| **Peer tabs** | `common/widgets/peer_tab_page.dart`, `peers_view.dart`, `peer_card.dart`, `address_book.dart`, `my_group.dart` | Recent, Favorites, Discovered, Address book, Groups. Grid/list view, search, sort (`peer-sorting`), online dots, context menu (connect, transfer, terminal, rename alias, tags, remove, WOL, create shortcut) |
| **Settings** | `desktop_setting_page.dart`, `common/widgets/setting_widgets.dart` | Tabs [R]: General (theme, language, audio input, recording, hwcodec, D3D render, …), Security (permissions, password, approval mode, 2FA, whitelist, …), Network (ID/relay/API/key, proxy, WebSocket), Display (default view, quality, codec, …), Account, Plugins, Printer, About |
| **Remote desktop** | `desktop/pages/remote_page.dart`, `remote_tab_page.dart`, `screen/desktop_remote_screen.dart`, `widgets/remote_toolbar.dart`, `common/widgets/toolbar.dart`, `remote_input.dart` | Canvas (`CanvasModel`), cursor (`CursorModel`), input (`InputModel`, `RelativeMouseModel`), toolbar (dockable top/edges, pin/collapse: `remote-menubar-*`, `collapse_toolbar` **[O]**), quality monitor, chat overlay, record/screenshot, monitor switcher |
| **File transfer** | `file_manager_page.dart`, `file_manager_tab_page.dart`, `screen/desktop_file_transfer_screen.dart` | Dual pane, job queue (`FileModel`, `JobController`) |
| **Port forward** | `port_forward_page.dart`, `port_forward_tab_page.dart` | Rule table: local port → remote host:port |
| **Terminal** | `terminal_page.dart`, `terminal_tab_page.dart`, `terminal_connection_manager.dart` | `TerminalModel`, `TerminalView` |
| **Camera** | `view_camera_page.dart`, `view_camera_tab_page.dart` | |
| **Connection Manager** | `desktop/pages/server_page.dart` (`DesktopServerPage`) | Incoming connections ([11](11-host-server-side.md) §4) |
| **Install** | `desktop/pages/install_page.dart` | Path, options |
| **Dialogs** | `common/widgets/dialog.dart`, `login.dart`, `custom_password.dart`, `audio_input.dart`, `desktop/widgets/kb_layout_type_chooser.dart` | msgbox, password prompt, 2FA, OS login, account login, keyboard layout choice |
| **Plugins** | `plugin/{manager,model,ui_manager,handlers,event}.dart`, `plugin/widgets/desc_ui.dart` | Render plugin-described UI |

Mobile pages (`mobile/pages/home_page.dart`, `remote_page.dart`) are compiled in but unused on desktop.

## 3. Event stream: Rust → Dart

- A global stream (`wire_start_global_event_stream(app_type)` / `wire_stop_global_event_stream` **[O]**) carries app-level events per window type ("main", "cm", "remote"…).
- A per-session stream is returned by `wire_session_start` **[O]**.
- Payload: JSON object `{"name": <event>, …fields}`. Event names seen in `app.so` are listed in [02](02-architecture.md) §6.
- A `msgbox` event `{type, title, text, link, hasRetry}` drives dialogs. Types seen **[O]**: `input-password`, `re-input-password`, `error`, `success`, `info`, `custom-nocancel`, `custom-nook-nocancel-hasclose`, `custom-error-nook-nocancel-hasclose`, `session-login`, `session-re-login`, `elevation-error`, `wait-uac`, `input-2fa`, … ("custom-*" means a dialog with certain buttons hidden).

## 4. FFI catalogue [O]

All 354 exports of `librustdesk.dll` are grouped below. Signatures are not recoverable without decompilation. Purposes come from the names and their use sites.

### 4.1 Bridge runtime (flutter_rust_bridge plumbing)
`init_frb_dart_api_dl`, `store_dart_post_cobject`, `new_dart_opaque`, `get_dart_object`, `drop_dart_object`, `free_WireSyncReturn`, `free_c_args`, `free_zero_copy_buffer_{u8,i8,u16,i16,u32,i32,u64,i64,f32,f64}`, `new_StringList_0`, `new_int_32_list_0`, `new_uint_8_list_0`.

### 4.2 Plain C exports (non-bridge)
| Export | Purpose |
|---|---|
| `rustdesk_core_main_args` | Role dispatch at process start ([02](02-architecture.md)) |
| `get_rustdesk_app_name` | App name (custom builds) |
| `session_get_rgba` | Pull the latest RGBA frame (non-texture render path) |
| `hwcodec_log`, `hwcodec_av_log_callback` | FFmpeg log sink |
| `privacy_mode_hook_keyboard`, `privacy_mode_hook_mouse` | Hook callbacks for privacy-mode input blocking ([10](10-extra-session-types.md) §9) |

### 4.3 Global events, translations, misc
`wire_start_global_event_stream`, `wire_stop_global_event_stream`, `wire_translate(name, locale)`, `wire_version_to_number`, `wire_send_url_scheme`, `wire_query_onlines`, `wire_option_synced`, `wire_set_cur_session_id`, `wire_get_next_texture_key`, `wire_get_double_click_time`, `wire_host_stop_system_key_propagate`, `wire_get_local_flutter_option` / `wire_set_local_flutter_option`, `wire_get_local_kb_layout_type` / `wire_set_local_kb_layout_type`, `wire_get_voice_call_input_device` / `wire_set_voice_call_input_device`, `wire_peer_get_sessions_count`, `wire_will_session_close_close_session`.

### 4.4 Custom-client / capability flags
`wire_is_custom_client`, `wire_is_incoming_only`, `wire_is_outgoing_only`, `wire_is_disable_settings`, `wire_is_disable_ab`, `wire_is_disable_account`, `wire_is_disable_group_panel`, `wire_is_disable_installation`, `wire_is_preset_password`, `wire_is_preset_password_mobile_only`, `wire_is_selinux_enforcing`, `wire_is_support_multi_ui_session`.

### 4.5 Install
`wire_install_install_me`, `wire_install_install_path`, `wire_install_install_options`, `wire_install_run_without_install`, `wire_install_show_run_without_install`.

### 4.6 Main window (`wire_main_*`)
| Area | Functions |
|---|---|
| Init / app info | `main_init`, `main_get_app_name`, `main_get_app_name_sync`, `main_get_version`, `main_get_build_date`, `main_get_license`, `main_get_langs`, `main_change_language`, `main_change_theme`, `main_get_home_dir`, `main_set_home_dir`, `main_get_data_dir_ios`, `main_get_env`, `main_set_env`, `main_get_error`, `main_on_main_window_close`, `main_hide_dock`, `main_uri_prefix_sync` |
| Identity / status | `main_get_my_id`, `main_get_uuid`, `main_device_id`, `main_device_name`, `main_get_fingerprint`, `main_change_id`, `main_get_connect_status`, `main_check_connect_status`, `main_get_async_status`, `main_get_last_remote_id`, `main_handle_relay_id` |
| Passwords / security | `main_get_temporary_password`, `main_update_temporary_password`, `main_set_permanent_password_with_result`, `main_get_unlock_pin`, `main_set_unlock_pin`, `main_generate2fa`, `main_verify2fa`, `main_has_valid_2fa_sync`, `main_verify_bot`, `main_has_valid_bot_sync`, `main_get_trusted_devices`, `main_remove_trusted_devices`, `main_clear_trusted_devices`, `main_max_encrypt_len` |
| Options | `main_get_option`, `main_get_option_sync`, `main_set_option`, `main_get_options`, `main_get_options_sync`, `main_set_options`, `main_get_local_option`, `main_set_local_option`, `main_get_user_default_option`, `main_set_user_default_option`, `main_get_hard_option`, `main_get_buildin_option`, `main_is_option_fixed`, `main_show_option`, `main_get_common`, `main_get_common_sync`, `main_set_common` |
| Server / network | `main_get_api_server`, `main_test_if_valid_server`, `main_get_socks`, `main_set_socks`, `main_get_proxy_status`, `main_is_using_public_server`, `main_get_http_status`, `main_http_request` |
| Peers | `main_get_peer_sync`, `main_peer_exists`, `main_peer_has_password`, `main_forget_password`, `main_remove_peer`, `main_set_peer_alias`, `main_get_peer_option`, `main_get_peer_option_sync`, `main_set_peer_option`, `main_set_peer_option_sync`, `main_get_peer_flutter_option_sync`, `main_set_peer_flutter_option_sync`, `main_load_recent_peers`, `main_load_fav_peers`, `main_get_fav`, `main_store_fav`, `main_get_new_stored_peers`, `main_load_lan_peers`, `main_get_lan_peers`, `main_discover`, `main_remove_discovered`, `main_wol`, `main_create_shortcut` |
| Account / AB / groups | `main_account_auth`, `main_account_auth_cancel`, `main_account_auth_result`, `main_load_ab`, `main_save_ab`, `main_clear_ab`, `main_load_recent_peers_for_ab`, `main_load_group`, `main_save_group`, `main_clear_group`, `main_resolve_avatar_url`, `main_get_login_device_info`, `main_deploy_device` |
| Service / install | `main_start_service`, `main_stop_service`, `main_is_installed`, `main_is_installed_daemon`, `main_is_installed_lower_version`, `main_goto_install`, `main_update_me`, `main_get_new_version`, `main_get_software_update_url`, `main_check_super_user_permission`, `main_is_root`, `main_is_share_rdp`, `main_set_share_rdp`, `main_start_ipc_url_server`, `main_start_dbus_server` |
| Media / devices | `main_get_displays`, `main_get_main_display`, `main_get_sound_inputs`, `main_audio_support_loopback`, `main_has_hwcodec`, `main_has_vram`, `main_check_hwcodec`, `main_supported_hwdecodings`, `main_has_gpu_texture_render`, `main_get_use_texture_render`, `main_video_save_directory`, `main_get_printer_names`, `main_support_remove_wallpaper`, `main_test_wallpaper`, `main_supported_privacy_mode_impls`, `main_default_privacy_mode_impl` |
| Input helpers | `main_get_input_source`, `main_set_input_source`, `main_init_input_source`, `main_supported_input_source`, `main_get_mouse_time`, `main_check_mouse_time`, `main_clip_cursor`, `main_set_cursor_position`, `main_has_file_clipboard` |
| macOS / Linux | `main_is_can_screen_recording`, `main_is_can_input_monitoring`, `main_is_process_trusted`, `main_current_is_wayland`, `main_is_login_wayland`, `main_handle_wayland_screencast_restore_token` |

### 4.7 Sessions (`wire_session_*`)
| Area | Functions |
|---|---|
| Lifecycle | `session_add_sync`, `session_add_existed_sync`, `session_start`, `session_start_with_displays`, `session_close`, `session_reconnect`, `session_login`, `session_send2fa`, `session_input_os_password`, `session_get_remember`, `session_is_multi_ui_session`, `session_send_selected_session_id`, `session_get_platform`, `session_request_new_display_init_msgs`, `session_on_waiting_for_image_dialog_show`, `session_get_conn_session_id`, `session_get_conn_token`, `session_get_enable_trusted_devices` |
| Options | `session_get_option`, `session_peer_option`, `session_get_peer_option`, `session_get_flutter_option`, `session_set_flutter_option`, `session_get_toggle_option`, `session_get_toggle_option_sync`, `session_toggle_option`, `session_get_common`, `session_get_common_sync`, `session_set_common` |
| Display / quality | `session_get_view_style`, `session_set_view_style`, `session_get_scroll_style`, `session_set_scroll_style`, `session_get_edge_scroll_edge_thickness`, `session_set_edge_scroll_edge_thickness`, `session_get_image_quality`, `session_set_image_quality`, `session_get_custom_image_quality`, `session_set_custom_image_quality`, `session_set_custom_fps`, `session_change_prefer_codec`, `session_alternative_codecs`, `session_switch_display`, `session_change_resolution`, `session_set_size`, `session_refresh`, `session_get_displays_as_individual_windows`, `session_set_displays_as_individual_windows`, `session_get_use_all_my_displays_for_the_remote_session`, `session_set_use_all_my_displays_for_the_remote_session`, `session_toggle_virtual_display`, `session_get_rgba_size`, `session_next_rgba`, `session_register_pixelbuffer_texture`, `session_register_gpu_texture` |
| Input | `session_send_mouse`, `session_send_pointer`, `session_input_key`, `session_input_string`, `session_handle_flutter_key_event`, `session_handle_flutter_raw_key_event`, `session_enter_or_leave`, `session_get_keyboard_mode`, `session_set_keyboard_mode`, `session_is_keyboard_mode_supported`, `session_get_reverse_mouse_wheel_sync`, `session_set_reverse_mouse_wheel`, `session_get_trackpad_speed`, `session_set_trackpad_speed`, `session_ctrl_alt_del`, `session_lock_screen` |
| Control actions | `session_restart_remote_device`, `session_toggle_privacy_mode`, `session_elevate_direct`, `session_elevate_with_logon`, `session_switch_sides`, `session_record_screen`, `session_get_is_recording`, `session_take_screenshot`, `session_handle_screenshot` |
| Chat / voice / audit | `session_send_chat`, `session_request_voice_call`, `session_close_voice_call`, `session_send_note`, `session_get_last_audit_note`, `session_get_audit_guid`, `session_set_audit_guid`, `session_get_audit_server_sync` |
| Files | see [09](09-clipboard-file-transfer.md) §3.4 |
| Port forward / RDP | `session_add_port_forward`, `session_remove_port_forward`, `session_new_rdp` |
| Terminal | `session_open_terminal`, `session_send_terminal_input`, `session_resize_terminal`, `session_close_terminal` |
| Printer | `session_printer_response` |

### 4.8 Connection Manager (`wire_cm_*`)
See [11](11-host-server-side.md) §4.2.

### 4.9 Plugins (`wire_plugin_*`)
`plugin_register_event_stream`, `plugin_event`, `plugin_list_reload`, `plugin_reload`, `plugin_install`, `plugin_enable`, `plugin_is_enabled`, `plugin_feature_is_enabled`, `plugin_get_session_option`, `plugin_set_session_option`, `plugin_get_shared_option`, `plugin_set_shared_option`, `plugin_sync_ui`.

## 5. Localization [O]
- Translations are **compiled into the Rust DLL** (many languages appear in its strings: Svenska, Shqip, Srpski, Hrvatski, Suomi…). Dart asks with `wire_translate(key, locale)` and `wire_main_get_langs`.
- Translation keys are kebab-case English IDs (`enable-2fa-title`, `relay-hint`, `insecure-connection`…), with English as the fallback.

## 6. Plugin system [O] API, [R] design
- Plugins are native libraries installed into the app data dir, listed from a server-side catalogue (`plugin_list_reload`), and enabled per plugin.
- A plugin describes its UI declaratively (`desc_ui.dart`: settings fields, toolbar buttons). Dart renders the description and sends events back (`plugin_event`).
- Peer↔peer plugin traffic uses `Misc.plugin_request` / `plugin_failure` ([10](10-extra-session-types.md) §11).
- Feature gate: `wire_plugin_feature_is_enabled`. Some builds ship with plugins disabled.

## 7. Clone notes [D]
- Keep the split: a **thin UI** that only renders and forwards intents, and a **core** that owns all state. Expose the core as a small async API (about 50 calls is enough for an MVP), plus one event stream per window.
- If you use Flutter: `flutter_rust_bridge` v2 with `StreamSink` for events, and an external texture for video.
- If you use Slint/egui: run the core on tokio, talk to the UI through `async_channel`, and upload video frames into a `wgpu` texture from the decoder thread.
