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

/// How a status is doing; picks its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Working: online, connected.
    Ok,
    /// On its way: connecting, retrying soon.
    Busy,
    /// Needs the user: offline, not set up, failed.
    Problem,
    /// Neutral information.
    Idle,
}

/// The link to the host process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLink {
    /// Not connected yet (and maybe starting it).
    Connecting,
    /// Connected; its overview.
    Connected(Box<HostOverview>),
    /// It could not be started.
    Unavailable(String),
    /// This OS cannot host ([`crate::server::HOSTING_SUPPORTED`]): the
    /// window is a viewer only and no host is started or contacted.
    ViewerOnly,
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
    /// The model of a window on an OS that cannot host.
    pub fn viewer_only() -> MainModel {
        MainModel {
            host: HostLink::ViewerOnly,
            ..MainModel::default()
        }
    }

    /// Whether this window is a viewer only (no "This desk", no host settings).
    pub fn is_viewer_only(&self) -> bool {
        self.host == HostLink::ViewerOnly
    }

    /// Shows `message` like a reply from the host (for what the window
    /// does itself, such as saving settings without a host).
    pub fn note(&mut self, ok: bool, message: &str, now: Instant) {
        self.reply = Some((ok, message.to_owned(), now));
    }

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

/// What "This desk" says on an OS that cannot host.
pub const VIEWER_ONLY_TEXT: &str = "This computer can control other desks, but others cannot connect to it: \
     sharing this screen needs Windows for now.";

/// The ID-server status as the main window shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkStatus {
    /// The dot's colour.
    pub level: Level,
    /// Next to the dot: "Online", "Connecting…", "Offline", "Not set up".
    pub short: &'static str,
    /// The sentence under the ID.
    pub text: String,
    /// The raw error, for the tooltip.
    pub detail: Option<String>,
    /// Whether to offer "Open Settings".
    pub settings: bool,
}

/// What "This desk" says on a viewer-only window about the ID server.
pub fn viewer_link_status(config: &Config) -> LinkStatus {
    let server = config.text("custom-rendezvous-server");
    if server.is_empty() || config.text("key").is_empty() {
        LinkStatus {
            level: Level::Problem,
            short: "Not set up",
            text: "Set an ID server in Settings before connecting.".to_owned(),
            detail: None,
            settings: true,
        }
    } else {
        LinkStatus {
            level: Level::Idle,
            short: "Ready",
            text: format!("ID server: {server}"),
            detail: None,
            settings: false,
        }
    }
}

/// "123 456 789", or a placeholder while there is no ID.
pub fn id_text(overview: &HostOverview) -> String {
    match overview.id.as_deref().map(str::parse::<DeviceId>) {
        Some(Ok(id)) => id.grouped(),
        _ => "— — —".to_owned(),
    }
}

/// How the host's ID-server link is doing.
pub fn link_status(overview: &HostOverview) -> LinkStatus {
    let server = overview
        .id_server_address
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("the ID server");
    let status = |level, short, text: String| LinkStatus {
        level,
        short,
        text,
        detail: None,
        settings: false,
    };
    match &overview.id_server {
        IdServerState::Online => status(
            Level::Ok,
            "Online",
            "Others can reach this desk by its ID.".to_owned(),
        ),
        IdServerState::Connecting => status(
            Level::Busy,
            "Connecting…",
            format!("Connecting to {server}…"),
        ),
        IdServerState::NotConfigured => LinkStatus {
            settings: true,
            ..status(
                Level::Problem,
                "Not set up",
                "Set an ID server in Settings so others can reach this desk.".to_owned(),
            )
        },
        IdServerState::Failed(e) => LinkStatus {
            detail: Some(e.clone()),
            settings: true,
            ..status(
                Level::Problem,
                "Offline",
                format!(
                    "Can't reach {server}: it {}. Retrying automatically.",
                    kanodesk_client::session::plain_cause(e)
                ),
            )
        },
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

/// The connect box as typed: keeps up to 9 digits and groups them in
/// threes ("123456" → "123 456"); anything else (spaces, dashes, "ID:")
/// is dropped, so pasted IDs work.
pub fn format_id_input(text: &str) -> String {
    let digits: Vec<char> = text.chars().filter(char::is_ascii_digit).take(9).collect();
    let mut out = String::new();
    for (i, d) in digits.iter().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(' ');
        }
        out.push(*d);
    }
    out
}

