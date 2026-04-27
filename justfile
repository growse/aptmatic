tasks:
    @just --list

build:
    cargo build

fmt:
    cargo fmt

lint:
    cargo fmt --check
    cargo clippy -- -D warnings

test:
    cargo test

loc:
    cloc --exclude-dir=.idea --exclude-dir=target .
