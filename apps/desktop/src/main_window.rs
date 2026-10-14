//! `kanodesk` (no arguments): the main window (15 §2, "Main", "Settings").
//!
//! - **This desk**: the device ID and the one-time password (copy, new
//!   password), how others get in, and whether the ID server is reachable
//!   (a coloured status; "Offline" stays while the host retries).
//!   They come from the host process over the control IPC; if no host runs
//!   in this Windows session, the window starts one (`kanodesk --server`,
//!   no console, log in `<data>\logs\host.log`) and stops it again on exit.
//! - **Control a remote desk**: an ID box (digits grouped as typed,
//!   Connect enabled at 9 digits) and the recent sessions; Connect opens a
//!   viewer window (`kanodesk --connect ID`, its own process).
//! - **Settings**: ID server address and key (checked as typed), approve
//!   mode, password length, default permissions, viewer defaults, saved
//!   with Save to the config file (the host re-reads it at once). Leaving
//!   the page or closing the window with unsaved changes asks first. The
//!   permanent password applies at once, and removing it asks first.
//!
//! On an OS that cannot host ([`crate::server::HOSTING_SUPPORTED`]) the
//! window is a viewer only: no host is started, "This desk" says so, and
//! Settings shows only what a viewer uses (ID server and key, viewer
//! defaults).
//!
//! All decisions and texts are in [`crate::main_model`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use kanodesk_client::peers::{self, RecentPeers};
use kanodesk_config::Config;
use kanodesk_config::options::{APPROVE_MODES, IMAGE_QUALITIES, VIEW_STYLES};
use kanodesk_ipc::cm::unix_ms;
use kanodesk_ipc::control::{CONTROL_PROTOCOL, CONTROL_ROLE, HostToUi, UiToHost};
use tokio::sync::{mpsc, oneshot};

use egui::RichText;

use crate::main_model::{
    HostLink, Level, LinkStatus, MainModel, Page, SettingsForm, VIEWER_ONLY_TEXT, ago,
    approve_text, format_id_input, id_progress, id_text, key_hint, link_status, parse_target,
    server_hint, viewer_link_status,
};
use crate::theme::{self, palette};
use crate::widgets;

/// How often the recent list is re-read (viewers add to it).
const RELOAD_RECENT: Duration = Duration::from_secs(2);
/// How long to wait for a host this window started before giving up.
const HOST_START: Duration = Duration::from_secs(20);

enum Outgoing {
    Command(UiToHost),
    /// Stop the host this window started, then confirm.
    StopHost(oneshot::Sender<()>),
}

/// Runs the window, opened on `start`, until it is closed.
pub fn run(start: Page) -> Result<(), String> {
    let hosting = crate::server::HOSTING_SUPPORTED;
    let mut model = if hosting {
        MainModel::default()
    } else {
        MainModel::viewer_only()
    };
    model.page = start;
    let model = Arc::new(Mutex::new(model));
    let (out, out_rx) = mpsc::unbounded_channel();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("KanoDesk")
            .with_inner_size([820.0, 560.0])
            .with_min_inner_size([640.0, 440.0])
            .with_icon(Arc::new(crate::icon::egui_icon())),
        ..Default::default()
    };
    let ipc_model = model.clone();
    eframe::run_native(
        "KanoDesk",
        options,
        Box::new(move |cc| {
            crate::theme::install(&cc.egui_ctx);
            // Without a host there is nobody to talk to; commands are dropped.
            if hosting {
                spawn_ipc(ipc_model, out_rx, cc.egui_ctx.clone());
            }
            Ok(Box::new(MainApp::new(model, out)))
        }),
    )
    .map_err(|e| e.to_string())
}

