# Hinagata vision

Hinagata (雛形, model or pattern) is a standalone, governance-unaware registry for classical ML models (gradient-boosted trees, linear models, small neural networks exported to ONNX, and similar).

## Vision
- A registry of models, versions and immutable artifacts addressed by digest and optionally signed.
- Typed signatures: every version declares its input and output schema, which can be bound to a dataset schema or an object type and is checked at registration.
- Lineage: training dataset version, code reference, metrics and the run receipt of the training job.
- Stages (candidate, staging, production, archived) with an approval hook that the caller governs, and an audit event for every change.
- Deployment descriptors for online scoring (run by the deployer) and batch scoring jobs (run by the batch runner). Hinagata never runs models itself.
- Monitoring: prediction statistics, input drift (PSI, KS), quality metrics once labels arrive, and alert thresholds per version.
- Authorization and markings are enforced by the caller in front of Hinagata. Hinagata only maps an authenticated identity to a namespace.

## Roadmap
1. **v0.1 MVP:** registry and versions; artifact store with digests; signatures and schema checks; lineage; stages with an approval hook; deployment descriptors; drift and quality monitoring; gRPC and HTTP API with an auth hook; metrics and offline packaging.
2. **v0.2:** shadow and A/B rollout descriptors, a feature-set reference, champion and challenger comparison.
3. **Later:** an explainability report per version, and retraining triggers on drift.
