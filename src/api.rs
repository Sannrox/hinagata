//! gRPC (primary) and HTTP/JSON (secondary) from the one versioned
//! protobuf package, on one listener. Every call first resolves its
//! namespace through the [`Authenticator`]; requests carry none.

use crate::artifacts::{ArtifactError, ArtifactStore, MAX_CHUNK_BYTES};
use crate::auth::{AuthError, Authenticator, Identity, bearer};
use crate::drift;
use crate::quality;
use crate::registry::{self, Actor, Registry, RegistryError, Stage};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::sync::Arc;

pub mod proto {
    tonic::include_proto!("hinagata.v1");
}

use proto::hinagata_server::{Hinagata, HinagataServer};

/// An API failure, carried as a gRPC status or an HTTP status with a JSON
/// `error` body.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: tonic::Code,
    pub message: String,
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        Self {
            code: tonic::Code::Unauthenticated,
            message: error.to_string(),
        }
    }
}

impl From<RegistryError> for ApiError {
    fn from(error: RegistryError) -> Self {
        let code = match &error {
            RegistryError::Invalid(_) => tonic::Code::InvalidArgument,
            RegistryError::NotFound(_) => tonic::Code::NotFound,
            RegistryError::Conflict(_) => tonic::Code::AlreadyExists,
            RegistryError::Refused(_) => tonic::Code::FailedPrecondition,
            RegistryError::TooLarge(_) => tonic::Code::ResourceExhausted,
            RegistryError::Storage(detail) => {
                // The detail can name the database file; keep it in the log.
                eprintln!("hinagata: registry storage: {detail}");
                return Self {
                    code: tonic::Code::Internal,
                    message: "the registry could not complete the request".into(),
                };
            }
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

impl From<ApiError> for tonic::Status {
    fn from(error: ApiError) -> Self {
        tonic::Status::new(error.code, error.message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.code {
            tonic::Code::Unauthenticated => StatusCode::UNAUTHORIZED,
            tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
            tonic::Code::NotFound => StatusCode::NOT_FOUND,
            tonic::Code::AlreadyExists | tonic::Code::FailedPrecondition => StatusCode::CONFLICT,
            tonic::Code::ResourceExhausted => StatusCode::PAYLOAD_TOO_LARGE,
            tonic::Code::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(serde_json::json!({"error": self.message}))).into_response()
    }
}

/// The API over one authentication hook, one registry and one artifact
/// store.
#[derive(Clone)]
pub struct Api {
    auth: Arc<dyn Authenticator>,
    registry: Arc<Registry>,
    artifacts: Arc<ArtifactStore>,
    setup: Arc<Setup>,
    signer: Option<Arc<crate::signing::Signer>>,
    approval_hook: Option<Arc<crate::promotion::ApprovalHook>>,
    trusted_keys: Option<Arc<crate::signing::TrustedKeys>>,
}

/// How the server is configured, without secrets: what the diagnostics
/// bundle reports.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Setup {
    /// `static_token` or `jwt`.
    pub auth: String,
    /// `local` or `s3`.
    pub artifact_store: String,
    pub tls: bool,
    pub mutual_tls: bool,
}

impl From<ArtifactError> for ApiError {
    fn from(error: ArtifactError) -> Self {
        let code = match &error {
            ArtifactError::NotFound => tonic::Code::NotFound,
            ArtifactError::Corrupt(_) => tonic::Code::DataLoss,
            ArtifactError::Mismatch(_) => tonic::Code::InvalidArgument,
            ArtifactError::Storage(detail) => {
                eprintln!("hinagata: artifact storage: {detail}");
                return Self {
                    code: tonic::Code::Unavailable,
                    message: "the artifact store could not complete the request".into(),
                };
            }
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

fn upload_status(upload: registry::ArtifactUpload) -> proto::ArtifactUploadStatus {
    proto::ArtifactUploadStatus {
        upload_id: upload.upload_id,
        digest: upload.digest,
        size_bytes: upload.size_bytes,
        received_bytes: upload.received_bytes,
        complete: upload.complete,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn model_out(model: registry::Model) -> proto::Model {
    proto::Model {
        name: model.name,
        description: model.description,
        created_at_ms: model.created_at_ms,
        created_by: model.created_by,
    }
}

fn spec_in(spec: proto::TensorSpec) -> registry::TensorSpec {
    registry::TensorSpec {
        name: spec.name,
        dtype: spec.dtype,
        shape: spec.shape,
        nullable: spec.nullable,
    }
}

fn spec_out(spec: registry::TensorSpec) -> proto::TensorSpec {
    proto::TensorSpec {
        name: spec.name,
        dtype: spec.dtype,
        shape: spec.shape,
        nullable: spec.nullable,
    }
}

fn lineage_in(lineage: proto::Lineage) -> registry::Lineage {
    let code = lineage.code.unwrap_or_default();
    registry::Lineage {
        training_datasets: lineage
            .training_datasets
            .into_iter()
            .map(|dataset| registry::DatasetVersion {
                dataset_id: dataset.dataset_id,
                version: dataset.version,
                digest: dataset.digest,
            })
            .collect(),
        code: registry::CodeReference {
            repository: code.repository,
            commit: code.commit,
            digest: code.digest,
        },
        parameters: lineage.parameters.into_iter().collect(),
        run_receipt_id: lineage.run_receipt_id,
        run_receipt_digest: lineage.run_receipt_digest,
    }
}

fn lineage_out(lineage: registry::Lineage) -> proto::Lineage {
    proto::Lineage {
        training_datasets: lineage
            .training_datasets
            .into_iter()
            .map(|dataset| proto::DatasetVersion {
                dataset_id: dataset.dataset_id,
                version: dataset.version,
                digest: dataset.digest,
            })
            .collect(),
        code: Some(proto::CodeReference {
            repository: lineage.code.repository,
            commit: lineage.code.commit,
            digest: lineage.code.digest,
        }),
        parameters: lineage.parameters.into_iter().collect(),
        run_receipt_id: lineage.run_receipt_id,
        run_receipt_digest: lineage.run_receipt_digest,
    }
}

/// The lineage a registration declares: as given, or derived from the
/// training run receipt with the given repository, commit and parameters.
fn declared_lineage(
    lineage: Option<proto::Lineage>,
    receipt: &str,
    artifact_digest: &str,
) -> Result<registry::Lineage, ApiError> {
    let given = lineage_in(lineage.unwrap_or_default());
    if receipt.is_empty() {
        return Ok(given);
    }
    let invalid = |message: &str| {
        ApiError::from(RegistryError::Invalid(format!(
            "with a training run receipt, {message}"
        )))
    };
    if !given.training_datasets.is_empty() {
        return Err(invalid("the training datasets come from the receipt"));
    }
    let derived = registry::Lineage::from_run_receipt(receipt, artifact_digest)?;
    let matches = |given: &str, derived: &str| given.is_empty() || given == derived;
    if !matches(&given.run_receipt_id, &derived.run_receipt_id)
        || !matches(&given.run_receipt_digest, &derived.run_receipt_digest)
        || !matches(&given.code.digest, &derived.code.digest)
    {
        return Err(invalid(
            "the run id, receipt digest and code digest must match it",
        ));
    }
    Ok(registry::Lineage {
        code: registry::CodeReference {
            repository: given.code.repository,
            commit: given.code.commit,
            ..derived.code
        },
        parameters: given.parameters,
        ..derived
    })
}

/// Signature reads at registration that may hold an artifact in memory at once.
static SIGNATURE_READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Largest ONNX artifact whose signature is read at registration.
const MAX_SIGNATURE_READ_BYTES: u64 = 256 * 1024 * 1024;

fn binding_in(binding: proto::SignatureBinding) -> crate::signature::Binding {
    crate::signature::Binding {
        target_kind: binding.target_kind,
        target: binding.target,
        fields: binding
            .fields
            .into_iter()
            .map(|field| crate::signature::Field {
                name: field.name,
                dtype: field.dtype,
                nullable: field.nullable,
            })
            .collect(),
        inputs: binding
            .inputs
            .into_iter()
            .map(|(input, fields)| (input, fields.fields))
            .collect(),
    }
}

fn binding_out(binding: crate::signature::Binding) -> proto::SignatureBinding {
    proto::SignatureBinding {
        target_kind: binding.target_kind,
        target: binding.target,
        fields: binding
            .fields
            .into_iter()
            .map(|field| proto::BindingField {
                name: field.name,
                dtype: field.dtype,
                nullable: field.nullable,
            })
            .collect(),
        inputs: binding
            .inputs
            .into_iter()
            .map(|(input, fields)| (input, proto::InputFields { fields }))
            .collect(),
    }
}

fn version_out(version: registry::Version) -> proto::Version {
    proto::Version {
        model: version.model,
        version: version.version,
        artifact_digest: version.artifact_digest,
        framework: version.framework,
        format: version.format,
        signature: Some(proto::Signature {
            inputs: version.signature.inputs.into_iter().map(spec_out).collect(),
            outputs: version
                .signature
                .outputs
                .into_iter()
                .map(spec_out)
                .collect(),
        }),
        metrics: version.metrics.into_iter().collect(),
        tags: version.tags.into_iter().collect(),
        lineage: Some(lineage_out(version.lineage)),
        binding: version.binding.map(binding_out),
        stage: version.stage.as_str().into(),
        created_at_ms: version.created_at_ms,
        created_by: version.created_by,
    }
}

impl Api {
    pub fn new(
        auth: Arc<dyn Authenticator>,
        registry: Arc<Registry>,
        artifacts: Arc<ArtifactStore>,
    ) -> Self {
        Self {
            auth,
            registry,
            artifacts,
            setup: Arc::new(Setup::default()),
            signer: None,
            approval_hook: None,
            trusted_keys: None,
        }
    }

    /// Ask `hook` about every stage change ([`crate::promotion`]).
    pub fn with_approval_hook(mut self, hook: crate::promotion::ApprovalHook) -> Self {
        self.approval_hook = Some(Arc::new(hook));
        self
    }

    /// Admit imported versions signed by `keys`.
    pub fn with_trusted_keys(mut self, keys: crate::signing::TrustedKeys) -> Self {
        self.trusted_keys = Some(Arc::new(keys));
        self
    }

    /// Check an imported version and register it; see `ImportVersion`.
    pub async fn import_version(
        &self,
        authorization: Option<String>,
        request: proto::ImportVersionRequest,
    ) -> Result<proto::ImportVersionResponse, ApiError> {
        let identity = self.identity(authorization.as_deref())?;
        let keys = self.trusted_keys.as_ref().ok_or_else(|| {
            ApiError::from(RegistryError::Refused(
                "this registry trusts no signing keys, so it imports nothing".into(),
            ))
        })?;
        let invalid = |message: String| ApiError::from(RegistryError::Invalid(message));
        let bundled = request.manifest.unwrap_or_default();
        let envelope = crate::signing::Envelope {
            format: bundled.format,
            key_id: bundled.key_id,
            manifest: bundled.manifest,
            signature: bundled.signature,
        };
        let manifest = keys.verify(&envelope).map_err(invalid)?;
        crate::signing::check_contents(&manifest, None, Some(bundled.metadata.as_bytes()))
            .map_err(invalid)?;
        let new = crate::signing::parse_metadata(&bundled.metadata).map_err(invalid)?;
        if request.validate_only {
            return Ok(proto::ImportVersionResponse { version: None });
        }
        // The source read the signature from the graph or checked it
        // against it; this registry checks again with its own reader.
        if new.format == "onnx" {
            let checked = self
                .onnx_signature(&identity, &new.artifact_digest, new.signature.clone())
                .await?;
            if checked != new.signature {
                return Err(invalid(
                    "the signed signature does not match the ONNX graph".into(),
                ));
            }
        }
        let provenance = serde_json::json!({
            "source_namespace": manifest.namespace,
            "source_model": manifest.model,
            "source_version": manifest.version,
            "key_id": envelope.key_id,
            "artifact_digest": manifest.artifact_digest,
            "metadata_digest": manifest.metadata_digest,
        });
        let registry = self.registry.clone();
        let version = blocking(move || {
            Ok(registry.import_version(actor(&identity), &manifest.model, new, &provenance)?)
        })
        .await?;
        Ok(proto::ImportVersionResponse {
            version: Some(version_out(version)),
        })
    }

    /// The configuration the diagnostics bundle reports.
    pub fn with_setup(mut self, setup: Setup) -> Self {
        self.setup = Arc::new(setup);
        self
    }

    /// Which parts cannot do work: the registry database and the artifact
    /// store each get a probe.
    async fn failing(&self) -> Vec<&'static str> {
        let registry = self.registry.clone();
        let mut failing = Vec::new();
        if blocking(move || Ok(registry.health()?)).await.is_err() {
            failing.push("registry");
        }
        if self.artifacts.health().await.is_err() {
            failing.push("artifact_store");
        }
        failing
    }

    /// Prometheus text: build, health and registry counts over all
    /// namespaces. No names, no per-tenant figures.
    async fn metrics(&self) -> String {
        let up = self.failing().await.is_empty();
        let registry = self.registry.clone();
        let totals = blocking(move || Ok(registry.totals(None)?)).await.ok();
        let mut text = format!(
            "# HELP hinagata_build_info The running build.\n\
             # TYPE hinagata_build_info gauge\n\
             hinagata_build_info{{version=\"{}\"}} 1\n\
             # HELP hinagata_up Whether the registry and the artifact store answer.\n\
             # TYPE hinagata_up gauge\n\
             hinagata_up {}\n",
            env!("CARGO_PKG_VERSION"),
            u8::from(up)
        );
        if let Some(totals) = totals {
            for (name, help, value) in [
                (
                    "hinagata_models",
                    "Models in all namespaces.",
                    totals.models,
                ),
                (
                    "hinagata_versions",
                    "Versions in all namespaces.",
                    totals.versions,
                ),
                (
                    "hinagata_artifacts",
                    "Stored artifacts, per namespace that stored them.",
                    totals.artifacts,
                ),
                (
                    "hinagata_drift_alerts",
                    "Drift and quality alerts raised.",
                    totals.drift_alerts,
                ),
                (
                    "hinagata_schema_version",
                    "Registry database schema version.",
                    totals.schema_version,
                ),
            ] {
                text.push_str(&format!(
                    "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n"
                ));
            }
        }
        text
    }

    /// The diagnostics bundle for an authenticated caller: build, schema,
    /// configuration without secrets, health, and its namespace's counts.
    /// It never includes credentials, paths, bucket names or artifacts.
    pub async fn diagnostics(
        &self,
        authorization: Option<String>,
    ) -> Result<serde_json::Value, ApiError> {
        let identity = self.identity(authorization.as_deref())?;
        let failing = self.failing().await;
        let (registry, namespace) = (self.registry.clone(), identity.namespace().to_string());
        let totals = blocking(move || Ok(registry.totals(Some(&namespace))?)).await?;
        Ok(serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "schema_version": totals.schema_version,
            "setup": &*self.setup,
            "max_artifact_bytes": self.artifacts.max_bytes(),
            "healthy": failing.is_empty(),
            "failing": failing,
            "namespace": identity.namespace(),
            "namespace_totals": totals,
        }))
    }

    /// Sign version manifests with `signer`.
    pub fn with_signer(mut self, signer: crate::signing::Signer) -> Self {
        self.signer = Some(Arc::new(signer));
        self
    }

    /// Build a descriptor for a version with `build` and sign it.
    fn signed_descriptor(
        &self,
        authorization: Option<&str>,
        model: &str,
        version: u64,
        build: impl FnOnce(
            &str,
            &registry::Version,
            &serde_json::Value,
        ) -> Result<serde_json::Value, RegistryError>,
    ) -> Result<proto::SignedDescriptor, ApiError> {
        let identity = self.identity(authorization)?;
        let signer = self.signer.as_ref().ok_or_else(|| {
            ApiError::from(RegistryError::Refused(
                "this registry has no signing key, so it signs no descriptors".into(),
            ))
        })?;
        let version = self
            .registry
            .get_version(identity.namespace(), model, version)?;
        let (envelope, metadata) = signer
            .sign(identity.namespace(), &version)
            .map_err(RegistryError::Storage)?;
        let manifest = serde_json::json!({
            "format": envelope.format,
            "key_id": envelope.key_id,
            "manifest": envelope.manifest,
            "signature": envelope.signature,
            "metadata": metadata,
        });
        let descriptor =
            crate::signing::canonical(&build(identity.namespace(), &version, &manifest)?)
                .map_err(RegistryError::Storage)?;
        Ok(proto::SignedDescriptor {
            format: crate::descriptor::SIGNED_FORMAT.into(),
            key_id: signer.key_id().into(),
            signature: signer.sign_bytes(descriptor.as_bytes()),
            descriptor,
        })
    }

    pub fn get_online_descriptor(
        &self,
        authorization: Option<&str>,
        request: proto::GetOnlineDescriptorRequest,
    ) -> Result<proto::SignedDescriptor, ApiError> {
        let resources = request.resources.unwrap_or_default();
        let options = crate::descriptor::OnlineOptions {
            runtime_image: request.runtime_image,
            resources: crate::descriptor::Resources {
                cpu_millis: resources.cpu_millis,
                memory_mib: resources.memory_mib,
                replicas: resources.replicas,
            },
        };
        self.signed_descriptor(
            authorization,
            &request.model,
            request.version,
            |namespace, version, manifest| {
                crate::descriptor::online(namespace, version, manifest, &options)
            },
        )
    }

    pub fn get_batch_descriptor(
        &self,
        authorization: Option<&str>,
        request: proto::GetBatchDescriptorRequest,
    ) -> Result<proto::SignedDescriptor, ApiError> {
        let options = crate::descriptor::BatchOptions {
            runtime_image: request.runtime_image,
            input: request.input,
            output_dataset: request.output_dataset,
            partition_column: request.partition_column,
            partitions: request.partitions,
        };
        self.signed_descriptor(
            authorization,
            &request.model,
            request.version,
            |namespace, version, manifest| {
                crate::descriptor::batch(namespace, version, manifest, &options)
            },
        )
    }

    pub fn get_version_manifest(
        &self,
        authorization: Option<&str>,
        request: proto::GetVersionManifestRequest,
    ) -> Result<proto::VersionManifest, ApiError> {
        let identity = self.identity(authorization)?;
        let signer = self.signer.as_ref().ok_or_else(|| {
            ApiError::from(RegistryError::Refused(
                "this registry has no signing key".into(),
            ))
        })?;
        let version =
            self.registry
                .get_version(identity.namespace(), &request.model, request.version)?;
        let (envelope, metadata) = signer
            .sign(identity.namespace(), &version)
            .map_err(RegistryError::Storage)?;
        Ok(proto::VersionManifest {
            format: envelope.format,
            key_id: envelope.key_id,
            manifest: envelope.manifest,
            signature: envelope.signature,
            metadata,
        })
    }

    /// Open or resume an upload. A stored artifact whose bytes no longer
    /// verify is reopened, so its namespace can send them again.
    pub async fn begin_artifact_upload(
        &self,
        authorization: Option<String>,
        request: proto::BeginArtifactUploadRequest,
    ) -> Result<proto::ArtifactUploadStatus, ApiError> {
        let identity = self.identity(authorization.as_deref())?;
        let (registry, namespace, subject, max) = (
            self.registry.clone(),
            identity.namespace().to_string(),
            identity.subject().to_string(),
            self.artifacts.max_bytes(),
        );
        let (digest, size) = (request.digest.clone(), request.size_bytes);
        let upload = {
            let (registry, namespace, subject, digest) = (
                registry.clone(),
                namespace.clone(),
                subject.clone(),
                digest.clone(),
            );
            blocking(move || {
                let actor = Actor {
                    namespace: &namespace,
                    subject: &subject,
                    now_ms: now_ms(),
                };
                Ok(registry.begin_artifact_upload(actor, &digest, size, max)?)
            })
            .await?
        };
        if !upload.complete {
            return Ok(upload_status(upload));
        }
        match self
            .artifacts
            .verify(digest.trim_start_matches("sha256:"))
            .await
        {
            Ok(_) => Ok(upload_status(upload)),
            Err(ArtifactError::NotFound | ArtifactError::Corrupt(_)) => {
                let upload = blocking(move || {
                    let actor = Actor {
                        namespace: &namespace,
                        subject: &subject,
                        now_ms: now_ms(),
                    };
                    Ok(registry.reopen_artifact_upload(actor, &digest)?)
                })
                .await?;
                Ok(upload_status(upload))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Stage a chunk, then advance the upload past it. Staging first means
    /// a crash in between leaves the upload where it was, and the chunk is
    /// sent again.
    pub async fn upload_artifact_chunk(
        &self,
        authorization: Option<String>,
        request: proto::UploadArtifactChunkRequest,
    ) -> Result<proto::ArtifactUploadStatus, ApiError> {
        let identity = self.identity(authorization.as_deref())?;
        let (registry, namespace, upload_id) = (
            self.registry.clone(),
            identity.namespace().to_string(),
            request.upload_id.clone(),
        );
        let (offset, len) = (request.offset, request.data.len());
        let (stage, attempt) = blocking(move || {
            Ok(registry.check_artifact_chunk(
                &namespace,
                &upload_id,
                offset,
                len,
                MAX_CHUNK_BYTES,
            )?)
        })
        .await?;
        let key = if stage {
            Some(
                self.artifacts
                    .stage_chunk(&request.upload_id, attempt, offset, request.data.into())
                    .await?,
            )
        } else {
            None
        };
        let (registry, namespace) = (self.registry.clone(), identity.namespace().to_string());
        let upload = blocking(move || {
            Ok(match key {
                Some(key) => registry.advance_artifact_upload(
                    &namespace,
                    &request.upload_id,
                    attempt,
                    offset,
                    len,
                    &key,
                )?,
                None => registry.get_artifact_upload(&namespace, &request.upload_id)?,
            })
        })
        .await?;
        Ok(upload_status(upload))
    }

    /// Hash the staged bytes and store the artifact when they match. A
    /// mismatch drops the bytes and starts the upload over.
    pub async fn complete_artifact_upload(
        &self,
        authorization: Option<String>,
        request: proto::CompleteArtifactUploadRequest,
    ) -> Result<proto::ArtifactUploadStatus, ApiError> {
        let identity = self.identity(authorization.as_deref())?;
        let (registry, namespace, upload_id) = (
            self.registry.clone(),
            identity.namespace().to_string(),
            request.upload_id,
        );
        let upload = {
            let (registry, namespace, upload_id) =
                (registry.clone(), namespace.clone(), upload_id.clone());
            blocking(move || Ok(registry.get_artifact_upload(&namespace, &upload_id)?)).await?
        };
        if upload.complete {
            // A crash after recording may have left staged bytes behind.
            self.artifacts
                .discard_upload(&upload_id, upload.attempt)
                .await?;
            return Ok(upload_status(upload));
        }
        if upload.received_bytes != upload.size_bytes {
            return Err(RegistryError::Refused(format!(
                "the upload holds {} of {} bytes",
                upload.received_bytes, upload.size_bytes
            ))
            .into());
        }
        let hex = upload.digest.trim_start_matches("sha256:").to_string();
        let chunks = {
            let (registry, namespace, upload_id) =
                (registry.clone(), namespace.clone(), upload_id.clone());
            blocking(move || Ok(registry.artifact_upload_chunks(&namespace, &upload_id)?)).await?
        };
        // Publish, record, then drop the staged bytes: a crash at any point
        // leaves an upload that completing again finishes.
        if let Err(error) = self
            .artifacts
            .publish(&chunks, &hex, upload.size_bytes)
            .await
        {
            if matches!(error, ArtifactError::Mismatch(_)) {
                let (registry, namespace, reset_id) =
                    (registry.clone(), namespace.clone(), upload_id.clone());
                let attempt = upload.attempt;
                blocking(move || {
                    Ok(registry.reset_artifact_upload(&namespace, &reset_id, attempt)?)
                })
                .await?;
                self.artifacts.discard_upload(&upload_id, attempt).await?;
            }
            return Err(error.into());
        }
        let subject = identity.subject().to_string();
        let attempt = upload.attempt;
        let recorded = {
            let upload_id = upload_id.clone();
            blocking(move || {
                let actor = Actor {
                    namespace: &namespace,
                    subject: &subject,
                    now_ms: now_ms(),
                };
                Ok(registry.finish_artifact_upload(actor, &upload_id, attempt)?)
            })
            .await?
        };
        self.artifacts.discard_upload(&upload_id, attempt).await?;
        Ok(upload_status(recorded))
    }

    /// A stored artifact of the caller's namespace, verified before the first
    /// byte. Another namespace's artifact is `NOT_FOUND`.
    pub async fn read_artifact(
        &self,
        authorization: Option<String>,
        request: proto::ReadArtifactRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<proto::ArtifactChunk, tonic::Status>>,
        ApiError,
    > {
        use futures::StreamExt;
        let identity = self.identity(authorization.as_deref())?;
        let (registry, namespace, digest) = (
            self.registry.clone(),
            identity.namespace().to_string(),
            request.digest.clone(),
        );
        blocking(move || Ok(registry.artifact_size(&namespace, &digest)?))
            .await?
            .ok_or_else(|| ApiError::from(ArtifactError::NotFound))?;
        let hex = request.digest.trim_start_matches("sha256:");
        let stream = self.artifacts.read_verified(hex).await?;
        Ok(stream
            .map(|chunk| {
                chunk
                    .map(|data| proto::ArtifactChunk {
                        data: data.to_vec(),
                    })
                    .map_err(|error| tonic::Status::from(ApiError::from(error)))
            })
            .boxed())
    }

    /// The caller's identity from its `authorization` value.
    fn identity(&self, authorization: Option<&str>) -> Result<Identity, ApiError> {
        Ok(self.auth.authenticate(bearer(authorization))?)
    }

    pub fn get_identity(
        &self,
        authorization: Option<&str>,
        _request: proto::GetIdentityRequest,
    ) -> Result<proto::GetIdentityResponse, ApiError> {
        let identity = self.identity(authorization)?;
        Ok(proto::GetIdentityResponse {
            namespace: identity.namespace().to_string(),
            subject: identity.subject().to_string(),
        })
    }

    pub fn create_model(
        &self,
        authorization: Option<&str>,
        request: proto::CreateModelRequest,
    ) -> Result<proto::Model, ApiError> {
        let identity = self.identity(authorization)?;
        let model =
            self.registry
                .create_model(actor(&identity), &request.name, &request.description)?;
        Ok(model_out(model))
    }

    pub fn get_model(
        &self,
        authorization: Option<&str>,
        request: proto::GetModelRequest,
    ) -> Result<proto::Model, ApiError> {
        let identity = self.identity(authorization)?;
        Ok(model_out(
            self.registry
                .get_model(identity.namespace(), &request.name)?,
        ))
    }

    pub fn list_models(
        &self,
        authorization: Option<&str>,
        request: proto::ListModelsRequest,
    ) -> Result<proto::ListModelsResponse, ApiError> {
        let identity = self.identity(authorization)?;
        let models =
            self.registry
                .list_models(identity.namespace(), &request.after, request.limit)?;
        Ok(proto::ListModelsResponse {
            models: models.into_iter().map(model_out).collect(),
        })
    }

    /// Register a version. For an ONNX artifact the signature comes from the
    /// model graph when omitted, and must match it when given.
    pub async fn register_version(
        &self,
        authorization: Option<String>,
        request: proto::RegisterVersionRequest,
    ) -> Result<proto::Version, ApiError> {
        let identity = self.identity(authorization.as_deref())?;
        let declared = request.signature.unwrap_or_default();
        let declared = registry::Signature {
            inputs: declared.inputs.into_iter().map(spec_in).collect(),
            outputs: declared.outputs.into_iter().map(spec_in).collect(),
        };
        // A retry of a stored registration keeps the signature it stored,
        // including one from before signatures were checked against the
        // graph; the registry compares the rest.
        let existing = {
            let (registry, namespace, model, digest) = (
                self.registry.clone(),
                identity.namespace().to_string(),
                request.model.clone(),
                request.artifact_digest.clone(),
            );
            blocking(move || Ok(registry.version_for_artifact(&namespace, &model, &digest)?))
                .await?
        };
        let signature =
            if let Some(existing) = existing.filter(|existing| existing.signature == declared) {
                existing.signature
            } else if request.format == "onnx" {
                self.onnx_signature(&identity, &request.artifact_digest, declared)
                    .await?
            } else {
                declared
            };
        let lineage = declared_lineage(
            request.lineage,
            &request.training_run_receipt_json,
            &request.artifact_digest,
        )?;
        let new = registry::NewVersion {
            artifact_digest: request.artifact_digest,
            framework: request.framework,
            format: request.format,
            signature,
            metrics: request.metrics.into_iter().collect(),
            tags: request.tags.into_iter().collect(),
            lineage,
            binding: request.binding.map(binding_in),
        };
        let (registry, namespace, subject, model) = (
            self.registry.clone(),
            identity.namespace().to_string(),
            identity.subject().to_string(),
            request.model,
        );
        let version = blocking(move || {
            let actor = Actor {
                namespace: &namespace,
                subject: &subject,
                now_ms: now_ms(),
            };
            Ok(registry.register_version(actor, &model, new)?)
        })
        .await?;
        Ok(version_out(version))
    }

    /// The signature of a stored ONNX artifact: read from its graph, or the
    /// declared one when it matches. An artifact too large to read here needs
    /// a declared signature. An unstored one keeps the declaration, and
    /// registration refuses it anyway.
    async fn onnx_signature(
        &self,
        identity: &Identity,
        digest: &str,
        declared: registry::Signature,
    ) -> Result<registry::Signature, ApiError> {
        use futures::StreamExt;
        let (registry, namespace, key) = (
            self.registry.clone(),
            identity.namespace().to_string(),
            digest.to_string(),
        );
        let size = blocking(move || Ok(registry.artifact_size(&namespace, &key)?)).await?;
        match size {
            None => return Ok(declared),
            Some(size) if size > MAX_SIGNATURE_READ_BYTES => {
                return if declared.inputs.is_empty() {
                    Err(RegistryError::Invalid(format!(
                        "declare a signature: the artifact is over {MAX_SIGNATURE_READ_BYTES} bytes, too large to read one from"
                    ))
                    .into())
                } else {
                    Ok(declared)
                };
            }
            Some(_) => {}
        }
        // Reading and parsing a large model is bounded: at most a few at once,
        // and the parse runs on the blocking pool.
        let permit = SIGNATURE_READS
            .acquire()
            .await
            .map_err(|_| ApiError::from(RegistryError::Storage("signature reads closed".into())))?;
        let mut bytes = Vec::new();
        let mut stream = self
            .artifacts
            .read_verified(digest.trim_start_matches("sha256:"))
            .await?;
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        let invalid = |message: String| ApiError::from(RegistryError::Invalid(message));
        // The permit moves into the parse, so a cancelled request cannot free
        // it while the parse still holds the bytes.
        let graph = match blocking(move || {
            let _permit = permit;
            Ok(crate::signature::read_graph(&bytes))
        })
        .await?
        {
            Ok(graph) => graph,
            // A graph too large to read cannot be checked, so it is refused.
            Err(error) if error.starts_with(crate::signature::LIMIT_ERROR) => {
                return Err(invalid(error));
            }
            // Not an ONNX model at all: there is nothing to check the
            // declaration against.
            Err(error) if declared.inputs.is_empty() => {
                return Err(invalid(format!(
                    "declare a signature: none could be read from the artifact ({error})"
                )));
            }
            Err(_) => return Ok(declared),
        };
        if declared.inputs.is_empty() && declared.outputs.is_empty() {
            let take = |values: Vec<crate::signature::GraphValue>| {
                values
                    .iter()
                    .map(|value| {
                        value.spec().map_err(|why| {
                            invalid(format!("declare a signature: the graph cannot give {why}"))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            };
            return Ok(registry::Signature {
                inputs: take(graph.0)?,
                outputs: take(graph.1)?,
            });
        }
        crate::signature::verify_against_graph(&declared, &graph).map_err(|why| {
            invalid(format!(
                "the declared signature does not match the ONNX graph: {why}"
            ))
        })?;
        Ok(declared)
    }

    pub fn check_signature_compatibility(
        &self,
        authorization: Option<&str>,
        request: proto::CheckSignatureCompatibilityRequest,
    ) -> Result<proto::CheckSignatureCompatibilityResponse, ApiError> {
        let identity = self.identity(authorization)?;
        let from = self.registry.get_version(
            identity.namespace(),
            &request.model,
            request.from_version,
        )?;
        let to =
            self.registry
                .get_version(identity.namespace(), &request.model, request.to_version)?;
        let breaking = crate::signature::breaking_changes(&from.signature, &to.signature);
        Ok(proto::CheckSignatureCompatibilityResponse {
            compatible: breaking.is_empty(),
            breaking,
        })
    }

    pub fn get_version(
        &self,
        authorization: Option<&str>,
        request: proto::GetVersionRequest,
    ) -> Result<proto::Version, ApiError> {
        let identity = self.identity(authorization)?;
        let version =
            self.registry
                .get_version(identity.namespace(), &request.model, request.version)?;
        Ok(version_out(version))
    }

    pub fn list_versions(
        &self,
        authorization: Option<&str>,
        request: proto::ListVersionsRequest,
    ) -> Result<proto::ListVersionsResponse, ApiError> {
        let identity = self.identity(authorization)?;
        let versions = self.registry.list_versions(
            identity.namespace(),
            &request.model,
            request.after,
            request.limit,
        )?;
        Ok(proto::ListVersionsResponse {
            versions: versions.into_iter().map(version_out).collect(),
        })
    }

    pub fn list_versions_trained_on(
        &self,
        authorization: Option<&str>,
        request: proto::ListVersionsTrainedOnRequest,
    ) -> Result<proto::ListVersionsResponse, ApiError> {
        let identity = self.identity(authorization)?;
        let digest = (!request.digest.is_empty()).then_some(request.digest.as_str());
        let versions = self.registry.list_versions_trained_on(
            identity.namespace(),
            &request.dataset_id,
            digest,
            (&request.after_model, request.after_version),
            request.limit,
        )?;
        Ok(proto::ListVersionsResponse {
            versions: versions.into_iter().map(version_out).collect(),
        })
    }

    /// Change a version's stage. With an approval hook, the change is
    /// checked (including the model's promotion gates for production), the
    /// hook is asked, and the change applies only on an allow; a deny is
    /// audited and refused with the hook's reason.
    pub async fn set_version_stage(
        &self,
        authorization: Option<String>,
        request: proto::SetVersionStageRequest,
    ) -> Result<proto::Version, ApiError> {
        let identity = self.identity(authorization.as_deref())?;
        let stage = Stage::parse(&request.stage)?;
        let expected = match request.expected_stage.as_str() {
            "" => None,
            value => Some(Stage::parse(value)?),
        };
        let Some(hook) = self.approval_hook.clone() else {
            let registry = self.registry.clone();
            return blocking(move || {
                let version = registry.set_stage(
                    actor(&identity),
                    &request.model,
                    request.version,
                    stage,
                    expected,
                )?;
                Ok(version_out(version))
            })
            .await;
        };
        let signed = self.signer.is_some();
        let pending = {
            let (registry, namespace, model) = (
                self.registry.clone(),
                identity.namespace().to_string(),
                request.model.clone(),
            );
            let version = request.version;
            blocking(move || {
                Ok(registry
                    .prepare_stage_change(&namespace, &model, version, stage, expected, signed)?)
            })
            .await?
        };
        if pending.version.stage == stage {
            return Ok(version_out(pending.version));
        }
        let manifest = match &self.signer {
            Some(signer) => {
                let (envelope, metadata) = signer
                    .sign(identity.namespace(), &pending.version)
                    .map_err(RegistryError::Storage)?;
                serde_json::json!({
                    "format": envelope.format,
                    "key_id": envelope.key_id,
                    "manifest": envelope.manifest,
                    "signature": envelope.signature,
                    "metadata": metadata,
                })
            }
            None => serde_json::Value::Null,
        };
        let request_id = crate::promotion::request_id(
            identity.namespace(),
            identity.subject(),
            &pending,
            stage,
            &manifest,
        );
        let decision = hook
            .decide(&serde_json::json!({
                "format": crate::promotion::REQUEST_FORMAT,
                "request_id": request_id,
                "namespace": identity.namespace(),
                "actor": identity.subject(),
                "model": pending.version.model,
                "version": pending.version.version,
                "from_stage": pending.version.stage.as_str(),
                "to_stage": stage.as_str(),
                "artifact_digest": pending.version.artifact_digest,
                "metrics": pending.version.metrics,
                "gates": pending.gates,
                "manifest": manifest,
            }))
            .await;
        let registry = self.registry.clone();
        blocking(move || {
            if decision.allow {
                let version = registry.apply_approved_stage(
                    actor(&identity),
                    &pending,
                    &request_id,
                    stage,
                    &decision,
                    signed,
                )?;
                return Ok(version_out(version));
            }
            registry.record_stage_denial(
                actor(&identity),
                &pending,
                &request_id,
                stage,
                &decision,
            )?;
            Err(RegistryError::Refused(format!(
                "the approval hook denied the change: {}",
                decision.reason
            ))
            .into())
        })
        .await
    }

    pub fn put_promotion_gates(
        &self,
        authorization: Option<&str>,
        request: proto::PutPromotionGatesRequest,
    ) -> Result<proto::PromotionGates, ApiError> {
        let identity = self.identity(authorization)?;
        let gates = request.gates.unwrap_or_default();
        let gates = self.registry.put_promotion_gates(
            actor(&identity),
            &request.model,
            &crate::promotion::Gates {
                min_metrics: gates.min_metrics.into_iter().collect(),
                require_compatible_signature: gates.require_compatible_signature,
                require_signed_manifest: gates.require_signed_manifest,
            },
        )?;
        Ok(gates_out(gates))
    }

    pub fn get_promotion_gates(
        &self,
        authorization: Option<&str>,
        request: proto::GetPromotionGatesRequest,
    ) -> Result<proto::PromotionGates, ApiError> {
        let identity = self.identity(authorization)?;
        Ok(gates_out(
            self.registry
                .promotion_gates(identity.namespace(), &request.model)?,
        ))
    }

    pub fn put_drift_baseline(
        &self,
        authorization: Option<&str>,
        request: proto::PutDriftBaselineRequest,
    ) -> Result<proto::PutDriftBaselineResponse, ApiError> {
        let identity = self.identity(authorization)?;
        let baseline = request.baseline.unwrap_or_default();
        let thresholds = request.thresholds.unwrap_or_default();
        self.registry.put_drift_baseline(
            actor(&identity),
            &request.model,
            request.version,
            &drift::Baseline {
                features: distributions_in(baseline.features)?,
                outputs: distributions_in(baseline.outputs)?,
            },
            drift::Thresholds {
                psi: thresholds.psi,
                ks: thresholds.ks,
            },
        )?;
        Ok(proto::PutDriftBaselineResponse {})
    }

    pub fn submit_prediction_summary(
        &self,
        authorization: Option<&str>,
        request: proto::SubmitPredictionSummaryRequest,
    ) -> Result<proto::DriftReport, ApiError> {
        let identity = self.identity(authorization)?;
        let summary = request.summary.unwrap_or_default();
        if summary.format != drift::SUMMARY_FORMAT {
            return Err(RegistryError::Invalid(format!(
                "summary format must be {}",
                drift::SUMMARY_FORMAT
            ))
            .into());
        }
        let report = self.registry.submit_prediction_summary(
            actor(&identity),
            &request.model,
            request.version,
            &drift::Summary {
                summary_id: summary.summary_id,
                window_start_ms: summary.window_start_ms,
                window_end_ms: summary.window_end_ms,
                prediction_count: summary.prediction_count,
                latency_p50_ms: summary.latency_p50_ms,
                latency_p95_ms: summary.latency_p95_ms,
                features: distributions_in(summary.features)?,
                outputs: distributions_in(summary.outputs)?,
            },
        )?;
        Ok(proto::DriftReport {
            summary_id: report.summary_id,
            drift: report
                .drift
                .into_iter()
                .map(|feature| proto::FeatureDrift {
                    role: feature.role,
                    name: feature.name,
                    psi: feature.psi,
                    ks: feature.ks,
                })
                .collect(),
            alerts: report.alerts.into_iter().map(alert_out).collect(),
        })
    }

    pub fn put_quality_thresholds(
        &self,
        authorization: Option<&str>,
        request: proto::PutQualityThresholdsRequest,
    ) -> Result<proto::PutQualityThresholdsResponse, ApiError> {
        let identity = self.identity(authorization)?;
        let thresholds = request.thresholds.unwrap_or_default();
        self.registry.put_quality_thresholds(
            actor(&identity),
            &request.model,
            request.version,
            quality::QualityThresholds {
                min_accuracy: thresholds.min_accuracy,
                min_auc: thresholds.min_auc,
                max_calibration_error: thresholds.max_calibration_error,
                max_mae: thresholds.max_mae,
                max_rmse: thresholds.max_rmse,
            },
        )?;
        Ok(proto::PutQualityThresholdsResponse {})
    }

    pub fn submit_labeled_aggregate(
        &self,
        authorization: Option<&str>,
        request: proto::SubmitLabeledAggregateRequest,
    ) -> Result<proto::QualityReport, ApiError> {
        use proto::labeled_aggregate::Kind;
        let identity = self.identity(authorization)?;
        let aggregate = request.aggregate.unwrap_or_default();
        if aggregate.format != quality::AGGREGATE_FORMAT {
            return Err(RegistryError::Invalid(format!(
                "aggregate format must be {}",
                quality::AGGREGATE_FORMAT
            ))
            .into());
        }
        let kind = match aggregate.kind {
            Some(Kind::Classifier(classifier)) => quality::Aggregate::Classifier {
                labels: classifier.labels,
                confusion: classifier
                    .confusion
                    .into_iter()
                    .map(|row| row.counts)
                    .collect(),
                score_histograms: classifier
                    .score_histograms
                    .into_iter()
                    .map(|(class, histogram)| {
                        (
                            class,
                            quality::ScoreHistogram {
                                positive_counts: histogram.positive_counts,
                                negative_counts: histogram.negative_counts,
                                score_sums: histogram.score_sums,
                            },
                        )
                    })
                    .collect(),
            },
            Some(Kind::Regressor(regressor)) => quality::Aggregate::Regressor {
                count: regressor.count,
                absolute_error_sum: regressor.absolute_error_sum,
                squared_error_sum: regressor.squared_error_sum,
            },
            None => {
                return Err(RegistryError::Invalid(
                    "an aggregate is a classifier or a regressor".into(),
                )
                .into());
            }
        };
        let report = self.registry.submit_labeled_aggregate(
            actor(&identity),
            &request.model,
            request.version,
            &quality::LabeledAggregate {
                aggregate_id: aggregate.aggregate_id,
                window_start_ms: aggregate.window_start_ms,
                window_end_ms: aggregate.window_end_ms,
                aggregate: kind,
            },
        )?;
        Ok(proto::QualityReport {
            aggregate_id: report.aggregate_id,
            label_count: report.label_count,
            metrics: report
                .metrics
                .into_iter()
                .map(|metric| proto::QualityMetric {
                    name: metric.name,
                    metric: metric.metric,
                    value: metric.value,
                    tolerance: metric.tolerance,
                })
                .collect(),
            alerts: report.alerts.into_iter().map(alert_out).collect(),
        })
    }

    pub fn list_drift_alerts(
        &self,
        authorization: Option<&str>,
        request: proto::ListDriftAlertsRequest,
    ) -> Result<proto::ListDriftAlertsResponse, ApiError> {
        let identity = self.identity(authorization)?;
        let model = (!request.model.is_empty()).then_some(request.model.as_str());
        let alerts = self.registry.list_drift_alerts(
            identity.namespace(),
            model,
            request.after_seq,
            request.limit,
        )?;
        Ok(proto::ListDriftAlertsResponse {
            alerts: alerts.into_iter().map(alert_out).collect(),
        })
    }

    pub fn list_audit_events(
        &self,
        authorization: Option<&str>,
        request: proto::ListAuditEventsRequest,
    ) -> Result<proto::ListAuditEventsResponse, ApiError> {
        let identity = self.identity(authorization)?;
        let model = (!request.model.is_empty()).then_some(request.model.as_str());
        let events = self.registry.list_audit_events(
            identity.namespace(),
            model,
            request.after_seq,
            request.limit,
        )?;
        Ok(proto::ListAuditEventsResponse {
            events: events
                .into_iter()
                .map(|event| proto::AuditEvent {
                    seq: event.seq,
                    at_ms: event.at_ms,
                    actor: event.actor,
                    action: event.action,
                    model: event.model,
                    version: event.version,
                    before: event.before.unwrap_or_default(),
                    after: event.after.unwrap_or_default(),
                })
                .collect(),
        })
    }
}

fn distribution_in(
    name: &str,
    distribution: proto::Distribution,
) -> Result<drift::Distribution, ApiError> {
    use proto::distribution::Bins;
    match distribution.bins {
        Some(Bins::Numeric(bins)) => Ok(drift::Distribution::Numeric {
            edges: bins.edges,
            counts: bins.counts,
        }),
        Some(Bins::Categorical(bins)) => Ok(drift::Distribution::Categorical {
            counts: bins.counts.into_iter().collect(),
        }),
        None => Err(
            RegistryError::Invalid(format!("`{name}` needs numeric or categorical bins")).into(),
        ),
    }
}

fn distributions_in(
    set: std::collections::HashMap<String, proto::Distribution>,
) -> Result<std::collections::BTreeMap<String, drift::Distribution>, ApiError> {
    set.into_iter()
        .map(|(name, distribution)| Ok((name.clone(), distribution_in(&name, distribution)?)))
        .collect()
}

fn alert_out(alert: drift::DriftAlert) -> proto::DriftAlert {
    proto::DriftAlert {
        seq: alert.seq,
        at_ms: alert.at_ms,
        model: alert.model,
        version: alert.version,
        summary_id: alert.summary_id,
        role: alert.role,
        name: alert.name,
        metric: alert.metric,
        value: alert.value,
        threshold: alert.threshold,
    }
}

/// The acting identity: its namespace scopes the write, its subject is
/// recorded as the actor.
fn gates_out(gates: crate::promotion::Gates) -> proto::PromotionGates {
    proto::PromotionGates {
        min_metrics: gates.min_metrics.into_iter().collect(),
        require_compatible_signature: gates.require_compatible_signature,
        require_signed_manifest: gates.require_signed_manifest,
    }
}

fn actor(identity: &Identity) -> Actor<'_> {
    Actor {
        namespace: identity.namespace(),
        subject: identity.subject(),
        now_ms: now_ms(),
    }
}

fn metadata_auth<T>(request: &tonic::Request<T>) -> Option<String> {
    request
        .metadata()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Run an API call on the blocking pool: registry calls wait on SQLite and
/// its lock, which must not hold up the async workers.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(call).await.unwrap_or_else(|_| {
        Err(ApiError {
            code: tonic::Code::Internal,
            message: "the request did not complete".into(),
        })
    })
}

/// Each gRPC method calls the [`Api`] method of the same name with the
/// request's `authorization` metadata, on the blocking pool.
macro_rules! grpc_methods {
    ($($method:ident($request:ident) -> $response:ident;)* { $($extra:tt)* }) => {
        #[tonic::async_trait]
        impl Hinagata for Api {
            $($extra)*
            $(
                async fn $method(
                    &self,
                    request: tonic::Request<proto::$request>,
                ) -> Result<tonic::Response<proto::$response>, tonic::Status> {
                    let auth = metadata_auth(&request);
                    let (api, request) = (self.clone(), request.into_inner());
                    let response =
                        blocking(move || Api::$method(&api, auth.as_deref(), request)).await?;
                    Ok(tonic::Response::new(response))
                }
            )*
        }
    };
}

grpc_methods! {
    get_identity(GetIdentityRequest) -> GetIdentityResponse;
    create_model(CreateModelRequest) -> Model;
    get_model(GetModelRequest) -> Model;
    list_models(ListModelsRequest) -> ListModelsResponse;
    get_version(GetVersionRequest) -> Version;
    list_versions(ListVersionsRequest) -> ListVersionsResponse;
    list_versions_trained_on(ListVersionsTrainedOnRequest) -> ListVersionsResponse;
    put_promotion_gates(PutPromotionGatesRequest) -> PromotionGates;
    get_promotion_gates(GetPromotionGatesRequest) -> PromotionGates;
    check_signature_compatibility(CheckSignatureCompatibilityRequest) -> CheckSignatureCompatibilityResponse;
    get_version_manifest(GetVersionManifestRequest) -> VersionManifest;
    get_online_descriptor(GetOnlineDescriptorRequest) -> SignedDescriptor;
    get_batch_descriptor(GetBatchDescriptorRequest) -> SignedDescriptor;
    list_audit_events(ListAuditEventsRequest) -> ListAuditEventsResponse;
    put_drift_baseline(PutDriftBaselineRequest) -> PutDriftBaselineResponse;
    submit_prediction_summary(SubmitPredictionSummaryRequest) -> DriftReport;
    list_drift_alerts(ListDriftAlertsRequest) -> ListDriftAlertsResponse;
    put_quality_thresholds(PutQualityThresholdsRequest) -> PutQualityThresholdsResponse;
    submit_labeled_aggregate(SubmitLabeledAggregateRequest) -> QualityReport;
    {
        async fn register_version(
            &self,
            request: tonic::Request<proto::RegisterVersionRequest>,
        ) -> Result<tonic::Response<proto::Version>, tonic::Status> {
            let auth = metadata_auth(&request);
            let response = Api::register_version(self, auth, request.into_inner()).await?;
            Ok(tonic::Response::new(response))
        }

        async fn set_version_stage(
            &self,
            request: tonic::Request<proto::SetVersionStageRequest>,
        ) -> Result<tonic::Response<proto::Version>, tonic::Status> {
            let auth = metadata_auth(&request);
            let response = Api::set_version_stage(self, auth, request.into_inner()).await?;
            Ok(tonic::Response::new(response))
        }

        async fn import_version(
            &self,
            request: tonic::Request<proto::ImportVersionRequest>,
        ) -> Result<tonic::Response<proto::ImportVersionResponse>, tonic::Status> {
            let auth = metadata_auth(&request);
            let response = Api::import_version(self, auth, request.into_inner()).await?;
            Ok(tonic::Response::new(response))
        }

        async fn begin_artifact_upload(
            &self,
            request: tonic::Request<proto::BeginArtifactUploadRequest>,
        ) -> Result<tonic::Response<proto::ArtifactUploadStatus>, tonic::Status> {
            let auth = metadata_auth(&request);
            let response = Api::begin_artifact_upload(self, auth, request.into_inner()).await?;
            Ok(tonic::Response::new(response))
        }

        type ReadArtifactStream =
            futures::stream::BoxStream<'static, Result<proto::ArtifactChunk, tonic::Status>>;

        async fn upload_artifact_chunk(
            &self,
            request: tonic::Request<proto::UploadArtifactChunkRequest>,
        ) -> Result<tonic::Response<proto::ArtifactUploadStatus>, tonic::Status> {
            let auth = metadata_auth(&request);
            let response = Api::upload_artifact_chunk(self, auth, request.into_inner()).await?;
            Ok(tonic::Response::new(response))
        }

        async fn complete_artifact_upload(
            &self,
            request: tonic::Request<proto::CompleteArtifactUploadRequest>,
        ) -> Result<tonic::Response<proto::ArtifactUploadStatus>, tonic::Status> {
            let auth = metadata_auth(&request);
            let response = Api::complete_artifact_upload(self, auth, request.into_inner()).await?;
            Ok(tonic::Response::new(response))
        }

        async fn read_artifact(
            &self,
            request: tonic::Request<proto::ReadArtifactRequest>,
        ) -> Result<tonic::Response<Self::ReadArtifactStream>, tonic::Status> {
            let auth = metadata_auth(&request);
            let stream = Api::read_artifact(self, auth, request.into_inner()).await?;
            Ok(tonic::Response::new(stream))
        }
    }
}

fn header_auth(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
}

/// One HTTP call: the [`Api`] method with the `authorization` header, on
/// the blocking pool, answered as JSON.
async fn call<Req: Send + 'static, Res: Send + 'static>(
    api: Api,
    headers: &HeaderMap,
    request: Req,
    method: fn(&Api, Option<&str>, Req) -> Result<Res, ApiError>,
) -> Result<Json<Res>, ApiError> {
    let auth = header_auth(headers).map(str::to_string);
    blocking(move || method(&api, auth.as_deref(), request))
        .await
        .map(Json)
}

/// The HTTP/JSON routes. Bodies and query strings are the request messages
/// in JSON; path segments fill the model and version.
fn http_router(api: Api) -> axum::Router {
    axum::Router::new()
        // Health and metrics answer without a credential: they report only
        // whether the server can work, and totals.
        .route(
            "/healthz",
            get(|State(api): State<Api>| async move {
                let failing = api.failing().await;
                let status = if failing.is_empty() {
                    StatusCode::OK
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                (
                    status,
                    Json(serde_json::json!({
                        "status": if failing.is_empty() { "ok" } else { "unavailable" },
                        "failing": failing,
                    })),
                )
            }),
        )
        .route(
            "/metrics",
            get(|State(api): State<Api>| async move {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; version=0.0.4",
                    )],
                    api.metrics().await,
                )
            }),
        )
        .route(
            "/v1/diagnostics",
            get(|State(api): State<Api>, headers: HeaderMap| async move {
                let auth = header_auth(&headers).map(str::to_string);
                api.diagnostics(auth).await.map(Json)
            }),
        )
        .route(
            "/v1/identity",
            get(|State(api): State<Api>, headers: HeaderMap| async move {
                call(
                    api,
                    &headers,
                    proto::GetIdentityRequest {},
                    Api::get_identity,
                )
                .await
            }),
        )
        .route(
            "/v1/models",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Json(request): Json<proto::CreateModelRequest>| async move {
                    call(api, &headers, request, Api::create_model).await
                },
            )
            .get(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Query(request): Query<proto::ListModelsRequest>| async move {
                    call(api, &headers, request, Api::list_models).await
                },
            ),
        )
        .route(
            "/v1/models/{model}",
            get(
                |State(api): State<Api>, headers: HeaderMap, Path(name): Path<String>| async move {
                    let request = proto::GetModelRequest { name };
                    call(api, &headers, request, Api::get_model).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path(model): Path<String>,
                 Json(request): Json<proto::RegisterVersionRequest>| async move {
                    let request = proto::RegisterVersionRequest { model, ..request };
                    let auth = header_auth(&headers).map(str::to_string);
                    api.register_version(auth, request).await.map(Json)
                },
            )
            .get(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path(model): Path<String>,
                 Query(request): Query<proto::ListVersionsRequest>| async move {
                    let request = proto::ListVersionsRequest { model, ..request };
                    call(api, &headers, request, Api::list_versions).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions/{version}",
            get(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path((model, version)): Path<(String, u64)>| async move {
                    let request = proto::GetVersionRequest { model, version };
                    call(api, &headers, request, Api::get_version).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions/{version}/stage",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path((model, version)): Path<(String, u64)>,
                 Json(request): Json<proto::SetVersionStageRequest>| async move {
                    let request = proto::SetVersionStageRequest {
                        model,
                        version,
                        ..request
                    };
                    let auth = header_auth(&headers).map(str::to_string);
                    api.set_version_stage(auth, request).await.map(Json)
                },
            ),
        )
        .route(
            "/v1/models/{model}/promotion-gates",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path(model): Path<String>,
                 Json(gates): Json<proto::PromotionGates>| async move {
                    let request = proto::PutPromotionGatesRequest {
                        model,
                        gates: Some(gates),
                    };
                    call(api, &headers, request, Api::put_promotion_gates).await
                },
            )
            .get(
                |State(api): State<Api>, headers: HeaderMap, Path(model): Path<String>| async move {
                    let request = proto::GetPromotionGatesRequest { model };
                    call(api, &headers, request, Api::get_promotion_gates).await
                },
            ),
        )
        .route(
            "/v1/lineage/trained-on",
            get(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Query(request): Query<proto::ListVersionsTrainedOnRequest>| async move {
                    call(api, &headers, request, Api::list_versions_trained_on).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions/{version}/drift-baseline",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path((model, version)): Path<(String, u64)>,
                 Json(request): Json<proto::PutDriftBaselineRequest>| async move {
                    let request = proto::PutDriftBaselineRequest {
                        model,
                        version,
                        ..request
                    };
                    call(api, &headers, request, Api::put_drift_baseline).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions/{version}/prediction-summaries",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path((model, version)): Path<(String, u64)>,
                 Json(summary): Json<proto::PredictionSummary>| async move {
                    let request = proto::SubmitPredictionSummaryRequest {
                        model,
                        version,
                        summary: Some(summary),
                    };
                    call(api, &headers, request, Api::submit_prediction_summary).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions/{version}/quality-thresholds",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path((model, version)): Path<(String, u64)>,
                 Json(thresholds): Json<proto::QualityThresholds>| async move {
                    let request = proto::PutQualityThresholdsRequest {
                        model,
                        version,
                        thresholds: Some(thresholds),
                    };
                    call(api, &headers, request, Api::put_quality_thresholds).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions/{version}/labeled-aggregates",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path((model, version)): Path<(String, u64)>,
                 Json(aggregate): Json<proto::LabeledAggregate>| async move {
                    let request = proto::SubmitLabeledAggregateRequest {
                        model,
                        version,
                        aggregate: Some(aggregate),
                    };
                    call(api, &headers, request, Api::submit_labeled_aggregate).await
                },
            ),
        )
        .route(
            "/v1/drift-alerts",
            get(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Query(request): Query<proto::ListDriftAlertsRequest>| async move {
                    call(api, &headers, request, Api::list_drift_alerts).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions/{version}/online-descriptor",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path((model, version)): Path<(String, u64)>,
                 Json(request): Json<proto::GetOnlineDescriptorRequest>| async move {
                    let request = proto::GetOnlineDescriptorRequest {
                        model,
                        version,
                        ..request
                    };
                    call(api, &headers, request, Api::get_online_descriptor).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions/{version}/batch-descriptor",
            post(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path((model, version)): Path<(String, u64)>,
                 Json(request): Json<proto::GetBatchDescriptorRequest>| async move {
                    let request = proto::GetBatchDescriptorRequest {
                        model,
                        version,
                        ..request
                    };
                    call(api, &headers, request, Api::get_batch_descriptor).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/versions/{version}/manifest",
            get(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path((model, version)): Path<(String, u64)>| async move {
                    let request = proto::GetVersionManifestRequest { model, version };
                    call(api, &headers, request, Api::get_version_manifest).await
                },
            ),
        )
        .route(
            "/v1/models/{model}/signature-compatibility",
            get(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Path(model): Path<String>,
                 Query(request): Query<proto::CheckSignatureCompatibilityRequest>| async move {
                    let request = proto::CheckSignatureCompatibilityRequest { model, ..request };
                    call(api, &headers, request, Api::check_signature_compatibility).await
                },
            ),
        )
        .route(
            "/v1/audit-events",
            get(
                |State(api): State<Api>,
                 headers: HeaderMap,
                 Query(request): Query<proto::ListAuditEventsRequest>| async move {
                    call(api, &headers, request, Api::list_audit_events).await
                },
            ),
        )
        .with_state(api)
}

/// A TLS server configuration from PEM: the server's certificate chain and
/// key, and with `client_ca` a required client certificate signed by it
/// (mutual TLS). ALPN offers HTTP/2 and HTTP/1.1, so gRPC and plain HTTP
/// JSON clients both negotiate.
pub fn tls_config(
    cert_pem: &str,
    key_pem: &str,
    client_ca_pem: Option<&str>,
) -> Result<Arc<rustls::ServerConfig>, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let chain = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("server certificate: {error}"))?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|error| format!("server key: {error}"))?;
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?;
    let builder = match client_ca_pem {
        Some(ca) => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_slice_iter(ca.as_bytes()) {
                roots
                    .add(cert.map_err(|error| format!("client CA: {error}"))?)
                    .map_err(|error| format!("client CA: {error}"))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                provider,
            )
            .build()
            .map_err(|error| format!("client CA: {error}"))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let mut config = builder
        .with_single_cert(chain, key)
        .map_err(|error| format!("server identity: {error}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// TLS handshakes in flight at once; accepting waits while this many run.
const MAX_HANDSHAKES: usize = 256;

/// Serve gRPC and HTTP/JSON on one listener until `shutdown` resolves.
/// With `tls`, both run over TLS (mutual TLS when it requires client
/// certificates). Each handshake runs in its own task with a time limit, so
/// a slow client cannot hold up accepting others, and at most
/// [`MAX_HANDSHAKES`] run at once, so unauthenticated connections cannot
/// exhaust descriptors.
pub async fn serve(
    api: Api,
    listener: tokio::net::TcpListener,
    tls: Option<Arc<rustls::ServerConfig>>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), tonic::transport::Error> {
    let routes = tonic::service::Routes::from(http_router(api.clone()))
        .add_service(HinagataServer::new(api));
    let server = tonic::transport::Server::builder()
        .accept_http1(true)
        .add_routes(routes);
    let Some(config) = tls else {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        return server
            .serve_with_incoming_shutdown(incoming, shutdown)
            .await;
    };
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_HANDSHAKES));
    let accepting = tokio::spawn(async move {
        loop {
            let Ok(permit) = handshakes.clone().acquire_owned().await else {
                return;
            };
            let tcp = match listener.accept().await {
                Ok((tcp, _)) => tcp,
                Err(_) => {
                    // A persistent failure (descriptors exhausted) would
                    // otherwise spin; back off and try again.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            };
            let (acceptor, sender) = (acceptor.clone(), sender.clone());
            tokio::spawn(async move {
                let _permit = permit;
                let handshake = acceptor.accept(tcp);
                if let Ok(Ok(stream)) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), handshake).await
                {
                    let _ = sender.send(Ok::<_, std::io::Error>(stream)).await;
                }
            });
        }
    });
    let incoming = tokio_stream::wrappers::ReceiverStream::new(receiver);
    let served = server
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await;
    accepting.abort();
    served
}
