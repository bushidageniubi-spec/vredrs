//! Compilation & execution benchmarks using criterion.
//!
//! Run with: `cargo bench`
//!
//! These benchmarks measure the bytecode VM pipeline (lex → parse →
//! compile → execute) end-to-end. The `interpret` benches use the VM.

use criterion::{black_box, criterion_group, criterion_main, Criterion};

const HELLO_WORLD: &str = r#"
paste, "Hello, World!\n"
set, x, 42
paste, x
paste, "\n"
"#;

const FIBONACCI: &str = r#"
fn, fib(n)
    if, n < 2
        return, n
    /end
    return, fib(n - 1) + fib(n - 2)
/end
paste, fib(20)
paste, "\n"
"#;

const LIST_OPS: &str = r#"
set, nums, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
set, total, 0
for, x, in, nums
    set, total, total + x * x
/end
paste, total
paste, "\n"
"#;

fn bench_lex(c: &mut Criterion) {
    c.bench_function("lex hello", |b| {
        b.iter(|| {
            let result = vredrs_compiler::compile_source(black_box(HELLO_WORLD));
            let _ = black_box(result);
        })
    });
}

fn bench_vm_fib(c: &mut Criterion) {
    c.bench_function("vm fib(20)", |b| {
        b.iter(|| {
            let _ = vredrs_compiler::run_bytecode_vm_source(black_box(FIBONACCI));
        })
    });
}

fn bench_vm_list(c: &mut Criterion) {
    c.bench_function("vm list_ops", |b| {
        b.iter(|| {
            let _ = vredrs_compiler::run_bytecode_vm_source(black_box(LIST_OPS));
        })
    });
}

criterion_group!(benches, bench_lex, bench_vm_fib, bench_vm_list);
criterion_main!(benches);
