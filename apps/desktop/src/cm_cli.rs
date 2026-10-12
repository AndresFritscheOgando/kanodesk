//! `kanodesk --cm-cmd`: the connection manager from the command line, for
//! scripts, tests and hosts without a desktop session.
//!
//! ```text
//! kanodesk --cm-cmd list
//! kanodesk --cm-cmd accept ID | deny ID | disconnect ID
//! kanodesk --cm-cmd perms ID keyboard=on|off mouse=on|off
//! ```
//!
//! It connects like the CM window (same endpoint, same messages), does one
//! thing and exits. The host's Accept delay applies to it as well.

use std::time::Duration;

use kanodesk_ipc::cm::{
    CM_PROTOCOL, CM_ROLE, CmToHost, HostToCm, Perms, SessionInfo, SessionState,
};

/// One command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmCmd {
    /// Print the host status and sessions.
    List,
    /// Accept a waiting session with the permissions it was offered.
    Accept(u64),
    /// Deny a waiting session.
    Deny(u64),
    /// End a session.
    Disconnect(u64),
    /// Change a session's permissions.
    Perms(u64, Option<bool>, Option<bool>),
}

/// Parses the arguments after `--cm-cmd`.
pub fn parse(args: &[String]) -> Result<CmCmd, String> {
    let id = |i: usize| -> Result<u64, String> {
        args.get(i)
            .ok_or("a session id is needed")?
            .parse()
            .map_err(|e| format!("session id: {e}"))
    };
    let on_off = |v: &str| match v {
        "on" | "y" | "yes" | "true" => Ok(true),
        "off" | "n" | "no" | "false" => Ok(false),
        other => Err(format!("{other}: use on or off")),
    };
    let cmd = match args.first().map(String::as_str) {
        Some("list") => CmCmd::List,
        Some("accept") => CmCmd::Accept(id(1)?),
        Some("deny") => CmCmd::Deny(id(1)?),
        Some("disconnect") => CmCmd::Disconnect(id(1)?),
        Some("perms") => {
            let (mut keyboard, mut mouse) = (None, None);
            for a in &args[2..] {
                match a.split_once('=') {
                    Some(("keyboard", v)) => keyboard = Some(on_off(v)?),
                    Some(("mouse", v)) => mouse = Some(on_off(v)?),
                    _ => return Err(format!("{a}: use keyboard=on|off or mouse=on|off")),
                }
            }
            CmCmd::Perms(id(1)?, keyboard, mouse)
        }
        Some(other) => return Err(format!("unknown command {other}")),
        None => return Err("list, accept, deny, disconnect or perms".into()),
    };
    let extra = match cmd {
        CmCmd::List => 1,
        CmCmd::Perms(..) => args.len(),
        _ => 2,
    };
    if args.len() > extra {
        return Err(format!("unexpected {}", args[extra]));
    }
    Ok(cmd)
}

fn line(s: &SessionInfo) -> String {
    let state = match &s.state {
        SessionState::AwaitingApproval { .. } => "waiting".to_owned(),
        SessionState::Active => "active".to_owned(),
        SessionState::Ended { reason } => format!("ended: {reason}"),
    };
    format!(
        "{:>4}  {:<10} {}@{} ({}) from {}  keyboard {} mouse {}",
        s.id,
        state,
        s.username,
        s.hostname,
        s.platform,
        s.address,
        if s.permissions.keyboard { "on" } else { "off" },
        if s.permissions.mouse { "on" } else { "off" },
    )
}

