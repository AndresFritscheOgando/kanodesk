//! `spike-host`: the host engine on a direct UDP port, without a window.
//!
//! ```text
//! spike-host [--listen 127.0.0.1:9000] [--fps 30] [--bitrate KBPS] [--speed 6] [--display N]
//!            [--data-dir DIR] [--password-length 8] [--set-permanent PASSWORD | --clear-permanent]
//!            [--reset-login-limits] [--view-only]
//! ```
//!
//! A thin wrapper around `kanodesk_host::HostEngine` with password logins
//! and a console instead of the connection manager (every logged-in viewer
//! is accepted; sessions are printed). Prints its device key and a
//! temporary password; the viewer needs both (`spike-viewer --host-key KEY`,
//! then the password). A temporary password admits one login (a new one is
//! printed). Login failures, backoffs and bans are kept in `--data-dir`
//! across restarts (`--reset-login-limits` forgets them). The device key
//! (and any permanent password) is kept DPAPI-protected in `--data-dir`.
//!
//! Several viewers may watch at once; they share one capture and encoder
//! per display. Viewers control the keyboard and mouse (07); `--view-only`
//! starts with input refused, and pressing Enter in this console toggles it
//! for current and later viewers.
//!
//! For click approval and the connection-manager window, run
//! `kanodesk --server` instead.

#[cfg(windows)]
fn main() {
    if let Err(e) = host::run() {
        eprintln!("spike-host: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("spike-host: no capture backend for this OS yet");
}

#[cfg(windows)]
mod host {
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use kanodesk_config::ApproveMode;
    use kanodesk_crypto::password::{
        Charset, DEFAULT_TEMPORARY_LEN, HostPasswords, TemporaryPolicy,
    };
    use kanodesk_crypto::throttle::{self, ThrottleConfig};
    use kanodesk_host::engine::{EngineParts, EngineSettings, platform_displays, platform_input};
    use kanodesk_host::video::{VideoSettings, capture_producer};
    use kanodesk_host::{AutoAccept, HostEngine, VideoHub};
    use kanodesk_input::Permissions;
    use kanodesk_spike::{AGENT, DEFAULT_ADDR, default_data_dir, device_identity, key_store};
    use kanodesk_transport::endpoint;
    use kanodesk_transport::tls::Identity;

    type BoxError = Box<dyn std::error::Error + Send + Sync>;

    /// Login failures, backoffs and bans, kept across restarts (in the data dir).
    const THROTTLE_FILE: &str = "login-throttle";

    #[derive(Debug, Clone)]
    struct Settings {
        listen: SocketAddr,
        video: VideoSettings,
        display: Option<u32>,
        data_dir: PathBuf,
        password_length: usize,
        /// `Some(Some(p))` sets the permanent password, `Some(None)` clears it.
        permanent: Option<Option<String>>,
        reset_login_limits: bool,
        view_only: bool,
    }

    fn parse_args() -> Result<Settings, String> {
        let mut s = Settings {
            listen: DEFAULT_ADDR.parse().expect("valid default"),
            video: VideoSettings::default(),
            display: None,
            data_dir: default_data_dir("spike-host"),
            password_length: DEFAULT_TEMPORARY_LEN,
            permanent: None,
            reset_login_limits: false,
            view_only: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(flag) = it.next() {
            let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
            let num = |v: String| v.parse::<i64>().map_err(|e| e.to_string());
            match flag.as_str() {
                "--listen" => s.listen = value()?.parse().map_err(|e| format!("--listen: {e}"))?,
                "--fps" => s.video.fps = num(value()?)?.clamp(1, 240) as u32,
                "--bitrate" => s.video.bitrate_kbps = Some(num(value()?)?.max(1) as u32),
                "--speed" => s.video.speed = num(value()?)? as i32,
                "--display" => s.display = Some(num(value()?)? as u32),
                "--data-dir" => s.data_dir = PathBuf::from(value()?),
                "--password-length" => s.password_length = num(value()?)?.max(0) as usize,
                "--set-permanent" => s.permanent = Some(Some(value()?)),
                "--clear-permanent" => s.permanent = Some(None),
                "--reset-login-limits" => s.reset_login_limits = true,
                "--view-only" => s.view_only = true,
                other => return Err(format!("unknown argument {other}")),
            }
        }
        Ok(s)
    }

    fn describe_input(p: Permissions) {
        if p == Permissions::NONE {
            println!("input: view only (press Enter to allow keyboard and mouse)");
        } else {
            println!("input: keyboard and mouse allowed (press Enter for view only)");
        }
    }

    /// Enter in this console toggles between full input and view only.
    fn toggle_input_on_enter(engine: HostEngine) {
        std::thread::spawn(move || {
            let mut line = String::new();
            while matches!(std::io::stdin().read_line(&mut line), Ok(n) if n > 0) {
                line.clear();
                let next = if engine.default_permissions() == Permissions::NONE {
                    Permissions::ALL
                } else {
                    Permissions::NONE
                };
                engine.set_default_permissions(next);
                engine.set_all_permissions(next);
                describe_input(next);
            }
        });
    }

    pub fn run() -> Result<(), BoxError> {
        let settings = parse_args()?;
        let store = key_store(&settings.data_dir)?;
        let identity = device_identity(store.as_ref())?;
        let policy = TemporaryPolicy::new(settings.password_length, Charset::Alphanumeric)?;
        let mut passwords = HostPasswords::load(policy, store.as_ref())?;
        if let Some(permanent) = &settings.permanent {
            passwords.set_permanent(permanent.as_deref(), store.as_ref())?;
            println!(
                "permanent password {}",
                if permanent.is_some() {
                    "saved"
                } else {
                    "cleared"
                }
            );
        }
        let has_permanent = passwords.has_permanent();
        let throttle_path = settings.data_dir.join(THROTTLE_FILE);
        if settings.reset_login_limits {
            match std::fs::remove_file(&throttle_path) {
                Ok(()) => println!("login limits reset"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let permissions = if settings.view_only {
            Permissions::NONE
        } else {
            Permissions::ALL
        };

        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(async {
            let engine = HostEngine::new(EngineParts {
                identity,
                passwords,
                throttle: throttle::load(&throttle_path, ThrottleConfig::default()),
                throttle_path: Some(throttle_path),
                settings: EngineSettings {
                    approve_mode: ApproveMode::Password,
                    approve_timeout: Duration::from_secs(60),
                    click_delay: Duration::ZERO,
                    permissions,
                    display: settings.display,
                    agent: AGENT.to_owned(),
                    listen: Some(settings.listen.to_string()),
                },
                cm: Arc::new(AutoAccept),
                video: VideoHub::new(capture_producer(settings.video)),
                displays: platform_displays(),
                input: platform_input(),
            });
            let tls = Identity::self_signed()?;
            let server = endpoint::server(settings.listen, &tls)?;
            let addr = server.local_addr()?;
            let key = engine.status().device_key;
            println!("spike-host listening on {addr}");
            println!("device key {key}  ({})", settings.data_dir.display());
            if has_permanent {
                println!("a permanent password is also set");
            }
            println!(
                "run: cargo run --release -p kanodesk-spike --bin spike-viewer -- --connect {addr} --host-key {key} --pin {}",
                tls.fingerprint()
            );
            describe_input(permissions);
            toggle_input_on_enter(engine.clone());
            engine.serve(server).await;
            Ok(())
        })
    }
}
