# Vredrs 1.0 Makefile

VREDRS ?= ./target/release/vredrs

.PHONY: build debug test clean run-tests

build:
	cargo build --release

debug:
	cargo build

test: run-tests
	cargo test --lib

run-tests:
	./tests/run_tests.sh $(VREDRS)

clean:
	cargo clean

bench:
	cargo bench

fmt:
	cargo fmt

clippy:
	cargo clippy -- -W clippy::all
