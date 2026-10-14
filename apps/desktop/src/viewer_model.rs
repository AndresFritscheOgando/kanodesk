//! The viewer window's state, independent of the window: which dialog
//! shows (connecting, password, relayed connection, error), what the
//! toolbar shows and offers, and whether input may go to the remote side.
//! Driven by [`SessionEvent`]s from `kanodesk_client::session`.

use std::time::{Duration, Instant};

use kanodesk_client::connect::PathKind;
use kanodesk_client::input::ViewStyle;
use kanodesk_client::session::{
    EndReason, Notice, PasswordPrompt, RemoteHost, SessionEvent, Stage, prompt_text,
};
use kanodesk_input::Permissions;
use kanodesk_proto::v1::{DisplayInfo, ImageQuality};

use crate::main_model::Level;

/// How long a toolbar message stays.
pub const TOAST: Duration = Duration::from_secs(6);

/// Where the session is, as the window shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    /// Starting: the "Connecting" dialog with this stage.
    Starting(Stage),
    /// The host wants a password: the password dialog.
    Password(PasswordPrompt),
    /// Logged in: the remote screen.
    Streaming,
    /// Over: the end dialog.
    Ended(EndReason),
}

/// The viewer's state.
#[derive(Debug)]
pub struct ViewerModel {
    /// The ID being viewed, grouped ("123 456 789").
    pub id: String,
    /// Where it is.
    pub phase: Phase,
    /// How the connection was made.
    pub path: Option<PathKind>,
    /// Show the "relayed connection" note (until dismissed).
    pub relay_note: bool,
    /// The host, once logged in.
    pub remote: Option<RemoteHost>,
    /// The host's displays.
    pub displays: Vec<DisplayInfo>,
    /// The display the video shows.
    pub display: Option<u32>,
    /// What the host allows.
    pub permissions: Permissions,
    /// Fit or original size.
    pub view: ViewStyle,
    /// The quality preset asked for.
    pub quality: ImageQuality,
    /// A short message in the toolbar, and when it appeared.
    pub toast: Option<(String, Instant)>,
    /// Frames shown in the last full second.
    pub fps: u32,
    /// The ID server's address, when one is set (for the dialogs).
    pub server: Option<String>,
    /// When the current connecting stage began.
    pub stage_since: Option<Instant>,
    frames: u32,
    second: Option<Instant>,
}

impl ViewerModel {
    /// A model for viewing `id` with these defaults.
    pub fn new(id: String, view: ViewStyle, quality: ImageQuality) -> ViewerModel {
        ViewerModel {
            id,
            phase: Phase::Starting(Stage::IdServer),
            path: None,
            relay_note: false,
            remote: None,
            displays: Vec::new(),
            display: None,
            permissions: Permissions::NONE,
            view,
            quality,
            toast: None,
            fps: 0,
            server: None,
            stage_since: None,
            frames: 0,
            second: None,
        }
    }

    /// Starts over (Try again).
    pub fn restart(&mut self) {
        let server = self.server.take();
        *self = ViewerModel::new(std::mem::take(&mut self.id), self.view, self.quality);
        self.server = server;
    }

    /// The session (re)started at `now`: the first stage's clock starts.
    pub fn started(&mut self, now: Instant) {
        self.stage_since = Some(now);
    }

