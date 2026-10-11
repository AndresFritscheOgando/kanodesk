//! `kanodesk-idserver` binary entry point. The logic lives in the `kanodesk_idserver` library.
//!
//! ```text
//! kanodesk-idserver [serve] [--listen 0.0.0.0:21116] [--data-dir DIR]
//!                   [--relay HOST:PORT]... [--trusted-network CIDR]...
//!                   [--touch-interval SECONDS]
//!                   [--per-ip B/S] [--per-device B/S] [--new-ids-per-ip B/S]
//!                   [--per-target B/S] [--connects-per-target B/S]
//! kanodesk-idserver list               [--data-dir DIR] [--server ADDR]
//! kanodesk-idserver show ID            [--data-dir DIR] [--server ADDR]
//! kanodesk-idserver remove ID          [--data-dir DIR] [--server ADDR]
//! kanodesk-idserver relays             [--data-dir DIR]
//! kanodesk-idserver set-relays [HOST:PORT]... [--data-dir DIR] [--server ADDR]
//! ```
//!
//! Rates are `BURST/SECONDS`: a burst, then one request every SECONDS.
//! The data directory holds the SQLite database and the server signing key
//! (DPAPI, machine scope, on Windows; an owner-only file elsewhere). The
//! printed key goes into client configs.
//!
//! Admin commands work on the database directly when the server is stopped.
//! If it is running, they go through its admin API instead (`--server`,
//! default `127.0.0.1:21116`), authenticated with the server key from the
//! data directory, and take effect at once.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kanodesk_crypto::{DeviceIdentity, KeyStore};
use kanodesk_idserver::admin::{self, DeviceStatus, OnlineAdmin};
use kanodesk_idserver::db::DeviceRow;
use kanodesk_idserver::{ServerConfig, ServerError, VERSION, start};
use kanodesk_rendezvous::{DEFAULT_PORT, DeviceId};

type BoxError = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("kanodesk-idserver: {e}");
        std::process::exit(1);
    }
}

fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    let base = std::env::var_os("ProgramData").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = Some(PathBuf::from("/var/lib"));
    base.map(|b| b.join("kanodesk").join("idserver"))
        .unwrap_or_else(|| PathBuf::from("idserver-data"))
}

fn key_store(dir: &Path) -> Result<Box<dyn KeyStore>, BoxError> {
    #[cfg(windows)]
    {
        use kanodesk_crypto::keystore::{DpapiKeyStore, DpapiScope};
        Ok(Box::new(DpapiKeyStore::new(dir, DpapiScope::LocalMachine)?))
    }
    #[cfg(not(windows))]
    {
        Ok(Box::new(kanodesk_crypto::keystore::FileKeyStore::new(dir)?))
    }
}

fn print_row(row: &DeviceRow, online: Option<bool>) {
    println!(
        "{}  key {}  created {}  last seen {}  from {}{}",
        row.id,
        row.key,
        row.created_at,
        row.last_seen,
        row.last_addr.as_deref().unwrap_or("-"),
        match online {
            Some(true) => "  ONLINE",
            Some(false) => "  offline",
            None => "",
        }
    );
}

fn print_status(s: &DeviceStatus) {
    print_row(&s.row, Some(s.online));
}

struct Options {
    config: ServerConfig,
    data_dir: PathBuf,
    server: SocketAddr,
    positionals: Vec<String>,
}

fn parse(args: Vec<String>) -> Result<Options, BoxError> {
    let mut o = Options {
        config: ServerConfig::new(([0, 0, 0, 0], DEFAULT_PORT).into(), PathBuf::new()),
        data_dir: default_data_dir(),
        server: ([127, 0, 0, 1], DEFAULT_PORT).into(),
        positionals: Vec::new(),
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or(format!("{arg} needs a value"));
        let c = &mut o.config;
        match arg.as_str() {
            "--listen" => c.listen = value()?.parse()?,
            "--data-dir" => o.data_dir = PathBuf::from(value()?),
            "--server" => o.server = value()?.parse()?,
            "--relay" => c.relays.push(value()?),
            "--trusted-network" => c.trusted_networks.push(value()?.parse()?),
            "--touch-interval" => c.touch_interval = Duration::from_secs_f64(value()?.parse()?),
            "--per-ip" => c.limits.per_ip = value()?.parse()?,
            "--per-device" => c.limits.per_device = value()?.parse()?,
            "--new-ids-per-ip" => c.limits.new_ids_per_ip = value()?.parse()?,
            "--per-target" => c.limits.per_target = value()?.parse()?,
            "--connects-per-target" => c.limits.connects_per_target = value()?.parse()?,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}").into()),
            positional => o.positionals.push(positional.to_owned()),
        }
    }
    Ok(o)
}

