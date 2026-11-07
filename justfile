default: help

# List the available recipes.
help:
    @just --list

# Build the workspace.
build:
    cargo build -j 2

# Type-check the workspace.
check:
    cargo check --workspace -j 2

# Run the test suite.
test:
    cargo test -j 2

# Run the application.
run *args:
    cargo run -p rchat-blazar -j 2 -- {{args}}

# Format the tree.
fmt:
    cargo fmt --all

# Verify formatting (gate).
fmt-check:
    cargo fmt --all --check

# Lint with warnings as errors (gate).
clippy:
    cargo clippy --workspace --all-targets -j 2 -- -D warnings

# Everything a change must pass before it lands.
gate: fmt-check clippy test
    @echo "gate green"
