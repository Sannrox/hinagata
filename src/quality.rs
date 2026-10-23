//! Model quality from labeled aggregates (#15).
//!
//! Hinagata never sees prediction rows or labels. The caller joins labels
//! to its own predictions and sends counts for one window:
//!
//! - Classifiers: the confusion matrix, and per class a histogram of the
//!   predicted probability of that class in [`SCORE_BINS`] equal-width bins
//!   over [0, 1], split into rows of that class (positives) and the rest,
//!   with the sum of the scores in each bin.
//! - Regressors: the row count and the sums of absolute and squared errors.
//!
//! From those Hinagata computes accuracy, per-class AUC and calibration
//! error, or MAE and RMSE. Accuracy, calibration, MAE and RMSE are exact.
//! AUC counts a positive and a negative in the same bin as half ordered, so
//! it is within half the share of positive and negative pairs that share a
//! bin of the AUC of the raw scores; the report gives that bound.
//!
//! A version's thresholds turn a metric past them into an alert in the
//! drift alert stream, with role `quality`.

use crate::drift::DriftAlert;
use crate::registry::{Actor, Registry, RegistryError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Aggregate format, published for callers.
pub const AGGREGATE_FORMAT: &str = "hinagata.labeled-aggregate/v1";
/// Score histogram bins: bin `i` holds scores in `[i / 100, (i + 1) / 100)`,
/// and a score of exactly 1 falls in the last bin.
pub const SCORE_BINS: usize = 100;
/// Calibration bins: each is ten score bins, so of width 0.1.
pub const CALIBRATION_BINS: usize = 10;
const MAX_CLASSES: usize = 256;

type Result<T> = std::result::Result<T, RegistryError>;

fn invalid(message: impl Into<String>) -> RegistryError {
    RegistryError::Invalid(message.into())
}

/// One class's scores against the labels, in [`SCORE_BINS`] bins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreHistogram {
    /// Rows labeled with the class, per bin.
    pub positive_counts: Vec<u64>,
    /// Rows labeled otherwise, per bin.
    pub negative_counts: Vec<u64>,
    /// The sum of the class's predicted probability over each bin's rows.
    pub score_sums: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Aggregate {
    Classifier {
        /// Class labels, the order of the confusion matrix.
        labels: Vec<String>,
        /// `confusion[actual][predicted]`.
        confusion: Vec<Vec<u64>>,
        /// Optional per class; AUC and calibration need one.
        score_histograms: BTreeMap<String, ScoreHistogram>,
    },
    Regressor {
        count: u64,
        absolute_error_sum: f64,
        squared_error_sum: f64,
    },
}

/// What the caller sends for one window of one version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabeledAggregate {
    /// Unique per version; sending the same aggregate again is a no-op.
    pub aggregate_id: String,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub aggregate: Aggregate,
}

