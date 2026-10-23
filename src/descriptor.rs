//! Deployment descriptors (#6): what a deployer needs to run a version as an
//! online scoring service, and what a batch runner needs to score a dataset
//! or object set with it. Hinagata runs no model; it describes one.
//!
//! A descriptor is canonical JSON (compact, keys sorted) holding the
//! version's signed manifest, so it names exactly one artifact and its
//! metadata. The registry signs the descriptor bytes with its signing key.
//! The formats are published as JSON Schemas under `docs/schemas/`.

use crate::registry::{RegistryError, Version};
use serde::Serialize;

pub const ONLINE_FORMAT: &str = "hinagata.online-descriptor/v1";
pub const BATCH_FORMAT: &str = "hinagata.batch-descriptor/v1";
/// The signed wrapper around either descriptor.
pub const SIGNED_FORMAT: &str = "hinagata.signed-descriptor/v1";
/// The paths a deployed scoring service answers for health.
pub const LIVENESS_PATH: &str = "/healthz";
pub const READINESS_PATH: &str = "/readyz";

type Result<T> = std::result::Result<T, RegistryError>;

fn invalid(message: impl Into<String>) -> RegistryError {
    RegistryError::Invalid(message.into())
}

/// Compute and memory per replica, and replicas. Zero takes the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Resources {
    pub cpu_millis: u32,
    pub memory_mib: u32,
    pub replicas: u32,
}

impl Resources {
    fn checked(self) -> Result<Self> {
        let pick = |value: u32, default: u32, max: u32, what: &str| match value {
            0 => Ok(default),
            value if value <= max => Ok(value),
            _ => Err(invalid(format!("{what} must be at most {max}"))),
        };
        Ok(Self {
            cpu_millis: pick(self.cpu_millis, 1000, 256_000, "cpu_millis")?,
            memory_mib: pick(self.memory_mib, 512, 1_048_576, "memory_mib")?,
            replicas: pick(self.replicas, 1, 1000, "replicas")?,
        })
    }
}

pub struct OnlineOptions {
    /// Empty for the deployer's default ONNX runtime image.
    pub runtime_image: String,
    pub resources: Resources,
}

pub struct BatchOptions {
    pub runtime_image: String,
    /// The dataset version or object set to score, in the bound target.
    pub input: String,
    pub output_dataset: String,
    /// A field of the binding to partition by; empty for none.
    pub partition_column: String,
    /// Zero for one partition.
    pub partitions: u32,
}

fn runtime(version: &Version, image: &str) -> Result<serde_json::Value> {
    if version.format != "onnx" {
        return Err(invalid(format!(
            "no scoring runtime for format `{}`",
            version.format
        )));
    }
    if image.len() > 512 || !image.chars().all(|c| c.is_ascii_graphic()) {
        return Err(invalid(
            "runtime_image must be at most 512 visible characters",
        ));
    }
    Ok(serde_json::json!({ "engine": "onnx", "image": image }))
}

fn monitoring(version: &Version) -> serde_json::Value {
    let base = format!("/v1/models/{}/versions/{}", version.model, version.version);
    serde_json::json!({
        "prediction_summaries": {
            "path": format!("{base}/prediction-summaries"),
            "format": crate::drift::SUMMARY_FORMAT,
        },
        "labeled_aggregates": {
            "path": format!("{base}/labeled-aggregates"),
            "format": crate::quality::AGGREGATE_FORMAT,
        },
    })
}

/// The online descriptor for `version` in `namespace`. `manifest` is the
/// version's `GetVersionManifest` response.
pub fn online(
    namespace: &str,
    version: &Version,
    manifest: &serde_json::Value,
    options: &OnlineOptions,
) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "format": ONLINE_FORMAT,
        "namespace": namespace,
        "model": version.model,
        "version": version.version,
        "artifact_digest": version.artifact_digest,
        "version_manifest": manifest,
        "runtime": runtime(version, &options.runtime_image)?,
        "resources": options.resources.checked()?,
        "signature": version.signature,
        "health": {
            "liveness_path": LIVENESS_PATH,
            "readiness_path": READINESS_PATH,
        },
        "monitoring": monitoring(version),
    }))
}

