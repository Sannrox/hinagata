//! Hinagata server. Configuration comes from the environment; see
//! [`hinagata::config`].

use hinagata::api::{Api, serve};
use hinagata::config;
use hinagata::registry::Registry;
use std::process::ExitCode;
use std::sync::Arc;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

async fn run() -> Result<(), String> {
    let auth = config::authenticator(var)?;
    let tls = config::tls(var)?;
    let db = var("HINAGATA_DB_PATH").unwrap_or_else(|| config::DEFAULT_DB_PATH.into());
    let registry = Arc::new(
        Registry::open(std::path::Path::new(&db)).map_err(|error| format!("{db}: {error}"))?,
    );
    let listen = var("HINAGATA_LISTEN").unwrap_or_else(|| config::DEFAULT_LISTEN.into());
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|error| format!("{listen}: {error}"))?;
    eprintln!(
        "hinagata {} listening on {listen}",
        env!("CARGO_PKG_VERSION")
    );
    serve(Api::new(auth, registry), listener, tls, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|error| error.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hinagata: {error}");
            ExitCode::FAILURE
        }
    }
}
