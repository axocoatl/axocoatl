//! Reproducible process-memory sampling for Axocoatl's in-process actor control plane.
//!
//! This is intentionally not a model, tool, workspace, browser, terminal, git, or
//! sandbox benchmark. It measures one optimized process containing a fixed
//! single-thread Tokio runtime, an `AgentRegistry`, and idle `AgentActor` tasks
//! backed by a deterministic no-network behavior.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axocoatl_actor::{AgentActor, AgentBehavior, AgentError, AgentRegistry};
use axocoatl_core::{AgentConfig, AgentId, AgentInput, AgentOutput};
use axocoatl_workspace::resource_footprint_contract::{validate_document, ACTOR_COUNTS, SCHEMA};
use ractor::Actor;
use serde_json::{json, Value};
use tokio::runtime::{Builder, Runtime};

const DEFAULT_TRIALS: usize = 7;
const DEFAULT_READS: usize = 5;
const DEFAULT_SETTLE_MS: u64 = 100;
const DEFAULT_READ_INTERVAL_MS: u64 = 20;

struct NoNetworkBehavior;

#[async_trait::async_trait]
impl AgentBehavior for NoNetworkBehavior {
    async fn on_start(&mut self, _config: &AgentConfig) -> Result<(), AgentError> {
        Ok(())
    }

    async fn execute(&mut self, _input: AgentInput) -> Result<AgentOutput, AgentError> {
        Ok(AgentOutput::text("benchmark-noop"))
    }

    async fn on_stop(&mut self) -> Result<(), AgentError> {
        Ok(())
    }
}

#[derive(Debug)]
struct Args {
    trials: usize,
    reads: usize,
    settle: Duration,
    read_interval: Duration,
    output: Option<PathBuf>,
    validate: Option<PathBuf>,
    worker_count: Option<usize>,
    worker_repetition: Option<usize>,
    worker_execution_order: Option<usize>,
}

#[derive(Debug)]
struct MemoryReading {
    primary_kib: u64,
    rss_kib: u64,
    pss_kib: Option<u64>,
}

#[derive(Debug)]
struct StateSample {
    primary_kib: u64,
    primary_readings_kib: Vec<u64>,
    rss_readings_kib: Vec<u64>,
    pss_readings_kib: Option<Vec<u64>>,
}

