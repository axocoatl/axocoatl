//! Output-contract validation for the resource-footprint benchmark.

use std::collections::BTreeSet;

use serde_json::Value;

pub const SCHEMA: &str = "axocoatl.resource-footprint.v1";
pub const ACTOR_COUNTS: [usize; 7] = [1, 4, 8, 16, 32, 64, 100];

/// Validate the benchmark's machine-readable output.
///
/// Incremental measurements are signed because process-level memory readings can
/// move down between samples. Treating that measurement noise as zero would make
/// the raw output look more precise than it is.
pub fn validate_document(document: &Value) -> Result<(), String> {
    let object = document
        .as_object()
        .ok_or_else(|| "document must be a JSON object".to_string())?;

    require_string(object.get("schema"), "schema", Some(SCHEMA))?;
    require_string(object.get("generated_at_utc"), "generated_at_utc", None)?;

    let benchmark = object
        .get("benchmark")
        .and_then(Value::as_object)
        .ok_or_else(|| "benchmark must be an object".to_string())?;
    let samples = require_positive_u64(benchmark.get("trials_per_count"), "trials_per_count")?;
    let reads = require_positive_u64(benchmark.get("reads_per_state"), "reads_per_state")?;
    let counts = benchmark
        .get("actor_counts")
        .and_then(Value::as_array)
        .ok_or_else(|| "benchmark.actor_counts must be an array".to_string())?;
    let parsed_counts = counts
        .iter()
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| {
                    "benchmark.actor_counts entries must be unsigned integers".to_string()
                })
                .and_then(|count| {
                    usize::try_from(count)
                        .map_err(|_| "benchmark.actor_counts entry is too large".to_string())
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parsed_counts != ACTOR_COUNTS {
        return Err(format!(
            "benchmark.actor_counts must be {ACTOR_COUNTS:?}, got {parsed_counts:?}"
        ));
    }
    if benchmark.get("optimized_build").and_then(Value::as_bool) != Some(true) {
        return Err("benchmark.optimized_build must be true".to_string());
    }
    if benchmark
        .get("fresh_process_per_trial")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Err("benchmark.fresh_process_per_trial must be true".to_string());
    }

    let measurement = object
        .get("measurement")
        .and_then(Value::as_object)
        .ok_or_else(|| "measurement must be an object".to_string())?;
    require_string(
        measurement.get("primary_metric"),
        "measurement.primary_metric",
        None,
    )?;
    require_string(measurement.get("source"), "measurement.source", None)?;
    let limitations = measurement
        .get("limitations")
        .and_then(Value::as_array)
        .ok_or_else(|| "measurement.limitations must be an array".to_string())?;
    if limitations.is_empty() || limitations.iter().any(|item| item.as_str().is_none()) {
        return Err("measurement.limitations must contain strings".to_string());
    }

    let executable_hash = object
        .get("source")
        .and_then(Value::as_object)
        .and_then(|source| source.get("benchmark_executable_sha256"))
        .and_then(Value::as_str)
        .ok_or_else(|| "source.benchmark_executable_sha256 must be a string".to_string())?;
    if executable_hash.len() != 64 || !executable_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("source.benchmark_executable_sha256 must be a SHA-256 hex digest".to_string());
    }

    let sandboxes = object
        .get("attempt_sandboxes")
        .and_then(Value::as_object)
        .ok_or_else(|| "attempt_sandboxes must be an object".to_string())?;
    require_string(
        sandboxes.get("status"),
        "attempt_sandboxes.status",
        Some("unmeasured"),
    )?;
    require_string(sandboxes.get("reason"), "attempt_sandboxes.reason", None)?;

    let trials = object
        .get("trials")
        .and_then(Value::as_array)
        .ok_or_else(|| "trials must be an array".to_string())?;
    let expected_trials = samples
        .checked_mul(ACTOR_COUNTS.len() as u64)
        .ok_or_else(|| "expected trial count overflowed".to_string())?;
    if trials.len() as u64 != expected_trials {
        return Err(format!(
            "trials must contain {expected_trials} entries, got {}",
            trials.len()
        ));
    }
    if require_u64(
        benchmark.get("worker_processes"),
        "benchmark.worker_processes",
    )? != expected_trials
    {
        return Err("benchmark.worker_processes must equal the trial count".to_string());
    }

    let mut seen = BTreeSet::new();
    for trial in trials {
        let trial = trial
            .as_object()
            .ok_or_else(|| "each trial must be an object".to_string())?;
        require_positive_u64(trial.get("worker_pid"), "trial.worker_pid")?;
        let repetition = require_u64(trial.get("repetition"), "trial.repetition")?;
        if repetition >= samples {
            return Err(format!("trial.repetition {repetition} is out of range"));
        }
        let actor_count = require_u64(trial.get("actor_count"), "trial.actor_count")?;
        let actor_count = usize::try_from(actor_count)
            .map_err(|_| "trial.actor_count is too large".to_string())?;
        if !ACTOR_COUNTS.contains(&actor_count) {
            return Err(format!("unexpected trial.actor_count {actor_count}"));
        }
        let execution_order = require_u64(trial.get("execution_order"), "trial.execution_order")?;
        if execution_order >= ACTOR_COUNTS.len() as u64 {
            return Err(format!(
                "trial.execution_order {execution_order} is out of range"
            ));
        }
        if !seen.insert((repetition, actor_count)) {
            return Err(format!(
                "duplicate trial for repetition {repetition}, actor_count {actor_count}"
            ));
        }

        let baseline = validate_state(trial.get("baseline"), "trial.baseline", reads)?;
        let loaded = validate_state(trial.get("loaded"), "trial.loaded", reads)?;
        let cleanup = validate_state(trial.get("post_cleanup"), "trial.post_cleanup", reads)?;
        let incremental = require_i64(trial.get("incremental_kib"), "trial.incremental_kib")?;
        let retained = require_i64(
            trial.get("retained_after_cleanup_kib"),
            "trial.retained_after_cleanup_kib",
        )?;

        if incremental != signed_difference(loaded, baseline)? {
            return Err("trial.incremental_kib does not match loaded - baseline".to_string());
        }
        if retained != signed_difference(cleanup, baseline)? {
            return Err(
                "trial.retained_after_cleanup_kib does not match post_cleanup - baseline"
                    .to_string(),
            );
        }
    }

    let summaries = object
        .get("summary")
        .and_then(Value::as_array)
        .ok_or_else(|| "summary must be an array".to_string())?;
    if summaries.len() != ACTOR_COUNTS.len() {
        return Err(format!(
            "summary must contain {} entries, got {}",
            ACTOR_COUNTS.len(),
            summaries.len()
        ));
    }
    for (summary, expected_count) in summaries.iter().zip(ACTOR_COUNTS) {
        let summary = summary
            .as_object()
            .ok_or_else(|| "each summary must be an object".to_string())?;
        if require_u64(summary.get("actor_count"), "summary.actor_count")? != expected_count as u64
        {
            return Err("summary actor counts are missing or out of order".to_string());
        }
        if require_u64(summary.get("trials"), "summary.trials")? != samples {
            return Err("summary.trials does not match trials_per_count".to_string());
        }
        for field in ["incremental_kib", "retained_after_cleanup_kib"] {
            let stats = summary
                .get(field)
                .and_then(Value::as_object)
                .ok_or_else(|| format!("summary.{field} must be an object"))?;
            for statistic in ["min", "median", "mean", "max"] {
                if stats.get(statistic).and_then(Value::as_f64).is_none() {
                    return Err(format!("summary.{field}.{statistic} must be numeric"));
                }
            }
        }
    }

    Ok(())
}