/// "6 of 9 digits" while the connect box is incomplete.
pub fn id_progress(text: &str) -> Option<String> {
    let n = text.chars().filter(char::is_ascii_digit).count();
    (1..9).contains(&n).then(|| format!("{n} of 9 digits"))
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
        // The key field wraps, and pasted keys may carry line breaks.
        let key: String = self.server_key.split_whitespace().collect();
        let key = key.as_str();
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

    /// Whether the saved fields differ from `saved` (the permanent password
    /// fields do not count: they apply on their own).
    pub fn differs_from(&self, saved: &SettingsForm) -> bool {
        let strip = |f: &SettingsForm| SettingsForm {
            new_password: String::new(),
            confirm_password: String::new(),
            ..f.clone()
        };
        strip(self) != strip(saved)
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

/// What to say under the server key field while it is typed, and how.
pub fn key_hint(text: &str) -> Option<(Level, String)> {
    let key: String = text.split_whitespace().collect();
    let n = key.chars().count();
    if n == 0 {
        None
    } else if !key.chars().all(|c| c.is_ascii_hexdigit()) {
        Some((
            Level::Problem,
            "Only 0–9 and a–f are allowed in the key.".to_owned(),
        ))
    } else if n < 64 {
        Some((Level::Idle, format!("{n} of 64 characters")))
    } else if n > 64 {
        Some((
            Level::Problem,
            format!("Too long: a key is 64 characters, this is {n}."),
        ))
    } else if key.parse::<DevicePublicKey>().is_ok() {
        Some((Level::Ok, "✔ Valid key".to_owned()))
    } else {
        Some((Level::Problem, "This is not a valid server key.".to_owned()))
    }
}

/// What to say under the ID server field while it is typed, and how.
pub fn server_hint(text: &str) -> Option<(Level, String)> {
    let t = text.trim();
    if t.is_empty() {
        None
    } else if t.contains(char::is_whitespace) {
        Some((
            Level::Problem,
            "The address cannot contain spaces.".to_owned(),
        ))
    } else if t.contains(':') {
        None
    } else {
        Some((
            Level::Idle,
            "Port 21116 is used when none is given.".to_owned(),
        ))
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
        assert!(!m.is_viewer_only());
    }

    #[test]
    fn viewer_only() {
        let mut m = MainModel::viewer_only();
        let t0 = Instant::now();
        assert!(m.is_viewer_only() && m.overview().is_none());
        assert_eq!(m.page, Page::Home);
        m.disconnected();
        assert!(m.is_viewer_only(), "nothing to lose");
        m.note(true, "Settings saved.", t0);
        assert_eq!(m.current_reply(t0), Some((true, "Settings saved.")));
        assert!(VIEWER_ONLY_TEXT.contains("Windows"));
        assert!(!VIEWER_ONLY_TEXT.contains("  "), "one space between words");

        let mut config = Config::in_memory();
        let s = viewer_link_status(&config);
        assert!(s.level == Level::Problem && s.settings, "{s:?}");
        assert!(s.text.contains("Settings"), "{}", s.text);
        config.set("custom-rendezvous-server", "10.0.0.31").unwrap();
        assert_eq!(
            viewer_link_status(&config).level,
            Level::Problem,
            "the key is missing"
        );
        config.set("key", KEY).unwrap();
        let s = viewer_link_status(&config);
        assert_eq!(
            (s.level, s.text.as_str()),
            (Level::Idle, "ID server: 10.0.0.31")
        );
        assert!(!s.settings);
    }

    #[test]
    fn texts() {
        let mut o = HostOverview::default();
        assert_eq!(id_text(&o), "— — —");
        let s = link_status(&o);
        assert_eq!((s.level, s.short), (Level::Problem, "Not set up"));
        assert!(s.settings && s.detail.is_none());
        o.id_server = IdServerState::Online;
        let s = link_status(&o);
        assert_eq!((s.level, s.short, s.settings), (Level::Ok, "Online", false));
        o.id_server = IdServerState::Connecting;
        assert_eq!(link_status(&o).text, "Connecting to the ID server…");
        o.id_server_address = Some("10.0.0.31".into());
        let s = link_status(&o);
        assert_eq!(
            (s.level, s.text.as_str()),
            (Level::Busy, "Connecting to 10.0.0.31…")
        );
        o.id_server = IdServerState::Failed("transport: connection: timed out".into());
        let s = link_status(&o);
        assert_eq!((s.level, s.short), (Level::Problem, "Offline"));
        assert_eq!(
            s.text,
            "Can't reach 10.0.0.31: it did not answer. Retrying automatically."
        );
        assert_eq!(
            s.detail.as_deref(),
            Some("transport: connection: timed out")
        );
        assert!(s.settings);
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
        assert_eq!(
            parse_target("123-456-789").unwrap(),
            "123456789".parse().unwrap()
        );
    }

    #[test]
    fn connect_box_formatting() {
        assert_eq!(format_id_input(""), "");
        assert_eq!(format_id_input("12"), "12");
        assert_eq!(format_id_input("123"), "123");
        assert_eq!(format_id_input("1234"), "123 4");
        assert_eq!(format_id_input("123456789"), "123 456 789");
        assert_eq!(format_id_input("123-456-789"), "123 456 789");
        assert_eq!(format_id_input("ID: 123 456 789"), "123 456 789");
        assert_eq!(format_id_input("1234567890123"), "123 456 789", "at most 9");
        assert_eq!(format_id_input("abc"), "");
        assert_eq!(format_id_input("123 456 78"), "123 456 78");
        assert_eq!(id_progress(""), None);
        assert_eq!(id_progress("123 4"), Some("4 of 9 digits".into()));
        assert_eq!(id_progress("123 456 789"), None);
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
    fn settings_hints_and_changes() {
        assert_eq!(key_hint(""), None);
        assert_eq!(
            key_hint(
                "  
 "
            ),
            None
        );
        assert_eq!(
            key_hint(&KEY[..10]),
            Some((Level::Idle, "10 of 64 characters".into()))
        );
        assert_eq!(key_hint(KEY).unwrap().0, Level::Ok);
        let split = format!(
            "{} 
{}",
            &KEY[..32],
            &KEY[32..]
        );
        assert_eq!(
            key_hint(&split).unwrap().0,
            Level::Ok,
            "line breaks are ignored"
        );
        assert_eq!(key_hint("xyz").unwrap().0, Level::Problem);
        assert!(
            key_hint(&format!("{KEY}00"))
                .unwrap()
                .1
                .contains("Too long")
        );
        assert_eq!(server_hint(""), None);
        assert_eq!(server_hint("ids.example.com:21116"), None);
        assert_eq!(server_hint("ids.example.com").unwrap().0, Level::Idle);
        assert_eq!(server_hint("ids example").unwrap().0, Level::Problem);

        // A key pasted over two lines is saved in one piece.
        let mut config = Config::in_memory();
        let mut f = SettingsForm::from_config(&config);
        f.id_server = "10.0.0.31".into();
        f.server_key = split;
        f.apply(&mut config).unwrap();
        assert_eq!(config.text("key"), KEY);

        let saved = SettingsForm::from_config(&config);
        let mut f = saved.clone();
        assert!(!f.differs_from(&saved));
        f.new_password = "typed but not set".into();
        assert!(
            !f.differs_from(&saved),
            "password fields apply on their own"
        );
        f.approve_mode = "click".into();
        assert!(f.differs_from(&saved));
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
