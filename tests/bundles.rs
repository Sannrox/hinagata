//! Bundles (#13): a signed bundle round-trips between two registries with
//! identical digests and metadata, and a tampered bundle, or one signed by
//! a key the target does not trust, is refused and leaves the target
//! unchanged.

use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    BeginArtifactUploadRequest, CodeReference, CompleteArtifactUploadRequest, CreateModelRequest,
    GetVersionRequest, ImportVersionRequest, Lineage, ListAuditEventsRequest, ListModelsRequest,
    ReadArtifactRequest, RegisterVersionRequest, TensorSpec, UploadArtifactChunkRequest,
    VersionManifest,
};
use hinagata::api::{Api, serve};
use hinagata::auth::StaticToken;
use hinagata::registry::Registry;
use hinagata::signing::{Signer, TrustedKeys};
use std::path::Path;
use std::sync::Arc;

const SOURCE_SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const OTHER_SEED: &str = "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb";

type Client = HinagataClient<tonic::transport::Channel>;

fn keys_json(seed: &str) -> String {
    let signer = Signer::from_seed_hex("site-a-2026", seed).unwrap();
    format!(
        r#"{{"keys":[{{"key_id":"site-a-2026","public_key":"{}"}}]}}"#,
        signer.public_key_hex()
    )
}

/// A registry for `namespace` behind `token`, signing with `seed` or
/// trusting `trusted`.
async fn server(
    token: &str,
    namespace: &str,
    seed: Option<&str>,
    trusted: Option<&str>,
) -> (String, Client) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let mut api = Api::new(
        Arc::new(StaticToken::new(token, namespace)),
        Arc::new(Registry::open_in_memory().unwrap()),
        Arc::new(hinagata::artifacts::ArtifactStore::new(
            Arc::new(object_store::memory::InMemory::new()),
            1 << 22,
        )),
    );
    if let Some(seed) = seed {
        api = api.with_signer(Signer::from_seed_hex("site-a-2026", seed).unwrap());
    }
    if let Some(trusted) = trusted {
        api = api.with_trusted_keys(TrustedKeys::from_json(trusted).unwrap());
    }
    tokio::spawn(serve(api, listener, None, async {
        std::future::pending::<()>().await
    }));
    let client = HinagataClient::connect(address.clone()).await.unwrap();
    (address, client)
}

