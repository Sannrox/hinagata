//! Deployment descriptors (#6): fixture descriptors and the ones a
//! registry signs validate against the published schemas, and a sample
//! online deployer and batch runner, written only from the documented
//! format, consume them.

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    BeginArtifactUploadRequest, BindingField, CompleteArtifactUploadRequest, CreateModelRequest,
    GetBatchDescriptorRequest, GetOnlineDescriptorRequest, InputFields, ReadArtifactRequest,
    RegisterVersionRequest, Resources, SignatureBinding, SignedDescriptor, TensorSpec,
    UploadArtifactChunkRequest,
};
use hinagata::api::{Api, serve};
use hinagata::auth::StaticToken;
use hinagata::registry::Registry;
use hinagata::signing::Signer;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;

const TOKEN: &str = "descriptor-test-token";
const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const MODEL: &[u8] = b"churn model weights";

type Client = HinagataClient<tonic::transport::Channel>;

fn authed<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    request
}

async fn server(signed: bool) -> Client {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let mut api = Api::new(
        Arc::new(StaticToken::new(TOKEN, "tenant-a")),
        Arc::new(Registry::open_in_memory().unwrap()),
        Arc::new(hinagata::artifacts::ArtifactStore::new(
            Arc::new(object_store::memory::InMemory::new()),
            1 << 20,
        )),
    );
    if signed {
        api = api.with_signer(Signer::from_seed_hex("registry-1", SEED).unwrap());
    }
    tokio::spawn(serve(api, listener, None, async {
        std::future::pending::<()>().await
    }));
    HinagataClient::connect(address).await.unwrap()
}

fn spec(name: &str, dtype: &str, shape: &[i64]) -> TensorSpec {
    TensorSpec {
        name: name.into(),
        dtype: dtype.into(),
        shape: shape.to_vec(),
        nullable: false,
    }
}

/// Register `churn` version 1, bound to the `customers` dataset when
/// `bound`; version 2 is unbound.
async fn register(client: &mut Client, bound: bool) {
    let _ = client
        .create_model(authed(CreateModelRequest {
            name: "churn".into(),
            description: String::new(),
        }))
        .await;
    let bytes = if bound {
        MODEL.to_vec()
    } else {
        b"unbound weights".to_vec()
    };
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let upload = client
        .begin_artifact_upload(authed(BeginArtifactUploadRequest {
            digest: digest.clone(),
            size_bytes: bytes.len() as u64,
        }))
        .await
        .unwrap()
        .into_inner();
    client
        .upload_artifact_chunk(authed(UploadArtifactChunkRequest {
            upload_id: upload.upload_id.clone(),
            offset: 0,
            data: bytes,
        }))
        .await
        .unwrap();
    client
        .complete_artifact_upload(authed(CompleteArtifactUploadRequest {
            upload_id: upload.upload_id,
        }))
        .await
        .unwrap();
    let field = |name: &str, dtype: &str| BindingField {
        name: name.into(),
        dtype: dtype.into(),
        nullable: false,
    };
    client
        .register_version(authed(RegisterVersionRequest {
            model: "churn".into(),
            artifact_digest: digest,
            framework: "lightgbm".into(),
            format: "onnx".into(),
            signature: Some(hinagata::api::proto::Signature {
                inputs: vec![spec("features", "float32", &[-1, 3])],
                outputs: vec![
                    spec("score", "float32", &[-1, 1]),
                    spec("label", "int64", &[-1]),
                ],
            }),
            binding: bound.then(|| SignatureBinding {
                target_kind: "dataset".into(),
                target: "customers".into(),
                fields: vec![
                    field("customer_id", "string"),
                    field("region", "string"),
                    field("age", "int16"),
                    field("tenure", "float32"),
                    field("spend", "float32"),
                ],
                inputs: [(
                    "features".to_string(),
                    InputFields {
                        fields: vec!["age".into(), "tenure".into(), "spend".into()],
                    },
                )]
                .into(),
            }),
            ..Default::default()
        }))
        .await
        .unwrap();
}