/// Starts `kanodesk --server` without a console, logging to a file.
fn launch_host() -> std::io::Result<()> {
    let mut args = vec!["--server".to_owned()];
    if let Some(dir) = crate::paths::log_dir() {
        std::fs::create_dir_all(&dir)?;
        args.push("--log".into());
        args.push(dir.join("host.log").to_string_lossy().into_owned());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    crate::paths::spawn_self(&refs)
}

/// Talks to the host on a background thread; starts the host if none runs.
fn spawn_ipc(
    model: Arc<Mutex<MainModel>>,
    mut out_rx: mpsc::UnboundedReceiver<Outgoing>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            let name = kanodesk_ipc::endpoint_name(CONTROL_ROLE);
            let mut launched_at: Option<Instant> = None;
            loop {
                match kanodesk_ipc::connect(&name).await {
                    Ok(conn) => {
                        let (mut tx, mut rx) = conn.split();
                        if tx
                            .send(&UiToHost::Hello {
                                protocol: CONTROL_PROTOCOL,
                            })
                            .await
                            .is_ok()
                        {
                            loop {
                                tokio::select! {
                                    msg = rx.recv::<HostToUi>() => match msg {
                                        Ok(Some(msg)) => {
                                            model.lock().expect("not poisoned").apply(msg, Instant::now());
                                            ctx.request_repaint();
                                        }
                                        _ => break,
                                    },
                                    cmd = out_rx.recv() => match cmd {
                                        Some(Outgoing::Command(c)) => {
                                            if tx.send(&c).await.is_err() {
                                                break;
                                            }
                                        }
                                        Some(Outgoing::StopHost(done)) => {
                                            let _ = tx.send(&UiToHost::Shutdown).await;
                                            let _ = done.send(());
                                            return;
                                        }
                                        None => return,
                                    },
                                }
                            }
                        }
                        model.lock().expect("not poisoned").disconnected();
                        ctx.request_repaint();
                    }
                    Err(_) => {
                        let started = launched_at.get_or_insert_with(|| {
                            let ok = launch_host();
                            let mut m = model.lock().expect("not poisoned");
                            match ok {
                                Ok(()) => m.launched_host = true,
                                Err(e) => {
                                    m.host = HostLink::Unavailable(format!(
                                        "could not start the KanoDesk host: {e}"
                                    ))
                                }
                            }
                            Instant::now()
                        });
                        if started.elapsed() > HOST_START {
                            let mut m = model.lock().expect("not poisoned");
                            if m.host == HostLink::Connecting {
                                m.host = HostLink::Unavailable(
                                    "the KanoDesk host did not start; see logs\\host.log".into(),
                                );
                            }
                            ctx.request_repaint();
                        }
                        // Commands sent while disconnected are dropped; a
                        // stop request still gets its answer.
                        while let Ok(cmd) = out_rx.try_recv() {
                            if let Outgoing::StopHost(done) = cmd {
                                let _ = done.send(());
                                return;
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    });
}

/// A question the window asks before going on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirm {
    /// Settings has unsaved changes and the user picked this page.
    Leave(Page),
    /// Settings has unsaved changes and the window is closing.
    Close,
    /// Remove the permanent password?
    RemovePassword,
}

struct MainApp {
    model: Arc<Mutex<MainModel>>,
    out: mpsc::UnboundedSender<Outgoing>,
    connect_text: String,
    connect_error: Option<String>,
    /// The connect error is about the ID server settings (offer Settings).
    connect_needs_settings: bool,
    recent: RecentPeers,
    recent_loaded: Instant,
    config: Config,
    form: SettingsForm,
    /// The form as loaded or last saved, to tell unsaved changes.
    saved_form: SettingsForm,
    form_problems: Vec<String>,
    password_problem: Option<String>,
    confirm: Option<Confirm>,
    /// The user chose to close despite unsaved changes (or saved them).
    close_confirmed: bool,
}

fn load_config() -> Config {
    crate::paths::config_path()
        .and_then(|p| Config::load(&p).ok())
        .unwrap_or_else(Config::in_memory)
}

fn load_recent() -> RecentPeers {
    crate::paths::peers_path()
        .map(|p| RecentPeers::load(&p))
        .unwrap_or_else(RecentPeers::in_memory)
}

/// Width of the settings labels, so every section lines up.
const LABEL_WIDTH: f32 = 180.0;
/// Settings stay readable on wide windows.
const SETTINGS_WIDTH: f32 = 680.0;

impl MainApp {
    fn new(model: Arc<Mutex<MainModel>>, out: mpsc::UnboundedSender<Outgoing>) -> MainApp {
        let config = load_config();
        let form = SettingsForm::from_config(&config);
        MainApp {
            model,
            out,
            connect_text: String::new(),
            connect_error: None,
            connect_needs_settings: false,
            recent: load_recent(),
            recent_loaded: Instant::now(),
            saved_form: form.clone(),
            form,
            config,
            form_problems: Vec::new(),
            password_problem: None,
            confirm: None,
            close_confirmed: false,
        }
    }

    fn send(&self, c: UiToHost) {
        let _ = self.out.send(Outgoing::Command(c));
    }

    /// Whether Settings holds changes that are not saved.
    fn dirty(&self) -> bool {
        self.form.differs_from(&self.saved_form)
    }

    /// Reloads the settings form from the config file.
    fn reload_form(&mut self) {
        self.config = load_config();
        self.form = SettingsForm::from_config(&self.config);
        self.saved_form = self.form.clone();
        self.form_problems.clear();
    }

    /// Opens a viewer for `id` (checked by the caller).
    fn open_viewer(&mut self, id: &str) {
        self.config = load_config();
        if self.config.text("custom-rendezvous-server").is_empty()
            || self.config.text("key").is_empty()
        {
            self.connect_error =
                Some("Set the ID server and its key in Settings first.".to_owned());
            self.connect_needs_settings = true;
            return;
        }
        self.connect_needs_settings = false;
        match crate::paths::spawn_self(&["--connect", id]) {
            Ok(()) => self.connect_error = None,
            Err(e) => self.connect_error = Some(format!("Could not open the viewer: {e}")),
        }
    }

    /// The "This desk" card.
    fn this_desk(&mut self, ui: &mut egui::Ui, model: &MainModel, page: &mut Page) {
        let p = palette(ui);
        let status = match &model.host {
            HostLink::Connected(o) => link_status(o),
            HostLink::ViewerOnly => viewer_link_status(&self.config),
            HostLink::Unavailable(_) => LinkStatus {
                level: Level::Problem,
                short: "Not running",
                text: String::new(),
                detail: None,
                settings: false,
            },
            HostLink::Connecting => LinkStatus {
                level: Level::Busy,
                short: "Starting…",
                text: String::new(),
                detail: None,
                settings: false,
            },
        };
        ui.horizontal(|ui| {
            widgets::section_title(ui, "This desk");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let dot = widgets::status_dot(ui, status.level, status.short);
                if let Some(d) = &status.detail {
                    dot.on_hover_text(d);
                }
            });
        });
        ui.add_space(4.0);
        let Some(o) = model.overview() else {
            match &model.host {
                HostLink::Unavailable(e) => {
                    ui.label(RichText::new(e).color(p.danger_text));
                }
                HostLink::ViewerOnly => {
                    ui.label(VIEWER_ONLY_TEXT);
                    ui.add_space(8.0);
                    self.status_line(ui, &status, page);
                }
                _ => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Starting the KanoDesk host…");
                    });
                }
            }
            return;
        };

        widgets::field_label(ui, "Your ID");
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(id_text(o))
                    .text_style(theme::display())
                    .color(p.text_strong),
            );
            if let Some(id) = &o.id {
                widgets::copy_button(ui, "id", id).on_hover_text("Copy the ID");
            }
        });
        self.status_line(ui, &status, page);
        ui.add_space(10.0);

        // Click-only hosts have no password: the approve text says how others get in.
        if let Some(pw) = &o.status.temporary_password {
            widgets::field_label(ui, "One-time password");
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(pw)
                        .text_style(theme::display_small())
                        .color(p.text_strong),
                );
                widgets::copy_button(ui, "password", pw).on_hover_text("Copy the password");
                if widgets::quiet_button(ui, RichText::new("New password").color(p.accent_text))
                    .on_hover_text("Replace the one-time password now")
                    .clicked()
                {
                    self.send(UiToHost::RefreshPassword);
                }
            });
            ui.label(
                RichText::new("It works for one login, then a new one appears.").color(p.text_weak),
            );
            ui.add_space(10.0);
        }
        ui.label(approve_text(&o.status.approve_mode));
        if o.status.has_permanent_password {
            ui.label(RichText::new("A permanent password is set too.").color(p.text_weak));
        }
        if o.active_sessions > 0 {
            ui.add_space(6.0);
            widgets::banner_text(
                ui,
                Level::Busy,
                &format!(
                    "{} remote session{} connected to this desk",
                    o.active_sessions,
                    if o.active_sessions == 1 { "" } else { "s" }
                ),
            );
        }
    }

    /// The ID-server sentence, with Open Settings when that would help.
    fn status_line(&self, ui: &mut egui::Ui, status: &LinkStatus, page: &mut Page) {
        if status.text.is_empty() {
            return;
        }
        let p = palette(ui);
        let color = if status.level == Level::Problem {
            p.warn
        } else {
            p.text_weak
        };
        let line = ui.add(egui::Label::new(RichText::new(&status.text).color(color)).wrap());
        if let Some(d) = &status.detail {
            line.on_hover_text(d);
        }
        if status.settings
            && widgets::quiet_button(ui, RichText::new("Open Settings").color(p.accent_text))
                .clicked()
        {
            *page = Page::Settings;
        }
    }

    /// The "Control a remote desk" card.
    fn remote_desk(&mut self, ui: &mut egui::Ui, page: &mut Page) {
        let p = palette(ui);
        widgets::section_title(ui, "Control a remote desk");
        ui.add_space(4.0);
        widgets::field_label(ui, "Remote ID");
        let mut go = false;
        let valid = parse_target(&self.connect_text).is_ok();
        ui.horizontal(|ui| {
            let button = 96.0;
            let edit = ui.add(
                egui::TextEdit::singleline(&mut self.connect_text)
                    .hint_text("123 456 789")
                    .font(egui::FontId::monospace(20.0))
                    .margin(egui::vec2(8.0, 6.0))
                    .desired_width(ui.available_width() - button - 8.0),
            );
            if edit.changed() {
                self.connect_text = format_id_input(&self.connect_text);
                self.connect_error = None;
                // Reformatting may move the text under the cursor: keep it at the end.
                if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), edit.id) {
                    let end = egui::text::CCursor::new(self.connect_text.chars().count());
                    state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::one(end)));
                    state.store(ui.ctx(), edit.id);
                }
            }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && valid {
                go = true;
            }
            let connect = ui
                .add_enabled_ui(valid, |ui| {
                    ui.add_sized(
                        [button, 34.0],
                        egui::Button::new(RichText::new("Connect").color(p.on_accent))
                            .fill(p.accent),
                    )
                })
                .inner;
            if connect.clicked() {
                go = true;
            }
        });
        if go {
            match parse_target(&self.connect_text) {
                Ok(id) => self.open_viewer(&id.to_string()),
                Err(e) => self.connect_error = Some(e),
            }
        }
        if let Some(e) = &self.connect_error {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(e).color(p.danger_text));
                if self.connect_needs_settings
                    && widgets::quiet_button(
                        ui,
                        RichText::new("Open Settings").color(p.accent_text),
                    )
                    .clicked()
                {
                    *page = Page::Settings;
                }
            });
        } else if let Some(progress) = id_progress(&self.connect_text) {
            ui.label(RichText::new(progress).color(p.text_weak));
        }

        ui.add_space(14.0);
        widgets::field_label(ui, "Recent");
        if self.recent.peers().is_empty() {
            ui.label(RichText::new("Devices you connect to appear here.").color(p.text_weak));
            return;
        }
        let now = unix_ms(SystemTime::now());
        let mut open = None;
        let mut forget = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for peer in self.recent.peers() {
                    let row = ui.id().with(("recent", &peer.id));
                    let hovered: bool = ui.data(|d| d.get_temp(row).unwrap_or(false));
                    let response = egui::Frame::new()
                        .fill(if hovered {
                            p.button
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .corner_radius(egui::CornerRadius::same(theme::WIDGET_RADIUS))
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.label(
                                        RichText::new(peers::label(peer)).color(p.text_strong),
                                    );
                                    let mut line = format!(
                                        "{} · {}",
                                        peers::group_id(&peer.id),
                                        ago(peer.last_unix_ms, now)
                                    );
                                    if !peer.path.is_empty() {
                                        line.push_str(&format!(" · {}", peer.path));
                                    }
                                    ui.label(RichText::new(line).small().color(p.text_weak));
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let remove = widgets::quiet_button(
                                            ui,
                                            RichText::new("✕").color(p.text_weak),
                                        )
                                        .on_hover_text("Remove from the list");
                                        if widgets::name_button(remove, "Remove from the list")
                                            .clicked()
                                        {
                                            forget = Some(peer.id.clone());
                                        }
                                        if ui.button("Connect").clicked() {
                                            open = Some(peer.id.clone());
                                        }
                                    },
                                );
                            });
                        })
                        .response;
                    let now_hovered = response.contains_pointer();
                    if now_hovered != hovered {
                        ui.data_mut(|d| d.insert_temp(row, now_hovered));
                        ui.ctx().request_repaint();
                    }
                }
            });
        if let Some(id) = open {
            self.open_viewer(&id);
        }
        if let Some(id) = forget {
            self.recent.remove(&id);
            let _ = self.recent.save();
        }
    }

    /// A settings row: the label (with a lock when an administrator fixed
    /// the value), then the field.
    fn row_label(ui: &mut egui::Ui, text: &str, locked: bool) {
        if locked {
            ui.label(format!("{text} 🔒"))
                .on_hover_text("Set by your administrator");
        } else {
            ui.label(text);
        }
    }

    /// A hint under a settings field, in its level's colour.
    fn hint(ui: &mut egui::Ui, hint: Option<(Level, String)>) {
        if let Some((level, text)) = hint {
            let color = widgets::level_color(ui, level);
            ui.label(RichText::new(text).small().color(color));
        }
    }

    fn grid(id: &str) -> egui::Grid {
        egui::Grid::new(id)
            .num_columns(2)
            .min_col_width(LABEL_WIDTH)
            .spacing([16.0, 12.0])
    }

    fn settings(&mut self, ui: &mut egui::Ui, viewer_only: bool, has_permanent: bool) {
        let config = self.config.clone();
        let locked = |key: &str| SettingsForm::locked(&config, key);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(SETTINGS_WIDTH);
                widgets::card(ui, |ui| {
                    widgets::section_title(ui, "Network");
                    ui.add_space(4.0);
                    Self::grid("network").show(ui, |ui| {
                        let l = locked("custom-rendezvous-server");
                        Self::row_label(ui, "ID server", l);
                        ui.vertical(|ui| {
                            ui.add_enabled(
                                !l,
                                egui::TextEdit::singleline(&mut self.form.id_server)
                                    .hint_text("host or host:port")
                                    .margin(theme::FIELD_MARGIN)
                                    .desired_width(f32::INFINITY),
                            );
                            Self::hint(ui, server_hint(&self.form.id_server));
                        });
                        ui.end_row();
                        let l = locked("key");
                        Self::row_label(ui, "Server key", l);
                        ui.vertical(|ui| {
                            ui.add_enabled(
                                !l,
                                egui::TextEdit::multiline(&mut self.form.server_key)
                                    .hint_text("64 hex characters, printed by kanodesk-idserver")
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY)
                                    .font(egui::TextStyle::Monospace),
                            );
                            Self::hint(ui, key_hint(&self.form.server_key));
                        });
                        ui.end_row();
                    });
                });
                if !viewer_only {
                    ui.add_space(12.0);
                    self.security_settings(ui, &locked);
                    ui.add_space(12.0);
                    self.permanent_password(ui, has_permanent);
                }
                ui.add_space(12.0);
                widgets::card(ui, |ui| {
                    widgets::section_title(ui, "Viewer");
                    ui.add_space(4.0);
                    Self::grid("viewer").show(ui, |ui| {
                        ui.label("Remote screen");
                        egui::ComboBox::from_id_salt("view")
                            .selected_text(view_label(&self.form.view_style))
                            .show_ui(ui, |ui| {
                                for v in VIEW_STYLES {
                                    ui.selectable_value(
                                        &mut self.form.view_style,
                                        (*v).to_owned(),
                                        view_label(v),
                                    );
                                }
                            });
                        ui.end_row();
                        ui.label("Image quality");
                        egui::ComboBox::from_id_salt("quality")
                            .selected_text(quality_label(&self.form.image_quality))
                            .show_ui(ui, |ui| {
                                for q in IMAGE_QUALITIES {
                                    ui.selectable_value(
                                        &mut self.form.image_quality,
                                        (*q).to_owned(),
                                        quality_label(q),
                                    );
                                }
                            });
                        ui.end_row();
                    });
                });
                ui.add_space(12.0);
            });
    }

    /// The settings only a host uses: approve mode, password length,
    /// default permissions.
    fn security_settings(&mut self, ui: &mut egui::Ui, locked: &dyn Fn(&str) -> bool) {
        widgets::card(ui, |ui| {
            widgets::section_title(ui, "Security");
            ui.add_space(4.0);
            Self::grid("security").show(ui, |ui| {
                let l = locked("approve-mode");
                Self::row_label(ui, "Incoming connections", l);
                ui.add_enabled_ui(!l, |ui| {
                    egui::ComboBox::from_id_salt("approve")
                        .width(240.0)
                        .selected_text(approve_label(&self.form.approve_mode))
                        .show_ui(ui, |ui| {
                            for m in APPROVE_MODES {
                                ui.selectable_value(
                                    &mut self.form.approve_mode,
                                    (*m).to_owned(),
                                    approve_label(m),
                                );
                            }
                        });
                });
                ui.end_row();
                let l = locked("temporary-password-length");
                Self::row_label(ui, "One-time password length", l);
                ui.add_enabled(
                    !l,
                    egui::Slider::new(&mut self.form.password_length, 6..=32).suffix(" characters"),
                );
                ui.end_row();
                Self::row_label(
                    ui,
                    "Remote users may use",
                    locked("enable-keyboard") && locked("enable-mouse"),
                );
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        !locked("enable-keyboard"),
                        egui::Checkbox::new(&mut self.form.keyboard, "Keyboard"),
                    );
                    ui.add_enabled(
                        !locked("enable-mouse"),
                        egui::Checkbox::new(&mut self.form.mouse, "Mouse"),
                    );
                });
                ui.end_row();
            });
        });
    }

    /// The permanent password: applies at once, not with Save.
    fn permanent_password(&mut self, ui: &mut egui::Ui, has_permanent: bool) {
        widgets::card(ui, |ui| {
            let p = palette(ui);
            ui.horizontal(|ui| {
                widgets::section_title(ui, "Permanent password");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if has_permanent {
                        widgets::status_dot(ui, Level::Ok, "Set");
                    } else {
                        widgets::status_dot(ui, Level::Idle, "Not set");
                    }
                });
            });
            ui.label(
                RichText::new(
                    "Lets you in without the one-time password (for your own devices). \
                     Applies right away; it does not wait for Save.",
                )
                .color(p.text_weak),
            );
            ui.add_space(4.0);
            let mut submit = false;
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.form.new_password)
                        .password(true)
                        .hint_text("new password")
                        .margin(theme::FIELD_MARGIN)
                        .desired_width(170.0),
                );
                let again = ui.add(
                    egui::TextEdit::singleline(&mut self.form.confirm_password)
                        .password(true)
                        .hint_text("again")
                        .margin(theme::FIELD_MARGIN)
                        .desired_width(170.0),
                );
                if again.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                let label = if has_permanent {
                    "Change password"
                } else {
                    "Set password"
                };
                if widgets::primary_button(ui, label).clicked() {
                    submit = true;
                }
            });
            if submit {
                match self.form.permanent_password_request() {
                    Ok(req) => {
                        self.send(req);
                        self.form.new_password.clear();
                        self.form.confirm_password.clear();
                        self.password_problem = None;
                    }
                    Err(e) => self.password_problem = Some(e),
                }
            }
            if let Some(problem) = &self.password_problem {
                ui.label(RichText::new(problem).color(p.danger_text));
            }
            ui.add_space(4.0);
            let remove = ui
                .add_enabled_ui(has_permanent, |ui| {
                    widgets::danger_button(ui, "Remove permanent password")
                })
                .inner;
            if remove.clicked() {
                self.confirm = Some(Confirm::RemovePassword);
            }
        });
    }

    /// Saves the form; true when it was saved.
    fn save_settings(&mut self) -> bool {
        let mut config = load_config();
        match self.form.apply(&mut config) {
            Ok(()) => match config.save() {
                Ok(()) => {
                    self.form_problems.clear();
                    self.config = config;
                    self.saved_form = self.form.clone();
                    let mut model = self.model.lock().expect("not poisoned");
                    if model.is_viewer_only() {
                        model.note(true, "Settings saved.", Instant::now());
                    } else {
                        drop(model);
                        self.send(UiToHost::ReloadConfig);
                    }
                    true
                }
                Err(e) => {
                    self.form_problems = vec![format!("Could not save: {e}")];
                    false
                }
            },
            Err(problems) => {
                self.form_problems = problems;
                false
            }
        }
    }

    /// The bar under Settings: unsaved changes, problems, Revert and Save.
    fn settings_footer(&mut self, ui: &mut egui::Ui) {
        let p = palette(ui);
        let dirty = self.dirty();
        egui::Panel::bottom("settings_footer")
            .frame(
                egui::Frame::new()
                    .fill(p.surface)
                    .stroke(egui::Stroke::new(1.0, p.border))
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| {
                for problem in &self.form_problems {
                    ui.label(RichText::new(problem).color(p.danger_text));
                }
                ui.horizontal(|ui| {
                    if dirty {
                        ui.label(RichText::new("Unsaved changes").color(p.warn));
                    } else {
                        ui.label(RichText::new("All changes saved").color(p.text_weak));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let save = ui
                            .add_enabled_ui(dirty, |ui| widgets::primary_button(ui, "Save"))
                            .inner;
                        if save.clicked() {
                            self.save_settings();
                        }
                        if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                            self.reload_form();
                        }
                    });
                });
            });
    }

    /// The open question, if any; returns the page to switch to once
    /// answered.
    fn confirm_dialog(&mut self, ctx: &egui::Context) -> Option<Page> {
        let confirm = self.confirm?;
        let mut next_page = None;
        let mut answered = false;
        match confirm {
            Confirm::Leave(_) | Confirm::Close => {
                egui::Modal::new(egui::Id::new("unsaved")).show(ctx, |ui| {
                    ui.set_width(380.0);
                    widgets::section_title(ui, "Save your changes?");
                    ui.label("Some settings are changed but not saved yet.");
                    ui.add_space(4.0);
                    let mut proceed = false;
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            answered = true;
                        }
                        if ui.button("Discard").clicked() {
                            self.reload_form();
                            proceed = true;
                        }
                        if widgets::primary_button(ui, "Save").clicked() && self.save_settings() {
                            proceed = true;
                        }
                    });
                    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                        answered = true;
                    }
                    if proceed {
                        answered = true;
                        match confirm {
                            Confirm::Leave(page) => next_page = Some(page),
                            _ => {
                                self.close_confirmed = true;
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                    }
                });
            }
            Confirm::RemovePassword => {
                egui::Modal::new(egui::Id::new("remove_password")).show(ctx, |ui| {
                    ui.set_width(380.0);
                    widgets::section_title(ui, "Remove the permanent password?");
                    ui.label(
                        "Only the one-time password (or your acceptance) will let people in then.",
                    );
                    ui.add_space(4.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            answered = true;
                        }
                        if widgets::danger_button(ui, "Remove").clicked() {
                            self.send(UiToHost::SetPermanentPassword(None));
                            answered = true;
                        }
                    });
                    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                        answered = true;
                    }
                });
            }
        }
        if answered {
            self.confirm = None;
        }
        next_page
    }
}

