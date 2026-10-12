//! `kanodesk --server`: the host process (02 §1.1).
//!
//! Reads the config (`kanodesk_config`, created with defaults on first run),
//! loads the device key and passwords from the key store, and runs the host
//! engine on the direct-access port. The connection manager runs as a
//! separate process (`kanodesk --cm`), started on demand when a session
//! appears; it talks to this process over IPC. With `hide_cm` (password-only
//! hosts) sessions are accepted without a window.
//!
//! ```text
//! kanodesk --server [--config FILE] [--data-dir DIR] [--listen ADDR]
//!                   [--approve-mode password|click|both] [--display N] [--no-cm-launch]
//! ```
//!
//! Command-line values override the config for this run only.

use std::path::PathBuf;

/// Parsed `--server` arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerArgs {
    /// Config file (default: the per-user config directory).
    pub config: Option<PathBuf>,
    /// Secrets and state directory (default: the per-user data directory).
    pub data_dir: Option<PathBuf>,
    /// Direct-access address (overrides `direct-access-listen`).
    pub listen: Option<String>,
    /// Approve mode (overrides `approve-mode`).
    pub approve_mode: Option<String>,
    /// Display to show.
    pub display: Option<u32>,
    /// Do not start the CM process (it must be started by hand).
    pub no_cm_launch: bool,
}

/// Parses the arguments after `--server`.
pub fn parse(args: &[String]) -> Result<ServerArgs, String> {
    let mut a = ServerArgs::default();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--config" => a.config = Some(value()?.into()),
            "--data-dir" => a.data_dir = Some(value()?.into()),
            "--listen" => a.listen = Some(value()?),
            "--approve-mode" => {
                let v = value()?;
                if !kanodesk_config::options::APPROVE_MODES.contains(&v.as_str()) {
                    return Err(format!("--approve-mode: {v} (password, click or both)"));
                }
                a.approve_mode = Some(v);
            }
            "--display" => {
                a.display = Some(value()?.parse().map_err(|e| format!("--display: {e}"))?)
            }
            "--no-cm-launch" => a.no_cm_launch = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

/// Runs the host process until Ctrl+C.
#[cfg(windows)]
pub fn run(args: ServerArgs) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::sync::Arc;

    use kanodesk_config::{Config, HostSettings, secrets};
    use kanodesk_crypto::DeviceIdentity;
    use kanodesk_crypto::password::{Charset, HostPasswords, TemporaryPolicy};
    use kanodesk_crypto::throttle::{self, ThrottleConfig};
    use kanodesk_host::cm::Launcher;
    use kanodesk_host::engine::{EngineParts, EngineSettings, platform_displays, platform_input};
    use kanodesk_host::video::{VideoSettings, capture_producer};
    use kanodesk_host::{
        AutoAccept, ConnectionManager, HostEngine, IpcConnectionManager, VideoHub,
    };
    use kanodesk_transport::endpoint;
    use kanodesk_transport::tls::Identity;

    let config_path = match args.config.clone() {
        Some(p) => p,
        None => kanodesk_config::config::default_path().ok_or("no config directory")?,
    };
    let mut config = Config::load(&config_path)?;
    if !config_path.exists() {
        // First run: write the defaults so they can be edited.
        config.save()?;
    }
    if let Some(mode) = &args.approve_mode {
        config.set("approve-mode", mode)?;
    }
    if let Some(listen) = &args.listen {
        config.set("direct-access-listen", listen)?;
        config.set("direct-server", "Y")?;
    }
    let settings = HostSettings::from_config(&config);

    let data_dir = match args.data_dir.clone() {
        Some(d) => d,
        None => secrets::default_data_dir()
            .ok_or("no data directory")?
            .join("host"),
    };
    let store = secrets::key_store(&data_dir)?;
    let (identity, _) = DeviceIdentity::load_or_create(store.as_ref())?;
    let policy = TemporaryPolicy::new(settings.temporary_password_length, Charset::Alphanumeric)?;
    let passwords = HostPasswords::load(policy, store.as_ref())?;
    let throttle_path = data_dir.join("login-throttle");

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        let (cm, commands): (Arc<dyn ConnectionManager>, _) = if settings.hide_cm {
            (Arc::new(AutoAccept), None)
        } else {
            let launcher: Option<Launcher> =
                (!args.no_cm_launch).then(|| Box::new(launch_cm) as Launcher);
            let (cm, commands) = IpcConnectionManager::start(launcher)?;
            (cm, Some(commands))
        };
        let mut engine_settings = EngineSettings::from_host(&settings, crate::AGENT);
        engine_settings.display = args.display;
        let engine = HostEngine::new(EngineParts {
            identity,
            passwords,
            throttle: throttle::load(&throttle_path, ThrottleConfig::default()),
            throttle_path: Some(throttle_path),
            settings: engine_settings,
            cm,
            video: VideoHub::new(capture_producer(VideoSettings {
                fps: settings.fps,
                bitrate_kbps: settings.bitrate_kbps,
                speed: 6,
            })),
            displays: platform_displays(),
            input: platform_input(),
        });
        if let Some(mut commands) = commands {
            let engine = engine.clone();
            tokio::spawn(async move {
                while let Some(cmd) = commands.recv().await {
                    engine.apply(cmd);
                }
            });
        }

        let status = engine.status();
        println!("kanodesk {} host", crate::VERSION);
        println!("config {}", config_path.display());
        println!("device key {}", status.device_key);
        println!("approve mode {}", status.approve_mode);
        if let Some(pw) = &status.temporary_password {
            println!("temporary password {pw}");
        }
        if !settings.direct_server {
            println!("direct access is off (direct-server = N); nothing to accept yet");
            tokio::signal::ctrl_c().await?;
            return Ok(());
        }
        let listen: std::net::SocketAddr = settings
            .direct_listen
            .parse()
            .map_err(|e| format!("direct-access-listen {}: {e}", settings.direct_listen))?;
        let server = endpoint::server(listen, &Identity::self_signed()?)?;
        println!("listening on {}", server.local_addr()?);
        println!(
            "viewer: spike-viewer --connect <this address> --host-key {} {}",
            status.device_key,
            if settings.approve_mode.needs_password() {
                "(then the password)"
            } else {
                "--ask-host"
            }
        );
        tokio::select! {
            _ = engine.serve(server) => {}
            _ = tokio::signal::ctrl_c() => println!("stopping"),
        }
        Ok(())
    })
}

/// Starts `kanodesk --cm` next to this executable, without a console window.
#[cfg(windows)]
fn launch_cm() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(std::env::current_exe()?)
        .arg("--cm")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
}

/// The host process needs a capture backend; only Windows has one so far.
#[cfg(not(windows))]
pub fn run(_args: ServerArgs) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("the host needs a capture backend; only Windows has one so far".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn server_arguments() {
        assert_eq!(parse(&[]).unwrap(), ServerArgs::default());
        let a = parse(&args(&[
            "--approve-mode",
            "click",
            "--listen",
            "127.0.0.1:9000",
            "--display",
            "1",
            "--no-cm-launch",
            "--data-dir",
            "x",
        ]))
        .unwrap();
        assert_eq!(a.approve_mode.as_deref(), Some("click"));
        assert_eq!(a.listen.as_deref(), Some("127.0.0.1:9000"));
        assert_eq!(a.display, Some(1));
        assert!(a.no_cm_launch);
        assert_eq!(a.data_dir, Some(PathBuf::from("x")));
        assert!(parse(&args(&["--approve-mode", "sometimes"])).is_err());
        assert!(parse(&args(&["--display"])).is_err());
        assert!(parse(&args(&["--bogus"])).is_err());
    }
}
