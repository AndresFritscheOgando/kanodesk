//! `kanodesk --cm`: the connection-manager window (11 §4).
//!
//! Connects to the host process over IPC (and reconnects if it restarts),
//! lists the incoming sessions and lets the user:
//!
//! - **Accept** or **Deny** a waiting session, choosing its keyboard and
//!   mouse permissions first. Accept stays disabled for the host's
//!   anti-spoof delay, so a remote viewer cannot click it the moment the
//!   request appears;
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

use crate::cm_model::{Attention, CmModel, accept_enabled};

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
            .with_min_inner_size([360.0, 300.0]),
        ..Default::default()
    };
    let ipc_model = model.clone();
    eframe::run_native(
        "KanoDesk connection manager",
        options,
        Box::new(move |cc| {
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
        ui.checkbox(&mut p.keyboard, "Keyboard");
        ui.checkbox(&mut p.mouse, "Mouse");
    });
    (p != current).then_some(p)
}

impl CmApp {
    fn header(ui: &mut egui::Ui, model: &CmModel) {
        if !model.connected {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Not connected to the KanoDesk host. Retrying…",
            );
            return;
        }
        if let Some(s) = &model.status {
            ui.label(format!("This computer: {}", short_key(&s.device_key)));
            match s.approve_mode.as_str() {
                "click" => ui.label("Viewers connect when you accept them."),
                "both" => ui.label("Viewers need the password and your acceptance."),
                _ => ui.label("Viewers connect with the password."),
            };
            if let Some(pw) = &s.temporary_password {
                ui.horizontal(|ui| {
                    ui.label("One-time password:");
                    ui.monospace(pw);
                });
            }
        }
    }
}

impl eframe::App for CmApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        let now_ms = unix_ms(SystemTime::now());
        let mut actions: Vec<CmToHost> = Vec::new();
        let mut choices: Vec<(u64, Perms)> = Vec::new();
        let done = {
            let mut model = self.model.lock().expect("not poisoned");
            model.prune(now);
            egui::CentralPanel::default().show(ui, |ui| {
                Self::header(ui, &model);
                ui.separator();
                if model.entries().next().is_none() {
                    ui.label("No one is connected.");
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for e in model.entries() {
                        let info = &e.info;
                        ui.group(|ui| {
                            ui.set_width(ui.available_width());
                            ui.strong(describe(info));
                            ui.small(format!(
                                "from {} · device {} · {}",
                                info.address,
                                short_key(&info.device_key),
                                info.agent
                            ));
                            match &info.state {
                                SessionState::AwaitingApproval {
                                    accept_after_unix_ms,
                                    expires_unix_ms,
                                } => {
                                    ui.label(match &info.password {
                                        Some(p) => format!(
                                            "wants to control this computer (used the {p} password)"
                                        ),
                                        None => "wants to control this computer".to_owned(),
                                    });
                                    if let Some(p) = permission_boxes(ui, e.choice) {
                                        choices.push((info.id, p));
                                    }
                                    ui.horizontal(|ui| {
                                        let enabled = accept_enabled(info, now_ms);
                                        let wait = accept_after_unix_ms.saturating_sub(now_ms);
                                        let label = if enabled {
                                            "Accept".to_owned()
                                        } else {
                                            format!("Accept ({})", wait.div_ceil(1000))
                                        };
                                        if ui
                                            .add_enabled(enabled, egui::Button::new(label))
                                            .clicked()
                                            && let Some(a) = model.accept(info.id, now_ms)
                                        {
                                            actions.push(a);
                                        }
                                        if ui.button("Deny").clicked()
                                            && let Some(a) = model.deny(info.id)
                                        {
                                            actions.push(a);
                                        }
                                        let left = expires_unix_ms.saturating_sub(now_ms) / 1000;
                                        ui.small(format!("{left} s left"));
                                    });
                                }
                                SessionState::Active => {
                                    ui.label(format!(
                                        "connected {} · password: {}",
                                        elapsed(info.started_unix_ms, now_ms),
                                        info.password.as_deref().unwrap_or("none (accepted)")
                                    ));
                                    if let Some(p) = permission_boxes(ui, info.permissions)
                                        && let Some(a) = model.set_permissions(info.id, p)
                                    {
                                        actions.push(a);
                                    }
                                    if ui.button("Disconnect").clicked()
                                        && let Some(a) = model.disconnect(info.id)
                                    {
                                        actions.push(a);
                                    }
                                }
                                SessionState::Ended { reason } => {
                                    ui.weak(format!("Ended: {reason}"));
                                }
                            }
                        });
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
    }
}
