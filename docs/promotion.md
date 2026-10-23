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

## Gates

`PutPromotionGates` (`POST /v1/models/{model}/promotion-gates`) sets what a model requires before a version goes to production, audited as `model.promotion_gates_set`. `GetPromotionGates` reads them.

- `min_metrics`: each named metric the version records is at least this value. A missing metric fails.
- `require_compatible_signature`: no breaking signature change against any version already in production (see `CheckSignatureCompatibility`).
- `require_signed_manifest`: the registry holds a signing key, so the hook receives a signed manifest.
