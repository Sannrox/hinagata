//! Server configuration from the environment, read through `var` so it can
//! be tested without touching the process environment.
//!
//! - `HINAGATA_LISTEN`: address for gRPC and HTTP/JSON, default
//!   `127.0.0.1:7700`.
//! - Auth, one of (the server refuses to start without one):
//!   - `HINAGATA_STATIC_TOKEN_FILE` and `HINAGATA_STATIC_NAMESPACE`: one
//!     shared token for standalone or development use;
//!   - `HINAGATA_JWKS_FILE` and `HINAGATA_JWT_AUDIENCE`, optionally
//!     `HINAGATA_JWT_ISSUER` and `HINAGATA_TENANT_CLAIM` (default `tenant`):
//!     JWTs whose tenant claim is the namespace, verified against a local
//!     JWKS file, so the server starts and runs offline.
//! - `HINAGATA_DB_PATH`: the registry database file, default
//!   `hinagata.db`. One server owns it.
//! - Artifacts: `HINAGATA_ARTIFACT_DIR` (default `artifacts`) on local disk,
//!   or `HINAGATA_S3_BUCKET` for S3-compatible storage, with
//!   `HINAGATA_S3_ENDPOINT` for a non-AWS store and the standard `AWS_*`
//!   credentials. `HINAGATA_MAX_ARTIFACT_BYTES` caps one artifact (default
//!   1 GiB).
//! - `HINAGATA_SIGNING_KEY_FILE` (the 32-byte Ed25519 seed as 64 hex
//!   characters) with `HINAGATA_SIGNING_KEY_ID` signs version manifests.
//! - `HINAGATA_APPROVAL_URL` asks that approval hook about every stage
//!   change, sending the bearer token in `HINAGATA_APPROVAL_TOKEN_FILE` when
//!   set, and waiting `HINAGATA_APPROVAL_TIMEOUT_MS` (default 10 s, at most
//!   60 s) before denying.
//! - `HINAGATA_TLS_CERT` and `HINAGATA_TLS_KEY` (PEM) serve over TLS; adding
//!   `HINAGATA_TLS_CLIENT_CA` requires client certificates (mTLS).

use crate::artifacts::{ArtifactStore, DEFAULT_MAX_ARTIFACT_BYTES};
use crate::auth::{Authenticator, Jwt, StaticToken};
use std::sync::Arc;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7700";
pub const DEFAULT_DB_PATH: &str = "hinagata.db";
pub const DEFAULT_ARTIFACT_DIR: &str = "artifacts";

/// The artifact store the environment configures.
pub fn artifact_store(var: impl Fn(&str) -> Option<String>) -> Result<Arc<ArtifactStore>, String> {
    let max_bytes = match var("HINAGATA_MAX_ARTIFACT_BYTES") {
        None => DEFAULT_MAX_ARTIFACT_BYTES,
        Some(value) => value
            .parse()
            .ok()
            .filter(|bytes| *bytes > 0)
            .ok_or("HINAGATA_MAX_ARTIFACT_BYTES must be a positive number of bytes")?,
    };
    let store = match var("HINAGATA_S3_BUCKET") {
        Some(bucket) => {
            ArtifactStore::s3(&bucket, var("HINAGATA_S3_ENDPOINT").as_deref(), max_bytes)?
        }
        None => {
            let dir = var("HINAGATA_ARTIFACT_DIR").unwrap_or_else(|| DEFAULT_ARTIFACT_DIR.into());
            ArtifactStore::local(std::path::Path::new(&dir), max_bytes)?
        }
    };
    Ok(Arc::new(store))
}

fn read(var: &impl Fn(&str) -> Option<String>, name: &str) -> Result<Option<String>, String> {
    var(name)
        .map(|path| std::fs::read_to_string(&path).map_err(|error| format!("{name}: {error}")))
        .transpose()
}