fn approve_label(mode: &str) -> &'static str {
    match mode {
        "click" => "Accept each one (no password)",
        "both" => "Password and accept",
        _ => "Password",
    }
}

/// Viewer option labels (shared with the viewer's toolbar).
pub fn view_label(style: &str) -> &'static str {
    match style {
        "original" => "Original size",
        _ => "Fit to window",
    }
}

/// Quality option labels (shared with the viewer's toolbar).
pub fn quality_label(q: &str) -> &'static str {
    match q {
        "best" => "Best",
        "low" => "Low bandwidth",
        _ => "Balanced",
    }
}

impl eframe::App for MainApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        if now.duration_since(self.recent_loaded) >= RELOAD_RECENT {
            self.recent = load_recent();
            self.recent_loaded = now;
        }
        let model = self.model.lock().expect("not poisoned");
        let page = model.page;
        let reply = model.current_reply(now).map(|(ok, m)| (ok, m.to_owned()));
        let snapshot = MainModel {
            host: model.host.clone(),
            launched_host: model.launched_host,
            reply: None,
            page,
        };
        drop(model);
        let viewer_only = snapshot.is_viewer_only();
        let has_permanent = snapshot
            .overview()
            .is_some_and(|o| o.status.has_permanent_password);
        let p = palette(ui);

        // Closing with unsaved settings asks first.
        if ui.ctx().input(|i| i.viewport().close_requested())
            && !self.close_confirmed
            && self.dirty()
        {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm = Some(Confirm::Close);
        }

        let mut new_page = page;
        egui::Panel::top("tabs")
            .frame(
                egui::Frame::new()
                    .fill(p.surface)
                    .stroke(egui::Stroke::new(1.0, p.border))
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("KanoDesk")
                            .size(16.0)
                            .strong()
                            .color(p.text_strong),
                    );
                    ui.add_space(8.0);
                    ui.selectable_value(&mut new_page, Page::Home, "Home");
                    ui.selectable_value(&mut new_page, Page::Settings, "Settings");
                    let version = match snapshot.overview() {
                        Some(o) => Some(o.version.clone()),
                        None if viewer_only => Some(crate::VERSION.to_owned()),
                        None => None,
                    };
                    if let Some(v) = version {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(format!("v{v}")).small().color(p.text_weak));
                        });
                    }
                });
            });
        if let Some((ok, message)) = reply {
            egui::Panel::bottom("reply")
                .frame(
                    egui::Frame::new()
                        .fill(p.bg)
                        .inner_margin(egui::Margin::symmetric(16, 8)),
                )
                .show(ui, |ui| {
                    widgets::banner_text(ui, if ok { Level::Ok } else { Level::Problem }, &message);
                });
        }
        if page == Page::Settings {
            self.settings_footer(ui);
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ui, |ui| match page {
                Page::Home => {
                    ui.columns(2, |cols| {
                        widgets::card(&mut cols[0], |ui| {
                            ui.set_min_height(ui.available_height());
                            self.this_desk(ui, &snapshot, &mut new_page);
                        });
                        widgets::card(&mut cols[1], |ui| {
                            ui.set_min_height(ui.available_height());
                            self.remote_desk(ui, &mut new_page);
                        });
                    });
                }
                Page::Settings => self.settings(ui, viewer_only, has_permanent),
            });
        if let Some(target) = self.confirm_dialog(ui.ctx()) {
            self.switch_page(target);
        } else if new_page != page {
            if page == Page::Settings && self.dirty() {
                self.confirm = Some(Confirm::Leave(new_page));
            } else {
                self.switch_page(new_page);
            }
        }
        // The overview and the recent list change on their own.
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if !self.model.lock().expect("not poisoned").launched_host {
            return;
        }
        // Stop the host this window started (and wait briefly for it).
        let (done, wait) = oneshot::channel();
        if self.out.send(Outgoing::StopHost(done)).is_ok() {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut wait = wait;
            while Instant::now() < deadline {
                match wait.try_recv() {
                    Err(oneshot::error::TryRecvError::Empty) => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    _ => break,
                }
            }
        }
    }
}

impl MainApp {
    /// Shows `page`; entering Settings reloads the form unless it holds
    /// unsaved changes.
    fn switch_page(&mut self, page: Page) {
        if page == Page::Settings && !self.dirty() {
            self.reload_form();
        }
        self.model.lock().expect("not poisoned").page = page;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(approve_label("click"), "Accept each one (no password)");
        assert_eq!(approve_label("password"), "Password");
        assert_eq!(view_label("original"), "Original size");
        assert_eq!(view_label("fit"), "Fit to window");
        assert_eq!(quality_label("low"), "Low bandwidth");
        assert_eq!(quality_label("balanced"), "Balanced");
    }
}
