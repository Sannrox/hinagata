# Export and import bundles

A bundle carries registered versions between registries with no network path between them. A source registry signs each version ([signed manifests](signatures.md)). A target registry admits a version only when a key it trusts signed it and every digest matches.

## Layout

A bundle is a directory, format `hinagata.bundle/v1`:

| Path | Content |
| --- | --- |
| `bundle.json` | `{"format": "hinagata.bundle/v1"}` |
| `manifests/<model>-<version>.json` | the source's `GetVersionManifest` response: envelope fields and the metadata bytes |
| `artifacts/<hex>` | each artifact once, named by the lowercase hex of its SHA-256 |

The manifests carry the signatures. `bundle.json` and the file names are not signed, and nothing depends on them beyond finding the files.

## Export

```sh
hinagata export --server https://registry-a:7700 --ca ca.pem --token-file token \
  --model churn --version 3 --version 4 --out churn-bundle
```

The source must hold a signing key. Export writes into a new or empty directory and checks each artifact against its digest as it writes it.

## Import

```sh
hinagata import --server https://registry-b:7700 --ca ca.pem --token-file token \
  --keys trusted.json --bundle churn-bundle
```

Import changes nothing until every check passes:

1. Locally, against `trusted.json`: each manifest's signature, its metadata digest, and the SHA-256 of its artifact file.
2. On the target, with `ImportVersion` and `validate_only`: each signature against the target's own `HINAGATA_TRUSTED_KEYS_FILE`, and the metadata, which must be exactly the canonical bytes the signature covers.
3. Only then, for each version in source order (by model, then version), it creates the model if missing, uploads the artifact (resuming a partial upload), and calls `ImportVersion`. The target checks the signature and metadata again and, for ONNX, the signature against the graph. It then registers the version.

The target numbers versions itself. The metadata (artifact digest, framework, format, signature, metrics, tags, lineage, binding) is identical to the source's. Each import is audited as `version.imported` with the source namespace, model, version, key id and digests, in the same transaction as the registration. Importing a bundle again returns the stored versions and records nothing.

A target without `HINAGATA_TRUSTED_KEYS_FILE` refuses every import.
