//! Benchmark: WASM isolation sandbox startup and precompilation.
//!
//! Measures WasmtimeSandbox::new() instantiation time and precompile_tool with a
//! minimal WASM module. Target: <1ms instantiation after engine is warm.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use axocoatl_isolation::WasmtimeSandbox;

/// Minimal valid WASM module (empty, compiled from WAT).
fn minimal_wasm_module() -> Vec<u8> {
    wat::parse_str("(module)").expect("Failed to parse minimal WAT module")
}

/// WASM module with a memory export (slightly more realistic).
fn wasm_with_memory() -> Vec<u8> {
    wat::parse_str(
        r#"(module
            (memory (export "memory") 1)
        )"#,
    )
    .expect("Failed to parse WAT module with memory")
}

/// WASM module with a function and memory.
fn wasm_with_function() -> Vec<u8> {
    wat::parse_str(
        r#"(module
            (memory (export "memory") 1)
            (func (export "add") (param i32 i32) (result i32)
                local.get 0
                local.get 1
                i32.add
            )
        )"#,
    )
    .expect("Failed to parse WAT module with function")
}

// ---------------------------------------------------------------------------
// Sandbox instantiation benchmarks
// ---------------------------------------------------------------------------

/// Benchmark creating a new WasmtimeSandbox (Engine + config).
fn bench_sandbox_new(c: &mut Criterion) {
    c.bench_function("sandbox_new", |b| {
        b.iter(|| {
            let sandbox = WasmtimeSandbox::new().unwrap();
            black_box(sandbox);
        });
    });
}

// ---------------------------------------------------------------------------
// Module precompilation benchmarks
// ---------------------------------------------------------------------------

/// Benchmark precompiling a minimal (empty) WASM module.
fn bench_precompile_minimal(c: &mut Criterion) {
    let wasm = minimal_wasm_module();
    c.bench_function("precompile_minimal_module", |b| {
        b.iter(|| {
            let mut sandbox = WasmtimeSandbox::new().unwrap();
            sandbox
                .precompile_tool("minimal", black_box(&wasm))
                .unwrap();
            black_box(&sandbox);
        });
    });
}

/// Benchmark precompiling a WASM module with memory.
fn bench_precompile_with_memory(c: &mut Criterion) {
    let wasm = wasm_with_memory();
    c.bench_function("precompile_memory_module", |b| {
        b.iter(|| {
            let mut sandbox = WasmtimeSandbox::new().unwrap();
            sandbox
                .precompile_tool("with_memory", black_box(&wasm))
                .unwrap();
            black_box(&sandbox);
        });
    });
}

/// Benchmark precompiling a WASM module with a function.
fn bench_precompile_with_function(c: &mut Criterion) {
    let wasm = wasm_with_function();
    c.bench_function("precompile_function_module", |b| {
        b.iter(|| {
            let mut sandbox = WasmtimeSandbox::new().unwrap();
            sandbox
                .precompile_tool("with_func", black_box(&wasm))
                .unwrap();
            black_box(&sandbox);
        });
    });
}

/// Benchmark precompiling multiple tools into the same sandbox.
fn bench_precompile_multiple_tools(c: &mut Criterion) {
    let minimal = minimal_wasm_module();
    let with_mem = wasm_with_memory();
    let with_func = wasm_with_function();

    c.bench_function("precompile_3_tools", |b| {
        b.iter(|| {
            let mut sandbox = WasmtimeSandbox::new().unwrap();
            sandbox.precompile_tool("tool_a", &minimal).unwrap();
            sandbox.precompile_tool("tool_b", &with_mem).unwrap();
            sandbox.precompile_tool("tool_c", &with_func).unwrap();
            black_box(sandbox.tool_names());
        });
    });
}

/// Benchmark just the precompilation step on an already-created sandbox.
/// This isolates Module::new() cost from Engine::new() cost.
fn bench_precompile_reuse_engine(c: &mut Criterion) {
    let wasm = wasm_with_function();
    let mut sandbox = WasmtimeSandbox::new().unwrap();

    c.bench_function("precompile_reuse_engine", |b| {
        let mut i = 0u64;
        b.iter(|| {
            let name = format!("tool_{i}");
            sandbox.precompile_tool(&name, black_box(&wasm)).unwrap();
            i += 1;
        });
    });
}

/// Benchmark has_tool lookup on a populated cache.
fn bench_has_tool_lookup(c: &mut Criterion) {
    let wasm = minimal_wasm_module();
    let mut sandbox = WasmtimeSandbox::new().unwrap();
    for i in 0..100 {
        sandbox
            .precompile_tool(&format!("tool_{i}"), &wasm)
            .unwrap();
    }

    c.bench_function("has_tool_lookup_100", |b| {
        b.iter(|| {
            let found = sandbox.has_tool(black_box("tool_50"));
            black_box(found);
        });
    });
}

criterion_group!(
    benches,
    bench_sandbox_new,
    bench_precompile_minimal,
    bench_precompile_with_memory,
    bench_precompile_with_function,
    bench_precompile_multiple_tools,
    bench_precompile_reuse_engine,
    bench_has_tool_lookup,
);
criterion_main!(benches);