#[derive(Debug)]
struct Trial {
    worker_pid: u32,
    repetition: usize,
    execution_order: usize,
    actor_count: usize,
    baseline: StateSample,
    loaded: StateSample,
    post_cleanup: StateSample,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("resource-footprint: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args(env::args().skip(1))?;

    if let Some(path) = &args.validate {
        let bytes = fs::read(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let document: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("{} is not valid JSON: {error}", path.display()))?;
        validate_document(&document)
            .map_err(|error| format!("{} failed validation: {error}", path.display()))?;
        println!("validated {}", path.display());
        return Ok(());
    }

    if cfg!(debug_assertions) {
        return Err(
            "this benchmark requires an optimized build; run it with `cargo bench --bench resource_footprint -- ...`"
                .to_string(),
        );
    }

    if let Some(actor_count) = args.worker_count {
        let runtime = build_runtime()?;
        runtime.block_on(warm_actor_runtime())?;
        thread::sleep(args.settle);
        let trial = runtime.block_on(run_trial(
            args.worker_repetition
                .ok_or_else(|| "worker repetition is missing".to_string())?,
            args.worker_execution_order
                .ok_or_else(|| "worker execution order is missing".to_string())?,
            actor_count,
            args.reads,
            args.read_interval,
            args.settle,
        ))?;
        println!(
            "{}",
            serde_json::to_string(&trial_json(&trial))
                .map_err(|error| format!("could not serialize worker result: {error}"))?
        );
        return Ok(());
    }

    let mut trials = Vec::with_capacity(args.trials * ACTOR_COUNTS.len());
    for repetition in 0..args.trials {
        let mut order = ACTOR_COUNTS.to_vec();
        let order_len = order.len();
        order.rotate_left(repetition % order_len);

        for (execution_order, actor_count) in order.into_iter().enumerate() {
            eprintln!(
                "sampling repetition {}/{}, actors {}",
                repetition + 1,
                args.trials,
                actor_count
            );
            trials.push(run_worker_process(
                repetition,
                execution_order,
                actor_count,
                args.reads,
                args.read_interval,
                args.settle,
            )?);
        }
    }

    let document = build_document(&args, &trials)?;
    validate_document(&document)
        .map_err(|error| format!("generated output failed its contract: {error}"))?;
    let rendered = serde_json::to_string_pretty(&document)
        .map_err(|error| format!("could not serialize results: {error}"))?;

    if let Some(path) = &args.output {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        fs::write(path, format!("{rendered}\n"))
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        eprintln!("wrote {}", path.display());
    }

    println!("{rendered}");
    Ok(())
}

fn build_runtime() -> Result<Runtime, String> {
    Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("axocoatl-footprint")
        .build()
        .map_err(|error| format!("could not create Tokio runtime: {error}"))
}

async fn warm_actor_runtime() -> Result<(), String> {
    let id = AgentId::new("resource-warmup");
    let config = benchmark_config(id.clone());
    let (actor_ref, handle) = AgentActor::spawn(
        Some("resource-warmup".to_string()),
        AgentActor,
        (
            config,
            Box::new(NoNetworkBehavior) as Box<dyn AgentBehavior>,
        ),
    )
    .await
    .map_err(|error| format!("warm-up actor failed to spawn: {error}"))?;
    actor_ref.stop(None);
    handle
        .await
        .map_err(|error| format!("warm-up actor failed to join: {error}"))?;
    Ok(())
}

async fn run_trial(
    repetition: usize,
    execution_order: usize,
    actor_count: usize,
    reads: usize,
    read_interval: Duration,
    settle: Duration,
) -> Result<Trial, String> {
    let registry = AgentRegistry::new();
    let baseline = sample_state(reads, read_interval)?;

    let mut actors = Vec::with_capacity(actor_count);
    for index in 0..actor_count {
        let id = AgentId::new(format!("resource-{repetition}-{actor_count}-{index}"));
        let actor_name = id.to_string();
        let (actor_ref, handle) = AgentActor::spawn(
            Some(actor_name),
            AgentActor,
            (
                benchmark_config(id.clone()),
                Box::new(NoNetworkBehavior) as Box<dyn AgentBehavior>,
            ),
        )
        .await
        .map_err(|error| format!("actor {index} of {actor_count} failed to spawn: {error}"))?;
        registry.register(id.clone(), actor_ref.clone()).await;
        actors.push((id, actor_ref, handle));
    }
    if registry.count().await != actor_count {
        return Err(format!(
            "registry contained {} actors after spawning {actor_count}",
            registry.count().await
        ));
    }

    tokio::time::sleep(settle).await;
    let loaded = sample_state(reads, read_interval)?;

    for (_, actor_ref, _) in &actors {
        actor_ref.stop(None);
    }
    for (id, _, handle) in actors {
        handle
            .await
            .map_err(|error| format!("actor {id} failed to join: {error}"))?;
        registry.remove(&id).await;
    }
    if registry.count().await != 0 {
        return Err("registry was not empty after cleanup".to_string());
    }

    tokio::time::sleep(settle).await;
    let post_cleanup = sample_state(reads, read_interval)?;

    Ok(Trial {
        worker_pid: std::process::id(),
        repetition,
        execution_order,
        actor_count,
        baseline,
        loaded,
        post_cleanup,
    })
}

fn run_worker_process(
    repetition: usize,
    execution_order: usize,
    actor_count: usize,
    reads: usize,
    read_interval: Duration,
    settle: Duration,
) -> Result<Trial, String> {
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve benchmark executable: {error}"))?;
    let output = Command::new(executable)
        .arg("--worker-count")
        .arg(actor_count.to_string())
        .arg("--worker-repetition")
        .arg(repetition.to_string())
        .arg("--worker-execution-order")
        .arg(execution_order.to_string())
        .arg("--reads")
        .arg(reads.to_string())
        .arg("--settle-ms")
        .arg(duration_millis(settle).to_string())
        .arg("--read-interval-ms")
        .arg(duration_millis(read_interval).to_string())
        .output()
        .map_err(|error| format!("could not start actor-count worker: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "actor-count worker failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "actor-count worker returned invalid JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout).trim()
        )
    })?;
    trial_from_json(&value)
}

fn benchmark_config(id: AgentId) -> AgentConfig {
    AgentConfig {
        name: format!("Resource benchmark {id}"),
        id,
        provider: "benchmark-no-network".to_string(),
        model: "none".to_string(),
        ..AgentConfig::default()
    }
}

