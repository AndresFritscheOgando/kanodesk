//! `kanodesk --cm`: the connection-manager window (11 §4).
//!
//! Connects to the host process over IPC (and reconnects if it restarts),
//! lists the incoming sessions and lets the user:
//!
//! - **Accept** or **Deny** a waiting session, choosing its keyboard and
//!   mouse permissions first. Accept stays disabled for the host's
//!   anti-spoof delay, so a remote viewer cannot click it the moment the
//!   request appears, and it has no keyboard shortcut. A bar shows how
//!   long the request waits for an answer. Requests are listed first and
//!   stand out with an accent border;
//! - toggle an active session's permissions live;
//! - **Disconnect** a session (the host closes it and its video at once).
//!
//! The window comes to the front for each new request and closes itself a
//! few seconds after the last session has ended.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui;
use kanodesk_ipc::cm::{
    CM_PROTOCOL, CM_ROLE, CmToHost, HostToCm, Perms, SessionInfo, SessionState, unix_ms,
};
use tokio::sync::mpsc;

use egui::{CornerRadius, Frame, Margin, RichText, Stroke};

use crate::cm_model::{Attention, CmModel, Entry, accept_enabled, expiry_fraction};
use crate::main_model::Level;
use crate::theme::{CARD_RADIUS, palette};
use crate::widgets;

/// Close the window this long after the last session ended.
const CLOSE_AFTER: Duration = Duration::from_secs(5);

/// Runs the window until it is closed.
pub fn run() -> Result<(), String> {
    let model = Arc::new(Mutex::new(CmModel::default()));
    let (out, out_rx) = mpsc::unbounded_channel();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("KanoDesk: incoming connections")
            .with_inner_size([440.0, 560.0])
            .with_min_inner_size([360.0, 300.0])
            .with_icon(Arc::new(crate::icon::egui_icon())),
        ..Default::default()
    };
    let ipc_model = model.clone();
    eframe::run_native(
        "KanoDesk connection manager",
        options,
        Box::new(move |cc| {
            crate::theme::install(&cc.egui_ctx);
            spawn_ipc(ipc_model, out_rx, cc.egui_ctx.clone());
            Ok(Box::new(CmApp { model, out }))
        }),
    )
    .map_err(|e| e.to_string())
}

/// Talks to the host on a background thread; reconnects every second.
fn spawn_ipc(
    model: Arc<Mutex<CmModel>>,
    mut out_rx: mpsc::UnboundedReceiver<CmToHost>,
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
            let name = kanodesk_ipc::endpoint_name(CM_ROLE);
            loop {
                if let Ok(conn) = kanodesk_ipc::connect(&name).await {
                    let (mut tx, mut rx) = conn.split();
                    if tx
                        .send(&CmToHost::Hello {
                            protocol: CM_PROTOCOL,
                        })
                        .await
                        .is_ok()
                    {
                        loop {
                            tokio::select! {
                                msg = rx.recv::<HostToCm>() => match msg {
                                    Ok(Some(msg)) => {
                                        let attention = model
                                            .lock()
                                            .expect("not poisoned")
                                            .apply(msg, Instant::now());
                                        if attention == Attention::NewRequest {
                                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                                            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                                            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                                                egui::UserAttentionType::Critical,
                                            ));
                                        }
                                        ctx.request_repaint();
                                    }
                                    _ => break,
                                },
                                cmd = out_rx.recv() => match cmd {
                                    Some(cmd) => {
                                        if tx.send(&cmd).await.is_err() {
                                            break;
                                        }
                                    }
                                    None => return,
                                },
                            }
                        }
                    }
                    model
                        .lock()
                        .expect("not poisoned")
                        .disconnected(Instant::now());
                    ctx.request_repaint();
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    });
}

struct CmApp {
    model: Arc<Mutex<CmModel>>,
    out: mpsc::UnboundedSender<CmToHost>,
}

fn describe(info: &SessionInfo) -> String {
    let who = match (info.username.is_empty(), info.hostname.is_empty()) {
        (false, false) => format!("{} on {}", info.username, info.hostname),
        (true, false) => info.hostname.clone(),
        (false, true) => info.username.clone(),
        (true, true) => "Unknown computer".to_owned(),
    };
    if info.platform.is_empty() {
        who
    } else {
        format!("{who} ({})", info.platform)
    }
}

