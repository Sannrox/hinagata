# Promotion and the approval hook

Hinagata holds no promotion policy. A caller-governed approval hook decides stage changes, and Hinagata enforces the gates a model requires.

## Without a hook

Changes between `candidate` and `staging`, and to `archived`, apply. Promotion to `production` is refused.

## With a hook

`HINAGATA_APPROVAL_URL` names an HTTPS endpoint, or HTTP to a loopback address. Every stage change is sent to it, and applies only on an allow.

1. Hinagata checks the change. The version must exist, be in `expected_stage` when that is set, and be allowed to move. For `production`, the model's gates must pass. A failed gate refuses the change without asking the hook.
2. It sends a `POST` with a JSON body. When `HINAGATA_APPROVAL_TOKEN_FILE` is set, it also sends `authorization: Bearer <token>`.
3. It applies the change only on an allow, and only if the version has not changed stage since and the gates give the same results. Otherwise the change is refused and the caller asks again.

### Request

```json
{
  "format": "hinagata.approval-request/v1",
  "request_id": "<64 hex characters>",
  "namespace": "tenant-a",
  "actor": "svc-release",
  "model": "churn",
  "version": 3,
  "from_stage": "staging",
  "to_stage": "production",
  "artifact_digest": "sha256:<hex>",
  "metrics": {"auc": 0.91},
  "gates": [{"gate": "metric:auc", "passed": true, "detail": "0.91 is at least 0.85"}],
  "manifest": {"format": "hinagata.signature/v1", "key_id": "...", "manifest": "...", "signature": "...", "metadata": "..."}
}
```

`manifest` is the signed version manifest ([format](signatures.md)) when the registry holds a signing key, and `null` otherwise. `request_id` is a digest of everything the hook decides on: namespace, actor, model, version, both stages, the version's last stage change, the gate results and the manifest. Asking again about the same change sends the same id, so the hook can return its earlier decision. A different actor, gate result or stage history gives another id.

### Decision

```json
{"decision": "allow", "approver": "alice@governance", "reference": "CHG-1042", "reason": "evaluation reviewed"}
```

`decision` is `allow` or `deny`. An allow must name its `approver` and `reference`. The hook fails closed: all of the following are a deny with the cause as the reason:

- a timeout (`HINAGATA_APPROVAL_TIMEOUT_MS`, default 10 s);
- a status other than 2xx, or a redirect;
- a body over 64 KiB or one that does not parse;
- an unknown decision, or an allow without an approver and a reference.

### Audit

An applied change records `version.stage_changed`, and in the same transaction `version.stage_approved` with the request id, stages, approver, reference, reason and gate results. A deny records `version.stage_denied` with the same fields. `ListAuditEvents` returns them.

## Attestation

`GetPromotionAttestation` (`GET /v1/models/{model}/versions/{version}/promotion-attestation`) returns a signed proof that a version passed an approved stage change. Deployment descriptors carry no stage; this is how an offline deployer checks that the version was promoted. The registry must hold a signing key. The version must have a matching `version.stage_approved` record.

An empty `stage` returns the latest production approval, or the latest approved stage change if the version was never promoted to production. A given `stage` returns that approved change.

The response is `hinagata.signed-attestation/v1`:

| Field | Value |
| --- | --- |
| `key_id` | the registry's signing key id |
| `attestation` | compact JSON with keys sorted, exactly as signed |
| `signature` | standard padded base64 of the Ed25519 signature over the UTF-8 bytes of `attestation` |

The attestation is `hinagata.promotion-attestation/v1`:

```json
{"approver":"alice@governance","artifact_digest":"sha256:<hex>","format":"hinagata.promotion-attestation/v1","model":"churn","namespace":"tenant-a","reference":"CHG-1042","stage":"production","version":3}
```

It is bound to the artifact digest it covers. A verifier:

1. refuses unless `format` is `hinagata.signed-attestation/v1` and `key_id` is trusted;
2. verifies `signature` over the UTF-8 bytes of `attestation` with that key;
3. parses `attestation` and refuses unless its `format` is `hinagata.promotion-attestation/v1`;
4. compares the SHA-256 of the artifact with `artifact_digest`.

`hinagata verify --keys <trusted keys> --attestation <file> --artifact <file>` does these steps. A tampered byte, or an attestation for version N used with version N+1's artifact, is refused. Export includes the attestation in a bundle when the version has one ([bundles](bundles.md)).

## Gates

`PutPromotionGates` (`POST /v1/models/{model}/promotion-gates`) sets what a model requires before a version goes to production, audited as `model.promotion_gates_set`. `GetPromotionGates` reads them.

- `min_metrics`: each named metric the version records is at least this value. A missing metric fails.
- `require_compatible_signature`: no breaking signature change against any version already in production (see `CheckSignatureCompatibility`).
- `require_signed_manifest`: the registry holds a signing key, so the hook receives a signed manifest.
