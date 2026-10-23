//! Input and output drift against a training baseline (#14).
//!
//! A version registers a baseline: for each numeric feature the bin edges
//! and the training counts in those bins, and for each categorical feature
//! the training count per category. Serving and batch scoring send
//! prediction summaries in exactly those bins (no raw rows). Each summary is
//! compared with the baseline per feature and for the outputs:
//!
//! - PSI: `sum((a - e) * ln(a / e))` over bins, with each bin's share
//!   floored at [`MIN_SHARE`] so an empty bin stays finite.
//! - KS: the largest gap between the two cumulative distributions, taken at
//!   the bin edges. Within a bin both distributions are unknown, so this is
//!   a lower bound on the KS statistic of the raw values; it is exact when
//!   every edge a raw value could fall between is a bin edge. Categorical
//!   features have no order and get PSI only.
//!
//! A version's thresholds turn a value at or above them into an alert,
//! stored with its evidence and listed in order.

use crate::registry::{Actor, Registry, RegistryError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Summary format, published for deployers and batch runners.
pub const SUMMARY_FORMAT: &str = "hinagata.prediction-summary/v1";
/// Smallest share a bin counts with in PSI.
pub const MIN_SHARE: f64 = 1e-4;
const MAX_FEATURES: usize = 512;
const MAX_BINS: usize = 1000;

type Result<T> = std::result::Result<T, RegistryError>;

/// One feature's distribution. Numeric: `edges` ascending, `n` edges give
/// `n + 1` bins: below the first edge, between each pair (lower edge
/// inclusive), and at or above the last. Categorical: counts by category.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Distribution {
    Numeric { edges: Vec<f64>, counts: Vec<u64> },
    Categorical { counts: BTreeMap<String, u64> },
}

/// The training distributions of a version's inputs and outputs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub features: BTreeMap<String, Distribution>,
    pub outputs: BTreeMap<String, Distribution>,
}

/// Values at or above which a feature or output alerts. `None` never does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    pub psi: Option<f64>,
    pub ks: Option<f64>,
}

/// What serving or batch scoring sends for one window. Bins must be the
/// baseline's: the same edges and the same categories.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Unique per version; sending the same summary again is a no-op.
    pub summary_id: String,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub prediction_count: u64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub features: BTreeMap<String, Distribution>,
    pub outputs: BTreeMap<String, Distribution>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeatureDrift {
    /// `feature` or `output`.
    pub role: String,
    pub name: String,
    pub psi: f64,
    /// Absent for categorical distributions.
    pub ks: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftReport {
    pub summary_id: String,
    pub drift: Vec<FeatureDrift>,
    pub alerts: Vec<DriftAlert>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DriftAlert {
    pub seq: u64,
    pub at_ms: i64,
    pub model: String,
    pub version: u64,
    pub summary_id: String,
    pub role: String,
    pub name: String,
    /// `psi` or `ks`.
    pub metric: String,
    pub value: f64,
    pub threshold: f64,
}

fn invalid(message: impl Into<String>) -> RegistryError {
    RegistryError::Invalid(message.into())
}

impl Distribution {
    fn validate(&self, what: &str) -> Result<()> {
        match self {
            Self::Numeric { edges, counts } => {
                if edges.is_empty() || edges.len() > MAX_BINS {
                    return Err(invalid(format!("{what} needs 1 to {MAX_BINS} bin edges")));
                }
                if edges.iter().any(|edge| !edge.is_finite())
                    || edges.windows(2).any(|pair| pair[0] >= pair[1])
                {
                    return Err(invalid(format!(
                        "{what} bin edges must be finite and strictly ascending"
                    )));
                }
                if counts.len() != edges.len() + 1 {
                    return Err(invalid(format!(
                        "{what} has {} edges, so it needs {} counts",
                        edges.len(),
                        edges.len() + 1
                    )));
                }
            }
            Self::Categorical { counts } => {
                if counts.is_empty() || counts.len() > MAX_BINS {
                    return Err(invalid(format!("{what} needs 1 to {MAX_BINS} categories")));
                }
            }
        }
        self.total()
            .ok_or_else(|| invalid(format!("{what} counts add up past 2^64")))?;
        Ok(())
    }

    /// The count over all bins, if it fits.
    fn total(&self) -> Option<u64> {
        self.counts()
            .into_iter()
            .try_fold(0u64, |total, count| total.checked_add(count))
    }

    fn counts(&self) -> Vec<u64> {
        match self {
            Self::Numeric { counts, .. } => counts.clone(),
            Self::Categorical { counts } => counts.values().copied().collect(),
        }
    }

    /// Whether `other` uses the same bins.
    fn same_bins(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Numeric { edges: a, .. }, Self::Numeric { edges: b, .. }) => a == b,
            (Self::Categorical { counts: a }, Self::Categorical { counts: b }) => {
                a.keys().eq(b.keys())
            }
            _ => false,
        }
    }
}

