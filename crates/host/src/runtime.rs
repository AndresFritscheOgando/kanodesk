//! The host process's state around the engine: the ID-server link, the
//! overview the main window shows, and the main window's requests.
//!
//! [`HostRuntime`] owns the [`Presence`] (started from the config's ID
//! server address and key, restarted when they change) and keeps a
//! [`HostOverview`] current: the engine's status, the ID, the link's state,
//! the number of active sessions. [`HostRuntime::handle`] carries out
//! requests from the main window ([`crate::control`]):
//!
//! - a new temporary password;
//! - setting or clearing the permanent password (saved in the key store);
//! - re-reading the config after the user saved settings
//!   ([`HostRuntime::reload`]): approve mode, timeouts, default
//!   permissions, password length and the ID server apply to the next
//!   logins without a restart. Switching a host that runs without a
//!   connection manager (`hide_cm`) to click approval is refused: nobody
//!   could click, and accepting automatically would let anyone in.
//! - stopping the host ([`HostRuntime::stopped`] resolves).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kanodesk_config::{Config, HostSettings};
use kanodesk_crypto::password::{Charset, TemporaryPolicy};
use kanodesk_crypto::{DeviceIdentity, DevicePublicKey, KeyStore};
use kanodesk_input::Permissions;
use kanodesk_ipc::cm::SessionState;
use kanodesk_ipc::control::{HostOverview, IdServerState, UiToHost};
use tokio::sync::{Notify, watch};

use crate::control::Request;
use crate::engine::HostEngine;
use crate::incoming::HostOptions;
use crate::presence::{Presence, PresenceConfig, PresenceStatus};

/// How often the overview is refreshed besides status changes.
const REFRESH: Duration = Duration::from_millis(250);

/// Loads the device identity (for the presence's registrations).
pub type IdentityLoader = Arc<dyn Fn() -> Result<DeviceIdentity, String> + Send + Sync>;

/// What a [`HostRuntime`] is built from.
pub struct RuntimeParts {
    /// The engine.
    pub engine: HostEngine,
    /// The config file to re-read on [`UiToHost::ReloadConfig`].
    pub config_path: PathBuf,
    /// Where the permanent password is saved.
    pub store: Arc<dyn KeyStore>,
    /// Loads the device identity again for the ID-server link.
    pub identity: IdentityLoader,
    /// Where the device ID is remembered.
    pub data_dir: PathBuf,
    /// UDP port of the device socket (0: any).
    pub port: u16,
    /// The host's version, shown in the main window.
    pub version: String,
    /// Whether a connection manager can ask the host's user (false with
    /// `hide_cm`: sessions are accepted without asking).
    pub interactive_cm: bool,
}

struct Link {
    config: PresenceConfig,
    presence: Presence,
}

struct Inner {
    parts: RuntimeParts,
    link: Mutex<Option<Link>>,
    /// The state when there is no link (not configured, bad key).
    unlinked: Mutex<PresenceStatus>,
    overview: watch::Sender<HostOverview>,
    stop: Notify,
}

/// The host's runtime; cheap to clone.
#[derive(Clone)]
pub struct HostRuntime {
    inner: Arc<Inner>,
}

/// The ID-server link the settings ask for: `Ok(None)` when no server is
/// configured, `Err` when the key is not a valid key.
pub fn presence_config(
    settings: &HostSettings,
    data_dir: &std::path::Path,
    port: u16,
) -> Result<Option<PresenceConfig>, String> {
    let (Some(server), Some(key)) = (&settings.rendezvous_server, &settings.rendezvous_key) else {
        return Ok(None);
    };
    let server_key: DevicePublicKey = key
        .trim()
        .parse()
        .map_err(|e| format!("the ID server key is not valid: {e}"))?;
    Ok(Some(PresenceConfig {
        server: server.trim().to_owned(),
        server_key,
        nat_test: None,
        port,
        id_file: Some(data_dir.join("device-id")),
        options: HostOptions::default(),
    }))
}

