//! gRPC (primary) and HTTP/JSON (secondary) from the one versioned
//! protobuf package, on one listener. Every call first resolves its
//! namespace through the [`Authenticator`]; requests carry none.

use crate::auth::{AuthError, Authenticator, Identity, bearer};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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
            _ => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status, Json(serde_json::json!({"error": self.message}))).into_response()
    }
}

/// The API over one authentication hook.
#[derive(Clone)]
pub struct Api {
    auth: Arc<dyn Authenticator>,
}

impl Api {
    pub fn new(auth: Arc<dyn Authenticator>) -> Self {
        Self { auth }
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
}

fn metadata_auth<T>(request: &tonic::Request<T>) -> Option<String> {
    request
        .metadata()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[tonic::async_trait]
impl Hinagata for Api {
    async fn get_identity(
        &self,
        request: tonic::Request<proto::GetIdentityRequest>,
    ) -> Result<tonic::Response<proto::GetIdentityResponse>, tonic::Status> {
        let auth = metadata_auth(&request);
        Ok(tonic::Response::new(Api::get_identity(
            self,
            auth.as_deref(),
            request.into_inner(),
        )?))
    }
}

fn header_auth(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
}

/// The HTTP/JSON routes.
pub fn http_router(api: Api) -> axum::Router {
    axum::Router::new()
        .route(
            "/v1/identity",
            get(|State(api): State<Api>, headers: HeaderMap| async move {
                api.get_identity(header_auth(&headers), proto::GetIdentityRequest {})
                    .map(Json)
            }),
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
