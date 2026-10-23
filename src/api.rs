//! gRPC (primary) and HTTP/JSON (secondary) from the one versioned
//! protobuf package, on one listener. Every call first resolves its
//! namespace through the [`Authenticator`]; requests carry none.

use crate::auth::{AuthError, Authenticator, Identity, bearer};
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
            tonic::Code::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(serde_json::json!({"error": self.message}))).into_response()
    }
}

/// The API over one authentication hook and one registry.
#[derive(Clone)]
pub struct Api {
    auth: Arc<dyn Authenticator>,
    registry: Arc<Registry>,
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
        stage: version.stage.as_str().into(),
        created_at_ms: version.created_at_ms,
        created_by: version.created_by,
    }
}

impl Api {
    pub fn new(auth: Arc<dyn Authenticator>, registry: Arc<Registry>) -> Self {
        Self { auth, registry }
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

    pub fn register_version(
        &self,
        authorization: Option<&str>,
        request: proto::RegisterVersionRequest,
    ) -> Result<proto::Version, ApiError> {
        let identity = self.identity(authorization)?;
        let signature = request.signature.unwrap_or_default();
        let new = registry::NewVersion {
            artifact_digest: request.artifact_digest,
            framework: request.framework,
            format: request.format,
            signature: registry::Signature {
                inputs: signature.inputs.into_iter().map(spec_in).collect(),
                outputs: signature.outputs.into_iter().map(spec_in).collect(),
            },
            metrics: request.metrics.into_iter().collect(),
            tags: request.tags.into_iter().collect(),
        };
        let version = self
            .registry
            .register_version(actor(&identity), &request.model, new)?;
        Ok(version_out(version))
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

    pub fn set_version_stage(
        &self,
        authorization: Option<&str>,
        request: proto::SetVersionStageRequest,
    ) -> Result<proto::Version, ApiError> {
        let identity = self.identity(authorization)?;
        let stage = Stage::parse(&request.stage)?;
        let expected = match request.expected_stage.as_str() {
            "" => None,
            value => Some(Stage::parse(value)?),
        };
        let version = self.registry.set_stage(
            actor(&identity),
            &request.model,
            request.version,
            stage,
            expected,
        )?;
        Ok(version_out(version))
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

/// The acting identity: its namespace scopes the write, its subject is
/// recorded as the actor.
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
    ($($method:ident($request:ident) -> $response:ident;)*) => {
        #[tonic::async_trait]
        impl Hinagata for Api {
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
    register_version(RegisterVersionRequest) -> Version;
    get_version(GetVersionRequest) -> Version;
    list_versions(ListVersionsRequest) -> ListVersionsResponse;
    set_version_stage(SetVersionStageRequest) -> Version;
    list_audit_events(ListAuditEventsRequest) -> ListAuditEventsResponse;
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
pub fn http_router(api: Api) -> axum::Router {
    axum::Router::new()
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
                    call(api, &headers, request, Api::register_version).await
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
                    call(api, &headers, request, Api::set_version_stage).await
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