fn validate_state(value: Option<&Value>, field: &str, reads: u64) -> Result<u64, String> {
    let state = value
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{field} must be an object"))?;
    let primary = require_u64(state.get("primary_kib"), &format!("{field}.primary_kib"))?;
    let readings = state
        .get("readings_kib")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{field}.readings_kib must be an array"))?;
    if readings.len() as u64 != reads || readings.iter().any(|item| item.as_u64().is_none()) {
        return Err(format!(
            "{field}.readings_kib must contain exactly {reads} unsigned integers"
        ));
    }
    let mut sorted = readings
        .iter()
        .filter_map(Value::as_u64)
        .collect::<Vec<_>>();
    sorted.sort_unstable();
    let expected_median = sorted[sorted.len() / 2];
    if primary != expected_median {
        return Err(format!("{field}.primary_kib must be the readings median"));
    }
    Ok(primary)
}

fn signed_difference(left: u64, right: u64) -> Result<i64, String> {
    i64::try_from(i128::from(left) - i128::from(right))
        .map_err(|_| "memory difference does not fit in i64".to_string())
}

fn require_string(
    value: Option<&Value>,
    field: &str,
    expected: Option<&str>,
) -> Result<(), String> {
    let actual = value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{field} must be a non-empty string"))?;
    if let Some(expected) = expected {
        if actual != expected {
            return Err(format!("{field} must be {expected:?}, got {actual:?}"));
        }
    }
    Ok(())
}

