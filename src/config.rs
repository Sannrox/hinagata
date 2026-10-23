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
//! - `HINAGATA_TLS_CERT` and `HINAGATA_TLS_KEY` (PEM) serve over TLS; adding
//!   `HINAGATA_TLS_CLIENT_CA` requires client certificates (mTLS).

use crate::auth::{Authenticator, Jwt, StaticToken};
use std::sync::Arc;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7700";
pub const DEFAULT_DB_PATH: &str = "hinagata.db";

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