/// Verify a signed descriptor as docs/descriptors.md says, and return the
/// descriptor.
fn verified(signed: &SignedDescriptor) -> serde_json::Value {
    assert_eq!(signed.format, "hinagata.signed-descriptor/v1");
    let public = Signer::from_seed_hex("registry-1", SEED)
        .unwrap()
        .public_key_hex();
    let public: [u8; 32] = std::array::from_fn(|index| {
        u8::from_str_radix(&public[index * 2..index * 2 + 2], 16).unwrap()
    });
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&signed.signature)
        .unwrap();
    VerifyingKey::from_bytes(&public)
        .unwrap()
        .verify(
            signed.descriptor.as_bytes(),
            &Signature::from_slice(&signature).unwrap(),
        )
        .expect("the descriptor signature verifies");
    serde_json::from_str(&signed.descriptor).unwrap()
}

fn schema_check(schema: &str, instance: &serde_json::Value) -> Result<(), String> {
    let path = format!("{}/docs/schemas/{schema}", env!("CARGO_MANIFEST_DIR"));
    let mut schemas = boon::Schemas::new();
    let index = boon::Compiler::new()
        .compile(&path, &mut schemas)
        .map_err(|error| format!("{error:#}"))?;
    schemas
        .validate(instance, index)
        .map_err(|error| format!("{error:#}"))
}

fn fixture(name: &str) -> serde_json::Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// The fixture `name`, after rewriting it from `descriptor` when
/// `HINAGATA_WRITE_FIXTURES` is set (for a deliberate format change).
fn golden(name: &str, descriptor: &serde_json::Value) -> serde_json::Value {
    if std::env::var_os("HINAGATA_WRITE_FIXTURES").is_some() {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let mut body = serde_json::to_string_pretty(descriptor).unwrap();
        body.push('\n');
        std::fs::write(path, body).unwrap();
    }
    fixture(name)
}

#[test]
fn fixture_descriptors_validate_against_the_published_schemas() {
    schema_check(
        "online-descriptor.v1.json",
        &fixture("online-descriptor.json"),
    )
    .unwrap();
    schema_check(
        "batch-descriptor.v1.json",
        &fixture("batch-descriptor.json"),
    )
    .unwrap();
    // The schemas are strict: an unknown field or a wrong format is refused.
    let mut extra = fixture("online-descriptor.json");
    extra["replicas"] = 3.into();
    assert!(schema_check("online-descriptor.v1.json", &extra).is_err());
    assert!(
        schema_check(
            "batch-descriptor.v1.json",
            &fixture("online-descriptor.json")
        )
        .is_err()
    );
}

#[tokio::test]
async fn a_sample_online_deployment_consumes_its_descriptor() {
    let mut client = server(true).await;
    register(&mut client, true).await;
    let signed = client
        .get_online_descriptor(authed(GetOnlineDescriptorRequest {
            model: "churn".into(),
            version: 1,
            runtime_image: String::new(),
            resources: Some(Resources {
                cpu_millis: 2000,
                memory_mib: 0,
                replicas: 3,
            }),
        }))
        .await
        .unwrap()
        .into_inner();
    let descriptor = verified(&signed);
    schema_check("online-descriptor.v1.json", &descriptor).unwrap();
    // The fixture is what this registry signs for the same version.
    assert_eq!(descriptor, golden("online-descriptor.json", &descriptor));

    // The deployer: the artifact it pulls must be the signed one, then it
    // plans the service from the descriptor.
    let digest = descriptor["artifact_digest"].as_str().unwrap();
    let keys = hinagata::signing::TrustedKeys::from_json(&format!(
        r#"{{"keys":[{{"key_id":"registry-1","public_key":"{}"}}]}}"#,
        Signer::from_seed_hex("registry-1", SEED)
            .unwrap()
            .public_key_hex()
    ))
    .unwrap();
    let envelope: hinagata::signing::Envelope =
        serde_json::from_value(descriptor["version_manifest"].clone()).unwrap();
    assert_eq!(keys.verify(&envelope).unwrap().artifact_digest, digest);
    let mut stream = client
        .read_artifact(authed(ReadArtifactRequest {
            digest: digest.into(),
        }))
        .await
        .unwrap()
        .into_inner();
    let mut artifact = Vec::new();
    while let Some(chunk) = stream.message().await.unwrap() {
        artifact.extend(chunk.data);
    }
    assert_eq!(format!("sha256:{:x}", Sha256::digest(&artifact)), digest);
    let plan = (
        descriptor["runtime"]["engine"].as_str().unwrap(),
        descriptor["resources"]["replicas"].as_u64().unwrap(),
        descriptor["resources"]["memory_mib"].as_u64().unwrap(),
        descriptor["health"]["readiness_path"].as_str().unwrap(),
    );
    assert_eq!(plan, ("onnx", 3, 512, "/readyz"));
    assert_eq!(
        descriptor["monitoring"]["prediction_summaries"]["path"],
        "/v1/models/churn/versions/1/prediction-summaries"
    );
}