    /// How long the current connecting stage has taken.
    pub fn stage_waited(&self, now: Instant) -> Duration {
        self.stage_since
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t))
    }

    /// Applies a session event.
    pub fn apply(&mut self, event: &SessionEvent, now: Instant) {
        match event {
            SessionEvent::Stage(s) => {
                if matches!(self.phase, Phase::Starting(_) | Phase::Password(_)) {
                    if self.phase != Phase::Starting(*s) || self.stage_since.is_none() {
                        self.stage_since = Some(now);
                    }
                    self.phase = Phase::Starting(*s);
                }
            }
            SessionEvent::Connected(path) => {
                self.path = *path;
                self.relay_note = *path == Some(PathKind::Relay);
            }
            SessionEvent::PasswordNeeded(p) => self.phase = Phase::Password(*p),
            SessionEvent::LoggedIn(remote) => {
                self.remote = Some(remote.clone());
                self.phase = Phase::Streaming;
            }
            SessionEvent::Displays(d) => self.displays = d.clone(),
            SessionEvent::Permissions(p) => self.permissions = *p,
            SessionEvent::VideoStarted(d) => self.display = Some(*d),
            SessionEvent::FrameReady => self.frame(now),
            SessionEvent::CursorShape(_) | SessionEvent::CursorPosition(_) => {}
            SessionEvent::Ended(r) => {
                self.phase = Phase::Ended(r.clone());
                self.relay_note = false;
            }
        }
    }

    /// Counts a shown frame for the fps figure.
    pub fn frame(&mut self, now: Instant) {
        let start = *self.second.get_or_insert(now);
        if now.duration_since(start) >= Duration::from_secs(1) {
            self.fps = self.frames;
            self.frames = 0;
            self.second = Some(now);
        }
        self.frames += 1;
    }

    /// Whether a modal dialog covers the window (no input to the host then).
    pub fn modal(&self) -> bool {
        !matches!(self.phase, Phase::Streaming)
    }

    /// Whether input may go to the host.
    pub fn input_allowed(&self) -> bool {
        self.phase == Phase::Streaming
    }

    /// Shows a toolbar message.
    pub fn toast(&mut self, text: impl Into<String>, now: Instant) {
        self.toast = Some((text.into(), now));
    }

    /// The toolbar message to show now.
    pub fn current_toast(&self, now: Instant) -> Option<&str> {
        self.toast
            .as_ref()
            .filter(|(_, at)| now.duration_since(*at) < TOAST)
            .map(|(t, _)| t.as_str())
    }

    /// The window title.
    pub fn title(&self) -> String {
        match self.remote.as_ref().filter(|r| !r.hostname.is_empty()) {
            Some(r) => format!("{} ({}) – KanoDesk", self.id, r.hostname),
            None => format!("{} – KanoDesk", self.id),
        }
    }

    /// The end dialog's text, when ended.
    pub fn end_notice(&self) -> Option<Notice> {
        match &self.phase {
            Phase::Ended(r) => Some(r.notice()),
            _ => None,
        }
    }

    /// The toolbar's status chips while streaming: how the connection
    /// runs (relayed in amber), view only, frame rate.
    pub fn status_chips(&self) -> Vec<Chip> {
        let mut chips = Vec::new();
        if let Some(p) = self.path {
            let level = if p == PathKind::Relay {
                Level::Busy
            } else {
                Level::Idle
            };
            chips.push(Chip {
                text: path_chip(p).to_owned(),
                level,
            });
        }
        if self.phase == Phase::Streaming {
            if self.permissions == Permissions::NONE {
                chips.push(Chip {
                    text: "View only".to_owned(),
                    level: Level::Busy,
                });
            }
            chips.push(Chip {
                text: format!("{} fps", self.fps),
                level: Level::Idle,
            });
        }
        chips
    }

    /// What the toolbar shows in the current phase. Before the session
    /// streams, the dialog in front carries the actions (Cancel, Close), so
    /// the toolbar only says where things stand.
    pub fn toolbar(&self) -> ToolbarSpec {
        match &self.phase {
            Phase::Starting(stage) => ToolbarSpec {
                controls: false,
                status: stage_text(*stage).trim_end_matches('…').to_owned(),
                disconnect: false,
            },
            Phase::Password(_) => ToolbarSpec {
                controls: false,
                status: "Password needed".to_owned(),
                disconnect: false,
            },
            Phase::Streaming => ToolbarSpec {
                controls: true,
                status: String::new(),
                disconnect: true,
            },
            Phase::Ended(_) => ToolbarSpec {
                controls: false,
                status: "Ended".to_owned(),
                disconnect: false,
            },
        }
    }
}

/// What the viewer's toolbar shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolbarSpec {
    /// Display, view and quality pickers, the actions and the status chips.
    pub controls: bool,
    /// The status text after the ID (empty while streaming: chips instead).
    pub status: String,
    /// Whether the Disconnect button shows.
    pub disconnect: bool,
}

