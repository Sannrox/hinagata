# Prediction summary format

Format `hinagata.prediction-summary/v1`. Deployers and batch runners send one summary per window and model version: `SubmitPredictionSummary` over gRPC (`hinagata.v1.PredictionSummary`), or `POST /v1/models/{model}/versions/{version}/prediction-summaries` with the summary as the JSON body. A summary carries counts only, never rows.

```json
{
  "format": "hinagata.prediction-summary/v1",
  "summary_id": "2026-10-23T10:00Z/replica-3",
  "window_start_ms": 1761213600000,
  "window_end_ms": 1761217200000,
  "prediction_count": 100,
  "latency_p50_ms": 4.0,
  "latency_p95_ms": 11.0,
  "features": {
    "age": {"bins": {"numeric": {"edges": [30, 50], "counts": [21, 49, 30]}}},
    "plan": {"bins": {"categorical": {"counts": {"basic": 59, "pro": 41}}}}
  },
  "outputs": {
    "score": {"bins": {"numeric": {"edges": [0.5], "counts": [69, 31]}}}
  }
}
```

- `summary_id` is unique per version. Sending the same summary again returns the first report; different contents under the same id are refused.
- `features` and `outputs` name exactly the baseline's features and outputs, in the baseline's bins:
  - Numeric: the same `edges`. `n` edges give `n + 1` counts: below the first edge, between each pair (lower edge inclusive), and at or above the last.
  - Categorical: the same categories. Map unseen values to a category the baseline has, such as `other`.
  - A summary in other bins is refused.
- The training baseline is set once per version with `PutDriftBaseline`, in the same shape with training counts, together with the alert thresholds.

## Drift

For each feature and output the report gives:

- PSI: `sum((a - e) * ln(a / e))` over bins, where `a` and `e` are the summary's and baseline's shares, each floored at 0.0001 so empty bins stay finite.
- KS, for numeric distributions only: the largest gap between the two cumulative distributions at the bin edges. Values inside a bin are unknown, so this is a lower bound on the KS statistic of the raw values. It is exact when the raw values only fall on bin edges.

Each distribution's counts add up to `prediction_count`. A window with no predictions is stored and compares nothing, so an idle window raises nothing.

Every value at or above the version's `psi` or `ks` threshold is a drift alert, stored with its summary, feature, metric, value and threshold. `ListDriftAlerts` (`GET /v1/drift-alerts`) lists alerts in order after a sequence number.