/// Bounds a metric alerts past. `min_*` alert below, `max_*` at or above.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct QualityThresholds {
    pub min_accuracy: Option<f64>,
    pub min_auc: Option<f64>,
    pub max_calibration_error: Option<f64>,
    pub max_mae: Option<f64>,
    pub max_rmse: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityMetric {
    /// `model`, or the class for AUC and calibration.
    pub name: String,
    /// `accuracy`, `auc`, `calibration_error`, `mae` or `rmse`.
    pub metric: String,
    pub value: f64,
    /// For `auc`: the most it can differ from the AUC of the raw scores.
    pub tolerance: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityReport {
    pub aggregate_id: String,
    pub label_count: u64,
    pub metrics: Vec<QualityMetric>,
    pub alerts: Vec<DriftAlert>,
}

impl QualityThresholds {
    pub fn validate(&self) -> Result<()> {
        let unit = [self.min_accuracy, self.min_auc, self.max_calibration_error];
        if unit
            .iter()
            .flatten()
            .any(|value| !(0.0..=1.0).contains(value))
        {
            return Err(invalid(
                "accuracy, AUC and calibration thresholds must be within 0 and 1",
            ));
        }
        if [self.max_mae, self.max_rmse]
            .iter()
            .flatten()
            .any(|value| !value.is_finite() || *value < 0.0)
        {
            return Err(invalid(
                "MAE and RMSE thresholds must be finite and not negative",
            ));
        }
        Ok(())
    }
}

fn sum(counts: impl IntoIterator<Item = u64>) -> Result<u64> {
    counts
        .into_iter()
        .try_fold(0u64, u64::checked_add)
        .ok_or_else(|| invalid("counts add up past 2^64"))
}

impl ScoreHistogram {
    /// Check the shape, and that each bin's score sum is one its rows can
    /// have. Returns the positive and negative totals.
    fn validate(&self, class: &str) -> Result<(u64, u64)> {
        if self.positive_counts.len() != SCORE_BINS
            || self.negative_counts.len() != SCORE_BINS
            || self.score_sums.len() != SCORE_BINS
        {
            return Err(invalid(format!(
                "the score histogram of `{class}` needs {SCORE_BINS} bins of each"
            )));
        }
        for bin in 0..SCORE_BINS {
            let rows = self.positive_counts[bin] as f64 + self.negative_counts[bin] as f64;
            let (low, high) = (
                bin as f64 / SCORE_BINS as f64,
                (bin + 1) as f64 / SCORE_BINS as f64,
            );
            let sum = self.score_sums[bin];
            // A small slack for the caller's float rounding.
            let slack = 1e-9 * rows.max(1.0);
            if !sum.is_finite() || sum < low * rows - slack || sum > high * rows + slack {
                return Err(invalid(format!(
                    "the score sum of bin {bin} of `{class}` is outside what its rows can add up to"
                )));
            }
        }
        Ok((
            sum(self.positive_counts.iter().copied())?,
            sum(self.negative_counts.iter().copied())?,
        ))
    }

    /// AUC, counting pairs in the same bin as half ordered, and the most
    /// that differs from the AUC of the raw scores. `None` without both
    /// positives and negatives.
    fn auc(&self, positives: u64, negatives: u64) -> Option<(f64, f64)> {
        if positives == 0 || negatives == 0 {
            return None;
        }
        let (mut ordered, mut tied, mut negatives_below) = (0.0, 0.0, 0.0);
        for (positive, negative) in self.positive_counts.iter().zip(&self.negative_counts) {
            let (positive, negative) = (*positive as f64, *negative as f64);
            ordered += positive * negatives_below;
            tied += positive * negative;
            negatives_below += negative;
        }
        let pairs = positives as f64 * negatives as f64;
        Some(((ordered + tied / 2.0) / pairs, tied / 2.0 / pairs))
    }

    /// Expected calibration error over [`CALIBRATION_BINS`] bins: the
    /// row-weighted gap between the mean score and the share of positives.
    fn calibration_error(&self, rows: u64) -> Option<f64> {
        if rows == 0 {
            return None;
        }
        let width = SCORE_BINS / CALIBRATION_BINS;
        let gap: f64 = (0..CALIBRATION_BINS)
            .map(|group| {
                let bins = group * width..(group + 1) * width;
                let positives: f64 = self.positive_counts[bins.clone()]
                    .iter()
                    .map(|count| *count as f64)
                    .sum();
                let scores: f64 = self.score_sums[bins].iter().sum();
                (positives - scores).abs()
            })
            .sum();
        Some(gap / rows as f64)
    }
}

/// The metrics of a valid aggregate and its label count.
pub fn metrics(aggregate: &Aggregate) -> Result<(u64, Vec<QualityMetric>)> {
    let metric = |name: &str, metric: &str, value: f64, tolerance: f64| QualityMetric {
        name: name.into(),
        metric: metric.into(),
        value,
        tolerance,
    };
    match aggregate {
        Aggregate::Classifier {
            labels,
            confusion,
            score_histograms,
        } => {
            if labels.len() < 2 || labels.len() > MAX_CLASSES {
                return Err(invalid(format!(
                    "a classifier needs 2 to {MAX_CLASSES} labels"
                )));
            }
            let mut seen = std::collections::BTreeSet::new();
            if labels
                .iter()
                .any(|label| label.is_empty() || label.len() > 128 || !seen.insert(label))
            {
                return Err(invalid("labels must be distinct and 1 to 128 characters"));
            }
            if confusion.len() != labels.len()
                || confusion.iter().any(|row| row.len() != labels.len())
            {
                return Err(invalid(
                    "the confusion matrix needs a row and a column per label",
                ));
            }
            let row_totals = confusion
                .iter()
                .map(|row| sum(row.iter().copied()))
                .collect::<Result<Vec<_>>>()?;
            let total = sum(row_totals.iter().copied())?;
            let mut out = Vec::new();
            if total > 0 {
                let correct = sum((0..labels.len()).map(|index| confusion[index][index]))?;
                out.push(metric(
                    "model",
                    "accuracy",
                    correct as f64 / total as f64,
                    0.0,
                ));
            }
            for (class, histogram) in score_histograms {
                let index = labels
                    .iter()
                    .position(|label| label == class)
                    .ok_or_else(|| {
                        invalid(format!(
                            "score histogram for `{class}`, which is not a label"
                        ))
                    })?;
                let (positives, negatives) = histogram.validate(class)?;
                // The histogram counts the same rows as the confusion matrix.
                if positives != row_totals[index] || sum([positives, negatives])? != total {
                    return Err(invalid(format!(
                        "the score histogram of `{class}` counts other rows than the confusion matrix"
                    )));
                }
                if let Some((auc, tolerance)) = histogram.auc(positives, negatives) {
                    out.push(metric(class, "auc", auc, tolerance));
                }
                if let Some(error) = histogram.calibration_error(total) {
                    out.push(metric(class, "calibration_error", error, 0.0));
                }
            }
            Ok((total, out))
        }
        Aggregate::Regressor {
            count,
            absolute_error_sum,
            squared_error_sum,
        } => {
            if !absolute_error_sum.is_finite()
                || !squared_error_sum.is_finite()
                || *absolute_error_sum < 0.0
                || *squared_error_sum < 0.0
            {
                return Err(invalid("error sums must be finite and not negative"));
            }
            if *count == 0 {
                return Ok((0, Vec::new()));
            }
            let n = *count as f64;
            Ok((
                *count,
                vec![
                    metric("model", "mae", absolute_error_sum / n, 0.0),
                    metric("model", "rmse", (squared_error_sum / n).sqrt(), 0.0),
                ],
            ))
        }
    }
}

/// The metrics past `thresholds`.
fn alerts(metrics: &[QualityMetric], thresholds: QualityThresholds) -> Vec<&QualityMetric> {
    metrics
        .iter()
        .filter(|metric| match metric.metric.as_str() {
            "accuracy" => thresholds
                .min_accuracy
                .is_some_and(|min| metric.value < min),
            "auc" => thresholds.min_auc.is_some_and(|min| metric.value < min),
            "calibration_error" => thresholds
                .max_calibration_error
                .is_some_and(|max| metric.value >= max),
            "mae" => thresholds.max_mae.is_some_and(|max| metric.value >= max),
            "rmse" => thresholds.max_rmse.is_some_and(|max| metric.value >= max),
            _ => false,
        })
        .collect()
}

fn threshold_of(metric: &str, thresholds: QualityThresholds) -> Option<f64> {
    match metric {
        "accuracy" => thresholds.min_accuracy,
        "auc" => thresholds.min_auc,
        "calibration_error" => thresholds.max_calibration_error,
        "mae" => thresholds.max_mae,
        "rmse" => thresholds.max_rmse,
        _ => None,
    }
}

/// Quality storage (schema migration 6) on the registry database. Alerts
/// share the drift alert table.
pub(crate) const MIGRATION: &str = r#"
CREATE TABLE quality_thresholds (
    namespace TEXT NOT NULL,
    model TEXT NOT NULL,
    version INTEGER NOT NULL,
    thresholds TEXT NOT NULL,
    PRIMARY KEY (namespace, model, version),
    FOREIGN KEY (namespace, model, version) REFERENCES versions (namespace, model, version)
);
CREATE TABLE labeled_aggregates (
    namespace TEXT NOT NULL,
    model TEXT NOT NULL,
    version INTEGER NOT NULL,
    aggregate_id TEXT NOT NULL,
    aggregate TEXT NOT NULL,
    report TEXT NOT NULL,
    received_at_ms INTEGER NOT NULL,
    PRIMARY KEY (namespace, model, version, aggregate_id)
);
"#;

fn to_json(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|error| RegistryError::Storage(error.to_string()))
}

