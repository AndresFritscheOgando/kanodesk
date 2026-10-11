//! `kanodesk-cli` binary entry point. The logic lives in the `kanodesk_cli` library.
//!
//! ```text
//! kanodesk-cli p2p-host    --server HOST[:PORT] --server-key KEY [--port N] [--data-dir DIR]
//! kanodesk-cli p2p-connect ID --server HOST[:PORT] --server-key KEY
//!                          [--no-lan] [--no-direct] [--no-relay] [--relay-only]
//!                          [--direct-timeout SECONDS] [--port N] [--data-dir DIR]
//! ```

use kanodesk_cli::p2p::{self, Role};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("p2p-host") => match p2p::parse(Role::Host, &args[1..]) {
            Ok(a) => p2p::run_host(a).await,
            Err(e) => Err(e.into()),
        },
        Some("p2p-connect") => match p2p::parse(Role::Connect, &args[1..]) {
            Ok(a) => p2p::run_connect(a).await,
            Err(e) => Err(e.into()),
        },
        Some("--version") => {
            println!("kanodesk-cli {}", kanodesk_cli::VERSION);
            Ok(())
        }
        _ => Err(
            "usage: kanodesk-cli (p2p-host | p2p-connect ID) --server HOST[:PORT] --server-key KEY"
                .into(),
        ),
    };
    if let Err(e) = result {
        eprintln!("kanodesk-cli: {e}");
        std::process::exit(1);
    }
}