fn validate_set(what: &str, set: &BTreeMap<String, Distribution>) -> Result<()> {
    if set.len() > MAX_FEATURES {
        return Err(invalid(format!("at most {MAX_FEATURES} {what}s")));
    }
    for (name, distribution) in set {
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            return Err(invalid(format!(
                "{what} names must be 1 to 128 visible characters"
            )));
        }
        distribution.validate(&format!("{what} `{name}`"))?;
    }
    Ok(())
}

impl Baseline {
    pub fn validate(&self) -> Result<()> {
        if self.features.is_empty() && self.outputs.is_empty() {
            return Err(invalid("a baseline needs at least one feature or output"));
        }
        validate_set("feature", &self.features)?;
        validate_set("output", &self.outputs)?;
        for (name, distribution) in self.features.iter().chain(&self.outputs) {
            if distribution.total() == Some(0) {
                return Err(invalid(format!("baseline `{name}` has no training counts")));
            }
        }
        Ok(())
    }
}

impl Thresholds {
    pub fn validate(&self) -> Result<()> {
        if self.psi.is_some_and(|psi| !psi.is_finite() || psi <= 0.0)
            || self
                .ks
                .is_some_and(|ks| !ks.is_finite() || ks <= 0.0 || ks > 1.0)
        {
            return Err(invalid("thresholds must be positive; ks is at most 1"));
        }
        Ok(())
    }
}

/// The count over all bins, in floating point so it never overflows.
fn float_total(counts: &[u64]) -> f64 {
    counts
        .iter()
        .map(|count| *count as f64)
        .sum::<f64>()
        .max(1.0)
}

/// Shares of each bin, floored at [`MIN_SHARE`].
fn shares(counts: &[u64]) -> Vec<f64> {
    let total = float_total(counts);
    counts
        .iter()
        .map(|count| (*count as f64 / total).max(MIN_SHARE))
        .collect()
}

pub fn psi(expected: &[u64], actual: &[u64]) -> f64 {
    shares(expected)
        .into_iter()
        .zip(shares(actual))
        .map(|(e, a)| (a - e) * (a / e).ln())
        .sum()
}

/// Largest gap of the cumulative distributions at the bin edges.
pub fn binned_ks(expected: &[u64], actual: &[u64]) -> f64 {
    let (te, ta) = (float_total(expected), float_total(actual));
    let (mut ce, mut ca, mut gap) = (0.0, 0.0, 0.0_f64);
    for (e, a) in expected.iter().zip(actual) {
        ce += *e as f64 / te;
        ca += *a as f64 / ta;
        gap = gap.max((ce - ca).abs());
    }
    gap
}