/// The authentication hook the environment configures.
pub fn authenticator(
    var: impl Fn(&str) -> Option<String>,
) -> Result<Arc<dyn Authenticator>, String> {
    if let Some(token) = read(&var, "HINAGATA_STATIC_TOKEN_FILE")? {
        let namespace =
            var("HINAGATA_STATIC_NAMESPACE").ok_or("HINAGATA_STATIC_NAMESPACE is required")?;
        let token = token.trim();
        if token.is_empty() {
            return Err("HINAGATA_STATIC_TOKEN_FILE is empty".into());
        }
        return Ok(Arc::new(StaticToken::new(token, namespace)));
    }
    if let Some(jwks) = read(&var, "HINAGATA_JWKS_FILE")? {
        let audience = var("HINAGATA_JWT_AUDIENCE").ok_or("HINAGATA_JWT_AUDIENCE is required")?;
        let claim = var("HINAGATA_TENANT_CLAIM").unwrap_or_else(|| "tenant".into());
        return Ok(Arc::new(Jwt::new(
            &jwks,
            audience,
            var("HINAGATA_JWT_ISSUER"),
            claim,
        )?));
    }
    Err("no authentication configured: set HINAGATA_STATIC_TOKEN_FILE or HINAGATA_JWKS_FILE".into())
}

/// The manifest signer, if a signing key is configured. A key file without
/// a key id, or the reverse, is refused.
pub fn signer(
    var: impl Fn(&str) -> Option<String>,
) -> Result<Option<crate::signing::Signer>, String> {
    match (
        read(&var, "HINAGATA_SIGNING_KEY_FILE")?,
        var("HINAGATA_SIGNING_KEY_ID"),
    ) {
        (Some(seed), Some(key_id)) => {
            crate::signing::Signer::from_seed_hex(key_id, &seed).map(Some)
        }
        (None, None) => Ok(None),
        _ => Err("signing needs both HINAGATA_SIGNING_KEY_FILE and HINAGATA_SIGNING_KEY_ID".into()),
    }
}

/// The approval hook, if configured. A token file or timeout without a URL
/// is refused, so a half-configured hook never goes unnoticed.
pub fn approval_hook(
    var: impl Fn(&str) -> Option<String>,
) -> Result<Option<crate::promotion::ApprovalHook>, String> {
    let token = read(&var, "HINAGATA_APPROVAL_TOKEN_FILE")?;
    let timeout = var("HINAGATA_APPROVAL_TIMEOUT_MS");
    let Some(url) = var("HINAGATA_APPROVAL_URL") else {
        return match (token, timeout) {
            (None, None) => Ok(None),
            _ => Err("HINAGATA_APPROVAL_TOKEN_FILE and HINAGATA_APPROVAL_TIMEOUT_MS need HINAGATA_APPROVAL_URL".into()),
        };
    };
    let timeout_ms = match timeout {
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|ms| (1..=60_000).contains(ms))
            .ok_or("HINAGATA_APPROVAL_TIMEOUT_MS must be 1 to 60000")?,
        None => 10_000,
    };
    crate::promotion::ApprovalHook::new(&url, token, std::time::Duration::from_millis(timeout_ms))
        .map(Some)
}

/// The TLS configuration, if any. Any TLS setting without a full server
/// identity is refused rather than quietly serving plaintext.
pub fn tls(
    var: impl Fn(&str) -> Option<String>,
) -> Result<Option<Arc<rustls::ServerConfig>>, String> {
    let (cert, key, ca) = (
        read(&var, "HINAGATA_TLS_CERT")?,
        read(&var, "HINAGATA_TLS_KEY")?,
        read(&var, "HINAGATA_TLS_CLIENT_CA")?,
    );
    match (cert, key) {
        (Some(cert), Some(key)) => crate::api::tls_config(&cert, &key, ca.as_deref()).map(Some),
        (None, None) if ca.is_none() => Ok(None),
        _ => Err("TLS needs both HINAGATA_TLS_CERT and HINAGATA_TLS_KEY".into()),
    }
}
