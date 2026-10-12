//! The main window's state, independent of the window (15 §2, "Main" and
//! "Settings"): what the host process reports, the connect box, the
//! settings form and its validation, and the texts the window shows.

use std::time::{Duration, Instant};

use kanodesk_config::Config;
use kanodesk_crypto::DevicePublicKey;
use kanodesk_ipc::control::{HostOverview, HostToUi, IdServerState, UiToHost};
use kanodesk_rendezvous::DeviceId;

/// Shortest permanent password the host accepts.
pub const MIN_PERMANENT: usize = 8;

/// How long a reply from the host stays on screen.
pub const REPLY_SHOWN: Duration = Duration::from_secs(8);

/// The link to the host process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLink {
    /// Not connected yet (and maybe starting it).
    Connecting,
    /// Connected; its overview.
    Connected(Box<HostOverview>),
    /// It could not be started.
    Unavailable(String),
}

/// Which page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    /// ID, password, connect box, recent sessions.
    #[default]
    Home,
    /// Settings.
    Settings,
}

/// The main window's state.
#[derive(Debug)]
pub struct MainModel {
    /// The host process.
    pub host: HostLink,
    /// Whether this window started the host (and stops it on exit).
    pub launched_host: bool,
    /// The last reply from the host, and when it came.
    pub reply: Option<(bool, String, Instant)>,
    /// Which page shows.
    pub page: Page,
}

impl Default for MainModel {
    fn default() -> Self {
        MainModel {
            host: HostLink::Connecting,
            launched_host: false,
            reply: None,
            page: Page::Home,
        }
    }
}

impl MainModel {
    /// Applies a message from the host.
    pub fn apply(&mut self, msg: HostToUi, now: Instant) {
        match msg {
            HostToUi::Hello { overview, .. } | HostToUi::Overview(overview) => {
                self.host = HostLink::Connected(Box::new(overview));
            }
            HostToUi::Reply { ok, message } => self.reply = Some((ok, message, now)),
        }
    }

    /// The host went away.
    pub fn disconnected(&mut self) {
        if matches!(self.host, HostLink::Connected(_)) {
            self.host = HostLink::Connecting;
        }
    }

    /// The overview, when connected.
    pub fn overview(&self) -> Option<&HostOverview> {
        match &self.host {
            HostLink::Connected(o) => Some(o),
            _ => None,
        }
    }

    /// The reply to show now, if any.
    pub fn current_reply(&self, now: Instant) -> Option<(bool, &str)> {
        self.reply
            .as_ref()
            .filter(|(_, _, at)| now.duration_since(*at) < REPLY_SHOWN)
            .map(|(ok, m, _)| (*ok, m.as_str()))
    }
}

/// "123 456 789", or a placeholder while there is no ID.
pub fn id_text(overview: &HostOverview) -> String {
    match overview.id.as_deref().map(str::parse::<DeviceId>) {
        Some(Ok(id)) => id.grouped(),
        _ => "— — —".to_owned(),
    }
}

/// How the ID-server link is doing, and whether that is a problem.
pub fn link_text(overview: &HostOverview) -> (String, bool) {
    match &overview.id_server {
        IdServerState::Online => ("Ready: reachable by ID".to_owned(), false),
        IdServerState::Connecting => ("Connecting to the ID server…".to_owned(), false),
        IdServerState::NotConfigured => (
            "No ID server set: open Settings to set one".to_owned(),
            true,
        ),
        IdServerState::Failed(e) => (format!("ID server: {e} (retrying)"), true),
    }
}

/// What the approve mode means for someone connecting.
pub fn approve_text(mode: &str) -> &'static str {
    match mode {
        "click" => "Others connect when you accept them (no password).",
        "both" => "Others need the password and your acceptance.",
        _ => "Others connect with the password.",
    }
}

/// Parses the connect box: 9 digits, spaces allowed.
pub fn parse_target(text: &str) -> Result<DeviceId, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("Enter the ID of the device to control.".into());
    }
    t.parse::<DeviceId>()
        .map_err(|_| "An ID is 9 digits, like 123 456 789.".to_owned())
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub fn ago(then_unix_ms: u64, now_unix_ms: u64) -> String {
    let secs = now_unix_ms.saturating_sub(then_unix_ms) / 1000;
    match secs {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ if secs < 2 * 86_400 => "yesterday".to_owned(),
        _ => format!("{} days ago", secs / 86_400),
    }
}