/// Drift of every feature and output in `summary` against `baseline`. A
/// summary that does not use exactly the baseline's features, outputs and
/// bins is refused. One with no observations in the window is skipped:
/// there is nothing to compare, and an idle window is not drift.
pub fn compare(baseline: &Baseline, summary: &Summary) -> Result<Vec<FeatureDrift>> {
    let mut drift = Vec::new();
    for (role, expected, actual) in [
        ("feature", &baseline.features, &summary.features),
        ("output", &baseline.outputs, &summary.outputs),
    ] {
        if !expected.keys().eq(actual.keys()) {
            return Err(invalid(format!(
                "the summary's {role}s must be the baseline's: {}",
                expected.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        for (name, base) in expected {
            let seen = &actual[name];
            if !base.same_bins(seen) {
                return Err(invalid(format!(
                    "{role} `{name}` does not use the baseline's bins"
                )));
            }
            seen.validate(&format!("{role} `{name}`"))?;
            if seen.total() == Some(0) {
                continue;
            }
            let (e, a) = (base.counts(), seen.counts());
            drift.push(FeatureDrift {
                role: role.into(),
                name: name.clone(),
                psi: psi(&e, &a),
                ks: matches!(base, Distribution::Numeric { .. }).then(|| binned_ks(&e, &a)),
            });
        }
    }
    Ok(drift)
}

/// The alerts `drift` raises under `thresholds`.
fn alerts(
    drift: &[FeatureDrift],
    thresholds: Thresholds,
) -> Vec<(String, String, String, f64, f64)> {
    let mut raised = Vec::new();
    for feature in drift {
        if let Some(limit) = thresholds.psi.filter(|limit| feature.psi >= *limit) {
            raised.push((
                feature.role.clone(),
                feature.name.clone(),
                "psi".into(),
                feature.psi,
                limit,
            ));
        }
        if let (Some(ks), Some(limit)) = (feature.ks, thresholds.ks)
            && ks >= limit
        {
            raised.push((
                feature.role.clone(),
                feature.name.clone(),
                "ks".into(),
                ks,
                limit,
            ));
        }
    }
    raised
}

/// Drift storage (schema migration 4) on the registry database.
pub(crate) const MIGRATION: &str = r#"
CREATE TABLE drift_baselines (
    namespace TEXT NOT NULL,
    model TEXT NOT NULL,
    version INTEGER NOT NULL,
    baseline TEXT NOT NULL,
    thresholds TEXT NOT NULL,
    PRIMARY KEY (namespace, model, version),
    FOREIGN KEY (namespace, model, version) REFERENCES versions (namespace, model, version)
);
CREATE TABLE prediction_summaries (
    namespace TEXT NOT NULL,
    model TEXT NOT NULL,
    version INTEGER NOT NULL,
    summary_id TEXT NOT NULL,
    summary TEXT NOT NULL,
    report TEXT NOT NULL,
    received_at_ms INTEGER NOT NULL,
    PRIMARY KEY (namespace, model, version, summary_id)
);
CREATE TABLE drift_alerts (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace TEXT NOT NULL,
    model TEXT NOT NULL,
    version INTEGER NOT NULL,
    summary_id TEXT NOT NULL,
    role TEXT NOT NULL,
    name TEXT NOT NULL,
    metric TEXT NOT NULL,
    value REAL NOT NULL,
    threshold REAL NOT NULL,
    at_ms INTEGER NOT NULL
);
CREATE INDEX drift_alerts_by_namespace ON drift_alerts (namespace, seq);
"#;

fn to_json(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|error| RegistryError::Storage(error.to_string()))
}

fn from_json<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    serde_json::from_str(raw).map_err(|error| RegistryError::Storage(error.to_string()))
}

impl Registry {
    /// Set a version's training baseline and thresholds. The baseline is
    /// fixed once set; setting the same one again may change the thresholds
    /// (audited as `drift.thresholds_changed`).
    pub fn put_drift_baseline(
        &self,
        actor: Actor<'_>,
        model: &str,
        version: u64,
        baseline: &Baseline,
        thresholds: Thresholds,
    ) -> Result<()> {
        baseline.validate()?;
        thresholds.validate()?;
        let (body, limits) = (to_json(baseline)?, to_json(&thresholds)?);
        self.write(|tx| {
            crate::registry::load_version(tx, actor.namespace, model, version)?;
            let stored: Option<(String, String)> = tx
                .query_row(
                    "SELECT baseline, thresholds FROM drift_baselines
                     WHERE namespace = ?1 AND model = ?2 AND version = ?3",
                    params![actor.namespace, model, version],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match stored {
                None => {
                    tx.execute(
                        "INSERT INTO drift_baselines (namespace, model, version, baseline, thresholds)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![actor.namespace, model, version, body, limits],
                    )?;
                    crate::registry::audit(
                        tx,
                        actor,
                        "drift.baseline_set",
                        model,
                        version,
                        None::<&Thresholds>,
                        Some(&thresholds),
                    )
                }
                Some((stored_body, _)) if stored_body != body => Err(RegistryError::Conflict(
                    format!("version {version} of `{model}` already has another baseline"),
                )),
                Some((_, stored_limits)) if stored_limits == limits => Ok(()),
                Some((_, stored_limits)) => {
                    tx.execute(
                        "UPDATE drift_baselines SET thresholds = ?4
                         WHERE namespace = ?1 AND model = ?2 AND version = ?3",
                        params![actor.namespace, model, version, limits],
                    )?;
                    let before: Thresholds = from_json(&stored_limits)?;
                    crate::registry::audit(
                        tx,
                        actor,
                        "drift.thresholds_changed",
                        model,
                        version,
                        Some(&before),
                        Some(&thresholds),
                    )
                }
            }
        })
    }

    /// Compare a summary with the version's baseline, store it, and raise
    /// alerts for every value at or above a threshold. Sending the same
    /// summary again returns the first report and raises nothing new; a
    /// different summary under the same id is a conflict.
    pub fn submit_prediction_summary(
        &self,
        actor: Actor<'_>,
        model: &str,
        version: u64,
        summary: &Summary,
    ) -> Result<DriftReport> {
        if summary.summary_id.is_empty() || summary.summary_id.len() > 128 {
            return Err(invalid("summary_id must be 1 to 128 characters"));
        }
        if summary.window_end_ms < summary.window_start_ms
            || !summary.latency_p50_ms.is_finite()
            || !summary.latency_p95_ms.is_finite()
        {
            return Err(invalid(
                "a summary needs an ordered window and finite latencies",
            ));
        }
        // Every distribution counts the window's predictions once.
        for (name, distribution) in summary.features.iter().chain(&summary.outputs) {
            let total = distribution
                .total()
                .ok_or_else(|| invalid(format!("`{name}` counts add up past 2^64")))?;
            if total != summary.prediction_count {
                return Err(invalid(format!(
                    "`{name}` counts {total} predictions, the summary {}",
                    summary.prediction_count
                )));
            }
        }
        let body = to_json(summary)?;
        self.write(|tx| {
            crate::registry::load_version(tx, actor.namespace, model, version)?;
            let stored: Option<(String, String)> = tx
                .query_row(
                    "SELECT summary, report FROM prediction_summaries
                     WHERE namespace = ?1 AND model = ?2 AND version = ?3 AND summary_id = ?4",
                    params![actor.namespace, model, version, summary.summary_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((stored_body, report)) = stored {
                return if stored_body == body {
                    from_json(&report)
                } else {
                    Err(RegistryError::Conflict(format!(
                        "summary `{}` was already sent with other contents",
                        summary.summary_id
                    )))
                };
            }
            let (baseline, thresholds): (String, String) = tx
                .query_row(
                    "SELECT baseline, thresholds FROM drift_baselines
                     WHERE namespace = ?1 AND model = ?2 AND version = ?3",
                    params![actor.namespace, model, version],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| {
                    RegistryError::Refused(format!(
                        "version {version} of `{model}` has no drift baseline"
                    ))
                })?;
            let baseline: Baseline = from_json(&baseline)?;
            let thresholds: Thresholds = from_json(&thresholds)?;
            let drift = compare(&baseline, summary)?;
            let mut raised = Vec::new();
            for (role, name, metric, value, threshold) in alerts(&drift, thresholds) {
                tx.execute(
                    "INSERT INTO drift_alerts (namespace, model, version, summary_id, role, name,
                         metric, value, threshold, at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        actor.namespace,
                        model,
                        version,
                        summary.summary_id,
                        role,
                        name,
                        metric,
                        value,
                        threshold,
                        actor.now_ms
                    ],
                )?;
                raised.push(DriftAlert {
                    seq: tx.last_insert_rowid() as u64,
                    at_ms: actor.now_ms,
                    model: model.into(),
                    version,
                    summary_id: summary.summary_id.clone(),
                    role,
                    name,
                    metric,
                    value,
                    threshold,
                });
            }
            let report = DriftReport {
                summary_id: summary.summary_id.clone(),
                drift,
                alerts: raised,
            };
            tx.execute(
                "INSERT INTO prediction_summaries (namespace, model, version, summary_id, summary,
                     report, received_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    actor.namespace,
                    model,
                    version,
                    summary.summary_id,
                    body,
                    to_json(&report)?,
                    actor.now_ms
                ],
            )?;
            Ok(report)
        })
    }

    /// Drift alerts of the namespace in order, after `after_seq`, for one
    /// model when `model` is set.
    pub fn list_drift_alerts(
        &self,
        namespace: &str,
        model: Option<&str>,
        after_seq: u64,
        limit: u32,
    ) -> Result<Vec<DriftAlert>> {
        crate::registry::check_number("after_seq", after_seq)?;
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT seq, at_ms, model, version, summary_id, role, name, metric, value, threshold
                 FROM drift_alerts
                 WHERE namespace = ?1 AND (?2 IS NULL OR model = ?2) AND seq > ?3
                 ORDER BY seq LIMIT ?4",
            )?;
            let alerts = statement
                .query_map(
                    params![namespace, model, after_seq, crate::registry::page(limit)],
                    |row| {
                        Ok(DriftAlert {
                            seq: row.get(0)?,
                            at_ms: row.get(1)?,
                            model: row.get(2)?,
                            version: row.get(3)?,
                            summary_id: row.get(4)?,
                            role: row.get(5)?,
                            name: row.get(6)?,
                            metric: row.get(7)?,
                            value: row.get(8)?,
                            threshold: row.get(9)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<_>>()?;
            Ok(alerts)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(edges: &[f64], counts: &[u64]) -> Distribution {
        Distribution::Numeric {
            edges: edges.to_vec(),
            counts: counts.to_vec(),
        }
    }

    #[test]
    fn identical_distributions_have_no_drift_and_shifted_ones_do() {
        let base = [100, 300, 400, 200];
        assert!(psi(&base, &base).abs() < 1e-12);
        assert!(binned_ks(&base, &base).abs() < 1e-12);
        // Proportional counts are the same distribution.
        assert!(psi(&base, &[10, 30, 40, 20]).abs() < 1e-12);
        let shifted = [10, 90, 300, 600];
        assert!(psi(&base, &shifted) > 0.25, "{}", psi(&base, &shifted));
        assert!((binned_ks(&base, &shifted) - 0.4).abs() < 1e-9);
        // An empty bin stays finite.
        assert!(psi(&base, &[0, 0, 0, 1000]).is_finite());
    }

    #[test]
    fn summaries_must_use_the_baselines_bins() {
        let baseline = Baseline {
            features: BTreeMap::from([("age".into(), numeric(&[18.0, 40.0], &[10, 50, 40]))]),
            outputs: BTreeMap::new(),
        };
        let summary = |features: BTreeMap<String, Distribution>| Summary {
            summary_id: "w1".into(),
            window_start_ms: 0,
            window_end_ms: 1,
            prediction_count: 100,
            latency_p50_ms: 3.0,
            latency_p95_ms: 9.0,
            features,
            outputs: BTreeMap::new(),
        };
        let ok = summary(BTreeMap::from([(
            "age".into(),
            numeric(&[18.0, 40.0], &[9, 51, 40]),
        )]));
        assert_eq!(compare(&baseline, &ok).unwrap().len(), 1);
        // An idle window has nothing to compare.
        let idle = summary(BTreeMap::from([(
            "age".into(),
            numeric(&[18.0, 40.0], &[0, 0, 0]),
        )]));
        assert!(compare(&baseline, &idle).unwrap().is_empty());
        let overflowing = summary(BTreeMap::from([(
            "age".into(),
            numeric(&[18.0, 40.0], &[u64::MAX, 2, 0]),
        )]));
        assert!(matches!(
            compare(&baseline, &overflowing),
            Err(RegistryError::Invalid(_))
        ));
        for wrong in [
            BTreeMap::from([("age".into(), numeric(&[20.0, 40.0], &[9, 51, 40]))]),
            BTreeMap::from([("age".into(), numeric(&[18.0, 40.0], &[9, 51]))]),
            BTreeMap::from([("income".into(), numeric(&[18.0, 40.0], &[9, 51, 40]))]),
            BTreeMap::from([(
                "age".into(),
                Distribution::Categorical {
                    counts: BTreeMap::from([("young".into(), 1)]),
                },
            )]),
            BTreeMap::new(),
        ] {
            assert!(
                matches!(
                    compare(&baseline, &summary(wrong.clone())),
                    Err(RegistryError::Invalid(_))
                ),
                "{wrong:?}"
            );
        }
    }

    #[test]
    fn baselines_and_thresholds_are_validated() {
        let bad = [
            numeric(&[2.0, 1.0], &[1, 1, 1]),
            numeric(&[1.0, f64::NAN], &[1, 1, 1]),
            numeric(&[], &[1]),
            numeric(&[1.0], &[0, 0]),
        ];
        for distribution in bad {
            let baseline = Baseline {
                features: BTreeMap::from([("x".into(), distribution.clone())]),
                outputs: BTreeMap::new(),
            };
            assert!(baseline.validate().is_err(), "{distribution:?}");
        }
        assert!(
            Thresholds {
                psi: Some(0.0),
                ks: None
            }
            .validate()
            .is_err()
        );
        assert!(
            Thresholds {
                psi: None,
                ks: Some(1.5)
            }
            .validate()
            .is_err()
        );
        assert!(
            Thresholds {
                psi: Some(0.2),
                ks: Some(0.1)
            }
            .validate()
            .is_ok()
        );
    }
}
