//! Benchmark: TOON format vs JSON token efficiency.
//!
//! Measures serialization time and tiktoken token counts for uniform arrays
//! of 10, 100, and 1000 rows. Validates the claim that TOON uses 20-35% fewer
//! tokens than minified JSON for uniform tabular data.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::json;

use axocoatl_token::{
    adaptive_serialize, try_serialize_toon, FormatHint, TiktokenCounter, TokenCounter,
};

/// Build a uniform array of `n` objects, each with 4 string/number fields.
fn build_uniform_array(n: usize) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = (0..n)
        .map(|i| {
            json!({
                "id": i,
                "name": format!("agent_{}", i),
                "status": "active",
                "score": i * 10 + 42
            })
        })
        .collect();
    serde_json::Value::Array(rows)
}

/// Benchmark: serialize to TOON format.
fn bench_toon_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("toon_serialize");
    for size in [10, 100, 1000] {
        let data = build_uniform_array(size);
        group.bench_with_input(BenchmarkId::new("rows", size), &data, |b, data| {
            b.iter(|| {
                let result = try_serialize_toon(black_box(data));
                black_box(result)
            });
        });
    }
    group.finish();
}

/// Benchmark: serialize to minified JSON.
fn bench_json_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("json_serialize");
    for size in [10, 100, 1000] {
        let data = build_uniform_array(size);
        group.bench_with_input(BenchmarkId::new("rows", size), &data, |b, data| {
            b.iter(|| {
                let result = serde_json::to_string(black_box(data)).unwrap();
                black_box(result)
            });
        });
    }
    group.finish();
}

/// Benchmark: adaptive_serialize chooses the best format for uniform arrays.
fn bench_adaptive_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("adaptive_serialize");
    for size in [10, 100, 1000] {
        let data = build_uniform_array(size);
        group.bench_with_input(BenchmarkId::new("rows", size), &data, |b, data| {
            b.iter(|| {
                let result = adaptive_serialize(black_box(data), FormatHint::UniformArray);
                black_box(result)
            });
        });
    }
    group.finish();
}

/// Benchmark: tiktoken token counting of TOON vs JSON output.
/// This measures the actual token savings claim.
fn bench_token_count_comparison(c: &mut Criterion) {
    let counter = TiktokenCounter::o200k_base().expect("Failed to load o200k_base tokenizer");

    let mut group = c.benchmark_group("token_count");
    for size in [10, 100, 1000] {
        let data = build_uniform_array(size);

        let toon_str = try_serialize_toon(&data).expect("TOON serialization failed");
        let json_str = serde_json::to_string(&data).unwrap();

        // Benchmark counting TOON tokens
        group.bench_with_input(
            BenchmarkId::new("toon_count", size),
            &toon_str,
            |b, text| {
                b.iter(|| counter.count_text(black_box(text)));
            },
        );

        // Benchmark counting JSON tokens
        group.bench_with_input(
            BenchmarkId::new("json_count", size),
            &json_str,
            |b, text| {
                b.iter(|| counter.count_text(black_box(text)));
            },
        );

        // Report the actual savings (printed once, not benchmarked)
        let toon_tokens = counter.count_text(&toon_str);
        let json_tokens = counter.count_text(&json_str);
        let savings_pct = 100.0 * (1.0 - toon_tokens as f64 / json_tokens as f64);
        eprintln!(
            "[{size} rows] TOON: {toon_tokens} tokens, JSON: {json_tokens} tokens, savings: {savings_pct:.1}%"
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_toon_serialize,
    bench_json_serialize,
    bench_adaptive_serialize,
    bench_token_count_comparison,
);
criterion_main!(benches);