fn sample_state(reads: usize, interval: Duration) -> Result<StateSample, String> {
    let mut raw = Vec::with_capacity(reads);
    for index in 0..reads {
        raw.push(read_memory()?);
        if index + 1 < reads && !interval.is_zero() {
            thread::sleep(interval);
        }
    }

    let primary_readings_kib = raw
        .iter()
        .map(|reading| reading.primary_kib)
        .collect::<Vec<_>>();
    let rss_readings_kib = raw
        .iter()
        .map(|reading| reading.rss_kib)
        .collect::<Vec<_>>();
    let pss_readings_kib = raw
        .iter()
        .map(|reading| reading.pss_kib)
        .collect::<Option<Vec<_>>>();
    let primary_kib = median_u64(&primary_readings_kib);

    Ok(StateSample {
        primary_kib,
        primary_readings_kib,
        rss_readings_kib,
        pss_readings_kib,
    })
}

#[cfg(target_os = "linux")]
fn read_memory() -> Result<MemoryReading, String> {
    let contents = fs::read_to_string("/proc/self/smaps_rollup")
        .map_err(|error| format!("could not read /proc/self/smaps_rollup: {error}"))?;
    let rss_kib = parse_smaps_value(&contents, "Rss:")?;
    let pss_kib = parse_smaps_value(&contents, "Pss:")?;
    Ok(MemoryReading {
        primary_kib: pss_kib,
        rss_kib,
        pss_kib: Some(pss_kib),
    })
}

#[cfg(target_os = "linux")]
fn parse_smaps_value(contents: &str, label: &str) -> Result<u64, String> {
    contents
        .lines()
        .find_map(|line| {
            line.strip_prefix(label)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse().ok())
        })
        .ok_or_else(|| format!("{label} was absent from /proc/self/smaps_rollup"))
}

