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
    cargo run -p pilus

# Connect once and print the first frame, without a window.
probe:
    cargo run -p pilus --no-default-features -- --probe

# Build the release binary.
build:
    cargo build --release -p pilus

# Assemble a macOS .app bundle with the icon and Info.plist.
bundle:
    ./scripts/bundle-macos.sh

# Package a signed macOS DMG for OTA distribution (see docs/ota.md).
package-dmg:
    ./scripts/package-macos-dmg.sh

# Build the preview payload (DMG + manifest) the preview channel reads locally.
# The base URL is baked into the manifest's asset URLs, so it must be an address
# clients can reach: pass a Tailscale/LAN host to update another machine.
preview-feed base_url="http://127.0.0.1:8000":
    PILUS_PREVIEW_BASE_URL="{{base_url}}" ./scripts/preview-feed.sh

# Serve the preview payload on the port the preview channel expects.
serve-preview:
    python3 -m http.server 8000 --directory target/release/preview-feed

# Check the OTA release source without opening a window.
check-updates *args:
    cargo run -p pilus --no-default-features -- --check-updates {{args}}
