//! Where the app keeps things, and starting its other roles.
//!
//! - config: `kanodesk_config::config::default_path()` (`kanodesk.toml`),
//!   shared by the host and the viewer;
//! - recent sessions: `peers.toml` next to the config;
//! - secrets and state: `<data>\host` for the host process (device key,
//!   permanent password, device ID, login limits), `<data>\client` for
//!   viewers (their own device key and ID: a viewer registers with the ID
//!   server separately, so it never takes over the host's registration);
//! - logs of processes started without a console: `<data>\logs`.

use std::path::PathBuf;

/// The config file.
pub fn config_path() -> Option<PathBuf> {
    kanodesk_config::config::default_path()
}

/// The recent-sessions file.
pub fn peers_path() -> Option<PathBuf> {
    kanodesk_config::config::default_dir().map(|d| d.join(kanodesk_client::peers::FILE_NAME))
}

/// The host process's secrets and state.
pub fn host_data_dir() -> Option<PathBuf> {
    kanodesk_config::secrets::default_data_dir().map(|d| d.join("host"))
}

/// Viewers' secrets and state.
pub fn client_data_dir() -> Option<PathBuf> {
    kanodesk_config::secrets::default_data_dir().map(|d| d.join("client"))
}

/// Logs of processes started without a console.
pub fn log_dir() -> Option<PathBuf> {
    kanodesk_config::secrets::default_data_dir().map(|d| d.join("logs"))
}

/// Starts this executable with `args`, without a console window.
pub fn spawn_self(args: &[&str]) -> std::io::Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command.spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_distinct_and_named() {
        let host = host_data_dir().unwrap();
        let client = client_data_dir().unwrap();
        assert_ne!(host, client);
        assert!(host.ends_with("host") && client.ends_with("client"));
        assert!(peers_path().unwrap().ends_with("peers.toml"));
        assert_eq!(
            peers_path().unwrap().parent(),
            config_path().unwrap().parent(),
            "next to the config"
        );
        assert!(log_dir().unwrap().ends_with("logs"));
    }
}
