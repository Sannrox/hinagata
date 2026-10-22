//! The authentication hook: it turns a bearer credential into an
//! [`Identity`], whose namespace is the only thing Hinagata decides from it.
//! Hinagata makes no authorization decisions beyond that mapping; markings
//! and business policy belong to the governing caller.
//!
//! Two hooks ship: a static token for standalone or development use,
//! compared in constant time, and JWT verification against a JWKS supplied
//! at startup (no network calls at runtime) with an audience and a tenant
//! claim mapped to the namespace. Mutual TLS, when configured, is checked
//! at the transport before either runs.

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Who a verified credential belongs to. Only an [`Authenticator`] builds
/// one, and handlers take the namespace from it, never from a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    subject: String,
    namespace: String,
}

impl Identity {
    /// For authentication hooks only, from a verified credential.
    pub fn authenticated(subject: impl Into<String>, namespace: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            namespace: namespace.into(),
        }
    }

    pub fn subject(&self) -> &str {
        &self.subject
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    Missing,
    Invalid,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "a bearer credential is required",
            Self::Invalid => "the credential is not valid",
        })
    }
}

impl std::error::Error for AuthError {}

pub trait Authenticator: Send + Sync {
    /// The identity behind a bearer credential (the part after `Bearer `).
    fn authenticate(&self, bearer: Option<&str>) -> Result<Identity, AuthError>;
}

/// One shared token mapped to one namespace.
pub struct StaticToken {
    digest: [u8; 32],
    namespace: String,
}

impl StaticToken {
    pub fn new(token: &str, namespace: impl Into<String>) -> Self {
        Self {
            digest: Sha256::digest(token.as_bytes()).into(),
            namespace: namespace.into(),
        }
    }

    /// Constant-time match: both sides are fixed-length digests compared
    /// with `ct_eq`, so neither the token's content nor its length shows in
    /// the time taken.
    pub fn matches(&self, presented: &str) -> bool {
        let presented: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        presented.ct_eq(&self.digest).into()
    }
}

impl Authenticator for StaticToken {
    fn authenticate(&self, bearer: Option<&str>) -> Result<Identity, AuthError> {
        let token = bearer.ok_or(AuthError::Missing)?;
        if self.matches(token) {
            Ok(Identity::authenticated(
                "static-token",
                self.namespace.clone(),
            ))
        } else {
            Err(AuthError::Invalid)
        }
    }
}

/// JWTs signed by a key in a fixed JWKS, for one audience, with the
/// namespace taken from `tenant_claim`.
pub struct Jwt {
    keys: JwkSet,
    audience: String,
    issuer: Option<String>,
    tenant_claim: String,
}

impl Jwt {
    pub fn new(
        jwks_json: &str,
        audience: impl Into<String>,
        issuer: Option<String>,
        tenant_claim: impl Into<String>,
    ) -> Result<Self, String> {
        let keys: JwkSet = serde_json::from_str(jwks_json).map_err(|error| error.to_string())?;
        if keys.keys.is_empty() {
            return Err("the JWKS holds no keys".into());
        }
        // Tokens are verified with the algorithm their key names, never one
        // the token picks, so a key without `alg` could verify nothing:
        // refuse it at startup instead of failing every request.
        if let Some(key) = keys
            .keys
            .iter()
            .find(|key| key.common.key_algorithm.is_none())
        {
            return Err(format!(
                "JWKS key {} names no alg",
                key.common.key_id.as_deref().unwrap_or("without kid")
            ));
        }
        Ok(Self {
            keys,
            audience: audience.into(),
            issuer,
            tenant_claim: tenant_claim.into(),
        })
    }
}

impl Authenticator for Jwt {
    fn authenticate(&self, bearer: Option<&str>) -> Result<Identity, AuthError> {
        let token = bearer.ok_or(AuthError::Missing)?;
        let header = decode_header(token).map_err(|_| AuthError::Invalid)?;
        // With several keys a token names its key; with one, it may not.
        let jwk = match header.kid.as_deref() {
            Some(kid) => self.keys.find(kid),
            None if self.keys.keys.len() == 1 => self.keys.keys.first(),
            None => None,
        }
        .ok_or(AuthError::Invalid)?;
        let key = DecodingKey::from_jwk(jwk).map_err(|_| AuthError::Invalid)?;
        // The algorithm comes from the key, never from the token header, so
        // an unsigned (`none`) or downgraded token never verifies.
        let algorithm = jwk
            .common
            .key_algorithm
            .and_then(|alg| alg.to_string().parse().ok())
            .ok_or(AuthError::Invalid)?;
        if header.alg != algorithm {
            return Err(AuthError::Invalid);
        }
        let mut validation = Validation::new(algorithm);
        validation.set_audience(&[&self.audience]);
        // A configured issuer is required, not just checked when present.
        match &self.issuer {
            Some(issuer) => {
                validation.set_issuer(&[issuer]);
                validation.set_required_spec_claims(&["exp", "aud", "iss"]);
            }
            None => validation.set_required_spec_claims(&["exp", "aud"]),
        }
        // A token is not good before its `nbf`, when it carries one.
        validation.validate_nbf = true;
        let claims = decode::<serde_json::Value>(token, &key, &validation)
            .map_err(|_| AuthError::Invalid)?
            .claims;
        let tenant = claims
            .get(&self.tenant_claim)
            .and_then(serde_json::Value::as_str)
            .filter(|tenant| !tenant.is_empty())
            .ok_or(AuthError::Invalid)?;
        let subject = claims
            .get("sub")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        Ok(Identity::authenticated(subject, tenant))
    }
}

/// The credential in an `authorization: Bearer <token>` header value. The
/// scheme name is case-insensitive; the token is kept as sent.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let (scheme, token) = header?.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_token_matches_only_itself_whatever_the_length() {
        let token = StaticToken::new("s3cret-token", "tenant-a");
        assert!(token.matches("s3cret-token"));
        for wrong in [
            "",
            "s3cret-toke",
            "s3cret-tokeN",
            "s3cret-token-longer",
            "x",
        ] {
            assert!(!token.matches(wrong), "{wrong}");
        }
        let identity = token.authenticate(Some("s3cret-token")).unwrap();
        assert_eq!(identity.namespace(), "tenant-a");
        assert_eq!(token.authenticate(None), Err(AuthError::Missing));
        assert_eq!(token.authenticate(Some("nope")), Err(AuthError::Invalid));
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("bearer AbC")), Some("AbC"));
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(Some("Bearer")), None);
    }

    #[test]
    fn a_jwks_key_without_an_algorithm_is_refused_at_startup() {
        let jwks = r#"{"keys":[{"kty":"oct","kid":"k1","k":"c2VjcmV0"}]}"#;
        let Err(error) = Jwt::new(jwks, "hinagata", None, "tenant") else {
            panic!("a key without alg was accepted");
        };
        assert!(error.contains("k1"), "{error}");
    }
}