#[cfg(not(target_os = "linux"))]
fn read_memory() -> Result<MemoryReading, String> {
    let pid = std::process::id().to_string();
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .map_err(|error| format!("could not execute ps: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ps failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let rss_kib = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("could not parse ps RSS output: {error}"))?;
    Ok(MemoryReading {
        primary_kib: rss_kib,
        rss_kib,
        pss_kib: None,
    })
}

fn build_document(args: &Args, trials: &[Trial]) -> Result<Value, String> {
    let (primary_metric, source, limitations) = measurement_metadata();
    let trial_values = trials.iter().map(trial_json).collect::<Vec<_>>();
    let summaries = ACTOR_COUNTS
        .iter()
        .map(|actor_count| summary_json(*actor_count, trials))
        .collect::<Result<Vec<_>, _>>()?;
    let podman_version = command_line("podman", &["--version"]);
    let executable = env::current_exe()
        .map_err(|error| format!("could not resolve benchmark executable: {error}"))?;
    let executable_sha256 = file_sha256(&executable).ok_or_else(|| {
        format!(
            "could not hash benchmark executable {}",
            executable.display()
        )
    })?;

    Ok(json!({
        "schema": SCHEMA,
        "generated_at_utc": utc_now(),
        "benchmark": {
            "scope": "minimum idle in-process actor control-plane overhead",
            "actor_counts": ACTOR_COUNTS,
            "trials_per_count": args.trials,
            "reads_per_state": args.reads,
            "settle_ms": duration_millis(args.settle),
            "read_interval_ms": duration_millis(args.read_interval),
            "runtime_worker_threads": 1,
            "fresh_process_per_trial": true,
            "worker_processes": trials.len(),
            "optimized_build": !cfg!(debug_assertions),
            "build_invocation": "cargo bench --bench resource_footprint",
            "behavior": "deterministic no-network AgentBehavior; actors remain idle after spawn",
            "included": [
                "fresh optimized worker process and loaded code for every trial",
                "one fixed Tokio worker thread",
                "one AgentRegistry per trial",
                "AgentActor state, config, mailbox, task, and registry entry per actor"
            ],
            "excluded": [
                "model provider clients and model inference",
                "tool execution and MCP servers",
                "workspace/session data and durable memory contents",
                "attempt repository clones, git operations, terminals, and browser processes",
                "attempt sandbox containers or virtual machines",
                "production daemon HTTP/WebSocket services"
            ]
        },
        "host": {
            "os": env::consts::OS,
            "arch": env::consts::ARCH,
            "hostname": command_line("hostname", &[]),
            "os_version": os_version(),
            "logical_cpus": thread::available_parallelism().map(|value| value.get()).ok(),
            "rustc": command_line("rustc", &["-Vv"]),
            "orchestrator_pid": std::process::id()
        },
        "source": {
            "git_head": command_line("git", &["rev-parse", "HEAD"]),
            "repository_dirty": command_success_with_output("git", &["status", "--porcelain"])
                .map(|output| !output.trim().is_empty()),
            "benchmark_executable_sha256": executable_sha256
        },
        "measurement": {
            "primary_metric": primary_metric,
            "unit": "KiB",
            "source": source,
            "state_value": "median of the raw readings for that state",
            "incremental_definition": "loaded primary KiB minus the immediately preceding baseline primary KiB in the same process",
            "cleanup_definition": "post-cleanup primary KiB minus the same trial baseline; actors are stopped, joined, and removed before sampling",
            "limitations": limitations
        },
        "attempt_sandboxes": {
            "status": "unmeasured",
            "requested_counts": [1, 2, 4],
            "podman_detected_version": podman_version,
            "reason": "A generic idle container would not represent an Axocoatl attempt. A production attempt includes an independent no-hardlinks repository clone plus lifecycle state; this benchmark deliberately avoids creating repository clones or containers without a dedicated fixture and teardown proof."
        },
        "trials": trial_values,
        "summary": summaries
    }))
}

fn trial_json(trial: &Trial) -> Value {
    let incremental = signed_difference(trial.loaded.primary_kib, trial.baseline.primary_kib);
    let retained = signed_difference(trial.post_cleanup.primary_kib, trial.baseline.primary_kib);
    json!({
        "worker_pid": trial.worker_pid,
        "repetition": trial.repetition,
        "execution_order": trial.execution_order,
        "actor_count": trial.actor_count,
        "baseline": state_json(&trial.baseline),
        "loaded": state_json(&trial.loaded),
        "post_cleanup": state_json(&trial.post_cleanup),
        "incremental_kib": incremental,
        "retained_after_cleanup_kib": retained
    })
}

fn trial_from_json(value: &Value) -> Result<Trial, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "worker result must be a JSON object".to_string())?;
    Ok(Trial {
        worker_pid: required_u64(object.get("worker_pid"), "worker_pid")?
            .try_into()
            .map_err(|_| "worker_pid is too large".to_string())?,
        repetition: required_usize(object.get("repetition"), "repetition")?,
        execution_order: required_usize(object.get("execution_order"), "execution_order")?,
        actor_count: required_usize(object.get("actor_count"), "actor_count")?,
        baseline: state_from_json(object.get("baseline"), "baseline")?,
        loaded: state_from_json(object.get("loaded"), "loaded")?,
        post_cleanup: state_from_json(object.get("post_cleanup"), "post_cleanup")?,
    })
}

fn state_from_json(value: Option<&Value>, field: &str) -> Result<StateSample, String> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| format!("worker {field} must be an object"))?;
    Ok(StateSample {
        primary_kib: required_u64(object.get("primary_kib"), &format!("{field}.primary_kib"))?,
        primary_readings_kib: required_u64_array(
            object.get("readings_kib"),
            &format!("{field}.readings_kib"),
        )?,
        rss_readings_kib: required_u64_array(
            object.get("rss_readings_kib"),
            &format!("{field}.rss_readings_kib"),
        )?,
        pss_readings_kib: match object.get("pss_readings_kib") {
            None | Some(Value::Null) => None,
            value => Some(required_u64_array(
                value,
                &format!("{field}.pss_readings_kib"),
            )?),
        },
    })
}

fn required_usize(value: Option<&Value>, field: &str) -> Result<usize, String> {
    required_u64(value, field)?
        .try_into()
        .map_err(|_| format!("worker {field} is too large"))
}

fn required_u64(value: Option<&Value>, field: &str) -> Result<u64, String> {
    value
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("worker {field} must be an unsigned integer"))
}

fn required_u64_array(value: Option<&Value>, field: &str) -> Result<Vec<u64>, String> {
    value
        .and_then(Value::as_array)
        .ok_or_else(|| format!("worker {field} must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| format!("worker {field} entries must be unsigned integers"))
        })
        .collect()
}

fn state_json(sample: &StateSample) -> Value {
    json!({
        "primary_kib": sample.primary_kib,
        "readings_kib": sample.primary_readings_kib,
        "rss_readings_kib": sample.rss_readings_kib,
        "pss_readings_kib": sample.pss_readings_kib
    })
}

