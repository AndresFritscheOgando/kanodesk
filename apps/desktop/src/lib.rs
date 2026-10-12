//! # kanodesk-desktop
//!
//! The `kanodesk` app. One binary, several roles (02 §1.1):
//!
//! - `kanodesk --server`: the host process ([`server`]): config, device key,
//!   the host engine on the direct-access port, and the IPC endpoint for the
//!   connection manager.
//! - `kanodesk --cm`: the connection-manager window ([`cm`], state in
//!   [`cm_model`]): who is connecting, Accept/Deny, live permissions,
//!   Disconnect.
//! - `kanodesk --cm-cmd …`: the same from the command line ([`cm_cli`]).
//!
//! The main window and the remote viewer come with prompt 10 (until then
//! the viewer is `spike-viewer`). The CM window uses egui (eframe, glow
//! backend); the toolkit for the main window is decided there.
//!
//! Specified by:
//! - `docs/15-ui-and-ffi.md`
//! - `docs/02-architecture.md`
//! - `docs/11-host-server-side.md`
//!
//! Status: host process and connection manager.

pub mod cm;
pub mod cm_cli;
pub mod cm_model;
pub mod server;

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
  kanodesk --server [--config FILE] [--data-dir DIR] [--listen ADDR]
                    [--approve-mode password|click|both] [--display N] [--no-cm-launch]
  kanodesk --cm
  kanodesk --cm-cmd list | accept ID | deny ID | disconnect ID | perms ID keyboard=on|off mouse=on|off";

/// Which role the process plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
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
        Some("--server") => server::parse(&args[1..]).map(Role::Server),
        Some("--cm") if args.len() == 1 => Ok(Role::Cm),
        Some("--cm") => Err("--cm takes no arguments".into()),
        Some("--cm-cmd") => cm_cli::parse(&args[1..]).map(Role::CmCmd),
        None | Some("--help") | Some("-h") => Ok(Role::Help),
        Some(other) => Err(format!("unknown role {other}")),
    }
}

/// Runs the process; returns its exit code.
pub fn main_with_args(args: &[String]) -> i32 {
    match parse_role(args) {
        Ok(Role::Server(a)) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .try_init();
            match server::run(a) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("kanodesk --server: {e}");
                    1
                }
            }
        }
        Ok(Role::Cm) => match cm::run() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("kanodesk --cm: {e}");
                1
            }
        },
        Ok(Role::CmCmd(cmd)) => match cm_cli::run(cmd) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("kanodesk --cm-cmd: {e}");
                1
            }
        },
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
        assert_eq!(parse_role(&a(&[])).unwrap(), Role::Help);
        assert_eq!(parse_role(&a(&["--cm"])).unwrap(), Role::Cm);
        assert!(parse_role(&a(&["--cm", "x"])).is_err());
        assert!(matches!(
            parse_role(&a(&["--server", "--approve-mode", "both"])).unwrap(),
            Role::Server(s) if s.approve_mode.as_deref() == Some("both")
        ));
        assert!(parse_role(&a(&["--frobnicate"])).is_err());
        assert!(!VERSION.is_empty() && AGENT.starts_with("kanodesk/"));
    }
}
