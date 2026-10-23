//! Hinagata server. Configuration comes from the environment; see
//! [`hinagata::config`].
//!
//! `hinagata verify --keys <trusted.json> --envelope <manifest.json>
//! [--artifact <file>] [--metadata <file>]` checks a signed version
//! manifest offline, with only the trusted public keys. The envelope file is
//! the `GetVersionManifest` response as JSON; its `metadata` is checked too.

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
    let artifacts = config::artifact_store(var)?;
    let setup = hinagata::api::Setup {
        auth: if var("HINAGATA_STATIC_TOKEN_FILE").is_some() {
            "static_token".into()
        } else {
            "jwt".into()
        },
        artifact_store: if var("HINAGATA_S3_BUCKET").is_some() {
            "s3".into()
        } else {
            "local".into()
        },
        tls: tls.is_some(),
        mutual_tls: var("HINAGATA_TLS_CLIENT_CA").is_some(),
    };
    let mut api = Api::new(auth, registry, artifacts).with_setup(setup);
    if let Some(signer) = config::signer(var)? {
        api = api.with_signer(signer);
    }
    if let Some(hook) = config::approval_hook(var)? {
        api = api.with_approval_hook(hook);
    }
    serve(api, listener, tls, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|error| error.to_string())
}

/// `hinagata verify`: offline, against trusted keys only. Every flag takes
/// a path; an unknown, repeated or valueless flag is refused, so a missing
/// path never silently skips a check.
fn verify(args: &[String]) -> Result<String, String> {
    let mut flags = std::collections::BTreeMap::new();
    let mut args = args.iter();
    while let Some(name) = args.next() {
        if !["--keys", "--envelope", "--artifact", "--metadata"].contains(&name.as_str()) {
            return Err(format!("unknown argument `{name}`"));
        }
        let value = args
            .next()
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .ok_or_else(|| format!("{name} needs a path"))?;
        if flags.insert(name.as_str(), value.as_str()).is_some() {
            return Err(format!("{name} is given twice"));
        }
    }
    let read = |path: &str| std::fs::read(path).map_err(|error| format!("{path}: {error}"));
    let keys = flags
        .get("--keys")
        .ok_or("verify needs --keys <trusted keys file>")?;
    let envelope = flags
        .get("--envelope")
        .ok_or("verify needs --envelope <manifest file>")?;
    let keys = hinagata::signing::TrustedKeys::from_json(
        &String::from_utf8(read(keys)?).map_err(|_| "the keys file is not UTF-8")?,
    )?;
    let raw = read(envelope)?;
    let envelope: hinagata::signing::Envelope =
        serde_json::from_slice(&raw).map_err(|error| format!("envelope: {error}"))?;
    let manifest = keys.verify(&envelope)?;
    let metadata = match flags.get("--metadata") {
        Some(path) => read(path)?,
        None => {
            #[derive(serde::Deserialize)]
            struct Bundled {
                metadata: String,
            }
            serde_json::from_slice::<Bundled>(&raw)
                .map_err(|_| "the envelope has no metadata; pass --metadata <file>")?
                .metadata
                .into_bytes()
        }
    };
    let artifact = flags.get("--artifact").map(|path| read(path)).transpose()?;
    hinagata::signing::check_contents(&manifest, artifact.as_deref(), Some(&metadata))?;
    Ok(format!(
        "verified {}{} version {} of `{}` in `{}`, signed by `{}`",
        if artifact.is_some() {
            "artifact and metadata of "
        } else {
            "metadata of "
        },
        manifest.artifact_digest,
        manifest.version,
        manifest.model,
        manifest.namespace,
        envelope.key_id
    ))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("verify") {
        return match verify(&args[2..]) {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("hinagata verify: {error}");
                ExitCode::FAILURE
            }
        };
    }
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hinagata: {error}");
            ExitCode::FAILURE
        }
    }
}