/// The settings page's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsForm {
    /// ID server `host[:port]`.
    pub id_server: String,
    /// ID server key (hex).
    pub server_key: String,
    /// `password`, `click` or `both`.
    pub approve_mode: String,
    /// Temporary password length.
    pub password_length: u32,
    /// Default keyboard permission.
    pub keyboard: bool,
    /// Default mouse permission.
    pub mouse: bool,
    /// `fit` or `original`.
    pub view_style: String,
    /// `balanced`, `best` or `low`.
    pub image_quality: String,
    /// New permanent password.
    pub new_password: String,
    /// Its confirmation.
    pub confirm_password: String,
}

impl SettingsForm {
    /// The form filled from `config`.
    pub fn from_config(config: &Config) -> SettingsForm {
        SettingsForm {
            id_server: config.text("custom-rendezvous-server"),
            server_key: config.text("key"),
            approve_mode: config.text("approve-mode"),
            password_length: config.int("temporary-password-length") as u32,
            keyboard: config.bool("enable-keyboard"),
            mouse: config.bool("enable-mouse"),
            view_style: config.text("view-style"),
            image_quality: config.text("image-quality"),
            new_password: String::new(),
            confirm_password: String::new(),
        }
    }

    /// Whether `key` is fixed by an administrator (its field is disabled).
    pub fn locked(config: &Config, key: &str) -> bool {
        config.is_locked(key)
    }

