default:
    @just --list

build:
    cargo build --release --locked

check:
    cargo fmt --all --check
    cargo clippy --all-targets --locked -- -D warnings

test:
    cargo test --locked

audit:
    cargo audit

fmt:
    cargo fmt --all

pre-push: check test audit