fn short_key(key: &str) -> String {
    let k: String = key.chars().take(16).collect();
    format!("{k}…")
}

fn elapsed(started_unix_ms: u64, now_unix_ms: u64) -> String {
    let secs = now_unix_ms.saturating_sub(started_unix_ms) / 1000;
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// The permission checkboxes; returns the new value if one was clicked.
fn permission_boxes(ui: &mut egui::Ui, current: Perms) -> Option<Perms> {
    let mut p = current;
    ui.horizontal(|ui| {
        ui.label("Allow:");
        ui.checkbox(&mut p.keyboard, "Keyboard");
        ui.checkbox(&mut p.mouse, "Mouse");
    });
    (p != current).then_some(p)
}

/// "with the temporary password", "accepted by you".
fn login_text(info: &SessionInfo) -> String {
    match &info.password {
        Some(p) => format!("with the {p} password"),
        None => "accepted by you".to_owned(),
    }
}

impl CmApp {
    fn header(ui: &mut egui::Ui, model: &CmModel) {
        if !model.connected {
            widgets::banner_text(
                ui,
                Level::Busy,
                "Not connected to the KanoDesk host. Retrying…",
            );
            return;
        }
        if let Some(s) = &model.status {
            let p = palette(ui);
            ui.label(match s.approve_mode.as_str() {
                "click" => "Viewers connect when you accept them.",
                "both" => "Viewers need the password and your acceptance.",
                _ => "Viewers connect with the password.",
            });
            if let Some(pw) = &s.temporary_password {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("One-time password").color(p.text_weak));
                    ui.label(RichText::new(pw).monospace().color(p.text_strong));
                    widgets::copy_button(ui, "password", pw);
                });
            }
        }
    }

    /// A request waiting for Accept or Deny.
    fn request(
        ui: &mut egui::Ui,
        model: &CmModel,
        e: &Entry,
        now_ms: u64,
        actions: &mut Vec<CmToHost>,
        choices: &mut Vec<(u64, Perms)>,
    ) {
        let p = palette(ui);
        let info = &e.info;
        let SessionState::AwaitingApproval {
            accept_after_unix_ms,
            expires_unix_ms,
        } = info.state
        else {
            return;
        };
        ui.label(
            RichText::new(format!("{} wants to control this computer", describe(info)))
                .strong()
                .color(p.text_strong),
        );
        let mut line = format!("from {}", info.address);
        if !info.agent.is_empty() {
            line.push_str(&format!(" · {}", info.agent));
        }
        if let Some(pw) = &info.password {
            line.push_str(&format!(" · used the {pw} password"));
        }
        ui.label(RichText::new(line).small().color(p.text_weak))
            .on_hover_text(format!(
                "Device fingerprint: {}",
                short_key(&info.device_key)
            ));
        if let Some(c) = permission_boxes(ui, e.choice) {
            choices.push((info.id, c));
        }
        ui.horizontal(|ui| {
            let enabled = accept_enabled(info, now_ms);
            let wait = accept_after_unix_ms.saturating_sub(now_ms);
            let label = if enabled {
                "Accept".to_owned()
            } else {
                format!("Accept ({})", wait.div_ceil(1000))
            };
            let accept = ui
                .add_enabled_ui(enabled, |ui| widgets::primary_button(ui, &label))
                .inner;
            if accept.clicked()
                && let Some(a) = model.accept(info.id, now_ms)
            {
                actions.push(a);
            }
            if widgets::danger_button(ui, "Deny").clicked()
                && let Some(a) = model.deny(info.id)
            {
                actions.push(a);
            }
        });
        if let Some(f) = expiry_fraction(info, now_ms) {
            let fill = if f < 0.25 { p.warn } else { p.accent_text };
            ui.add(egui::ProgressBar::new(f).desired_height(4.0).fill(fill));
            let left = expires_unix_ms.saturating_sub(now_ms).div_ceil(1000);
            ui.label(
                RichText::new(format!("Ends without an answer in {left} s"))
                    .small()
                    .color(p.text_weak),
            );
        }
    }

    /// A running session: its permissions and Disconnect.
    fn active(
        ui: &mut egui::Ui,
        model: &CmModel,
        e: &Entry,
        now_ms: u64,
        actions: &mut Vec<CmToHost>,
    ) {
        let p = palette(ui);
        let info = &e.info;
        ui.horizontal(|ui| {
            ui.label(RichText::new(describe(info)).strong().color(p.text_strong));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::danger_button(ui, "Disconnect").clicked()
                    && let Some(a) = model.disconnect(info.id)
                {
                    actions.push(a);
                }
            });
        });
        ui.label(
            RichText::new(format!(
                "Connected {} · {}",
                elapsed(info.started_unix_ms, now_ms),
                login_text(info)
            ))
            .small()
            .color(p.text_weak),
        )
        .on_hover_text(format!(
            "from {} · device fingerprint {}",
            info.address,
            short_key(&info.device_key)
        ));
        if let Some(perms) = permission_boxes(ui, info.permissions)
            && let Some(a) = model.set_permissions(info.id, perms)
        {
            actions.push(a);
        }
    }
}

