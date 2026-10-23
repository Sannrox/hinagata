# Labeled aggregate format

Format `hinagata.labeled-aggregate/v1`. Hinagata does not store predictions, so it cannot join labels to them. The caller joins labels to its own predictions and sends counts for one window and model version: `SubmitLabeledAggregate` over gRPC (`hinagata.v1.LabeledAggregate`), or `POST /v1/models/{model}/versions/{version}/labeled-aggregates` with the aggregate as the JSON body. Hinagata never receives or stores prediction rows or labels.

## Classifier

```json
{
  "format": "hinagata.labeled-aggregate/v1",
  "aggregate_id": "2026-10-23/labels-backfill",
  "window_start_ms": 1761177600000,
  "window_end_ms": 1761264000000,
  "kind": {
    "classifier": {
      "labels": ["churn", "stay"],
      "confusion": [{"counts": [412, 88]}, {"counts": [97, 1403]}],
      "score_histograms": {
        "churn": {
          "positive_counts": [0, 1, "... 100 counts"],
          "negative_counts": [31, 28, "... 100 counts"],
          "score_sums": [0.14, 0.43, "... 100 sums"]
        }
      }
    }
  }
}
```

- `kind` holds `classifier` or `regressor`.
- `confusion` has one row per actual label in `labels` order, and each row counts rows by predicted label in the same order.
- `score_histograms` is optional per class. For a class it bins the predicted probability of that class over the same rows: 100 equal-width bins over [0, 1], bin `i` holding scores in `[i / 100, (i + 1) / 100)` and a score of exactly 1 in the last bin. `positive_counts` counts rows labeled with the class, `negative_counts` the rest, and `score_sums` adds up the scores in each bin. Its rows must be the confusion matrix's rows.

From these Hinagata computes:

- `accuracy`: the diagonal over all rows.
- `auc` per class with a histogram: the share of positive and negative pairs ordered correctly, counting a pair in the same bin as half. The AUC of the raw scores differs from it by at most half the share of pairs in the same bin, which the report gives as `tolerance`.
- `calibration_error` per class with a histogram: the expected calibration error over ten bins of width 0.1, each the sum of ten score bins: `sum(|positives - score_sum|) / rows`.

## Regressor

```json
{
  "format": "hinagata.labeled-aggregate/v1",
  "aggregate_id": "2026-10-23/labels-backfill",
  "window_start_ms": 1761177600000,
  "window_end_ms": 1761264000000,
  "kind": {
    "regressor": {"count": 2000, "absolute_error_sum": 1840.5, "squared_error_sum": 2711.2}
  }
}
```

From these Hinagata computes `mae` (`absolute_error_sum / count`) and `rmse` (`sqrt(squared_error_sum / count)`).

## Accuracy against the raw rows

Accuracy, calibration error, MAE and RMSE equal the same metrics computed on the raw rows, up to float rounding. AUC is within its `tolerance`. A window with no labeled rows is stored and computes nothing.

## Thresholds and alerts

`PutQualityThresholds` (`POST /v1/models/{model}/versions/{version}/quality-thresholds`) sets a version's `min_accuracy`, `min_auc`, `max_calibration_error`, `max_mae` and `max_rmse`. A metric below a `min_*` or at or above a `max_*` is an alert with role `quality`, the metric, the class or `model`, the value and the threshold, in the drift alert stream (`ListDriftAlerts`).

`aggregate_id` is unique per version. Sending the same aggregate again returns the first report; different contents under the same id are refused.
