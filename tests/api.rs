//! The API skeleton and its auth hook (#8): a tenant's credential resolves
//! to its namespace on gRPC and HTTP, nothing in a request can choose
//! another, and bad credentials are refused on both surfaces.

use hinagata::api::proto::GetIdentityRequest;
use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::{Api, serve};
use hinagata::auth::{Authenticator, Jwt};
use hinagata::config;
use jsonwebtoken::{EncodingKey, Header, encode};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static SEQ: AtomicU64 = AtomicU64::new(0);
const SECRET: &[u8] = b"test-only-hmac-secret-0123456789";
const FAR_FUTURE: u64 = 4_102_444_800;

fn jwks() -> String {
    use base64::Engine as _;
    let k = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(SECRET);
    format!(r#"{{"keys":[{{"kty":"oct","kid":"k1","alg":"HS256","k":"{k}"}}]}}"#)
}

fn signed(claims: serde_json::Value) -> String {
    let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some("k1".into());
    encode(&header, &claims, &EncodingKey::from_secret(SECRET)).unwrap()
}

fn token(tenant: &str) -> String {
    signed(serde_json::json!({
        "sub": format!("svc-{tenant}"),
        "aud": "hinagata",
        "tenant": tenant,
        "exp": FAR_FUTURE,
    }))
}

/// A token whose header says `none` and that carries no signature.
fn unsigned(tenant: &str) -> String {
    use base64::Engine as _;
    let encode = |value: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
    };
    format!(
        "{}.{}.",
        encode(serde_json::json!({"alg": "none", "typ": "JWT", "kid": "k1"})),
        encode(serde_json::json!({"aud": "hinagata", "tenant": tenant, "exp": FAR_FUTURE})),
    )
}

/// A path for this test alone, safe beside other test processes.
fn temp_file(name: &str, contents: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "hinagata-{}-{}-{name}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, contents).unwrap();
    path.to_string_lossy().into_owned()
}

async fn server_with(
    auth: Arc<dyn Authenticator>,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let registry = Arc::new(hinagata::registry::Registry::open_in_memory().unwrap());
    tokio::spawn(serve(Api::new(auth, registry), listener, tls, async {
        let _ = stopped.await;
    }));
    (address, stop)
}

async fn jwt_server() -> (String, tokio::sync::oneshot::Sender<()>) {
    server_with(
        Arc::new(Jwt::new(&jwks(), "hinagata", None, "tenant").unwrap()),
        None,
    )
    .await
}