fn require_u64(value: Option<&Value>, field: &str) -> Result<u64, String> {
    value
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{field} must be an unsigned integer"))
}

fn require_positive_u64(value: Option<&Value>, field: &str) -> Result<u64, String> {
    let value = require_u64(value, field)?;
    if value == 0 {
        Err(format!("{field} must be greater than zero"))
    } else {
        Ok(value)
    }
}

fn require_i64(value: Option<&Value>, field: &str) -> Result<i64, String> {
    value
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{field} must be a signed integer"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn valid_document() -> Value {
        let trials = ACTOR_COUNTS
            .iter()
            .map(|count| {
                json!({
                    "worker_pid": 1000 + *count,
                    "repetition": 0,
                    "execution_order": 0,
                    "actor_count": count,
                    "baseline": {"primary_kib": 100, "readings_kib": [99, 100, 101]},
                    "loaded": {"primary_kib": 110, "readings_kib": [109, 110, 111]},
                    "post_cleanup": {"primary_kib": 105, "readings_kib": [104, 105, 106]},
                    "incremental_kib": 10,
                    "retained_after_cleanup_kib": 5
                })
            })
            .collect::<Vec<_>>();
        let summary = ACTOR_COUNTS
            .iter()
            .map(|count| {
                json!({
                    "actor_count": count,
                    "trials": 1,
                    "incremental_kib": {"min": 10, "median": 10, "mean": 10.0, "max": 10},
                    "retained_after_cleanup_kib": {"min": 5, "median": 5, "mean": 5.0, "max": 5}
                })
            })
            .collect::<Vec<_>>();
        json!({
            "schema": SCHEMA,
            "generated_at_utc": "2026-08-17T00:00:00Z",
            "benchmark": {
                "actor_counts": ACTOR_COUNTS,
                "trials_per_count": 1,
                "reads_per_state": 3,
                "fresh_process_per_trial": true,
                "worker_processes": ACTOR_COUNTS.len(),
                "optimized_build": true
            },
            "measurement": {
                "primary_metric": "rss_kib",
                "source": "test",
                "limitations": ["test limitation"]
            },
            "source": {
                "benchmark_executable_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            },
            "attempt_sandboxes": {
                "status": "unmeasured",
                "reason": "not part of this test"
            },
            "trials": trials,
            "summary": summary
        })
    }

    #[test]
    fn accepts_complete_document() {
        validate_document(&valid_document()).unwrap();
    }

    #[test]
    fn rejects_wrong_actor_counts() {
        let mut document = valid_document();
        document["benchmark"]["actor_counts"] = json!([1, 4, 8]);
        assert!(validate_document(&document)
            .unwrap_err()
            .contains("actor_counts"));
    }

    #[test]
    fn rejects_inconsistent_increment() {
        let mut document = valid_document();
        document["trials"][0]["incremental_kib"] = json!(11);
        assert!(validate_document(&document)
            .unwrap_err()
            .contains("loaded - baseline"));
    }

    #[test]
    fn accepts_negative_measurement_noise() {
        let mut document = valid_document();
        document["trials"][0]["loaded"] = json!({"primary_kib": 95, "readings_kib": [94, 95, 96]});
        document["trials"][0]["incremental_kib"] = json!(-5);
        validate_document(&document).unwrap();
    }
}
