//! Typed signatures (#3): read from a stored ONNX model, checked against a
//! binding at registration, and compared between versions.

use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    BeginArtifactUploadRequest, BindingField, CheckSignatureCompatibilityRequest,
    CompleteArtifactUploadRequest, CreateModelRequest, InputFields, RegisterVersionRequest,
    Signature, SignatureBinding, TensorSpec, UploadArtifactChunkRequest,
};
use hinagata::api::{Api, serve};
use hinagata::auth::StaticToken;
use hinagata::registry::Registry;
use hinagata::signature::onnx;
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "signature-test-token";

type Client = HinagataClient<tonic::transport::Channel>;

async fn server() -> (String, Client) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let auth = Arc::new(StaticToken::new(TOKEN, "tenant-a"));
    let registry = Arc::new(Registry::open_in_memory().unwrap());
    let artifacts = Arc::new(hinagata::artifacts::ArtifactStore::new(
        Arc::new(object_store::memory::InMemory::new()),
        1 << 20,
    ));
    tokio::spawn(serve(
        Api::new(auth, registry, artifacts),
        listener,
        None,
        async { std::future::pending::<()>().await },
    ));
    let client = HinagataClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    (address, client)
}

fn authed<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    request
}

/// An ONNX model: a `[batch, width]` float input and an int64 label.
fn onnx_model(width: i64) -> Vec<u8> {
    use onnx::tensor_shape_proto::{Dimension, dimension};
    let tensor = |name: &str, elem_type: i32, dims: &[i64]| onnx::ValueInfoProto {
        name: name.into(),
        r#type: Some(onnx::TypeProto {
            value: Some(onnx::type_proto::Value::TensorType(
                onnx::type_proto::Tensor {
                    elem_type,
                    shape: Some(onnx::TensorShapeProto {
                        dim: dims
                            .iter()
                            .map(|dim| Dimension {
                                value: Some(if *dim > 0 {
                                    dimension::Value::DimValue(*dim)
                                } else {
                                    dimension::Value::DimParam("batch".into())
                                }),
                            })
                            .collect(),
                    }),
                },
            )),
        }),
    };
    onnx::ModelProto {
        graph: Some(onnx::GraphProto {
            initializer: Vec::new(),
            input: vec![tensor("float_input", 1, &[-1, width])],
            output: vec![tensor("label", 7, &[-1])],
        }),
    }
    .encode_to_vec()
}

async fn upload(client: &mut Client, bytes: Vec<u8>) -> String {
    use sha2::Digest as _;
    let digest = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
    let begun = client
        .begin_artifact_upload(authed(BeginArtifactUploadRequest {
            digest: digest.clone(),
            size_bytes: bytes.len() as u64,
        }))
        .await
        .unwrap()
        .into_inner();
    client
        .upload_artifact_chunk(authed(UploadArtifactChunkRequest {
            upload_id: begun.upload_id.clone(),
            offset: 0,
            data: bytes,
        }))
        .await
        .unwrap();
    client
        .complete_artifact_upload(authed(CompleteArtifactUploadRequest {
            upload_id: begun.upload_id,
        }))
        .await
        .unwrap();
    digest
}

fn binding(fields: &[(&str, &str, bool)]) -> SignatureBinding {
    SignatureBinding {
        target_kind: "dataset".into(),
        target: "customers".into(),
        fields: fields
            .iter()
            .map(|(name, dtype, nullable)| BindingField {
                name: name.to_string(),
                dtype: dtype.to_string(),
                nullable: *nullable,
            })
            .collect(),
        inputs: HashMap::from([(
            "float_input".to_string(),
            InputFields {
                fields: fields.iter().map(|(name, _, _)| name.to_string()).collect(),
            },
        )]),
    }
}

fn register(digest: &str, binding: Option<SignatureBinding>) -> RegisterVersionRequest {
    RegisterVersionRequest {
        model: "churn".into(),
        artifact_digest: digest.into(),
        framework: "xgboost".into(),
        format: "onnx".into(),
        binding,
        ..Default::default()
    }
}

#[tokio::test]
async fn signatures_come_from_onnx_bind_to_a_schema_and_flag_breaking_changes() {
    let (address, mut client) = server().await;
    client
        .create_model(authed(CreateModelRequest {
            name: "churn".into(),
            description: String::new(),
        }))
        .await
        .unwrap();
    let three = upload(&mut client, onnx_model(3)).await;

    // A mismatched binding is refused.
    let mismatched = client
        .register_version(authed(register(
            &three,
            Some(binding(&[
                ("age", "int64", false),
                ("tenure", "float32", true),
            ])),
        )))
        .await
        .unwrap_err();
    assert_eq!(mismatched.code(), tonic::Code::InvalidArgument);
    for expected in ["takes 3 fields", "age` is int64", "tenure` may be null"] {
        assert!(
            mismatched.message().contains(expected),
            "{expected}: {mismatched:?}"
        );
    }
    // A declared signature that contradicts the model is refused.
    let contradicting = client
        .register_version(authed(RegisterVersionRequest {
            signature: Some(Signature {
                inputs: vec![TensorSpec {
                    name: "float_input".into(),
                    dtype: "float64".into(),
                    shape: vec![-1, 3],
                    nullable: false,
                }],
                outputs: vec![],
            }),
            ..register(&three, None)
        }))
        .await
        .unwrap_err();
    assert!(
        contradicting.message().contains("ONNX graph"),
        "{contradicting:?}"
    );

    // Omitted, the signature is read from the graph; the binding fits it.
    let first = client
        .register_version(authed(register(
            &three,
            Some(binding(&[
                ("age", "int16", false),
                ("tenure", "float32", false),
                ("spend", "float32", false),
            ])),
        )))
        .await
        .unwrap()
        .into_inner();
    let signature = first.signature.unwrap();
    assert_eq!(signature.inputs[0].shape, vec![-1, 3]);
    assert_eq!(signature.outputs[0].dtype, "int64");
    assert_eq!(first.binding.unwrap().target, "customers");

    // A model that takes another feature width breaks callers of the first.
    let four = upload(&mut client, onnx_model(4)).await;
    client
        .register_version(authed(register(&four, None)))
        .await
        .unwrap();
    let check = client
        .check_signature_compatibility(authed(CheckSignatureCompatibilityRequest {
            model: "churn".into(),
            from_version: 1,
            to_version: 2,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(!check.compatible);
    assert!(check.breaking[0].contains("float_input"), "{check:?}");

    let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
    let request = format!(
        "GET /v1/models/churn/signature-compatibility?from_version=1&to_version=1 HTTP/1.1\r\nhost: hinagata\r\nauthorization: Bearer {TOKEN}\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let body: serde_json::Value =
        serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["compatible"], true);
}