fn from_json<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|error| RegistryError::Storage(error.to_string()))
}

impl Registry {
    /// Set a version's quality thresholds, audited as
    /// `quality.thresholds_set`. Setting the same ones again is a no-op.
    pub fn put_quality_thresholds(
        &self,
        actor: Actor<'_>,
        model: &str,
        version: u64,
        thresholds: QualityThresholds,
    ) -> Result<()> {
        thresholds.validate()?;
        let body = to_json(&thresholds)?;
        self.write(|tx| {
            crate::registry::load_version(tx, actor.namespace, model, version)?;
            let stored: Option<String> = tx
                .query_row(
                    "SELECT thresholds FROM quality_thresholds
                     WHERE namespace = ?1 AND model = ?2 AND version = ?3",
                    params![actor.namespace, model, version],
                    |row| row.get(0),
                )
                .optional()?;
            if stored.as_deref() == Some(body.as_str()) {
                return Ok(());
            }
            let before = stored
                .map(|raw| from_json::<QualityThresholds>(&raw))
                .transpose()?;
            tx.execute(
                "INSERT INTO quality_thresholds (namespace, model, version, thresholds)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (namespace, model, version) DO UPDATE SET thresholds = ?4",
                params![actor.namespace, model, version, body],
            )?;
            crate::registry::audit(
                tx,
                actor,
                "quality.thresholds_set",
                model,
                version,
                before.as_ref(),
                Some(&thresholds),
            )
        })
    }

