# Deployment descriptors

Hinagata runs no model. A descriptor tells a deployer how to run a version as an online scoring service, and tells a batch runner how to score a dataset or object set with it. Both are published as JSON Schemas: [online](schemas/online-descriptor.v1.json) and [batch](schemas/batch-descriptor.v1.json). Examples are in `tests/fixtures/`.

## Getting one

| HTTP | gRPC |
| --- | --- |
| `POST /v1/models/{model}/versions/{version}/online-descriptor` | `GetOnlineDescriptor` |
| `POST /v1/models/{model}/versions/{version}/batch-descriptor` | `GetBatchDescriptor` |

The request carries the deployment choices:
- Online: `runtime_image`, plus `resources` (`cpu_millis`, `memory_mib`, `replicas`). Zero takes the default of 1000, 512 and 1.
- Batch: `runtime_image`, `input` (the dataset version or object set to score), `output_dataset`, `partition_column` (a field of the binding) and `partitions`.

An empty `runtime_image` means the deployer's default ONNX runtime image.

The registry fills in the rest from the version. Descriptors hold no state, so the same version and request always give the same descriptor.

## Signed

The response is `hinagata.signed-descriptor/v1`:

| Field | Value |
| --- | --- |
| `key_id` | the registry's signing key id |
| `descriptor` | the descriptor: compact JSON with keys sorted, exactly as signed |
| `signature` | standard padded base64 of the Ed25519 signature over the UTF-8 bytes of `descriptor` |

The descriptor holds the version's signed manifest in `version_manifest` ([format](signatures.md)), so the descriptor, the artifact digest and the version metadata are signed together. A consumer:

1. verifies `signature` over `descriptor` with the trusted key for `key_id`;
2. verifies `version_manifest` the same way, and that its artifact digest is the descriptor's `artifact_digest`;
3. checks the pulled artifact's SHA-256 against `artifact_digest` before loading it.

A registry without a signing key issues no descriptors.

## Online: `hinagata.online-descriptor/v1`

- `artifact_digest`, `version_manifest`, `signature`: what to load, and its input and output tensors.
- `runtime`: `{"engine": "onnx", "image": "..."}`.
- `resources`: per replica, and the replica count.
- `health`: the service answers `GET /healthz` while it runs, and `GET /readyz` once the artifact named by `artifact_digest` is loaded and verified.
- `monitoring`: the registry paths for this version's prediction summaries ([format](prediction-summary.md)) and labeled aggregates ([format](labeled-aggregate.md)).

## Batch: `hinagata.batch-descriptor/v1`

- `artifact_digest`, `version_manifest`, `runtime`, `signature`, `monitoring`: as for online.
- `input`: the source:
  - `kind` is `dataset` or `object_set`.
  - `id` is the dataset version or object set.
  - `binding` is the version's binding. Each input reads the fields listed for it in `binding.inputs`, or the field of its own name.
- `output`: the dataset to write and its fields, one per output tensor, with the tensor's dimensions per row.
- `partitioning`: the field to partition by (empty for none) and the number of partitions.

A version has no batch descriptor if it is bound to no dataset or object type, or if any output lacks a leading batch axis (`-1`).
