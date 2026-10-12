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
            frames: 0,
            second: None,
        }
    }

    /// Starts over (Try again).
    pub fn restart(&mut self) {
        *self = ViewerModel::new(std::mem::take(&mut self.id), self.view, self.quality);
    }

    /// Applies a session event.
    pub fn apply(&mut self, event: &SessionEvent, now: Instant) {
        match event {
            SessionEvent::Stage(s) => {
                if matches!(self.phase, Phase::Starting(_) | Phase::Password(_)) {
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

    /// What the toolbar says about the session.
    pub fn status_text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(p) = self.path {
            parts.push(path_text(p).to_owned());
        }
        if self.phase == Phase::Streaming {
            if self.permissions == Permissions::NONE {
                parts.push("view only".to_owned());
            }
            parts.push(format!("{} fps", self.fps));
        }
        parts.join(" · ")
    }
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
        assert_eq!(m.status_text(), "relayed · view only · 0 fps");
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
