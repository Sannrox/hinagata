//! Quality (#15): metrics from labeled aggregates match a reference
//! computed on the raw rows they summarize, within the documented
//! tolerance, and a metric past a threshold alerts.

use hinagata::api::proto::hinagata_client::HinagataClient;
use hinagata::api::proto::{
    BeginArtifactUploadRequest, ClassifierAggregate, CompleteArtifactUploadRequest, ConfusionRow,
    CreateModelRequest, LabeledAggregate, PutQualityThresholdsRequest, QualityReport,
    QualityThresholds, RegisterVersionRequest, RegressorAggregate, ScoreHistogram,
    SubmitLabeledAggregateRequest, UploadArtifactChunkRequest, labeled_aggregate::Kind,
};
use hinagata::api::{Api, serve};
use hinagata::auth::StaticToken;
use hinagata::registry::Registry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "quality-test-token";

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

/// Create `churn` and register version 1.
async fn register(address: &str) -> HinagataClient<tonic::transport::Channel> {
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
            signature: Some(hinagata::api::proto::Signature {
                inputs: vec![hinagata::api::proto::TensorSpec {
                    name: "features".into(),
                    dtype: "float32".into(),
                    shape: vec![-1, 4],
                    nullable: false,
                }],
                outputs: vec![],
            }),
            ..Default::default()
        }))
        .await
        .unwrap();
    client
}

/// Deterministic fixture rows: a churn score and whether the customer
/// churned, roughly calibrated with some noise.
fn rows() -> Vec<(f64, bool)> {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..5_000)
        .map(|_| {
            let score = next();
            let churned = next() < (score * 0.8 + 0.1);
            (score, churned)
        })
        .collect()
}

fn bin(score: f64) -> usize {
    ((score * 100.0) as usize).min(99)
}

/// What a caller sends: confusion counts at 0.5 and the churn histogram.
fn classifier_aggregate(id: &str, rows: &[(f64, bool)]) -> LabeledAggregate {
    let mut confusion = vec![vec![0u64; 2]; 2];
    let mut histogram = ScoreHistogram {
        positive_counts: vec![0; 100],
        negative_counts: vec![0; 100],
        score_sums: vec![0.0; 100],
    };
    for &(score, churned) in rows {
        // Label order: churn, stay.
        confusion[usize::from(!churned)][usize::from(score < 0.5)] += 1;
        let counts = if churned {
            &mut histogram.positive_counts
        } else {
            &mut histogram.negative_counts
        };
        counts[bin(score)] += 1;
        histogram.score_sums[bin(score)] += score;
    }
    LabeledAggregate {
        format: "hinagata.labeled-aggregate/v1".into(),
        aggregate_id: id.into(),
        window_start_ms: 1_000,
        window_end_ms: 2_000,
        kind: Some(Kind::Classifier(ClassifierAggregate {
            labels: vec!["churn".into(), "stay".into()],
            confusion: confusion
                .into_iter()
                .map(|counts| ConfusionRow { counts })
                .collect(),
            score_histograms: HashMap::from([("churn".to_string(), histogram)]),
        })),
    }
}

/// Reference metrics on the raw rows: accuracy at 0.5, the AUC of the raw
/// scores (ties half), and calibration error in ten bins of width 0.1.
fn reference(rows: &[(f64, bool)]) -> (f64, f64, f64) {
    let n = rows.len() as f64;
    let accuracy = rows
        .iter()
        .filter(|(score, churned)| (*score >= 0.5) == *churned)
        .count() as f64
        / n;
    let positives: Vec<f64> = rows.iter().filter(|r| r.1).map(|r| r.0).collect();
    let negatives: Vec<f64> = rows.iter().filter(|r| !r.1).map(|r| r.0).collect();
    let mut ordered = 0.0;
    for p in &positives {
        for q in &negatives {
            ordered += if p > q {
                1.0
            } else if p == q {
                0.5
            } else {
                0.0
            };
        }
    }
    let auc = ordered / (positives.len() as f64 * negatives.len() as f64);
    let mut groups = [(0.0, 0.0); 10];
    for &(score, churned) in rows {
        let group = &mut groups[bin(score) / 10];
        group.0 += f64::from(u8::from(churned));
        group.1 += score;
    }
    let calibration = groups.iter().map(|(p, s)| (p - s).abs()).sum::<f64>() / n;
    (accuracy, auc, calibration)
}

