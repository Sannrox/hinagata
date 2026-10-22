# Hinagata

Hinagata is a standalone registry for classical machine learning models, written in Rust.

- Registers models and immutable, content-addressed model versions with their input and output schema, metrics and lineage (training dataset version, code reference, run receipt).
- Promotes versions through stages with an approval hook that the caller governs.
- Produces deployment descriptors: the deployer runs online scoring services, and the batch runner runs scoring jobs.
- Monitors deployed versions: prediction statistics, data drift and, once labels arrive, model quality, with alerts.

Hinagata doesn't make governance decisions. A caller in front of it enforces policy. It stores artifacts on local disk or S3-compatible object storage, and runs as a single binary with no runtime network calls.

## Run

One listener serves gRPC (`hinagata.v1.Hinagata`, see `proto/hinagata/v1/`) and HTTP/JSON (`GET /v1/identity`). Every call resolves its namespace from the caller's bearer credential, never from the request. Configure one auth hook:

- `HINAGATA_STATIC_TOKEN_FILE` and `HINAGATA_STATIC_NAMESPACE` for standalone or development use;
- `HINAGATA_JWKS_FILE` and `HINAGATA_JWT_AUDIENCE` (optionally `HINAGATA_JWT_ISSUER`, `HINAGATA_TENANT_CLAIM`) to verify JWTs offline against a local JWKS, with the tenant claim as namespace.

`HINAGATA_LISTEN` sets the address (default `127.0.0.1:7700`). `HINAGATA_TLS_CERT` and `HINAGATA_TLS_KEY` serve TLS; adding `HINAGATA_TLS_CLIENT_CA` requires client certificates.

Status: early. See [VISION.md](VISION.md) for the roadmap and GitHub Issues for planned work.

License: Apache-2.0.