async fn grpc_identity(
    address: &str,
    token: Option<&str>,
    extra: &[(&'static str, &str)],
) -> Result<(String, String), tonic::Status> {
    let mut client = HinagataClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    let mut request = tonic::Request::new(GetIdentityRequest {});
    if let Some(token) = token {
        request
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    for (key, value) in extra {
        request.metadata_mut().insert(*key, value.parse().unwrap());
    }
    let response = client.get_identity(request).await?.into_inner();
    Ok((response.namespace, response.subject))
}

/// One HTTP/1.1 request; the status code and body.
async fn http(address: &str, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let headers: String = headers
        .iter()
        .map(|(key, value)| format!("{key}: {value}\r\n"))
        .collect();
    let request =
        format!("GET {path} HTTP/1.1\r\nhost: hinagata\r\n{headers}connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

fn namespace_of(body: &str) -> String {
    let json: serde_json::Value = serde_json::from_str(body).unwrap();
    json["namespace"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn a_tenant_token_resolves_to_its_namespace_and_nothing_else_selects_one() {
    let (address, _stop) = jwt_server().await;
    let a = token("tenant-a");
    assert_eq!(
        grpc_identity(&address, Some(&a), &[("x-namespace", "tenant-b")])
            .await
            .unwrap(),
        ("tenant-a".into(), "svc-tenant-a".into())
    );
    assert_eq!(
        grpc_identity(&address, Some(&token("tenant-b")), &[])
            .await
            .unwrap()
            .0,
        "tenant-b"
    );
    let bearer = format!("Bearer {a}");
    for (path, headers) in [
        ("/v1/identity", vec![("authorization", bearer.as_str())]),
        (
            "/v1/identity?namespace=tenant-b",
            vec![
                ("authorization", bearer.as_str()),
                ("x-namespace", "tenant-b"),
            ],
        ),
    ] {
        let (code, body) = http(&address, path, &headers).await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(namespace_of(&body), "tenant-a");
    }
}

#[tokio::test]
async fn expired_wrong_audience_unsigned_and_missing_tokens_are_refused_on_both_surfaces() {
    let (address, _stop) = jwt_server().await;
    let expired = signed(serde_json::json!({
        "aud": "hinagata", "tenant": "tenant-a", "exp": 1_000_000_000u64,
    }));
    let wrong_audience = signed(serde_json::json!({
        "aud": "someone-else", "tenant": "tenant-a", "exp": FAR_FUTURE,
    }));
    let other_key = {
        let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("k1".into());
        let claims =
            serde_json::json!({"aud": "hinagata", "tenant": "tenant-a", "exp": FAR_FUTURE});
        encode(
            &header,
            &claims,
            &EncodingKey::from_secret(b"another-secret-0123456789abcdef"),
        )
        .unwrap()
    };
    let refused: HashMap<&str, Option<String>> = HashMap::from([
        ("expired", Some(expired)),
        ("wrong audience", Some(wrong_audience)),
        ("unsigned", Some(unsigned("tenant-a"))),
        ("other key", Some(other_key)),
        ("missing", None),
    ]);
    for (case, token) in &refused {
        let status = grpc_identity(&address, token.as_deref(), &[])
            .await
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated, "{case}");
        let bearer = token.as_ref().map(|token| format!("Bearer {token}"));
        let headers: Vec<(&str, &str)> = bearer
            .as_deref()
            .map(|bearer| vec![("authorization", bearer)])
            .unwrap_or_default();
        let (code, _) = http(&address, "/v1/identity", &headers).await;
        assert_eq!(code, 401, "{case}");
    }
}

#[test]
fn a_configured_issuer_is_required() {
    let jwt = Jwt::new(
        &jwks(),
        "hinagata",
        Some("https://issuer.test".into()),
        "tenant",
    )
    .unwrap();
    let without = token("tenant-a");
    assert!(jwt.authenticate(Some(&without)).is_err());
    let with = signed(serde_json::json!({
        "aud": "hinagata", "iss": "https://issuer.test", "tenant": "tenant-a", "exp": FAR_FUTURE,
    }));
    assert_eq!(
        jwt.authenticate(Some(&with)).unwrap().namespace(),
        "tenant-a"
    );
    let other = signed(serde_json::json!({
        "aud": "hinagata", "iss": "https://other.test", "tenant": "tenant-a", "exp": FAR_FUTURE,
    }));
    assert!(jwt.authenticate(Some(&other)).is_err());
}

#[tokio::test]
async fn it_starts_offline_from_a_local_jwks_file_or_a_static_token_file() {
    let jwks_file = temp_file("jwks.json", &jwks());
    let jwt = config::authenticator(|name| match name {
        "HINAGATA_JWKS_FILE" => Some(jwks_file.clone()),
        "HINAGATA_JWT_AUDIENCE" => Some("hinagata".into()),
        _ => None,
    })
    .unwrap();
    let (address, _stop) = server_with(jwt, None).await;
    assert_eq!(
        grpc_identity(&address, Some(&token("tenant-a")), &[])
            .await
            .unwrap()
            .0,
        "tenant-a"
    );
    let token_file = temp_file("token", "dev-token\n");
    let static_token = config::authenticator(|name| match name {
        "HINAGATA_STATIC_TOKEN_FILE" => Some(token_file.clone()),
        "HINAGATA_STATIC_NAMESPACE" => Some("dev".into()),
        _ => None,
    })
    .unwrap();
    let (address, _stop) = server_with(static_token, None).await;
    let (code, body) = http(
        &address,
        "/v1/identity",
        &[("authorization", "Bearer dev-token")],
    )
    .await;
    assert_eq!((code, namespace_of(&body)), (200, "dev".into()));
    let (code, _) = http(
        &address,
        "/v1/identity",
        &[("authorization", "Bearer dev-tokeN")],
    )
    .await;
    assert_eq!(code, 401);
    assert!(config::authenticator(|_| None).is_err());
    std::fs::remove_file(jwks_file).unwrap();
    std::fs::remove_file(token_file).unwrap();
}

#[tokio::test]
async fn mutual_tls_refuses_a_client_without_a_certificate() {
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
    let server_key = rcgen::KeyPair::generate().unwrap();
    let server_cert = rcgen::CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .signed_by(&server_key, &ca)
        .unwrap();
    let client_key = rcgen::KeyPair::generate().unwrap();
    let client_cert = rcgen::CertificateParams::new(vec!["client".into()])
        .unwrap()
        .signed_by(&client_key, &ca)
        .unwrap();
    let config = hinagata::api::tls_config(
        &server_cert.pem(),
        &server_key.serialize_pem(),
        Some(&ca.pem()),
    )
    .unwrap();
    let auth = Arc::new(Jwt::new(&jwks(), "hinagata", None, "tenant").unwrap());
    let (address, _stop) = server_with(auth, Some(config)).await;

    let identity_over_tls = |certificate: Option<(rcgen::Certificate, String)>| {
        let (address, ca_der) = (address.clone(), ca.der().clone());
        async move {
            use rustls::pki_types::pem::PemObject;
            let mut roots = rustls::RootCertStore::empty();
            roots.add(ca_der).unwrap();
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let builder = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots);
            let mut client = match certificate {
                Some((cert, key)) => builder
                    .with_client_auth_cert(
                        vec![cert.der().clone()],
                        rustls::pki_types::PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap(),
                    )
                    .unwrap(),
                None => builder.with_no_client_auth(),
            };
            client.alpn_protocols = vec![b"http/1.1".to_vec()];
            let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
            let tcp = tokio::net::TcpStream::connect(&address).await.unwrap();
            let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let mut stream = connector.connect(name, tcp).await.ok()?;
            let request = format!(
                "GET /v1/identity HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {}\r\nconnection: close\r\n\r\n",
                token("tenant-a")
            );
            stream.write_all(request.as_bytes()).await.ok()?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.ok()?;
            let response = String::from_utf8_lossy(&response).into_owned();
            response.starts_with("HTTP/1.1 200").then_some(response)
        }
    };
    let response = identity_over_tls(Some((client_cert, client_key.serialize_pem())))
        .await
        .expect("a client with a certificate is served");
    assert!(response.contains("tenant-a"), "{response}");
    assert!(identity_over_tls(None).await.is_none());
}
