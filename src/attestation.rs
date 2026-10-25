//! Signed promotion attestations (#39): proof, offline, that a version
//! passed an approved stage change. The format is in `docs/promotion.md`.
//!
//! The payload is compact sorted JSON naming the namespace, model, version,
//! stage, approver, decision reference and artifact digest. The registry
//! signs those bytes. A verifier needs only the public key, SHA-256,
//! Ed25519 and JSON parsing. The payload is bound to the artifact digest,
//! so an attestation for version N never covers version N+1's bytes.

use crate::promotion::Approval;
use crate::registry::Version;
use crate::signing::{self, Signer, TrustedKeys};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const ATTESTATION_FORMAT: &str = "hinagata.promotion-attestation/v1";
pub const SIGNED_FORMAT: &str = "hinagata.signed-attestation/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub format: String,
    pub namespace: String,
    pub model: String,
    pub version: u64,
    pub stage: String,
    pub approver: String,
    pub reference: String,
    pub artifact_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signed {
    pub format: String,
    pub key_id: String,
    /// The attestation bytes exactly as signed.
    pub attestation: String,
    /// Base64 (standard, padded) of the 64-byte Ed25519 signature.
    pub signature: String,
}

/// Sign `approval` for `version` in `namespace`.
pub fn sign(
    signer: &Signer,
    namespace: &str,
    version: &Version,
    approval: &Approval,
) -> Result<Signed, String> {
    let attestation = signing::canonical(&Attestation {
        format: ATTESTATION_FORMAT.into(),
        namespace: namespace.into(),
        model: version.model.clone(),
        version: version.version,
        stage: approval.stage.as_str().into(),
        approver: approval.approver.clone(),
        reference: approval.reference.clone(),
        artifact_digest: version.artifact_digest.clone(),
    })?;
    Ok(Signed {
        format: SIGNED_FORMAT.into(),
        key_id: signer.key_id().into(),
        signature: signer.sign_bytes(attestation.as_bytes()),
        attestation,
    })
}

/// Check a signed attestation: a trusted key signed exactly these bytes.
pub fn verify(keys: &TrustedKeys, signed: &Signed) -> Result<Attestation, String> {
    if signed.format != SIGNED_FORMAT {
        return Err(format!(
            "attestation envelope format must be {SIGNED_FORMAT}"
        ));
    }
    keys.verify_signature(
        &signed.key_id,
        signed.attestation.as_bytes(),
        &signed.signature,
    )
    .map_err(|error| {
        if error == "the signature does not match" {
            "the signature does not match the attestation".into()
        } else {
            error
        }
    })?;
    let attestation: Attestation = serde_json::from_str(&signed.attestation)
        .map_err(|error| format!("attestation: {error}"))?;
    if attestation.format != ATTESTATION_FORMAT {
        return Err(format!("attestation format must be {ATTESTATION_FORMAT}"));
    }
    Ok(attestation)
}

/// Refuse unless `artifact` hashes to the attestation's digest.
pub fn check_artifact(attestation: &Attestation, artifact: &[u8]) -> Result<(), String> {
    let digest = format!("sha256:{:x}", Sha256::digest(artifact));
    if digest != attestation.artifact_digest {
        return Err("the artifact does not match the signed digest".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::promotion::Approval;
    use crate::registry::{Lineage, Signature, Stage, Version};
    use std::collections::BTreeMap;

    const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";

    fn version(number: u64, artifact: &[u8]) -> Version {
        Version {
            model: "churn".into(),
            version: number,
            artifact_digest: format!("sha256:{:x}", Sha256::digest(artifact)),
            framework: "xgboost".into(),
            format: "onnx".into(),
            signature: Signature::default(),
            metrics: BTreeMap::new(),
            tags: BTreeMap::new(),
            lineage: Lineage::default(),
            binding: None,
            stage: Stage::Production,
            created_at_ms: 1,
            created_by: "svc".into(),
        }
    }

    fn approval() -> Approval {
        Approval {
            stage: Stage::Production,
            approver: "alice@governance".into(),
            reference: "CHG-1042".into(),
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
    fn a_signed_attestation_verifies_and_does_not_cover_another_version() {
        let signer = Signer::from_seed_hex("registry-1", SEED).unwrap();
        let v1 = version(1, b"weights-v1");
        let signed = sign(&signer, "tenant-a", &v1, &approval()).unwrap();
        assert_eq!(sign(&signer, "tenant-a", &v1, &approval()).unwrap(), signed);

        let keys = trusted(&signer);
        let attestation = verify(&keys, &signed).unwrap();
        assert_eq!(
            (
                attestation.model.as_str(),
                attestation.version,
                attestation.stage.as_str(),
                attestation.approver.as_str(),
                attestation.reference.as_str()
            ),
            ("churn", 1, "production", "alice@governance", "CHG-1042")
        );
        check_artifact(&attestation, b"weights-v1").unwrap();
        assert!(check_artifact(&attestation, b"weights-v2").is_err());

        let v2 = version(2, b"weights-v2");
        let other = sign(&signer, "tenant-a", &v2, &approval()).unwrap();
        let v2_attestation = verify(&keys, &other).unwrap();
        assert!(check_artifact(&v2_attestation, b"weights-v1").is_err());
        assert!(check_artifact(&attestation, b"weights-v2").is_err());

        let mut tampered = signed.clone();
        tampered.attestation = tampered.attestation.replace("CHG-1042", "CHG-0000");
        assert!(
            verify(&keys, &tampered)
                .unwrap_err()
                .contains("does not match")
        );
    }
}