/// The batch descriptor for `version` in `namespace`. The version must be
/// bound to a dataset or object type, which says how input fields feed its
/// inputs, and every output must have a leading batch axis (`-1`).
pub fn batch(
    namespace: &str,
    version: &Version,
    manifest: &serde_json::Value,
    options: &BatchOptions,
) -> Result<serde_json::Value> {
    let binding = version.binding.as_ref().ok_or_else(|| {
        RegistryError::Refused(format!(
            "version {} of `{}` is bound to no dataset or object type; register it with a binding to score it in batch",
            version.version, version.model
        ))
    })?;
    // The batch contract is per row: each output carries a leading batch
    // axis, and the binding names its target and every field.
    if binding.target.is_empty() || binding.fields.iter().any(|field| field.name.is_empty()) {
        return Err(RegistryError::Refused(
            "the version's binding names no target or has an unnamed field, so no batch descriptor can describe it".into(),
        ));
    }
    if let Some(output) = version
        .signature
        .outputs
        .iter()
        .find(|output| output.shape.first() != Some(&-1))
    {
        return Err(RegistryError::Refused(format!(
            "output `{}` has no leading batch axis, so it gives no value per row",
            output.name
        )));
    }
    let name = |what: &str, value: &str, max: usize| {
        if value.is_empty() || value.len() > max || value.chars().any(char::is_control) {
            Err(invalid(format!("{what} must be 1 to {max} characters")))
        } else {
            Ok(())
        }
    };
    name("input", &options.input, 256)?;
    name("output_dataset", &options.output_dataset, 256)?;
    if !options.partition_column.is_empty()
        && !binding
            .fields
            .iter()
            .any(|field| field.name == options.partition_column)
    {
        return Err(invalid(format!(
            "partition_column `{}` is not a field of the binding",
            options.partition_column
        )));
    }
    let partitions = match options.partitions {
        0 => 1,
        count if count <= 10_000 => count,
        _ => return Err(invalid("partitions must be at most 10000")),
    };
    let source_kind = match binding.target_kind.as_str() {
        "dataset" => "dataset",
        _ => "object_set",
    };
    let fields: Vec<_> = version
        .signature
        .outputs
        .iter()
        .map(|output| {
            serde_json::json!({
                "name": output.name,
                "dtype": output.dtype,
                "nullable": output.nullable,
                // Dimensions per row: the shape without its batch axis.
                "dimensions": output.shape.iter().skip(1).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(serde_json::json!({
        "format": BATCH_FORMAT,
        "namespace": namespace,
        "model": version.model,
        "version": version.version,
        "artifact_digest": version.artifact_digest,
        "version_manifest": manifest,
        "runtime": runtime(version, &options.runtime_image)?,
        "signature": version.signature,
        "input": {
            "kind": source_kind,
            "id": options.input,
            "binding": binding,
        },
        "output": {
            "dataset": options.output_dataset,
            "fields": fields,
        },
        "partitioning": {
            "column": options.partition_column,
            "partitions": partitions,
        },
        "monitoring": monitoring(version),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Lineage, Signature, Stage, TensorSpec};
    use crate::signature::{Binding, Field};

    fn version(output_shape: Vec<i64>, target: &str) -> Version {
        let spec = |name: &str, shape: Vec<i64>| TensorSpec {
            name: name.into(),
            dtype: "float32".into(),
            shape,
            nullable: false,
        };
        Version {
            model: "churn".into(),
            version: 1,
            artifact_digest: format!("sha256:{}", "a".repeat(64)),
            framework: "lightgbm".into(),
            format: "onnx".into(),
            signature: Signature {
                inputs: vec![spec("x", vec![-1])],
                outputs: vec![spec("y", output_shape)],
            },
            metrics: Default::default(),
            tags: Default::default(),
            lineage: Lineage::default(),
            binding: Some(Binding {
                target_kind: "dataset".into(),
                target: target.into(),
                fields: vec![Field {
                    name: "x".into(),
                    dtype: "float32".into(),
                    nullable: false,
                }],
                inputs: Default::default(),
            }),
            stage: Stage::Candidate,
            created_at_ms: 0,
            created_by: "svc".into(),
        }
    }

    fn options() -> BatchOptions {
        BatchOptions {
            runtime_image: String::new(),
            input: "rows@1".into(),
            output_dataset: "scores".into(),
            partition_column: String::new(),
            partitions: 0,
        }
    }

    #[test]
    fn batch_needs_per_row_outputs_and_a_named_target() {
        let manifest = serde_json::Value::Null;
        let described = batch(
            "tenant-a",
            &version(vec![-1, 2], "rows"),
            &manifest,
            &options(),
        )
        .unwrap();
        assert_eq!(
            described["output"]["fields"][0]["dimensions"],
            serde_json::json!([2])
        );
        assert_eq!(described["partitioning"]["partitions"], 1);
        for (shape, target) in [(vec![3], "rows"), (vec![], "rows"), (vec![-1], "")] {
            assert!(matches!(
                batch("tenant-a", &version(shape, target), &manifest, &options()),
                Err(RegistryError::Refused(_))
            ));
        }
    }
}