/// A small status label in the toolbar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    /// What it says.
    pub text: String,
    /// Its colour.
    pub level: Level,
}

/// A path as a toolbar chip.
pub fn path_chip(path: PathKind) -> &'static str {
    match path {
        PathKind::Lan => "LAN",
        PathKind::Ipv6 => "Direct (IPv6)",
        PathKind::Direct => "Direct",
        PathKind::Relay => "Relayed",
    }
}

/// How close to the top edge (points) the pointer reveals the fullscreen toolbar.
pub const REVEAL_EDGE: f32 = 6.0;
/// How long the fullscreen toolbar stays after the pointer leaves it.
pub const TOOLBAR_LINGER: Duration = Duration::from_millis(1500);

/// Whether the fullscreen toolbar is wanted right now: pinned, under the
/// pointer, or the pointer at the top edge.
pub fn toolbar_wanted(pinned: bool, over_toolbar: bool, pointer_y: Option<f32>) -> bool {
    pinned || over_toolbar || pointer_y.is_some_and(|y| y <= REVEAL_EDGE)
}

/// Whether the fullscreen toolbar shows: while wanted, and for
/// [`TOOLBAR_LINGER`] after it last was.
pub fn toolbar_visible(wanted: bool, last_wanted: Option<Instant>, now: Instant) -> bool {
    wanted || last_wanted.is_some_and(|t| now.saturating_duration_since(t) < TOOLBAR_LINGER)
}

/// How long a connecting stage may take before the dialog explains.
pub const SLOW_STAGE: Duration = Duration::from_secs(8);

/// A hint for a connecting stage that takes long; None at first, and
/// never while waiting for the remote user (people take their time).
pub fn slow_hint(stage: Stage, waited: Duration, server: Option<&str>) -> Option<String> {
    if waited < SLOW_STAGE {
        return None;
    }
    match stage {
        Stage::IdServer => Some(match server {
            Some(s) => format!(
                "The ID server at {s} has not answered yet. It may be offline or blocked by a firewall."
            ),
            None => {
                "The ID server has not answered yet. It may be offline or blocked by a firewall."
                    .to_owned()
            }
        }),
        Stage::Connecting => Some("Trying direct and relayed paths to the device…".to_owned()),
        Stage::LoggingIn => Some("This is taking longer than usual.".to_owned()),
        Stage::WaitingForApproval => None,
    }
}

/// The seconds a stage has taken, once worth showing ("12 s").
pub fn waited_text(waited: Duration) -> Option<String> {
    (waited >= Duration::from_secs(3)).then(|| format!("{} s", waited.as_secs()))
}

/// The connecting dialog's text for a stage.
pub fn stage_text(stage: Stage) -> &'static str {
    match stage {
        Stage::IdServer => "Contacting the ID server…",
        Stage::Connecting => "Connecting to the device…",
        Stage::LoggingIn => "Securing the connection and logging in…",
        Stage::WaitingForApproval => "Waiting for the remote user to accept…",
    }
}

/// The password dialog's text.
pub fn password_text(prompt: PasswordPrompt) -> &'static str {
    prompt_text(prompt)
}

/// A path, in words.
pub fn path_text(path: PathKind) -> &'static str {
    match path {
        PathKind::Lan => "LAN",
        PathKind::Ipv6 => "direct (IPv6)",
        PathKind::Direct => "direct",
        PathKind::Relay => "relayed",
    }
}

/// A display in the picker: "1: 1920×1080 (primary)".
pub fn display_label(index: usize, d: &DisplayInfo) -> String {
    format!(
        "{}: {}×{}{}",
        index + 1,
        d.width,
        d.height,
        if d.primary { " (primary)" } else { "" }
    )
}

/// The config's `image-quality` word as a preset.
pub fn quality_from_config(word: &str) -> ImageQuality {
    match word {
        "best" => ImageQuality::Best,
        "low" => ImageQuality::Low,
        _ => ImageQuality::Balanced,
    }
}

