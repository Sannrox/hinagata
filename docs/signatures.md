# Signed version manifests

A signed manifest proves, offline, that an artifact and its metadata were registered as a given version in a given namespace of a Hinagata registry. Verifying needs the registry's Ed25519 public key, SHA-256, base64 and a JSON parser. Nothing calls back to the registry.

## Envelope

`GetVersionManifest` (HTTP `GET /v1/models/{model}/versions/{version}/manifest`) returns:

| Field | Value |
| --- | --- |
| `format` | `hinagata.signature/v1` |
| `key_id` | the id the signing key is published under |
| `manifest` | the manifest bytes, exactly as signed |
| `signature` | standard padded base64 of the 64-byte Ed25519 signature over the UTF-8 bytes of `manifest` |
| `metadata` | the metadata bytes the manifest's `metadata_digest` covers |

The signature covers `manifest` as a string. A verifier checks the signature over those bytes and only then parses them. It never re-serializes the manifest.

## Manifest

Compact JSON (no whitespace) with keys sorted by byte order:

```json
{"artifact_digest":"sha256:<hex>","format":"hinagata.version-manifest/v1","metadata_digest":"sha256:<hex>","model":"churn","namespace":"tenant-a","version":3}
```

- `artifact_digest`: `sha256:` and the lowercase hex SHA-256 of the artifact bytes.
- `metadata_digest`: the same over the metadata bytes.
- `version`: the version number, an integer.

## Metadata

Compact JSON with keys sorted at every level, holding the version's fixed content: `artifact_digest`, `binding`, `format`, `framework`, `lineage`, `metrics`, `signature` (the input and output tensor schema) and `tags`. The stage is not part of it: it changes over a version's life and a signature stays valid across promotions. The verifier checks the metadata's digest and that its `artifact_digest` equals the manifest's.

## Trusted keys

A verifier lists the keys it trusts:

```json
{"keys":[{"key_id":"registry-2026","public_key":"<64 hex characters>"}]}
```

An envelope whose `key_id` is not listed is refused. To rotate, publish the new key under a new id, add it to verifiers, then switch the registry. Envelopes signed with the old key stay valid as long as verifiers keep it.

## Verifying

1. Refuse unless `format` is `hinagata.signature/v1` and `key_id` is trusted.
2. Verify the signature over the UTF-8 bytes of `manifest` with that key.
3. Parse `manifest`. Refuse unless its `format` is `hinagata.version-manifest/v1`.
4. Compare the SHA-256 of the artifact with `artifact_digest`, and of the metadata with `metadata_digest`.

`hinagata verify --keys <trusted keys> --envelope <envelope> [--artifact <file>] [--metadata <file>]` does these steps. Without `--metadata` it checks the envelope's own `metadata` field, and refuses an envelope without one. Without `--artifact` it checks only the manifest and metadata.

## Signing key

`HINAGATA_SIGNING_KEY_FILE` holds the 32-byte Ed25519 seed as 64 hex characters; `HINAGATA_SIGNING_KEY_ID` names it. Setting one without the other is refused at start. Ed25519 signatures are deterministic, so every instance holding the key returns the same envelope for a version.