fn metric(report: &QualityReport, metric: &str) -> (f64, f64) {
    let found = report
        .metrics
        .iter()
        .find(|candidate| candidate.metric == metric)
        .unwrap();
    (found.value, found.tolerance)
}

#[tokio::test]
async fn metrics_from_aggregates_match_the_raw_rows_and_alert_past_thresholds() {
    let address = server().await;
    let mut client = register(&address).await;
    let rows = rows();
    let submit = |aggregate: LabeledAggregate| SubmitLabeledAggregateRequest {
        model: "churn".into(),
        version: 1,
        aggregate: Some(aggregate),
    };

    let report = client
        .submit_labeled_aggregate(authed(submit(classifier_aggregate("w1", &rows))))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(report.label_count, 5_000);
    assert!(report.alerts.is_empty());
    let (accuracy, auc, calibration) = reference(&rows);
    assert!((metric(&report, "accuracy").0 - accuracy).abs() < 1e-12);
    assert!((metric(&report, "calibration_error").0 - calibration).abs() < 1e-9);
    let (binned_auc, tolerance) = metric(&report, "auc");
    assert!(tolerance < 0.01, "{tolerance}");
    assert!(
        (binned_auc - auc).abs() <= tolerance + 1e-12,
        "{binned_auc} {auc}"
    );

    // Thresholds the model misses alert, once per aggregate.
    client
        .put_quality_thresholds(authed(PutQualityThresholdsRequest {
            model: "churn".into(),
            version: 1,
            thresholds: Some(QualityThresholds {
                min_auc: Some(0.99),
                max_mae: Some(1.0),
                ..Default::default()
            }),
        }))
        .await
        .unwrap();
    let alerted = client
        .submit_labeled_aggregate(authed(submit(classifier_aggregate("w2", &rows))))
        .await
        .unwrap()
        .into_inner();
    let raised: Vec<_> = alerted
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
    assert_eq!(raised, [("quality", "churn", "auc")]);
    let again = client
        .submit_labeled_aggregate(authed(submit(classifier_aggregate("w2", &rows))))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(again, alerted);
    let changed = client
        .submit_labeled_aggregate(authed(submit(classifier_aggregate("w2", &rows[1..]))))
        .await
        .unwrap_err();
    assert_eq!(changed.code(), tonic::Code::AlreadyExists);

    // A regressor's error sums, over HTTP.
    let errors: Vec<f64> = rows.iter().map(|(score, _)| score - 0.5).collect();
    let n = errors.len() as f64;
    let mae = errors.iter().map(|error| error.abs()).sum::<f64>() / n;
    let rmse = (errors.iter().map(|error| error * error).sum::<f64>() / n).sqrt();
    let aggregate = LabeledAggregate {
        format: "hinagata.labeled-aggregate/v1".into(),
        aggregate_id: "w3".into(),
        window_start_ms: 2_000,
        window_end_ms: 3_000,
        kind: Some(Kind::Regressor(RegressorAggregate {
            count: errors.len() as u64,
            absolute_error_sum: errors.iter().map(|error| error.abs()).sum(),
            squared_error_sum: errors.iter().map(|error| error * error).sum(),
        })),
    };
    let body = serde_json::to_string(&aggregate).unwrap();
    let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
    let request = format!(
        "POST /v1/models/churn/versions/1/labeled-aggregates HTTP/1.1\r\nhost: hinagata\r\nauthorization: Bearer {TOKEN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let report: serde_json::Value =
        serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    let value = |name: &str| {
        report["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|metric| metric["metric"] == name)
            .unwrap()["value"]
            .as_f64()
            .unwrap()
    };
    assert!((value("mae") - mae).abs() < 1e-12);
    assert!((value("rmse") - rmse).abs() < 1e-12);
    assert!(report["alerts"].as_array().unwrap().is_empty());
}
