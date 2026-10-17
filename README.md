# Hinagata

Hinagata is a standalone registry for classical machine learning models, written in Rust.

- Registers models and immutable, content-addressed model versions with their input and output schema, metrics and lineage (training dataset version, code reference, run receipt).
- Promotes versions through stages with an approval hook that the caller governs.
- Produces deployment descriptors: the deployer runs online scoring services, and the batch runner runs scoring jobs.
- Monitors deployed versions: prediction statistics, data drift and, once labels arrive, model quality, with alerts.

Hinagata doesn't make governance decisions. A caller in front of it enforces policy. It stores artifacts on local disk or S3-compatible object storage, and runs as a single binary with no runtime network calls.

Status: early. See [VISION.md](VISION.md) for the roadmap and GitHub Issues for planned work.

License: Apache-2.0.