fn summary_json(actor_count: usize, trials: &[Trial]) -> Result<Value, String> {
    let matching = trials
        .iter()
        .filter(|trial| trial.actor_count == actor_count)
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return Err(format!("no trials found for actor count {actor_count}"));
    }
    let incremental = matching
        .iter()
        .map(|trial| signed_difference(trial.loaded.primary_kib, trial.baseline.primary_kib))
        .collect::<Vec<_>>();
    let retained = matching
        .iter()
        .map(|trial| signed_difference(trial.post_cleanup.primary_kib, trial.baseline.primary_kib))
        .collect::<Vec<_>>();
    let incremental_stats = stats_json(&incremental);
    let retained_stats = stats_json(&retained);
    let median_incremental_per_actor = median_i64(&incremental) / actor_count as f64;

    Ok(json!({
        "actor_count": actor_count,
        "trials": matching.len(),
        "incremental_kib": incremental_stats,
        "median_incremental_kib_per_actor": median_incremental_per_actor,
        "retained_after_cleanup_kib": retained_stats
    }))
}

fn stats_json(values: &[i64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let sum = sorted.iter().map(|value| *value as f64).sum::<f64>();
    json!({
        "min": sorted[0],
        "median": median_i64(&sorted),
        "mean": sum / sorted.len() as f64,
        "max": sorted[sorted.len() - 1]
    })
}

fn median_u64(values: &[u64]) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn median_i64(values: &[i64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    if sorted.len() & 1 == 0 {
        (sorted[middle - 1] as f64 + sorted[middle] as f64) / 2.0
    } else {
        sorted[middle] as f64
    }
}

fn signed_difference(left: u64, right: u64) -> i64 {
    let difference = i128::from(left) - i128::from(right);
    i64::try_from(difference).expect("process memory difference must fit in i64")
}

#[cfg(target_os = "linux")]
fn measurement_metadata() -> (&'static str, &'static str, Vec<&'static str>) {
    (
        "pss_kib",
        "/proc/self/smaps_rollup Pss",
        vec![
            "PSS apportions shared resident pages but does not attribute allocator arenas or runtime capacity to individual actors.",
            "Process-level deltas can be smaller than page granularity or negative because sampling and allocator activity are noisy.",
            "Cleanup retention is not by itself a leak: allocators and the Tokio runtime can retain pages for reuse.",
            "The result is minimum idle actor overhead, not memory use while an agent runs a model, tools, or a sandbox.",
        ],
    )
}

#[cfg(target_os = "macos")]
fn measurement_metadata() -> (&'static str, &'static str, Vec<&'static str>) {
    (
        "rss_kib",
        "ps -o rss= -p <pid>",
        vec![
            "macOS does not expose Linux-style PSS through this portable harness; RSS includes resident shared pages and cannot be apportioned per actor.",
            "Process-level RSS deltas can be smaller than page granularity or negative because sampling and allocator activity are noisy.",
            "Cleanup RSS is not by itself a leak: allocators and the Tokio runtime can retain resident pages for reuse.",
            "The result is minimum idle actor overhead, not memory use while an agent runs a model, tools, or a sandbox.",
        ],
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn measurement_metadata() -> (&'static str, &'static str, Vec<&'static str>) {
    (
        "rss_kib",
        "ps -o rss= -p <pid>",
        vec![
            "This platform uses process RSS because a proportional-set-size source is unavailable in this harness.",
            "Process-level RSS cannot be apportioned to individual actors and can move because of allocator activity.",
            "Cleanup RSS is not by itself a leak: allocators and the Tokio runtime can retain resident pages for reuse.",
            "The result is minimum idle actor overhead, not memory use while an agent runs a model, tools, or a sandbox.",
        ],
    )
}

fn parse_args(arguments: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        trials: DEFAULT_TRIALS,
        reads: DEFAULT_READS,
        settle: Duration::from_millis(DEFAULT_SETTLE_MS),
        read_interval: Duration::from_millis(DEFAULT_READ_INTERVAL_MS),
        output: None,
        validate: None,
        worker_count: None,
        worker_repetition: None,
        worker_execution_order: None,
    };
    let mut arguments = arguments.peekable();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            // Cargo appends this marker when running a harnessless `[[bench]]`.
            "--bench" => {}
            "--trials" => {
                args.trials =
                    parse_positive_usize(next_value(&mut arguments, "--trials")?, "--trials")?;
            }
            "--reads" => {
                args.reads =
                    parse_positive_usize(next_value(&mut arguments, "--reads")?, "--reads")?;
                if args.reads & 1 == 0 {
                    return Err(
                        "--reads must be odd so each state has an observed median".to_string()
                    );
                }
            }
            "--settle-ms" => {
                args.settle = Duration::from_millis(parse_u64(
                    next_value(&mut arguments, "--settle-ms")?,
                    "--settle-ms",
                )?);
            }
            "--read-interval-ms" => {
                args.read_interval = Duration::from_millis(parse_u64(
                    next_value(&mut arguments, "--read-interval-ms")?,
                    "--read-interval-ms",
                )?);
            }
            "--output" => {
                args.output = Some(PathBuf::from(next_value(&mut arguments, "--output")?));
            }
            "--validate" => {
                args.validate = Some(PathBuf::from(next_value(&mut arguments, "--validate")?));
            }
            "--worker-count" => {
                args.worker_count = Some(parse_positive_usize(
                    next_value(&mut arguments, "--worker-count")?,
                    "--worker-count",
                )?);
            }
            "--worker-repetition" => {
                args.worker_repetition = Some(parse_usize(
                    next_value(&mut arguments, "--worker-repetition")?,
                    "--worker-repetition",
                )?);
            }
            "--worker-execution-order" => {
                args.worker_execution_order = Some(parse_usize(
                    next_value(&mut arguments, "--worker-execution-order")?,
                    "--worker-execution-order",
                )?);
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            unknown => return Err(format!("unknown argument {unknown:?}; use --help")),
        }
    }
    if args.validate.is_some() && args.output.is_some() {
        return Err("--validate and --output cannot be used together".to_string());
    }
    let worker_fields = [
        args.worker_count.is_some(),
        args.worker_repetition.is_some(),
        args.worker_execution_order.is_some(),
    ];
    if worker_fields.iter().any(|present| *present) && worker_fields.iter().any(|present| !*present)
    {
        return Err("internal worker arguments must be supplied together".to_string());
    }
    if let Some(actor_count) = args.worker_count {
        if !ACTOR_COUNTS.contains(&actor_count) {
            return Err(format!("unsupported worker actor count {actor_count}"));
        }
        if args.output.is_some() || args.validate.is_some() {
            return Err("internal worker mode cannot write or validate an artifact".to_string());
        }
    }
    Ok(args)
}

fn next_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn parse_positive_usize(value: String, option: &str) -> Result<usize, String> {
    let value = parse_usize(value, option)?;
    if value == 0 {
        Err(format!("{option} must be greater than zero"))
    } else {
        Ok(value)
    }
}

fn parse_usize(value: String, option: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|error| format!("invalid {option} value: {error}"))
}

fn parse_u64(value: String, option: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|error| format!("invalid {option} value: {error}"))
}

