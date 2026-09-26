# Developer tasks. Run `just` to list them.
set shell := ["bash", "-c"]

default:
    @just --list

check:
    cargo check --workspace

test:
    cargo test --workspace

fmt:
    cargo fmt --all

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Attach to the default session.
run:
    cargo run -p l3s

# Connect once and print the first frame, without a window.
probe:
    cargo run -p l3s --no-default-features -- --probe

# Build the release binary.
build:
    cargo build --release -p l3s

# Assemble a macOS .app bundle with the icon and Info.plist.
bundle:
    ./scripts/bundle-macos.sh