/// Connects to the running server's admin API with the key from the data dir.
async fn online(o: &Options) -> Result<OnlineAdmin, BoxError> {
    println!("server is running; using its admin API at {}", o.server);
    let store = key_store(&o.data_dir)?;
    let seed = store
        .load(kanodesk_crypto::identity::STORE_NAME)?
        .ok_or("no server key in this data directory")?;
    let seed: &[u8; 32] = seed
        .as_slice()
        .try_into()
        .map_err(|_| "stored server key has the wrong length")?;
    let key = DeviceIdentity::from_seed(seed);
    Ok(OnlineAdmin::connect(o.server, &key).await?)
}

fn id_arg(o: &Options, command: &str) -> Result<DeviceId, BoxError> {
    match o.positionals.as_slice() {
        [id] => Ok(id.parse()?),
        _ => Err(format!("{command} needs exactly one ID").into()),
    }
}

async fn run() -> Result<(), BoxError> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let command = match args.first().map(String::as_str) {
        Some("serve" | "list" | "show" | "remove" | "relays" | "set-relays") => args.remove(0),
        _ => "serve".to_owned(),
    };
    let mut o = parse(args)?;
    std::fs::create_dir_all(&o.data_dir)?;
    o.config.database = o.data_dir.join("idserver.sqlite");
    let db = o.config.database.clone();

    match command.as_str() {
        "list" => match admin::list(&db).await {
            Err(ServerError::Locked(_)) => {
                let all = online(&o).await?.list().await?;
                all.iter().for_each(print_status);
                println!("{} devices", all.len());
            }
            rows => {
                let rows = rows?;
                rows.iter().for_each(|r| print_row(r, None));
                println!("{} devices", rows.len());
            }
        },
        "show" => {
            let id = id_arg(&o, "show")?;
            let found = match admin::show(&db, id).await {
                Err(ServerError::Locked(_)) => online(&o).await?.show(id).await?.map(|s| {
                    print_status(&s);
                }),
                row => row?.map(|r| print_row(&r, None)),
            };
            if found.is_none() {
                return Err(format!("{id} is not registered").into());
            }
        }
        "remove" => {
            let id = id_arg(&o, "remove")?;
            let removed = match admin::remove(&db, id).await {
                Err(ServerError::Locked(_)) => online(&o).await?.remove(id).await?,
                r => r?,
            };
            if !removed {
                return Err(format!("{id} is not registered").into());
            }
            println!("removed {id}; the ID is free to register again");
        }
        "relays" => {
            let relays = admin::relays(&db).await?;
            println!(
                "persisted relays: {}",
                if relays.is_empty() {
                    "none".to_owned()
                } else {
                    relays.join(", ")
                }
            );
        }
        "set-relays" => {
            let relays = o.positionals.clone();
            match admin::set_relays(&db, &relays).await {
                Err(ServerError::Locked(_)) => {
                    online(&o).await?.set_relays(&relays).await?;
                    println!("relays updated and pushed to connected devices");
                }
                r => {
                    r?;
                    println!("relays saved; used at the next start");
                }
            }
        }
        _ => {
            if !o.positionals.is_empty() {
                return Err(format!("unexpected argument {}", o.positionals[0]).into());
            }
            let store = key_store(&o.data_dir)?;
            let (signing, created) = DeviceIdentity::load_or_create(store.as_ref())?;
            let server = start(o.config, signing).await?;
            println!(
                "kanodesk-idserver {VERSION} listening on {} (UDP/QUIC)",
                server.local_addr()?
            );
            println!(
                "server key {} ({})",
                server.public_key(),
                if created { "new" } else { "loaded" }
            );
            println!("data directory {}", o.data_dir.display());
            println!("{} registered devices", server.device_count());
            let relays = server.relays();
            if relays.is_empty() {
                println!("no relay servers configured");
            } else {
                println!("relays: {}", relays.join(", "));
            }
            tokio::signal::ctrl_c().await?;
            println!("shutting down");
            server.shutdown().await;
        }
    }
    Ok(())
}