fn authed<T>(token: &str, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

async fn register(client: &mut Client, token: &str, bytes: &[u8], auc: f64) {
    let digest = {
        use sha2::Digest as _;
        format!("sha256:{:x}", sha2::Sha256::digest(bytes))
    };
    let upload = client
        .begin_artifact_upload(authed(
            token,
            BeginArtifactUploadRequest {
                digest: digest.clone(),
                size_bytes: bytes.len() as u64,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    // Two chunks, so import has to send them in order too.
    let half = bytes.len() / 2;
    for (offset, chunk) in [(0, &bytes[..half]), (half, &bytes[half..])] {
        client
            .upload_artifact_chunk(authed(
                token,
                UploadArtifactChunkRequest {
                    upload_id: upload.upload_id.clone(),
                    offset: offset as u64,
                    data: chunk.to_vec(),
                },
            ))
            .await
            .unwrap();
    }
    client
        .complete_artifact_upload(authed(
            token,
            CompleteArtifactUploadRequest {
                upload_id: upload.upload_id,
            },
        ))
        .await
        .unwrap();
    client
        .register_version(authed(
            token,
            RegisterVersionRequest {
                model: "churn".into(),
                artifact_digest: digest,
                framework: "lightgbm".into(),
                format: "onnx".into(),
                signature: Some(hinagata::api::proto::Signature {
                    inputs: vec![TensorSpec {
                        name: "features".into(),
                        dtype: "float32".into(),
                        shape: vec![-1, 4],
                        nullable: false,
                    }],
                    outputs: vec![],
                }),
                metrics: [("auc".to_string(), auc)].into(),
                tags: [("owner".to_string(), "risk".to_string())].into(),
                lineage: Some(Lineage {
                    code: Some(CodeReference {
                        repository: "github.com/example/churn-model".into(),
                        commit: "9fceb02d0ae598e95dc970b74767f19372d61af8".into(),
                        digest: String::new(),
                    }),
                    parameters: [("num_leaves".to_string(), "31".to_string())].into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ))
        .await
        .unwrap();
}

fn cli(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_hinagata"))
        .args(args)
        .output()
        .unwrap()
}

/// Everything about a version except its stage and registration time.
async fn content(client: &mut Client, token: &str, version: u64) -> serde_json::Value {
    let mut stored = serde_json::to_value(
        client
            .get_version(authed(
                token,
                GetVersionRequest {
                    model: "churn".into(),
                    version,
                },
            ))
            .await
            .unwrap()
            .into_inner(),
    )
    .unwrap();
    for field in ["stage", "created_at_ms", "created_by"] {
        stored.as_object_mut().unwrap().remove(field);
    }
    stored
}

async fn artifact(client: &mut Client, token: &str, digest: &str) -> Vec<u8> {
    let mut stream = client
        .read_artifact(authed(
            token,
            ReadArtifactRequest {
                digest: digest.into(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.message().await.unwrap() {
        bytes.extend(chunk.data);
    }
    bytes
}

/// Whether the registry still has no models and no audit events.
async fn untouched(client: &mut Client, token: &str) -> bool {
    let models = client
        .list_models(authed(token, ListModelsRequest::default()))
        .await
        .unwrap()
        .into_inner()
        .models;
    let events = client
        .list_audit_events(authed(token, ListAuditEventsRequest::default()))
        .await
        .unwrap()
        .into_inner()
        .events;
    models.is_empty() && events.is_empty()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

// The CLI runs as a blocking child process while the registries answer it
// from other runtime threads.
#[tokio::test(flavor = "multi_thread")]
async fn a_signed_bundle_round_trips_and_a_tampered_one_changes_nothing() {
    let trusted = keys_json(SOURCE_SEED);
    let (source_url, mut source) = server("a-token", "site-a", Some(SOURCE_SEED), None).await;
    let (target_url, mut target) = server("b-token", "site-b", None, Some(&trusted)).await;
    source
        .create_model(authed(
            "a-token",
            CreateModelRequest {
                name: "churn".into(),
                description: String::new(),
            },
        ))
        .await
        .unwrap();
    let weights = [vec![7u8; 3000], vec![9u8; 5000]];
    register(&mut source, "a-token", &weights[0], 0.88).await;
    register(&mut source, "a-token", &weights[1], 0.91).await;

    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    std::fs::write(path("a-token"), "a-token\n").unwrap();
    std::fs::write(path("b-token"), "b-token\n").unwrap();
    std::fs::write(path("keys.json"), &trusted).unwrap();
    let exported = cli(&[
        "export",
        "--server",
        &source_url,
        "--token-file",
        &path("a-token"),
        "--model",
        "churn",
        "--version",
        "1",
        "--version",
        "2",
        "--out",
        &path("bundle"),
    ]);
    assert!(exported.status.success(), "{exported:?}");

    // A tampered artifact, tampered metadata, or a key the target does not
    // trust: each is refused and the target stays as it was.
    let (tamper_url, mut tamper_target) = server("c-token", "site-c", None, Some(&trusted)).await;
    std::fs::write(path("c-token"), "c-token").unwrap();
    let hex = {
        use sha2::Digest as _;
        format!("{:x}", sha2::Sha256::digest(&weights[1]))
    };
    copy_dir(&dir.path().join("bundle"), &dir.path().join("bad-artifact"));
    std::fs::write(
        dir.path().join("bad-artifact/artifacts").join(&hex),
        vec![8u8; 5000],
    )
    .unwrap();
    copy_dir(&dir.path().join("bundle"), &dir.path().join("bad-metadata"));
    let manifest_path = dir.path().join("bad-metadata/manifests/churn-2.json");
    let mut bundled: VersionManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    bundled.metadata = bundled.metadata.replace("0.91", "0.99");
    std::fs::write(&manifest_path, serde_json::to_vec(&bundled).unwrap()).unwrap();
    for bundle in ["bad-artifact", "bad-metadata"] {
        let refused = cli(&[
            "import",
            "--server",
            &tamper_url,
            "--token-file",
            &path("c-token"),
            "--keys",
            &path("keys.json"),
            "--bundle",
            &path(bundle),
        ]);
        assert!(!refused.status.success(), "{bundle}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("does not match"),
            "{refused:?}"
        );
    }
    // The target itself refuses tampered metadata.
    let refused = tamper_target
        .import_version(authed(
            "c-token",
            ImportVersionRequest {
                manifest: Some(bundled),
                validate_only: false,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert!(untouched(&mut tamper_target, "c-token").await);
    // The importer trusts the key, the target lists another key under that
    // id: refused when the target validates, before any artifact is sent.
    let (untrusting_url, mut untrusting) =
        server("d-token", "site-d", None, Some(&keys_json(OTHER_SEED))).await;
    std::fs::write(path("d-token"), "d-token").unwrap();
    let refused = cli(&[
        "import",
        "--server",
        &untrusting_url,
        "--token-file",
        &path("d-token"),
        "--keys",
        &path("keys.json"),
        "--bundle",
        &path("bundle"),
    ]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("signature does not match"),
        "{refused:?}"
    );
    assert!(untouched(&mut untrusting, "d-token").await);

    // The intact bundle imports with identical digests and metadata.
    let imported = cli(&[
        "import",
        "--server",
        &target_url,
        "--token-file",
        &path("b-token"),
        "--keys",
        &path("keys.json"),
        "--bundle",
        &path("bundle"),
    ]);
    assert!(imported.status.success(), "{imported:?}");
    for (version, bytes) in [(1, &weights[0]), (2, &weights[1])] {
        let stored = content(&mut target, "b-token", version).await;
        assert_eq!(stored, content(&mut source, "a-token", version).await);
        let digest = stored["artifact_digest"].as_str().unwrap();
        assert_eq!(&artifact(&mut target, "b-token", digest).await, bytes);
    }
    let events = target
        .list_audit_events(authed("b-token", ListAuditEventsRequest::default()))
        .await
        .unwrap()
        .into_inner()
        .events;
    let imported: Vec<serde_json::Value> = events
        .iter()
        .filter(|event| event.action == "version.imported")
        .map(|event| serde_json::from_str(&event.after).unwrap())
        .collect();
    assert_eq!(imported.len(), 2);
    assert_eq!(imported[0]["source_namespace"], "site-a");
    assert_eq!(imported[0]["key_id"], "site-a-2026");

    // Importing again changes nothing.
    let again = cli(&[
        "import",
        "--server",
        &target_url,
        "--token-file",
        &path("b-token"),
        "--keys",
        &path("keys.json"),
        "--bundle",
        &path("bundle"),
    ]);
    assert!(again.status.success(), "{again:?}");
    let after = target
        .list_audit_events(authed("b-token", ListAuditEventsRequest::default()))
        .await
        .unwrap()
        .into_inner()
        .events;
    assert_eq!(after.len(), events.len());
}

#[tokio::test]
async fn a_registry_without_trusted_keys_imports_nothing() {
    let (_, mut target) = server("b-token", "site-b", None, None).await;
    let refused = target
        .import_version(authed(
            "b-token",
            ImportVersionRequest {
                manifest: Some(VersionManifest::default()),
                validate_only: true,
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
}