    /// Compute an aggregate's metrics, store it with its report, and raise
    /// an alert for every metric past the version's thresholds. Sending the
    /// same aggregate again returns the first report and raises nothing new;
    /// a different aggregate under the same id is a conflict.
    pub fn submit_labeled_aggregate(
        &self,
        actor: Actor<'_>,
        model: &str,
        version: u64,
        aggregate: &LabeledAggregate,
    ) -> Result<QualityReport> {
        if aggregate.aggregate_id.is_empty() || aggregate.aggregate_id.len() > 128 {
            return Err(invalid("aggregate_id must be 1 to 128 characters"));
        }
        if aggregate.window_end_ms < aggregate.window_start_ms {
            return Err(invalid("an aggregate needs an ordered window"));
        }
        let (label_count, metrics) = metrics(&aggregate.aggregate)?;
        let body = to_json(aggregate)?;
        self.write(|tx| {
            crate::registry::load_version(tx, actor.namespace, model, version)?;
            let stored: Option<(String, String)> = tx
                .query_row(
                    "SELECT aggregate, report FROM labeled_aggregates
                     WHERE namespace = ?1 AND model = ?2 AND version = ?3 AND aggregate_id = ?4",
                    params![actor.namespace, model, version, aggregate.aggregate_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((stored_body, report)) = stored {
                return if stored_body == body {
                    from_json(&report)
                } else {
                    Err(RegistryError::Conflict(format!(
                        "aggregate `{}` was already sent with other contents",
                        aggregate.aggregate_id
                    )))
                };
            }
            let thresholds: QualityThresholds = tx
                .query_row(
                    "SELECT thresholds FROM quality_thresholds
                     WHERE namespace = ?1 AND model = ?2 AND version = ?3",
                    params![actor.namespace, model, version],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .map(|raw| from_json(&raw))
                .transpose()?
                .unwrap_or_default();
            let mut raised = Vec::new();
            for metric in alerts(&metrics, thresholds) {
                let threshold = threshold_of(&metric.metric, thresholds).unwrap_or_default();
                tx.execute(
                    "INSERT INTO drift_alerts (namespace, model, version, summary_id, role, name,
                         metric, value, threshold, at_ms)
                     VALUES (?1, ?2, ?3, ?4, 'quality', ?5, ?6, ?7, ?8, ?9)",
                    params![
                        actor.namespace,
                        model,
                        version,
                        aggregate.aggregate_id,
                        metric.name,
                        metric.metric,
                        metric.value,
                        threshold,
                        actor.now_ms
                    ],
                )?;
                raised.push(DriftAlert {
                    seq: tx.last_insert_rowid() as u64,
                    at_ms: actor.now_ms,
                    model: model.into(),
                    version,
                    summary_id: aggregate.aggregate_id.clone(),
                    role: "quality".into(),
                    name: metric.name.clone(),
                    metric: metric.metric.clone(),
                    value: metric.value,
                    threshold,
                });
            }
            let report = QualityReport {
                aggregate_id: aggregate.aggregate_id.clone(),
                label_count,
                metrics,
                alerts: raised,
            };
            tx.execute(
                "INSERT INTO labeled_aggregates (namespace, model, version, aggregate_id,
                     aggregate, report, received_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    actor.namespace,
                    model,
                    version,
                    aggregate.aggregate_id,
                    body,
                    to_json(&report)?,
                    actor.now_ms
                ],
            )?;
            Ok(report)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn histogram(rows: &[(f64, bool)]) -> ScoreHistogram {
        let mut histogram = ScoreHistogram {
            positive_counts: vec![0; SCORE_BINS],
            negative_counts: vec![0; SCORE_BINS],
            score_sums: vec![0.0; SCORE_BINS],
        };
        for (score, positive) in rows {
            let bin = ((score * SCORE_BINS as f64) as usize).min(SCORE_BINS - 1);
            if *positive {
                histogram.positive_counts[bin] += 1;
            } else {
                histogram.negative_counts[bin] += 1;
            }
            histogram.score_sums[bin] += score;
        }
        histogram
    }

    fn classifier(rows: &[(f64, bool)]) -> Aggregate {
        let (mut confusion, mut histograms) = (vec![vec![0; 2]; 2], BTreeMap::new());
        for (score, positive) in rows {
            confusion[usize::from(!positive)][usize::from(*score < 0.5)] += 1;
        }
        histograms.insert("churn".to_string(), histogram(rows));
        Aggregate::Classifier {
            labels: vec!["churn".into(), "stay".into()],
            confusion,
            score_histograms: histograms,
        }
    }

    fn value(metrics: &[QualityMetric], metric: &str) -> f64 {
        metrics
            .iter()
            .find(|candidate| candidate.metric == metric)
            .unwrap()
            .value
    }

    #[test]
    fn separable_scores_give_perfect_quality_and_shuffled_ones_do_not() {
        let rows: Vec<_> = (0..100)
            .map(|index| (index as f64 / 100.0 + 0.005, index >= 50))
            .collect();
        let (count, metrics) = super::metrics(&classifier(&rows)).unwrap();
        assert_eq!(count, 100);
        assert_eq!(value(&metrics, "accuracy"), 1.0);
        assert_eq!(value(&metrics, "auc"), 1.0);

        let inverted: Vec<_> = rows
            .iter()
            .map(|(score, positive)| (*score, !positive))
            .collect();
        let (_, metrics) = super::metrics(&classifier(&inverted)).unwrap();
        assert_eq!(value(&metrics, "accuracy"), 0.0);
        assert_eq!(value(&metrics, "auc"), 0.0);
        assert!(value(&metrics, "calibration_error") > 0.4);
    }

    #[test]
    fn aggregates_that_do_not_add_up_are_refused() {
        let rows = [(0.9, true), (0.2, false)];
        let Aggregate::Classifier {
            labels,
            confusion,
            mut score_histograms,
        } = classifier(&rows)
        else {
            unreachable!()
        };
        let mut wrong = score_histograms.clone();
        wrong.get_mut("churn").unwrap().score_sums[90] = 5.0;
        let refused = super::metrics(&Aggregate::Classifier {
            labels: labels.clone(),
            confusion: confusion.clone(),
            score_histograms: wrong,
        });
        assert!(refused.unwrap_err().to_string().contains("score sum"));
        score_histograms.get_mut("churn").unwrap().negative_counts[0] += 1;
        let refused = super::metrics(&Aggregate::Classifier {
            labels,
            confusion,
            score_histograms,
        });
        assert!(refused.unwrap_err().to_string().contains("other rows"));
        assert!(
            super::metrics(&Aggregate::Regressor {
                count: 3,
                absolute_error_sum: -1.0,
                squared_error_sum: 1.0
            })
            .is_err()
        );
    }

    #[test]
    fn min_thresholds_alert_below_and_max_thresholds_at_or_above() {
        let metric = |metric: &str, value: f64| QualityMetric {
            name: "model".into(),
            metric: metric.into(),
            value,
            tolerance: 0.0,
        };
        let thresholds = QualityThresholds {
            min_accuracy: Some(0.8),
            max_mae: Some(2.0),
            ..Default::default()
        };
        let metrics = [
            metric("accuracy", 0.8),
            metric("mae", 2.0),
            metric("rmse", 9.0),
        ];
        let raised: Vec<_> = alerts(&metrics, thresholds)
            .iter()
            .map(|metric| metric.metric.as_str())
            .collect();
        assert_eq!(raised, ["mae"]);
        assert!(
            QualityThresholds {
                min_auc: Some(1.5),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
