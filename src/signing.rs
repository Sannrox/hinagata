//! Signed version manifests (#12): proof, offline, that an artifact and its
//! metadata came from this registry. The format is in
//! `docs/signatures.md`; a verifier needs only the public key, SHA-256,
//! Ed25519 and JSON parsing.
//!
//! - **Metadata** is the version's fixed content (artifact digest,
//!   framework, format, signature, metrics, tags, lineage, binding) as
//!   compact JSON with keys sorted. The stage changes over time and is not
//!   part of it.
//! - **Manifest** is compact sorted JSON naming the namespace, model,
//!   version, artifact digest, and the SHA-256 of the metadata bytes.
//! - **Envelope** carries the manifest bytes, the key id, and the Ed25519
//!   signature over the manifest bytes. Ed25519 is deterministic, so the
//!   same version always signs to the same envelope.

use crate::registry::Version;
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const MANIFEST_FORMAT: &str = "hinagata.version-manifest/v1";
pub const ENVELOPE_FORMAT: &str = "hinagata.signature/v1";

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Compact JSON with object keys sorted, as the format defines it.
fn canonical(value: &impl Serialize) -> Result<String, String> {
    // `serde_json::Value` keeps object keys sorted, so going through it
    // sorts every level.
    let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    serde_json::to_string(&value).map_err(|error| error.to_string())
}

/// The version's signed metadata bytes.
pub fn metadata(version: &Version) -> Result<String, String> {
    canonical(&serde_json::json!({
        "artifact_digest": version.artifact_digest,
        "framework": version.framework,
        "format": version.format,
        "signature": version.signature,
        "metrics": version.metrics,
        "tags": version.tags,
        "lineage": version.lineage,
        "binding": version.binding,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub namespace: String,
    pub model: String,
    pub version: u64,
    pub artifact_digest: String,
    pub metadata_digest: String,
}

/// A detached signature over a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub format: String,
    pub key_id: String,
    /// The manifest bytes exactly as signed.
    pub manifest: String,
    /// Base64 (standard, padded) of the 64-byte Ed25519 signature.
    pub signature: String,
}

/// The registry's signing key, read from a file holding the 32-byte Ed25519
/// seed as 64 hex characters, with the key id it is published under.
pub struct Signer {
    key_id: String,
    key: SigningKey,
}

fn parse_hex32(what: &str, text: &str) -> Result<[u8; 32], String> {
    let text = text.trim();
    if text.len() != 64 || !text.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{what} must be 64 hex characters"));
    }
    let mut bytes = [0u8; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|error| error.to_string())?;
    }
    Ok(bytes)
}

impl Signer {
    pub fn from_seed_hex(key_id: impl Into<String>, seed_hex: &str) -> Result<Self, String> {
        let key_id = key_id.into();
        if key_id.is_empty() || key_id.len() > 128 || !key_id.chars().all(|c| c.is_ascii_graphic())
        {
            return Err("the signing key id must be 1 to 128 visible characters".into());
        }
        Ok(Self {
            key_id,
            key: SigningKey::from_bytes(&parse_hex32("the signing key", seed_hex)?),
        })
    }

