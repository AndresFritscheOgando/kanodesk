//! # kanodesk-desktop
//!
//! The `kanodesk` app. One binary, several roles (02 §1.1), each its own
//! process:
//!
//! - `kanodesk`: the main window ([`main_window`], state in
//!   [`main_model`]): this device's ID and one-time password, a connect
//!   box, recent sessions and settings. It starts the host process if none
//!   runs.
//!   `kanodesk --settings` opens it on Settings (a viewer's error dialog
//!   does that for ID-server problems).
//! - `kanodesk --connect ID`: a viewer window ([`viewer`], state in
//!   [`viewer_model`]): the remote screen with a toolbar (display, fit or
//!   original size, quality, Ctrl+Alt+Del, disconnect) and the status and
//!   error dialogs.
//! - `kanodesk --server`: the host process ([`server`]): config, device key,
//!   the host engine reachable by ID and on the direct-access port, and the
//!   IPC endpoints for the main window and the connection manager.
//! - `kanodesk --cm`: the connection-manager window ([`cm`], state in
//!   [`cm_model`]): who is connecting, Accept/Deny, live permissions,
//!   Disconnect.
//! - `kanodesk --cm-cmd …`: the same from the command line ([`cm_cli`]).
//!
//! All windows share one look: [`theme`] (colours, text sizes, spacing),
//! [`widgets`] (buttons, cards, status lines) and [`icon`].
//!
//! The windows are thin: sessions live in `kanodesk_client::session`, the
//! host in `kanodesk_host`. The main window and the CM use egui through
//! eframe (glow); the viewer drives egui itself on top of the wgpu video
//! renderer, so it keeps raw winit input (left/right modifiers, media keys,
//! the keyboard grab) that eframe does not expose.
//!
//! Specified by:
//! - `docs/15-ui-and-ffi.md`
//! - `docs/02-architecture.md`
//! - `docs/11-host-server-side.md`
//!
//! Status: main window, viewer, host process and connection manager.

pub mod cm;
pub mod cm_cli;
pub mod cm_model;
pub mod icon;
pub mod main_model;
pub mod main_window;
pub mod paths;
pub mod server;
pub mod theme;
pub mod viewer;
pub mod viewer_model;
pub mod widgets;

/// Package version, shown by the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Agent string in `Hello` and `PeerInfo`.
pub const AGENT: &str = concat!("kanodesk/", env!("CARGO_PKG_VERSION"));

/// Spec documents, relative to the workspace root, that describe this crate's behaviour.
pub const SPEC_DOCS: &[&str] = &[
    "docs/15-ui-and-ffi.md",
    "docs/02-architecture.md",
    "docs/11-host-server-side.md",
];

const USAGE: &str = "usage:
  kanodesk                      the main window
  kanodesk --settings           the main window, on Settings
  kanodesk --connect ID         a viewer window for the device with this ID
  kanodesk --server [--config FILE] [--data-dir DIR] [--listen ADDR] [--port N]
                    [--approve-mode password|click|both] [--display N] [--no-cm-launch]
                    [--log FILE]
  kanodesk --cm
  kanodesk --cm-cmd list | accept ID | deny ID | disconnect ID | perms ID keyboard=on|off mouse=on|off
  kanodesk --help";

/// Which role the process plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// The main window.
    Main,
    /// The main window, opened on Settings (from a viewer's error dialog).
    Settings,
    /// A viewer window for this device ID.
    Connect(kanodesk_rendezvous::DeviceId),
    /// The host process.
    Server(server::ServerArgs),
    /// The connection-manager window.
    Cm,
    /// One connection-manager command.
    CmCmd(cm_cli::CmCmd),
    /// Print the usage.
    Help,
}