#[tokio::test]
async fn a_sample_batch_job_consumes_its_descriptor() {
    let mut client = server(true).await;
    register(&mut client, true).await;
    let request = |partition_column: &str| GetBatchDescriptorRequest {
        model: "churn".into(),
        version: 1,
        runtime_image: String::new(),
        input: "customers@2026-10-01".into(),
        output_dataset: "churn_scores".into(),
        partition_column: partition_column.into(),
        partitions: 8,
    };
    let signed = client
        .get_batch_descriptor(authed(request("region")))
        .await
        .unwrap()
        .into_inner();
    let descriptor = verified(&signed);
    schema_check("batch-descriptor.v1.json", &descriptor).unwrap();
    assert_eq!(descriptor, golden("batch-descriptor.json", &descriptor));

    // The batch runner: which columns feed each input, and the output
    // columns it writes.
    let binding = &descriptor["input"]["binding"];
    let fields: BTreeMap<&str, &str> = binding["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| {
            (
                field["name"].as_str().unwrap(),
                field["dtype"].as_str().unwrap(),
            )
        })
        .collect();
    let mut reads = Vec::new();
    for input in descriptor["signature"]["inputs"].as_array().unwrap() {
        let name = input["name"].as_str().unwrap();
        let columns: Vec<&str> = match binding["inputs"].get(name) {
            Some(columns) => columns
                .as_array()
                .unwrap()
                .iter()
                .map(|column| column.as_str().unwrap())
                .collect(),
            None => vec![name],
        };
        assert!(columns.iter().all(|column| fields.contains_key(column)));
        reads.push((name, columns));
    }
    assert_eq!(reads, [("features", vec!["age", "tenure", "spend"])]);
    let writes: Vec<_> = descriptor["output"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| (field["name"].as_str().unwrap(), field["dimensions"].clone()))
        .collect();
    assert_eq!(
        writes,
        [
            ("score", serde_json::json!([1])),
            ("label", serde_json::json!([]))
        ]
    );
    assert_eq!(descriptor["input"]["kind"], "dataset");
    assert_eq!(descriptor["partitioning"]["partitions"], 8);

    // A partition column outside the binding, or an unbound version, is
    // refused.
    let refused = client
        .get_batch_descriptor(authed(request("plan")))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    register(&mut client, false).await;
    let refused = client
        .get_batch_descriptor(authed(GetBatchDescriptorRequest {
            version: 2,
            ..request("")
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
}

#[tokio::test]
async fn without_a_signing_key_no_descriptor_is_issued() {
    let mut client = server(false).await;
    register(&mut client, true).await;
    let refused = client
        .get_online_descriptor(authed(GetOnlineDescriptorRequest {
            model: "churn".into(),
            version: 1,
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
}
