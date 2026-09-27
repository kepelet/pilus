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

# Package a signed macOS DMG for OTA distribution (see docs/ota.md).
package-dmg:
    ./scripts/package-macos-dmg.sh

# Check the OTA release source without opening a window.
check-updates:
    cargo run -p l3s --no-default-features -- --check-updates