    /// Checks the form and writes it into `config` (not saved). Returns
    /// every problem found; nothing is written then.
    pub fn apply(&self, config: &mut Config) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        let server = self.id_server.trim();
        let key = self.server_key.trim();
        if server.is_empty() != key.is_empty() {
            problems.push("Set both the ID server address and its key (or neither).".to_owned());
        }
        if !key.is_empty() && key.parse::<DevicePublicKey>().is_err() {
            problems.push(
                "The server key must be the 64 hex characters the ID server printed.".to_owned(),
            );
        }
        if server.contains(char::is_whitespace) {
            problems.push("The ID server address cannot contain spaces.".to_owned());
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        let values = [
            ("custom-rendezvous-server", server.to_owned()),
            ("key", key.to_ascii_lowercase()),
            ("approve-mode", self.approve_mode.clone()),
            (
                "temporary-password-length",
                self.password_length.to_string(),
            ),
            ("enable-keyboard", yes_no(self.keyboard)),
            ("enable-mouse", yes_no(self.mouse)),
            ("view-style", self.view_style.clone()),
            ("image-quality", self.image_quality.clone()),
        ];
        let mut updated = config.clone();
        for (k, v) in values {
            // A locked option keeps its value; only a real change is an error.
            if updated.is_locked(k) {
                if updated.text(k) != v {
                    problems.push(format!("{k} is fixed by your administrator."));
                }
                continue;
            }
            if let Err(e) = updated.set(k, &v) {
                problems.push(e.to_string());
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        *config = updated;
        Ok(())
    }

    /// The request that sets the permanent password, if the two fields agree.
    pub fn permanent_password_request(&self) -> Result<UiToHost, String> {
        if self.new_password.chars().count() < MIN_PERMANENT {
            return Err(format!(
                "A permanent password needs at least {MIN_PERMANENT} characters."
            ));
        }
        if self.new_password != self.confirm_password {
            return Err("The two passwords differ.".into());
        }
        Ok(UiToHost::SetPermanentPassword(Some(
            self.new_password.clone(),
        )))
    }
}

fn yes_no(b: bool) -> String {
    if b { "Y" } else { "N" }.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanodesk_ipc::cm::HostStatus;

    const KEY: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

    #[test]
    fn host_messages_update_the_model() {
        let mut m = MainModel::default();
        let t0 = Instant::now();
        assert!(m.overview().is_none());
        m.apply(
            HostToUi::Hello {
                protocol: 1,
                overview: HostOverview {
                    id: Some("123456789".into()),
                    ..Default::default()
                },
            },
            t0,
        );
        assert_eq!(id_text(m.overview().unwrap()), "123 456 789");
        m.apply(
            HostToUi::Reply {
                ok: false,
                message: "nope".into(),
            },
            t0,
        );
        assert_eq!(m.current_reply(t0), Some((false, "nope")));
        assert_eq!(m.current_reply(t0 + REPLY_SHOWN), None);
        m.disconnected();
        assert_eq!(m.host, HostLink::Connecting);
        m.host = HostLink::Unavailable("x".into());
        m.disconnected();
        assert!(matches!(m.host, HostLink::Unavailable(_)));
    }

    #[test]
    fn texts() {
        let mut o = HostOverview::default();
        assert_eq!(id_text(&o), "— — —");
        assert!(link_text(&o).1, "not configured is a problem");
        o.id_server = IdServerState::Online;
        assert_eq!(link_text(&o), ("Ready: reachable by ID".into(), false));
        o.id_server = IdServerState::Failed("timed out".into());
        assert!(link_text(&o).0.contains("timed out"));
        o.status = HostStatus {
            approve_mode: "click".into(),
            ..Default::default()
        };
        assert!(approve_text(&o.status.approve_mode).contains("accept"));
        assert!(approve_text("password").contains("password"));
        assert_eq!(ago(0, 30_000), "just now");
        assert_eq!(ago(0, 5 * 60_000), "5 min ago");
        assert_eq!(ago(0, 3 * 3_600_000), "3 h ago");
        assert_eq!(ago(0, 30 * 3_600_000), "yesterday");
        assert_eq!(ago(0, 5 * 86_400_000), "5 days ago");
        assert_eq!(ago(10, 0), "just now");
    }

    #[test]
    fn connect_box() {
        assert_eq!(
            parse_target(" 123 456 789 ").unwrap(),
            "123456789".parse().unwrap()
        );
        assert!(parse_target("").unwrap_err().contains("Enter"));
        assert!(parse_target("12345").unwrap_err().contains("9 digits"));
        assert!(parse_target("abc").is_err());
    }

    #[test]
    fn settings_round_trip_and_validation() {
        let mut config = Config::in_memory();
        let mut f = SettingsForm::from_config(&config);
        assert_eq!(f.approve_mode, "password");
        assert_eq!(f.password_length, 8);
        assert_eq!(
            (f.view_style.as_str(), f.image_quality.as_str()),
            ("fit", "balanced")
        );

        f.id_server = "ids.example.com".into();
        let errs = f.apply(&mut config).unwrap_err();
        assert!(errs[0].contains("both"), "{errs:?}");
        f.server_key = "nothex".into();
        assert!(f.apply(&mut config).unwrap_err()[0].contains("64 hex"));
        f.server_key = KEY.to_ascii_uppercase();
        f.approve_mode = "both".into();
        f.password_length = 12;
        f.mouse = false;
        f.view_style = "original".into();
        f.apply(&mut config).unwrap();
        assert_eq!(config.text("key"), KEY);
        assert_eq!(config.text("custom-rendezvous-server"), "ids.example.com");
        assert_eq!(config.text("approve-mode"), "both");
        assert_eq!(config.int("temporary-password-length"), 12);
        assert!(!config.bool("enable-mouse"));
        assert_eq!(config.text("view-style"), "original");
        let again = SettingsForm::from_config(&config);
        assert_eq!(again.server_key, KEY);

        // Values the option table refuses are reported and nothing changes.
        let mut bad = again.clone();
        bad.password_length = 3;
        assert!(bad.apply(&mut config).is_err());
        assert_eq!(config.int("temporary-password-length"), 12);
        // Clearing both is allowed.
        let mut none = again.clone();
        none.id_server.clear();
        none.server_key.clear();
        none.apply(&mut config).unwrap();
        assert_eq!(config.text("key"), "");
    }

    #[test]
    fn permanent_password_checks() {
        let mut f = SettingsForm::from_config(&Config::in_memory());
        f.new_password = "short".into();
        f.confirm_password = "short".into();
        assert!(f.permanent_password_request().is_err());
        f.new_password = "Correct Horse".into();
        assert!(
            f.permanent_password_request()
                .unwrap_err()
                .contains("differ")
        );
        f.confirm_password = "Correct Horse".into();
        assert_eq!(
            f.permanent_password_request().unwrap(),
            UiToHost::SetPermanentPassword(Some("Correct Horse".into()))
        );
    }
}
