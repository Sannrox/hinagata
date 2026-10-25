# Hinagata

Hinagata is a standalone registry for classical machine learning models, written in Rust.

- Registers models and immutable, content-addressed model versions with their input and output schema, metrics and lineage (training dataset version, code reference, run receipt).
- Promotes versions through stages with an approval hook that the caller governs.
- Produces signed deployment descriptors: the deployer runs online scoring services, and the batch runner runs scoring jobs.
- Monitors deployed versions: prediction statistics, data drift and, once labels arrive, model quality, with alerts.

Hinagata doesn't make governance decisions. A caller in front of it enforces policy. It stores artifacts on local disk or S3-compatible object storage, and runs as a single binary. Beyond an S3-compatible store, its only outbound call is the optional approval hook.

## Run

One listener serves gRPC (`hinagata.v1.Hinagata`, see `proto/hinagata/v1/`) and HTTP/JSON. Every call resolves its namespace from the caller's bearer credential, never from the request. Configure one auth hook:

- `HINAGATA_STATIC_TOKEN_FILE` and `HINAGATA_STATIC_NAMESPACE` for standalone or development use;
- `HINAGATA_JWKS_FILE` and `HINAGATA_JWT_AUDIENCE` (optionally `HINAGATA_JWT_ISSUER`, `HINAGATA_TENANT_CLAIM`) to verify JWTs offline against a local JWKS, with the tenant claim as namespace.

`HINAGATA_LISTEN` sets the address (default `127.0.0.1:7700`). `HINAGATA_TLS_CERT` and `HINAGATA_TLS_KEY` serve TLS; adding `HINAGATA_TLS_CLIENT_CA` requires client certificates.

## Registry

Models and versions live in the caller's namespace, in one embedded database file (`HINAGATA_DB_PATH`, default `hinagata.db`; [ADR 0001](docs/decisions/0001-embedded-registry-store.md)).