impl HostRuntime {
    /// Starts the ID-server link from `settings` and the overview updates.
    /// Needs a tokio runtime.
    pub fn start(parts: RuntimeParts, settings: &HostSettings) -> HostRuntime {
        let runtime = HostRuntime {
            inner: Arc::new(Inner {
                parts,
                link: Mutex::new(None),
                unlinked: Mutex::new(PresenceStatus::default()),
                overview: watch::channel(HostOverview::default()).0,
                stop: Notify::new(),
            }),
        };
        runtime.apply_presence(settings);
        runtime.refresh();
        let weak = Arc::downgrade(&runtime.inner);
        let mut status = runtime.inner.parts.engine.watch_status();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = status.changed() => if changed.is_err() { return },
                    _ = tokio::time::sleep(REFRESH) => {}
                }
                let Some(inner) = weak.upgrade() else { return };
                HostRuntime { inner }.refresh();
            }
        });
        runtime
    }

    /// The engine.
    pub fn engine(&self) -> &HostEngine {
        &self.inner.parts.engine
    }

    /// The overview, updated as it changes.
    pub fn overview(&self) -> watch::Receiver<HostOverview> {
        self.inner.overview.subscribe()
    }

    /// The ID-server link's state now.
    pub fn presence(&self) -> PresenceStatus {
        match &*self.inner.link.lock().expect("not poisoned") {
            Some(link) => link.presence.status().borrow().clone(),
            None => self.inner.unlinked.lock().expect("not poisoned").clone(),
        }
    }

    /// Recomputes the overview; pushes it if it changed.
    pub fn refresh(&self) {
        let engine = &self.inner.parts.engine;
        let presence = self.presence();
        let address = self
            .inner
            .link
            .lock()
            .expect("not poisoned")
            .as_ref()
            .map(|l| l.config.server.clone());
        let overview = HostOverview {
            status: engine.status(),
            id: presence.id.map(|id| id.to_string()),
            id_server: presence.state,
            id_server_address: address,
            active_sessions: engine
                .sessions()
                .iter()
                .filter(|s| s.state == SessionState::Active)
                .count() as u32,
            version: self.inner.parts.version.clone(),
        };
        self.inner.overview.send_if_modified(|o| {
            let changed = *o != overview;
            if changed {
                *o = overview;
            }
            changed
        });
    }

    /// Starts, restarts or stops the ID-server link to match `settings`.
    fn apply_presence(&self, settings: &HostSettings) {
        let parts = &self.inner.parts;
        let wanted = presence_config(settings, &parts.data_dir, parts.port);
        let mut link = self.inner.link.lock().expect("not poisoned");
        let keep_id = |link: &Option<Link>| {
            link.as_ref()
                .and_then(|l| l.presence.status().borrow().id)
                .or(self.inner.unlinked.lock().expect("not poisoned").id)
        };
        match wanted {
            Ok(Some(config)) => {
                if link.as_ref().is_some_and(|l| l.config == config) {
                    return;
                }
                let id = keep_id(&link);
                *link = None;
                match (parts.identity)() {
                    Ok(identity) => {
                        tracing::info!("ID server {}", config.server);
                        let presence =
                            Presence::start(parts.engine.clone(), identity, config.clone());
                        *link = Some(Link { config, presence });
                    }
                    Err(e) => {
                        *self.inner.unlinked.lock().expect("not poisoned") = PresenceStatus {
                            state: IdServerState::Failed(format!("device key: {e}")),
                            id,
                        };
                    }
                }
            }
            Ok(None) => {
                let id = keep_id(&link);
                *link = None;
                *self.inner.unlinked.lock().expect("not poisoned") = PresenceStatus {
                    state: IdServerState::NotConfigured,
                    id,
                };
            }
            Err(e) => {
                let id = keep_id(&link);
                *link = None;
                *self.inner.unlinked.lock().expect("not poisoned") = PresenceStatus {
                    state: IdServerState::Failed(e),
                    id,
                };
            }
        }
    }

    /// Re-reads the config and applies what can change while running.
    /// Returns a summary for the user.
    pub fn reload(&self) -> Result<String, String> {
        let config = Config::load(&self.inner.parts.config_path).map_err(|e| e.to_string())?;
        let settings = HostSettings::from_config(&config);
        Ok(self.apply_settings(&settings))
    }

    /// Applies `settings` (see [`Self::reload`]).
    pub fn apply_settings(&self, settings: &HostSettings) -> String {
        let engine = &self.inner.parts.engine;
        let mut notes = Vec::new();
        let mut s = engine.settings();
        if settings.approve_mode.needs_click() && !self.inner.parts.interactive_cm {
            notes.push(
                "Click approval needs the connection manager, which is hidden (hide_cm): \
                 restart KanoDesk to use it. The approve mode was not changed."
                    .to_owned(),
            );
        } else {
            s.approve_mode = settings.approve_mode;
        }
        s.approve_timeout = settings.approve_timeout;
        s.click_delay = settings.click_delay;
        s.permissions = Permissions {
            keyboard: settings.keyboard,
            mouse: settings.mouse,
        };
        engine.update_settings(s);
        match TemporaryPolicy::new(settings.temporary_password_length, Charset::Alphanumeric)
            .and_then(|p| engine.set_temporary_policy(p))
        {
            Ok(()) => {}
            Err(e) => notes.push(format!("temporary password: {e}")),
        }
        self.apply_presence(settings);
        self.refresh();
        if notes.is_empty() {
            "Settings applied.".to_owned()
        } else {
            notes.join(" ")
        }
    }

    /// Carries out a main-window request.
    pub fn handle(&self, request: Request) {
        let answer = match &request.command {
            UiToHost::Hello { .. } => None,
            UiToHost::RefreshPassword => match self.inner.parts.engine.refresh_temporary_password()
            {
                Ok(_) => None,
                Err(e) => Some((false, format!("could not make a new password: {e}"))),
            },
            UiToHost::SetPermanentPassword(password) => {
                match self
                    .inner
                    .parts
                    .engine
                    .set_permanent_password(password.as_deref(), self.inner.parts.store.as_ref())
                {
                    Ok(()) if password.is_some() => Some((true, "Permanent password set.".into())),
                    Ok(()) => Some((true, "Permanent password removed.".into())),
                    Err(e) => Some((false, e.to_string())),
                }
            }
            UiToHost::ReloadConfig => Some(match self.reload() {
                Ok(m) => (true, m),
                Err(e) => (false, e),
            }),
            UiToHost::Shutdown => {
                self.inner.stop.notify_waiters();
                self.inner.stop.notify_one();
                None
            }
        };
        self.refresh();
        request.reply(answer);
    }

    /// Resolves when a main window asked the host to stop.
    pub async fn stopped(&self) {
        self.inner.stop.notified().await;
    }
}
