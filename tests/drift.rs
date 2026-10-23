//! Drift (#14): a synthetic drifted summary alerts at the configured
//! threshold, an undrifted one does not, and a summary in other bins is
//! refused, over gRPC and HTTP.

use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    BeginArtifactUploadRequest, CategoryCounts, CompleteArtifactUploadRequest, CreateModelRequest,
    Distribution, DriftBaseline, DriftThresholds, NumericBins, PredictionSummary,
    PutDriftBaselineRequest, RegisterVersionRequest, SubmitPredictionSummaryRequest,
    UploadArtifactChunkRequest, distribution::Bins,
};
use hinagata::api::{Api, serve};
use hinagata::auth::StaticToken;
use hinagata::registry::Registry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "drift-test-token";

async fn server() -> String {
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
    address
}

fn authed<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    request
}

fn numeric(edges: &[f64], counts: &[u64]) -> Distribution {
    Distribution {
        bins: Some(Bins::Numeric(NumericBins {
            edges: edges.to_vec(),
            counts: counts.to_vec(),
        })),
    }
}

fn plans(basic: u64, pro: u64) -> Distribution {
    Distribution {
        bins: Some(Bins::Categorical(CategoryCounts {
            counts: [("basic".to_string(), basic), ("pro".to_string(), pro)].into(),
        })),
    }
}

fn summary(id: &str, age: &[u64], plan: (u64, u64), score: &[u64]) -> PredictionSummary {
    PredictionSummary {
        format: "hinagata.prediction-summary/v1".into(),
        summary_id: id.into(),
        window_start_ms: 1_000,
        window_end_ms: 2_000,
        prediction_count: age.iter().sum(),
        latency_p50_ms: 4.0,
        latency_p95_ms: 11.0,
        features: HashMap::from([
            ("age".to_string(), numeric(&[30.0, 50.0], age)),
            ("plan".to_string(), plans(plan.0, plan.1)),
        ]),
        outputs: HashMap::from([("score".to_string(), numeric(&[0.5], score))]),
    }
}

#[tokio::test]
async fn drifted_summaries_alert_at_the_threshold_and_undrifted_ones_do_not() {
    let address = server().await;
    let mut client = HinagataClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    client
        .create_model(authed(CreateModelRequest {
            name: "churn".into(),
            description: String::new(),
        }))
        .await
        .unwrap();
    let model = b"churn model".to_vec();
    let digest = {
        use sha2::Digest as _;
        format!("sha256:{:x}", sha2::Sha256::digest(&model))
    };
    let upload = client
        .begin_artifact_upload(authed(BeginArtifactUploadRequest {
            digest: digest.clone(),
            size_bytes: model.len() as u64,
        }))
        .await
        .unwrap()
        .into_inner();
    client
        .upload_artifact_chunk(authed(UploadArtifactChunkRequest {
            upload_id: upload.upload_id.clone(),
            offset: 0,
            data: model,
        }))
        .await
        .unwrap();
    client
        .complete_artifact_upload(authed(CompleteArtifactUploadRequest {
            upload_id: upload.upload_id,
        }))
        .await
        .unwrap();
    client
        .register_version(authed(RegisterVersionRequest {
            model: "churn".into(),
            artifact_digest: digest,
            framework: "lightgbm".into(),
            format: "onnx".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    client
        .put_drift_baseline(authed(PutDriftBaselineRequest {
            model: "churn".into(),
            version: 1,
            baseline: Some(DriftBaseline {
                features: HashMap::from([
                    ("age".to_string(), numeric(&[30.0, 50.0], &[200, 500, 300])),
                    ("plan".to_string(), plans(600, 400)),
                ]),
                outputs: HashMap::from([("score".to_string(), numeric(&[0.5], &[700, 300]))]),
            }),
            thresholds: Some(DriftThresholds {
                psi: Some(0.2),
                ks: Some(0.1),
            }),
        }))
        .await
        .unwrap();
    let submit = |summary: PredictionSummary| SubmitPredictionSummaryRequest {
        model: "churn".into(),
        version: 1,
        summary: Some(summary),
    };

    let calm = client
        .submit_prediction_summary(authed(submit(summary(
            "w1",
            &[21, 49, 30],
            (59, 41),
            &[69, 31],
        ))))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(calm.drift.len(), 3);
    assert!(calm.alerts.is_empty(), "{calm:?}");

    let drifted = client
        .submit_prediction_summary(authed(submit(summary(
            "w2",
            &[5, 30, 65],
            (58, 42),
            &[40, 60],
        ))))
        .await
        .unwrap()
        .into_inner();
    let raised: Vec<_> = drifted
        .alerts
        .iter()
        .map(|alert| {
            (
                alert.role.as_str(),
                alert.name.as_str(),
                alert.metric.as_str(),
            )
        })
        .collect();
    assert_eq!(
        raised,
        [
            ("feature", "age", "psi"),
            ("feature", "age", "ks"),
            ("output", "score", "psi"),
            ("output", "score", "ks"),
        ]
    );
    assert!(
        drifted
            .alerts
            .iter()
            .all(|alert| alert.value >= alert.threshold)
    );
    // The same summary again raises nothing new.
    let again = client
        .submit_prediction_summary(authed(submit(summary(
            "w2",
            &[5, 30, 65],
            (58, 42),
            &[40, 60],
        ))))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again, drifted);

    // Bins other than the baseline's are refused.
    let mut other = summary("w3", &[5, 30, 65], (58, 42), &[40, 60]);
    other
        .features
        .insert("age".into(), numeric(&[25.0, 50.0], &[5, 30, 65]));
    let refused = client
        .submit_prediction_summary(authed(submit(other)))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert!(refused.message().contains("baseline's bins"), "{refused:?}");

    // The alerts are events with their evidence, over HTTP too.
    let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
    let request = format!(
        "GET /v1/drift-alerts?model=churn HTTP/1.1\r\nhost: hinagata\r\nauthorization: Bearer {TOKEN}\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let body: serde_json::Value =
        serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let alerts = body["alerts"].as_array().unwrap();
    assert_eq!(alerts.len(), 4);
    assert_eq!(alerts[0]["summary_id"], "w2");
    assert_eq!(alerts[0]["threshold"], 0.2);
}