impl eframe::App for CmApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let now_ms = unix_ms(SystemTime::now());
        let mut actions: Vec<CmToHost> = Vec::new();
        let mut choices: Vec<(u64, Perms)> = Vec::new();
        let p = palette(ui);
        let done = {
            let mut model = self.model.lock().expect("not poisoned");
            model.prune(now);
            egui::CentralPanel::default()
                .frame(Frame::new().fill(p.bg).inner_margin(Margin::same(16)))
                .show(ui, |ui| {
                    Self::header(ui, &model);
                    ui.add_space(8.0);
                    if model.entries().next().is_none() {
                        ui.label(RichText::new("No one is connected.").color(p.text_weak));
                    }
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for e in model.ordered() {
                                // A request stands out; an ended session fades.
                                let stroke = if e.awaiting() {
                                    Stroke::new(2.0, p.accent_text)
                                } else {
                                    Stroke::new(1.0, p.border)
                                };
                                Frame::new()
                                    .fill(p.surface)
                                    .stroke(stroke)
                                    .corner_radius(CornerRadius::same(CARD_RADIUS))
                                    .inner_margin(Margin::same(14))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        match &e.info.state {
                                            SessionState::AwaitingApproval { .. } => Self::request(
                                                ui,
                                                &model,
                                                e,
                                                now_ms,
                                                &mut actions,
                                                &mut choices,
                                            ),
                                            SessionState::Active => {
                                                Self::active(ui, &model, e, now_ms, &mut actions)
                                            }
                                            SessionState::Ended { reason } => {
                                                ui.multiply_opacity(0.6);
                                                ui.label(
                                                    RichText::new(describe(&e.info))
                                                        .color(p.text_strong),
                                                );
                                                ui.label(
                                                    RichText::new(format!("Ended: {reason}"))
                                                        .small()
                                                        .color(p.text_weak),
                                                );
                                            }
                                        }
                                    });
                                ui.add_space(10.0);
                            }
                        });
                });
            for (id, p) in choices {
                model.choose(id, p);
            }
            model.done(now, CLOSE_AFTER)
        };
        for a in actions {
            let _ = self.out.send(a);
        }
        if done {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Countdowns and the Accept delay need regular repaints.
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let mut info = SessionInfo {
            id: 1,
            device_key: "0123456789abcdef0123".into(),
            hostname: "LAPTOP".into(),
            username: "ana".into(),
            platform: "windows".into(),
            agent: String::new(),
            address: String::new(),
            password: None,
            started_unix_ms: 0,
            permissions: Perms {
                keyboard: true,
                mouse: true,
            },
            state: SessionState::Active,
        };
        assert_eq!(describe(&info), "ana on LAPTOP (windows)");
        info.username.clear();
        info.platform.clear();
        assert_eq!(describe(&info), "LAPTOP");
        info.hostname.clear();
        assert_eq!(describe(&info), "Unknown computer");
        assert_eq!(short_key("0123456789abcdef0123"), "0123456789abcdef…");
        assert_eq!(elapsed(0, 3_723_000), "01:02:03");
        info.password = Some("temporary".into());
        assert_eq!(login_text(&info), "with the temporary password");
        info.password = None;
        assert_eq!(login_text(&info), "accepted by you");
    }
}
