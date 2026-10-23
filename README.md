# Hinagata

Hinagata is a standalone registry for classical machine learning models, written in Rust.

- Registers models and immutable, content-addressed model versions with their input and output schema, metrics and lineage (training dataset version, code reference, run receipt).
- Promotes versions through stages with an approval hook that the caller governs.
- Produces deployment descriptors: the deployer runs online scoring services, and the batch runner runs scoring jobs.
- Monitors deployed versions: prediction statistics, data drift and, once labels arrive, model quality, with alerts.

Hinagata doesn't make governance decisions. A caller in front of it enforces policy. It stores artifacts on local disk or S3-compatible object storage, and runs as a single binary with no runtime network calls.

## Run

One listener serves gRPC (`hinagata.v1.Hinagata`, see `proto/hinagata/v1/`) and HTTP/JSON. Every call resolves its namespace from the caller's bearer credential, never from the request. Configure one auth hook:

- `HINAGATA_STATIC_TOKEN_FILE` and `HINAGATA_STATIC_NAMESPACE` for standalone or development use;
- `HINAGATA_JWKS_FILE` and `HINAGATA_JWT_AUDIENCE` (optionally `HINAGATA_JWT_ISSUER`, `HINAGATA_TENANT_CLAIM`) to verify JWTs offline against a local JWKS, with the tenant claim as namespace.

`HINAGATA_LISTEN` sets the address (default `127.0.0.1:7700`). `HINAGATA_TLS_CERT` and `HINAGATA_TLS_KEY` serve TLS; adding `HINAGATA_TLS_CLIENT_CA` requires client certificates.

## Registry

Models and versions live in the caller's namespace, in one embedded database file (`HINAGATA_DB_PATH`, default `hinagata.db`; [ADR 0001](docs/decisions/0001-embedded-registry-store.md)).

- A version records the artifact digest (`sha256:<hex>`), framework, format (`onnx`), signature, metrics and tags. It is numbered 1, 2, … per model and never changes once registered. Registering the same artifact again with the same content returns the stored version; with other content it is refused.
- Lineage: each version records its training dataset versions (id, version, digest), code reference (repository, commit, code digest), parameters and the training run's receipt id, all fixed at registration. Registering with `training_run_receipt_json` (a batch runner receipt, schema 6) takes the datasets, run id and code digest from the receipt, records the receipt's digest, and refuses an artifact the run did not produce. `ListVersionsTrainedOn` answers the reverse question: which versions were trained on a dataset, optionally at one digest.
- Stages: a version starts as `candidate`. It moves between `candidate` and `staging`, and from either to `archived`, which is final. Promotion to `production` needs the approval hook and is refused until one is configured. `expected_stage` makes a change apply only if the version has not moved since it was read.
- Every change is an audit event with its actor (the credential's subject), action, and the record before and after.

| HTTP | gRPC |
| --- | --- |
| `GET /v1/identity` | `GetIdentity` |
| `POST /v1/models`, `GET /v1/models` | `CreateModel`, `ListModels` |
| `GET /v1/models/{model}` | `GetModel` |
| `POST /v1/models/{model}/versions`, `GET /v1/models/{model}/versions` | `RegisterVersion`, `ListVersions` |
| `GET /v1/models/{model}/versions/{version}` | `GetVersion` |
| `GET /v1/lineage/trained-on?dataset_id=…&digest=…` | `ListVersionsTrainedOn` |
| `POST /v1/models/{model}/versions/{version}/stage` | `SetVersionStage` |
| `GET /v1/audit-events` | `ListAuditEvents` |

Status: early. See [VISION.md](VISION.md) for the roadmap and GitHub Issues for planned work.

License: Apache-2.0.