/// A preset as the config's word.
pub fn quality_word(q: ImageQuality) -> &'static str {
    match q {
        ImageQuality::Best => "best",
        ImageQuality::Low => "low",
        _ => "balanced",
    }
}

/// The config's `view-style` word as a style.
pub fn view_from_config(word: &str) -> ViewStyle {
    match word {
        "original" => ViewStyle::Original,
        _ => ViewStyle::Fit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ViewerModel {
        ViewerModel::new("123 456 789".into(), ViewStyle::Fit, ImageQuality::Balanced)
    }

    #[test]
    fn a_session_moves_through_the_dialogs() {
        let t = Instant::now();
        let mut m = model();
        assert!(m.modal() && !m.input_allowed());
        m.apply(&SessionEvent::Stage(Stage::Connecting), t);
        assert_eq!(m.phase, Phase::Starting(Stage::Connecting));
        m.apply(&SessionEvent::Connected(Some(PathKind::Relay)), t);
        assert!(m.relay_note, "relayed connections are pointed out");
        m.apply(&SessionEvent::PasswordNeeded(PasswordPrompt::Required), t);
        assert_eq!(m.phase, Phase::Password(PasswordPrompt::Required));
        // Back to connecting after the password is sent.
        m.apply(&SessionEvent::Stage(Stage::Connecting), t);
        assert_eq!(m.phase, Phase::Starting(Stage::Connecting));
        m.apply(
            &SessionEvent::LoggedIn(RemoteHost {
                hostname: "OFFICE".into(),
                ..Default::default()
            }),
            t,
        );
        assert!(!m.modal() && m.input_allowed());
        assert_eq!(m.title(), "123 456 789 (OFFICE) – KanoDesk");
        // A late stage event does not reopen the dialog.
        m.apply(&SessionEvent::Stage(Stage::LoggingIn), t);
        assert_eq!(m.phase, Phase::Streaming);
        let chips: Vec<_> = m
            .status_chips()
            .into_iter()
            .map(|c| (c.text, c.level))
            .collect();
        assert_eq!(
            chips,
            [
                ("Relayed".to_owned(), Level::Busy),
                ("View only".to_owned(), Level::Busy),
                ("0 fps".to_owned(), Level::Idle),
            ]
        );
        m.apply(&SessionEvent::Permissions(Permissions::ALL), t);
        m.apply(&SessionEvent::VideoStarted(1), t);
        assert_eq!(m.display, Some(1));
        m.apply(&SessionEvent::Ended(EndReason::Denied), t);
        assert!(m.modal());
        assert_eq!(m.end_notice().unwrap().title, "Connection refused");
        assert!(!m.relay_note);
        m.restart();
        assert_eq!(m.phase, Phase::Starting(Stage::IdServer));
        assert_eq!(m.id, "123 456 789");
        assert!(m.remote.is_none());
    }

    #[test]
    fn connecting_progress() {
        let t = Instant::now();
        let mut m = model();
        m.server = Some("10.0.0.31".into());
        assert_eq!(m.stage_waited(t), Duration::ZERO, "not started");
        m.started(t);
        let later = t + Duration::from_secs(12);
        assert_eq!(m.stage_waited(later), Duration::from_secs(12));
        // The same stage again keeps its clock; a new stage restarts it.
        m.apply(
            &SessionEvent::Stage(Stage::IdServer),
            t + Duration::from_secs(5),
        );
        assert_eq!(m.stage_waited(later), Duration::from_secs(12));
        m.apply(
            &SessionEvent::Stage(Stage::Connecting),
            t + Duration::from_secs(10),
        );
        assert_eq!(m.stage_waited(later), Duration::from_secs(2));
        m.restart();
        assert_eq!(m.server.as_deref(), Some("10.0.0.31"), "kept for Try again");
        assert!(m.stage_since.is_none());

        let s = Some("10.0.0.31");
        assert_eq!(slow_hint(Stage::IdServer, Duration::from_secs(7), s), None);
        let hint = slow_hint(Stage::IdServer, SLOW_STAGE, s).unwrap();
        assert!(
            hint.contains("10.0.0.31") && hint.contains("firewall"),
            "{hint}"
        );
        assert!(
            !slow_hint(Stage::IdServer, SLOW_STAGE, None)
                .unwrap()
                .contains(" at ")
        );
        assert!(slow_hint(Stage::Connecting, SLOW_STAGE, s).is_some());
        assert!(slow_hint(Stage::LoggingIn, SLOW_STAGE, s).is_some());
        assert_eq!(
            slow_hint(Stage::WaitingForApproval, Duration::from_secs(600), s),
            None
        );
        assert_eq!(waited_text(Duration::from_secs(2)), None);
        assert_eq!(
            waited_text(Duration::from_millis(12_400)),
            Some("12 s".into())
        );
    }

    #[test]
    fn toolbar_by_phase() {
        let t = Instant::now();
        let mut m = model();
        let spec = m.toolbar();
        assert!(!spec.controls && !spec.disconnect);
        assert_eq!(spec.status, "Contacting the ID server");
        m.apply(&SessionEvent::PasswordNeeded(PasswordPrompt::Required), t);
        assert_eq!(m.toolbar().status, "Password needed");
        assert!(!m.toolbar().controls);
        m.apply(&SessionEvent::LoggedIn(RemoteHost::default()), t);
        let spec = m.toolbar();
        assert!(spec.controls && spec.disconnect);
        m.apply(&SessionEvent::Ended(EndReason::Closed), t);
        let spec = m.toolbar();
        assert!(!spec.controls && !spec.disconnect);
        assert_eq!(spec.status, "Ended");
    }

    #[test]
    fn fullscreen_toolbar_timing() {
        let t = Instant::now();
        assert!(toolbar_wanted(true, false, None), "pinned");
        assert!(
            toolbar_wanted(false, true, Some(300.0)),
            "under the pointer"
        );
        assert!(toolbar_wanted(false, false, Some(2.0)), "top edge");
        assert!(!toolbar_wanted(false, false, Some(40.0)));
        assert!(!toolbar_wanted(false, false, None));
        assert!(toolbar_visible(true, None, t));
        assert!(!toolbar_visible(false, None, t));
        assert!(toolbar_visible(
            false,
            Some(t),
            t + Duration::from_millis(1000)
        ));
        assert!(!toolbar_visible(false, Some(t), t + TOOLBAR_LINGER));
        assert_eq!(path_chip(PathKind::Lan), "LAN");
    }

    #[test]
    fn fps_and_toasts() {
        let t = Instant::now();
        let mut m = model();
        for i in 0..30 {
            m.frame(t + Duration::from_millis(i * 33));
        }
        m.frame(t + Duration::from_millis(1001));
        assert_eq!(m.fps, 30, "30 frames fell in the first second");
        m.toast("sent", t);
        assert_eq!(m.current_toast(t), Some("sent"));
        assert_eq!(m.current_toast(t + TOAST), None);
    }

    #[test]
    fn texts_and_config_words() {
        for s in [
            Stage::IdServer,
            Stage::Connecting,
            Stage::LoggingIn,
            Stage::WaitingForApproval,
        ] {
            assert!(stage_text(s).ends_with('…'));
        }
        assert!(password_text(PasswordPrompt::Wrong).contains("Wrong"));
        assert_eq!(path_text(PathKind::Relay), "relayed");
        let d = DisplayInfo {
            width: 1920,
            height: 1080,
            primary: true,
            ..Default::default()
        };
        assert_eq!(display_label(0, &d), "1: 1920×1080 (primary)");
        for q in [
            ImageQuality::Balanced,
            ImageQuality::Best,
            ImageQuality::Low,
        ] {
            assert_eq!(quality_from_config(quality_word(q)), q);
        }
        assert_eq!(quality_from_config("nonsense"), ImageQuality::Balanced);
        assert_eq!(view_from_config("original"), ViewStyle::Original);
        assert_eq!(view_from_config("fit"), ViewStyle::Fit);
    }
}
