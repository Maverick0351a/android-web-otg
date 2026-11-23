.PHONY: build release test lint fmt run clean

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

lint:
	cargo clippy --all-targets -- -D warnings

fmt:
	cargo fmt --check

run:
	cargo run

clean:
	cargo clean