fn print_help() {
    println!(
        "resource-footprint\n\n\
         Run with an optimized Cargo bench build.\n\n\
         Options:\n\
           --trials N               Repetitions per actor count (default: {DEFAULT_TRIALS})\n\
           --reads N                Odd raw readings per state (default: {DEFAULT_READS})\n\
           --settle-ms N            Wait after spawn/cleanup (default: {DEFAULT_SETTLE_MS})\n\
           --read-interval-ms N     Wait between raw readings (default: {DEFAULT_READ_INTERVAL_MS})\n\
           --output PATH            Write the validated JSON artifact\n\
           --validate PATH          Validate an existing artifact without sampling\n"
    );
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn utc_now() -> String {
    command_line("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or(0);
        format!("unix-ms:{millis}")
    })
}

fn os_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        return command_line("sw_vers", &["-productVersion"]);
    }
    #[cfg(target_os = "linux")]
    {
        return fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|contents| {
                contents
                    .lines()
                    .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                    .map(|value| value.trim_matches('"').to_string())
            });
    }
    #[allow(unreachable_code)]
    None
}

fn command_line(program: &str, arguments: &[&str]) -> Option<String> {
    command_success_with_output(program, arguments).and_then(|output| {
        let output = output.trim();
        (!output.is_empty()).then(|| output.to_string())
    })
}

fn command_success_with_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn file_sha256(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    #[cfg(target_os = "macos")]
    let output = command_line("shasum", &["-a", "256", path])?;
    #[cfg(not(target_os = "macos"))]
    let output = command_line("sha256sum", &[path])?;
    output.split_whitespace().next().map(str::to_string)
}
