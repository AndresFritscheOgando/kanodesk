//! `kanodesk-relay` binary entry point. The logic lives in the `kanodesk_relay` library.
//!
//! ```text
//! kanodesk-relay [--listen 0.0.0.0:21117] (--secret-file PATH | --open)
//!                [--session-bandwidth BYTES_PER_SECOND] [--port-range LO-HI]
//!                [--max-sessions N] [--sessions-per-ip N] [--idle-timeout SECONDS]
//! kanodesk-relay new-secret PATH
//! ```
//!
//! The secret file (64 hex digits) must be the one given to the ID server
//! with `--relay-secret-file`; the relay then only serves sessions the ID
//! server issued. `--open` skips ticket checks (local tests only).
//! Firewalls must allow UDP on the control port and on the data port range.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use kanodesk_relay::{DEFAULT_PORT, RelayConfig, VERSION, start};
use kanodesk_transport::relay::RelaySecret;

type BoxError = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("kanodesk-relay: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), BoxError> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("new-secret") {
        let path = args.get(1).ok_or("new-secret needs a file path")?;
        if Path::new(path).exists() {
            return Err(format!("{path} exists; not overwriting").into());
        }
        let secret = RelaySecret::generate()?;
        std::fs::write(path, format!("{}\n", secret.to_hex()))?;
        println!("wrote a new relay secret to {path}; give the same file to the ID server");
        return Ok(());
    }

    let mut config = RelayConfig::new(([0, 0, 0, 0], DEFAULT_PORT).into(), None);
    let mut open = false;
    let mut it = std::mem::take(&mut args).into_iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--listen" => config.listen = value()?.parse::<SocketAddr>()?,
            "--secret-file" => config.secret = Some(RelaySecret::read_file(Path::new(&value()?))?),
            "--open" => open = true,
            "--session-bandwidth" => config.session_bandwidth = value()?.parse()?,
            "--port-range" => {
                let v = value()?;
                let (lo, hi) = v.split_once('-').ok_or("--port-range is LO-HI")?;
                config.port_range = Some((lo.parse()?, hi.parse()?));
            }
            "--max-sessions" => config.max_sessions = value()?.parse()?,
            "--sessions-per-ip" => config.sessions_per_ip = value()?.parse()?,
            "--idle-timeout" => config.idle_timeout = Duration::from_secs_f64(value()?.parse()?),
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    if config.secret.is_none() && !open {
        return Err(
            "pass --secret-file (shared with the ID server), or --open for local tests".into(),
        );
    }
    let relay = start(config.clone()).await?;
    println!(
        "kanodesk-relay {VERSION} control on {} (UDP/QUIC)",
        relay.local_addr()?
    );
    println!(
        "{}; {} bytes/s per session; data ports {}",
        if config.secret.is_some() {
            "tickets required"
        } else {
            "OPEN MODE: no ticket checks"
        },
        config.session_bandwidth,
        config
            .port_range
            .map_or("any".to_owned(), |(lo, hi)| format!("{lo}-{hi}"))
    );
    tokio::signal::ctrl_c().await?;
    println!("shutting down");
    relay.shutdown().await;
    Ok(())
}