- A version records the artifact digest (`sha256:<hex>`), framework, format (`onnx`), signature, metrics and tags. It is numbered 1, 2, … per model and never changes once registered. Registering the same artifact again with the same content returns the stored version; with other content it is refused.
- Signatures: for an ONNX artifact the signature is read from the model graph when omitted, and a declared one must match it. A version can be bound to a dataset schema or an object type, given inline: each input takes a field of its name, or `n` fields in order for a `[batch, n]` tensor, and registration refuses a field that does not widen to the input's type or may be null where the input may not. `CheckSignatureCompatibility` lists the changes between two versions that break a caller of the first: new inputs, changed input or output types or shapes, inputs that stop accepting null, and removed outputs.
- Lineage: each version records its training dataset versions (id, version, digest), code reference (repository, commit, code digest), parameters and the training run's receipt id, all fixed at registration. Registering with `training_run_receipt_json` (a batch runner receipt, schema 6) takes the datasets, run id and code digest from the receipt, records the receipt's digest, and refuses an artifact the run did not produce. `ListVersionsTrainedOn` answers the reverse question: which versions were trained on a dataset, optionally at one digest.
- Stages: a version starts as `candidate`. It moves between `candidate` and `staging`, and from any stage but `archived` to `archived`, which is final. A caller-governed approval hook (`HINAGATA_APPROVAL_URL`) decides every change once configured, and promotion to `production` is refused without one. A model can require gates before production: metric minimums, signature compatibility with the versions in production, and a signed manifest. An approved change records the approver and decision reference ([promotion](docs/promotion.md)). `expected_stage` makes a change apply only if the version has not moved since it was read.
- Every change is an audit event with its actor (the credential's subject), action, and the record before and after.

## Signed manifests

With `HINAGATA_SIGNING_KEY_FILE` (an Ed25519 seed as 64 hex characters) and `HINAGATA_SIGNING_KEY_ID`, `GetVersionManifest` returns a version's metadata and a detached signature over its manifest: namespace, model, version number, artifact digest and metadata digest. Anyone holding the public key can check, offline, that an artifact and its metadata came from this registry: `hinagata verify --keys trusted.json --envelope manifest.json --artifact model.onnx`. The format is in [docs/signatures.md](docs/signatures.md), so other verifiers need no Hinagata code. Without a key the call is refused. After an approved stage change, `GetPromotionAttestation` returns a signed attestation bound to the same artifact digest (approver, decision reference, stage): `hinagata verify --keys trusted.json --attestation attestation.json --artifact model.onnx` ([promotion](docs/promotion.md)).

## Deployment descriptors

`GetOnlineDescriptor` and `GetBatchDescriptor` describe how to run a version: the artifact, runtime, resources, health contract and monitoring paths for a scoring service; or the input binding, output dataset schema and partitioning for a batch job. Each descriptor embeds the version's signed manifest and is signed by the registry. The formats are published as JSON Schemas ([descriptors](docs/descriptors.md)).

## Offline transfer

`hinagata export` writes signed versions and their artifacts into a bundle directory, and `hinagata import` moves it into another registry. The import verifies every signature and digest first. The target admits only versions signed by a key in its `HINAGATA_TRUSTED_KEYS_FILE`, and audits where each one came from ([bundles](docs/bundles.md)).

## Artifacts

Model artifacts are content-addressed: stored once by SHA-256 on local disk (`HINAGATA_ARTIFACT_DIR`, default `artifacts`) or in an S3-compatible bucket (`HINAGATA_S3_BUCKET`, with `HINAGATA_S3_ENDPOINT` for a non-AWS store and the standard `AWS_*` credentials). `HINAGATA_MAX_ARTIFACT_BYTES` caps one artifact (default 1 GiB).

- `BeginArtifactUpload` declares the digest and size, and returns the upload and how many bytes it holds. Calling it again after an interruption tells where to continue.
- `UploadArtifactChunk` sends up to 1 MiB at that offset. A resent chunk is a no-op; any other offset is refused, and a chunk past the declared size is refused as oversized.
- `CompleteArtifactUpload` stores the artifact only if the bytes hash to the digest; otherwise the upload starts over.
- `ReadArtifact` streams the bytes after checking that they still hash to the digest, and refuses a corrupted artifact with `DATA_LOSS`.

A namespace reads only artifacts it uploaded, and a version can only name an artifact its namespace stored. Artifact bytes are gRPC only. Calling `BeginArtifactUpload` for a stored artifact whose bytes no longer verify reopens the upload, so its namespace can send them again. A failed write aborts its multipart upload; for uploads left unfinished by a crashed process, give the bucket a lifecycle rule that aborts incomplete multipart uploads.

## Drift monitoring

A version can register a training baseline: per-feature bin edges and counts, per-category counts, and the outputs' distributions, with PSI and KS alert thresholds. Serving and batch scoring send prediction summaries in those bins ([format](docs/prediction-summary.md), no raw rows). Each summary gets PSI and binned KS per feature and output, and every value at or above a threshold becomes a drift alert with its evidence.

## Model quality

Hinagata stores no predictions or labels. The caller joins labels to its own predictions and sends labeled aggregates ([format](docs/labeled-aggregate.md)): confusion counts and per-class score histograms for classifiers, error sums for regressors. Each gets accuracy, AUC and calibration error, or MAE and RMSE, and a metric past the version's quality thresholds is an alert in the drift alert stream.

| HTTP | gRPC |
| --- | --- |
| `GET /v1/identity` | `GetIdentity` |
| `POST /v1/models`, `GET /v1/models` | `CreateModel`, `ListModels` |
| `GET /v1/models/{model}` | `GetModel` |
| `POST /v1/models/{model}/versions`, `GET /v1/models/{model}/versions` | `RegisterVersion`, `ListVersions` |
| `GET /v1/models/{model}/versions/{version}` | `GetVersion` |
| `GET /v1/models/{model}/versions/{version}/manifest` | `GetVersionManifest` |
| `GET /v1/models/{model}/versions/{version}/promotion-attestation` | `GetPromotionAttestation` |
| `POST /v1/models/{model}/versions/{version}/online-descriptor`, `.../batch-descriptor` | `GetOnlineDescriptor`, `GetBatchDescriptor` |
| gRPC only | `ImportVersion` |
| `GET /v1/models/{model}/signature-compatibility?from_version=…&to_version=…` | `CheckSignatureCompatibility` |
| `GET /v1/lineage/trained-on?dataset_id=…&digest=…` | `ListVersionsTrainedOn` |
| `POST /v1/models/{model}/versions/{version}/stage` | `SetVersionStage` |
| `POST /v1/models/{model}/promotion-gates`, `GET /v1/models/{model}/promotion-gates` | `PutPromotionGates`, `GetPromotionGates` |
| `POST /v1/models/{model}/versions/{version}/drift-baseline` | `PutDriftBaseline` |
| `POST /v1/models/{model}/versions/{version}/prediction-summaries` | `SubmitPredictionSummary` |
| `POST /v1/models/{model}/versions/{version}/quality-thresholds` | `PutQualityThresholds` |
| `POST /v1/models/{model}/versions/{version}/labeled-aggregates` | `SubmitLabeledAggregate` |
| `GET /v1/drift-alerts` | `ListDriftAlerts` |
| `GET /v1/audit-events` | `ListAuditEvents` |

## Operations

- `GET /healthz` answers 200 when the registry database and the artifact store both answer, and 503 naming the failing part otherwise. `GET /metrics` serves Prometheus text: build info, `hinagata_up`, and model, version, artifact and drift alert totals over all namespaces. Neither needs a credential.
- `GET /v1/diagnostics` gives an authenticated caller a bundle for support: version, schema version, configuration (auth kind, artifact store kind, TLS), limits, health, and its namespace's totals. It carries no credentials, paths, bucket names or artifacts.
- The `Dockerfile` builds one static binary into an otherwise empty image, with the builder pinned by digest. The server makes no network calls except to configured storage. CI runs the image with networking disabled.
- `cargo deny check licenses` (`deny.toml`) gates dependencies: CI fails on any license not on the permissive list, so no copyleft code enters the default build.

Status: early. See [VISION.md](VISION.md) for the roadmap and GitHub Issues for planned work.

License: Apache-2.0.