/// Picks the role from the command line (without the program name).
pub fn parse_role(args: &[String]) -> Result<Role, String> {
    match args.first().map(String::as_str) {
        None => Ok(Role::Main),
        Some("--settings") if args.len() == 1 => Ok(Role::Settings),
        Some("--settings") => Err("--settings takes no arguments".into()),
        Some("--connect") => match &args[1..] {
            [id] => id
                .parse()
                .map(Role::Connect)
                .map_err(|e| format!("--connect {id}: {e}")),
            _ => Err("--connect takes one device ID".into()),
        },
        Some("--server") => server::parse(&args[1..]).map(Role::Server),
        Some("--cm") if args.len() == 1 => Ok(Role::Cm),
        Some("--cm") => Err("--cm takes no arguments".into()),
        Some("--cm-cmd") => cm_cli::parse(&args[1..]).map(Role::CmCmd),
        Some("--help") | Some("-h") => Ok(Role::Help),
        Some(other) => Err(format!("unknown role {other}")),
    }
}

/// Logs to the console, or to `file` (appended) when given.
fn init_logging(file: Option<&std::path::Path>) {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let opened = file.and_then(|f| {
        if let Some(dir) = f.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(f)
            .ok()
    });
    let _ = match opened {
        Some(f) => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .try_init(),
        None => tracing_subscriber::fmt().with_env_filter(filter).try_init(),
    };
}

/// Runs the process; returns its exit code.
pub fn main_with_args(args: &[String]) -> i32 {
    let report = |role: &str, r: Result<(), String>| match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("kanodesk{role}: {e}");
            tracing::error!("kanodesk{role}: {e}");
            1
        }
    };
    match parse_role(args) {
        Ok(Role::Main) => report("", main_window::run(main_model::Page::Home)),
        Ok(Role::Settings) => report(" --settings", main_window::run(main_model::Page::Settings)),
        Ok(Role::Connect(id)) => {
            init_logging(paths::log_dir().map(|d| d.join("viewer.log")).as_deref());
            report(" --connect", viewer::run(id))
        }
        Ok(Role::Server(a)) => {
            init_logging(a.log.as_deref());
            report(" --server", server::run(a).map_err(|e| e.to_string()))
        }
        Ok(Role::Cm) => report(" --cm", cm::run()),
        Ok(Role::CmCmd(cmd)) => report(" --cm-cmd", cm_cli::run(cmd)),
        Ok(Role::Help) => {
            println!(
                "kanodesk {VERSION}
{USAGE}"
            );
            0
        }
        Err(e) => {
            eprintln!(
                "kanodesk: {e}
{USAGE}"
            );
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn spec_docs_exist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for doc in SPEC_DOCS {
            assert!(root.join(doc).is_file(), "missing spec doc {doc}");
        }
    }

    #[test]
    fn roles() {
        let a = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_role(&a(&[])).unwrap(), Role::Main);
        assert_eq!(parse_role(&a(&["--help"])).unwrap(), Role::Help);
        assert_eq!(parse_role(&a(&["--settings"])).unwrap(), Role::Settings);
        assert!(parse_role(&a(&["--settings", "x"])).is_err());
        assert_eq!(parse_role(&a(&["--cm"])).unwrap(), Role::Cm);
        assert!(parse_role(&a(&["--cm", "x"])).is_err());
        assert_eq!(
            parse_role(&a(&["--connect", "123456789"])).unwrap(),
            Role::Connect("123456789".parse().unwrap())
        );
        assert!(parse_role(&a(&["--connect"])).is_err());
        assert!(parse_role(&a(&["--connect", "12"])).is_err());
        assert!(parse_role(&a(&["--connect", "123456789", "x"])).is_err());
        assert!(matches!(
            parse_role(&a(&["--server", "--approve-mode", "both"])).unwrap(),
            Role::Server(s) if s.approve_mode.as_deref() == Some("both")
        ));
        assert!(parse_role(&a(&["--frobnicate"])).is_err());
        assert!(!VERSION.is_empty() && AGENT.starts_with("kanodesk/"));
    }
}
