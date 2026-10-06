.PHONY: dev test lint

# dev needs cargo-watch: cargo install cargo-watch
dev:
	cargo watch -x run

test:
	cargo test

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