    /// The public key as 64 hex characters, as a trusted-keys file lists it.
    pub fn public_key_hex(&self) -> String {
        self.key
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The envelope for `version` in `namespace`, and the metadata bytes it
    /// covers.
    pub fn sign(&self, namespace: &str, version: &Version) -> Result<(Envelope, String), String> {
        let metadata = metadata(version)?;
        let manifest = canonical(&Manifest {
            format: MANIFEST_FORMAT.into(),
            namespace: namespace.into(),
            model: version.model.clone(),
            version: version.version,
            artifact_digest: version.artifact_digest.clone(),
            metadata_digest: sha256(metadata.as_bytes()),
        })?;
        let signature = self.key.sign(manifest.as_bytes());
        Ok((
            Envelope {
                format: ENVELOPE_FORMAT.into(),
                key_id: self.key_id.clone(),
                manifest,
                signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
            },
            metadata,
        ))
    }
}

/// Public keys a verifier trusts, by key id: a JSON file
/// `{"keys": [{"key_id": "...", "public_key": "<64 hex>"}]}`.
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys {
    keys: BTreeMap<String, VerifyingKey>,
}

impl TrustedKeys {
    pub fn from_json(text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct File {
            keys: Vec<Entry>,
        }
        #[derive(Deserialize)]
        struct Entry {
            key_id: String,
            public_key: String,
        }
        let file: File =
            serde_json::from_str(text).map_err(|error| format!("trusted keys: {error}"))?;
        let mut keys = BTreeMap::new();
        for entry in file.keys {
            let bytes = parse_hex32(&format!("public key `{}`", entry.key_id), &entry.public_key)?;
            let key = VerifyingKey::from_bytes(&bytes)
                .map_err(|_| format!("public key `{}` is not an Ed25519 key", entry.key_id))?;
            if keys.insert(entry.key_id.clone(), key).is_some() {
                return Err(format!("key `{}` is listed twice", entry.key_id));
            }
        }
        Ok(Self { keys })
    }

    /// Check an envelope: a trusted key signed exactly these manifest bytes.
    /// Returns the manifest. An unknown key is refused, not skipped.
    pub fn verify(&self, envelope: &Envelope) -> Result<Manifest, String> {
        if envelope.format != ENVELOPE_FORMAT {
            return Err(format!("envelope format must be {ENVELOPE_FORMAT}"));
        }
        let key = self
            .keys
            .get(&envelope.key_id)
            .ok_or_else(|| format!("key `{}` is not trusted", envelope.key_id))?;
        let signature = base64::engine::general_purpose::STANDARD
            .decode(&envelope.signature)
            .ok()
            .and_then(|bytes| ed25519_dalek::Signature::from_slice(&bytes).ok())
            .ok_or("the signature is not a 64-byte Ed25519 signature")?;
        key.verify(envelope.manifest.as_bytes(), &signature)
            .map_err(|_| "the signature does not match the manifest".to_string())?;
        let manifest: Manifest = serde_json::from_str(&envelope.manifest)
            .map_err(|error| format!("manifest: {error}"))?;
        if manifest.format != MANIFEST_FORMAT {
            return Err(format!("manifest format must be {MANIFEST_FORMAT}"));
        }
        Ok(manifest)
    }
}

/// The version content in signed metadata bytes. Refused unless the bytes
/// are exactly what [`metadata`] gives for that content, so nothing is
/// admitted that the signature does not cover.
pub fn parse_metadata(raw: &str) -> Result<crate::registry::NewVersion, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Content {
        artifact_digest: String,
        framework: String,
        format: String,
        signature: crate::registry::Signature,
        metrics: BTreeMap<String, f64>,
        tags: BTreeMap<String, String>,
        lineage: crate::registry::Lineage,
        binding: Option<crate::signature::Binding>,
    }
    let content: Content =
        serde_json::from_str(raw).map_err(|error| format!("metadata: {error}"))?;
    let version = Version {
        model: String::new(),
        version: 0,
        artifact_digest: content.artifact_digest,
        framework: content.framework,
        format: content.format,
        signature: content.signature,
        metrics: content.metrics,
        tags: content.tags,
        lineage: content.lineage,
        binding: content.binding,
        stage: crate::registry::Stage::Candidate,
        created_at_ms: 0,
        created_by: String::new(),
    };
    if metadata(&version)? != raw {
        return Err("the metadata is not in canonical form".into());
    }
    Ok(crate::registry::NewVersion {
        artifact_digest: version.artifact_digest,
        framework: version.framework,
        format: version.format,
        signature: version.signature,
        metrics: version.metrics,
        tags: version.tags,
        lineage: version.lineage,
        binding: version.binding,
    })
}

