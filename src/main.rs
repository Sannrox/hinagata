//! Hinagata server. Configuration comes from the environment; see
//! [`hinagata::config`].
//!
//! `hinagata verify --keys <trusted.json> --envelope <manifest.json>
//! [--artifact <file>] [--metadata <file>]` checks a signed version
//! manifest offline, with only the trusted public keys. The envelope file is
//! the `GetVersionManifest` response as JSON; its `metadata` is checked too.
//!
//! `hinagata export --server <url> --token-file <file> [--ca <pem>] --model
//! <model> --version <n>... --out <dir>` writes a signed bundle, and
//! `hinagata import --server <url> --token-file <file> [--ca <pem>] --keys
//! <trusted.json> --bundle <dir>` verifies and imports one
//! (docs/bundles.md).

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
    if let Some(keys) = config::trusted_keys(var)? {
        api = api.with_trusted_keys(keys);
    }
    serve(api, listener, tls, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|error| error.to_string())
}

/// Parse `--flag value` pairs. Every flag takes a value; an unknown or
/// valueless flag is refused, and so is a repeated one unless it is in
/// `repeatable`, so a missing value never silently skips a step.
fn flags<'a>(
    args: &'a [String],
    allowed: &[&str],
    repeatable: &[&str],
) -> Result<std::collections::BTreeMap<&'a str, Vec<&'a str>>, String> {
    let mut flags = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    let mut args = args.iter();
    while let Some(name) = args.next() {
        if !allowed.contains(&name.as_str()) {
            return Err(format!("unknown argument `{name}`"));
        }
        let value = args
            .next()
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .ok_or_else(|| format!("{name} needs a value"))?;
        let values = flags.entry(name.as_str()).or_default();
        if !values.is_empty() && !repeatable.contains(&name.as_str()) {
            return Err(format!("{name} is given twice"));
        }
        values.push(value.as_str());
    }
    Ok(flags)
}

/// The single value of `name`, if given.
fn one<'a>(
    flags: &std::collections::BTreeMap<&'a str, Vec<&'a str>>,
    name: &str,
) -> Option<&'a str> {
    flags.get(name).map(|values| values[0])
}

/// `hinagata verify`: offline, against trusted keys only.
fn verify(args: &[String]) -> Result<String, String> {
    let flags = flags(
        args,
        &["--keys", "--envelope", "--artifact", "--metadata"],
        &[],
    )?;
    let read = |path: &str| std::fs::read(path).map_err(|error| format!("{path}: {error}"));
    let keys = one(&flags, "--keys").ok_or("verify needs --keys <trusted keys file>")?;
    let envelope = one(&flags, "--envelope").ok_or("verify needs --envelope <manifest file>")?;
    let keys = hinagata::signing::TrustedKeys::from_json(
        &String::from_utf8(read(keys)?).map_err(|_| "the keys file is not UTF-8")?,
    )?;
    let raw = read(envelope)?;
    let envelope: hinagata::signing::Envelope =
        serde_json::from_slice(&raw).map_err(|error| format!("envelope: {error}"))?;
    let manifest = keys.verify(&envelope)?;
    let metadata = match one(&flags, "--metadata") {
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
    let artifact = one(&flags, "--artifact").map(read).transpose()?;
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

/// `hinagata export` and `hinagata import`: move versions between
/// registries as a signed bundle.
async fn bundle(command: &str, args: &[String]) -> Result<Vec<String>, String> {
    let read = |path: &str| std::fs::read(path).map_err(|error| format!("{path}: {error}"));
    let common = ["--server", "--token-file", "--ca"];
    let flags = match command {
        "export" => flags(
            args,
            &[&common[..], &["--model", "--version", "--out"]].concat(),
            &["--version"],
        )?,
        _ => flags(args, &[&common[..], &["--keys", "--bundle"]].concat(), &[])?,
    };
    let need = |name: &str| one(&flags, name).ok_or(format!("{command} needs {name}"));
    let token = String::from_utf8(read(need("--token-file")?)?)
        .map_err(|_| "the token file is not UTF-8")?;
    let ca = one(&flags, "--ca").map(read).transpose()?;
    let mut client = hinagata::bundle::Client::connect(need("--server")?, token, ca).await?;
    if command == "export" {
        let versions = flags
            .get("--version")
            .ok_or("export needs --version")?
            .iter()
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("`{value}` is not a version"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let out = std::path::Path::new(need("--out")?);
        return hinagata::bundle::export(&mut client, need("--model")?, &versions, out).await;
    }
    let keys = hinagata::signing::TrustedKeys::from_json(
        &String::from_utf8(read(need("--keys")?)?).map_err(|_| "the keys file is not UTF-8")?,
    )?;
    hinagata::bundle::import(&mut client, &keys, std::path::Path::new(need("--bundle")?)).await
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
    if let Some(command @ ("export" | "import")) = args.get(1).map(String::as_str) {
        return match bundle(command, &args[2..]).await {
            Ok(lines) => {
                for line in lines {
                    println!("{command}ed {line}");
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("hinagata {command}: {error}");
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