/// The message for `cmd`, given the host's sessions.
pub fn message(cmd: &CmCmd, sessions: &[SessionInfo]) -> Result<Option<CmToHost>, String> {
    let find = |id: u64| {
        sessions
            .iter()
            .find(|s| s.id == id)
            .ok_or(format!("no session {id}"))
    };
    Ok(match *cmd {
        CmCmd::List => None,
        CmCmd::Accept(id) => Some(CmToHost::Decide {
            id,
            accept: true,
            permissions: find(id)?.permissions,
        }),
        CmCmd::Deny(id) => Some(CmToHost::Decide {
            id,
            accept: false,
            permissions: find(id)?.permissions,
        }),
        CmCmd::Disconnect(id) => {
            find(id)?;
            Some(CmToHost::Disconnect { id })
        }
        CmCmd::Perms(id, keyboard, mouse) => {
            let now = find(id)?.permissions;
            Some(CmToHost::SetPermissions {
                id,
                permissions: Perms {
                    keyboard: keyboard.unwrap_or(now.keyboard),
                    mouse: mouse.unwrap_or(now.mouse),
                },
            })
        }
    })
}

/// Runs one command against the host of this Windows session.
pub fn run(cmd: CmCmd) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async move {
        let name = kanodesk_ipc::endpoint_name(CM_ROLE);
        let conn = kanodesk_ipc::connect(&name)
            .await
            .map_err(|e| format!("no KanoDesk host running here ({e})"))?;
        let (mut tx, mut rx) = conn.split();
        tx.send(&CmToHost::Hello {
            protocol: CM_PROTOCOL,
        })
        .await
        .map_err(|e| e.to_string())?;
        let Some(HostToCm::Hello {
            status, sessions, ..
        }) = rx.recv().await.map_err(|e| e.to_string())?
        else {
            return Err("unexpected reply from the host".into());
        };
        if cmd == CmCmd::List {
            println!(
                "host {} · approve mode {}{}",
                status.device_key,
                status.approve_mode,
                status
                    .temporary_password
                    .map(|p| format!(" · temporary password {p}"))
                    .unwrap_or_default()
            );
            if sessions.is_empty() {
                println!("no sessions");
            }
            for s in &sessions {
                println!("{}", line(s));
            }
            return Ok(());
        }
        if let Some(msg) = message(&cmd, &sessions)? {
            tx.send(&msg).await.map_err(|e| e.to_string())?;
            // Let the host act before the pipe closes.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn session(id: u64) -> SessionInfo {
        SessionInfo {
            id,
            device_key: String::new(),
            hostname: "H".into(),
            username: "u".into(),
            platform: "windows".into(),
            agent: String::new(),
            address: "1.2.3.4:5".into(),
            password: None,
            started_unix_ms: 0,
            permissions: Perms {
                keyboard: true,
                mouse: true,
            },
            state: SessionState::Active,
        }
    }

    #[test]
    fn commands_parse() {
        assert_eq!(parse(&args(&["list"])).unwrap(), CmCmd::List);
        assert_eq!(parse(&args(&["accept", "3"])).unwrap(), CmCmd::Accept(3));
        assert_eq!(parse(&args(&["deny", "3"])).unwrap(), CmCmd::Deny(3));
        assert_eq!(
            parse(&args(&["disconnect", "4"])).unwrap(),
            CmCmd::Disconnect(4)
        );
        assert_eq!(
            parse(&args(&["perms", "4", "keyboard=off"])).unwrap(),
            CmCmd::Perms(4, Some(false), None)
        );
        for bad in [
            &["accept"][..],
            &["accept", "x"],
            &["list", "extra"],
            &["perms", "1", "volume=up"],
            &["perms", "1", "mouse=maybe"],
            &["reboot"],
            &[],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn messages_use_the_sessions() {
        let sessions = [session(4)];
        assert_eq!(message(&CmCmd::List, &sessions).unwrap(), None);
        assert_eq!(
            message(&CmCmd::Perms(4, Some(false), None), &sessions).unwrap(),
            Some(CmToHost::SetPermissions {
                id: 4,
                permissions: Perms {
                    keyboard: false,
                    mouse: true
                }
            })
        );
        assert!(matches!(
            message(&CmCmd::Accept(4), &sessions).unwrap(),
            Some(CmToHost::Decide { accept: true, .. })
        ));
        assert!(message(&CmCmd::Disconnect(5), &sessions).is_err());
        assert!(line(&sessions[0]).contains("active"));
    }
}