/// Check an artifact and its metadata bytes against a verified manifest.
pub fn check_contents(
    manifest: &Manifest,
    artifact: Option<&[u8]>,
    metadata: Option<&[u8]>,
) -> Result<(), String> {
    if let Some(artifact) = artifact
        && sha256(artifact) != manifest.artifact_digest
    {
        return Err("the artifact does not match the signed digest".into());
    }
    if let Some(metadata) = metadata {
        if sha256(metadata) != manifest.metadata_digest {
            return Err("the metadata does not match the signed digest".into());
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(metadata).map_err(|error| format!("metadata: {error}"))?;
        if parsed["artifact_digest"] != manifest.artifact_digest {
            return Err("the metadata names another artifact".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Lineage, Signature, Stage};

    pub const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

    fn version() -> Version {
        Version {
            model: "churn".into(),
            version: 3,
            artifact_digest: format!("sha256:{:x}", Sha256::digest(b"weights")),
            framework: "xgboost".into(),
            format: "onnx".into(),
            signature: Signature::default(),
            metrics: BTreeMap::from([("auc".into(), 0.91)]),
            tags: BTreeMap::new(),
            lineage: Lineage::default(),
            binding: None,
            stage: Stage::Candidate,
            created_at_ms: 1,
            created_by: "svc".into(),
        }
    }

    fn trusted(signer: &Signer) -> TrustedKeys {
        TrustedKeys::from_json(&format!(
            r#"{{"keys":[{{"key_id":"{}","public_key":"{}"}}]}}"#,
            signer.key_id(),
            signer.public_key_hex()
        ))
        .unwrap()
    }

    #[test]
    fn a_signed_manifest_verifies_and_tampering_fails() {
        let signer = Signer::from_seed_hex("registry-1", SEED).unwrap();
        let (envelope, metadata) = signer.sign("tenant-a", &version()).unwrap();
        // Deterministic: the same version signs to the same envelope.
        assert_eq!(signer.sign("tenant-a", &version()).unwrap().0, envelope);
        // A stage change is not a content change.
        let promoted = Version {
            stage: Stage::Staging,
            ..version()
        };
        assert_eq!(signer.sign("tenant-a", &promoted).unwrap().0, envelope);

        let keys = trusted(&signer);
        let manifest = keys.verify(&envelope).unwrap();
        assert_eq!((manifest.model.as_str(), manifest.version), ("churn", 3));
        check_contents(&manifest, Some(b"weights"), Some(metadata.as_bytes())).unwrap();
        assert!(check_contents(&manifest, Some(b"weightz"), None).is_err());
        let tampered = metadata.replace("0.91", "0.99");
        assert!(check_contents(&manifest, None, Some(tampered.as_bytes())).is_err());

        let forged = Envelope {
            manifest: envelope.manifest.replace("\"version\":3", "\"version\":4"),
            ..envelope.clone()
        };
        assert!(keys.verify(&forged).unwrap_err().contains("does not match"));
        let other = Signer::from_seed_hex("registry-2", &SEED.replace('9', "8")).unwrap();
        let (unknown, _) = other.sign("tenant-a", &version()).unwrap();
        assert!(keys.verify(&unknown).unwrap_err().contains("not trusted"));
        // An unknown key cannot pose as a trusted id either.
        let impostor = Envelope {
            key_id: "registry-1".into(),
            ..unknown
        };
        assert!(keys.verify(&impostor).is_err());
    }

    #[test]
    fn keys_are_validated() {
        assert!(Signer::from_seed_hex("k", "abc").is_err());
        assert!(Signer::from_seed_hex("", SEED).is_err());
        assert!(TrustedKeys::from_json(r#"{"keys":[{"key_id":"k","public_key":"zz"}]}"#).is_err());
        let signer = Signer::from_seed_hex("k", SEED).unwrap();
        let twice = format!(
            r#"{{"keys":[{{"key_id":"k","public_key":"{0}"}},{{"key_id":"k","public_key":"{0}"}}]}}"#,
            signer.public_key_hex()
        );
        assert!(
            TrustedKeys::from_json(&twice)
                .unwrap_err()
                .contains("twice")
        );
    }
}
