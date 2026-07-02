//! Compilation benchmarks using criterion.
//!
//! Run with: `cargo bench`

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use vredrs_compiler::run_interpreter_source;

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
paste, fib(15)
paste, "\n"
"#;

const LIST_OPS: &str = r#"
set, nums, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
set, squares, [x * x for x in nums]
paste, sum(squares)
paste, "\n"
"#;

fn bench_lex(c: &mut Criterion) {
    c.bench_function("lex hello", |b| {
        b.iter(|| {
            let mut lexer = vredrs_compiler::compile_source(black_box(HELLO_WORLD));
            let _ = black_box(&mut lexer);
        })
    });
}

fn bench_parse(c: &mut Criterion) {
    c.bench_function("parse fib", |b| {
        b.iter(|| {
            // Parse-only would require exposing internals; use interpreter as proxy.
            let _ = run_interpreter_source(black_box(FIBONACCI), 0);
        })
    });
}

fn bench_interpret(c: &mut Criterion) {
    c.bench_function("interpret list_ops", |b| {
        b.iter(|| {
            let _ = run_interpreter_source(black_box(LIST_OPS), 0);
        })
    });
}

criterion_group!(benches, bench_lex, bench_parse, bench_interpret);
criterion_main!(benches);
