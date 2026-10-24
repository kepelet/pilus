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
    cargo build -p pilus-agent
    cargo run -p pilus

# Connect once and print the first frame, without a window.
probe:
    cargo run -p pilus --no-default-features -- --probe

# Build the release binary.
build:
    cargo build --release -p pilus -p pilus-agent

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

# Write the channel manifest (`stable` or `preview`) for the packaged DMG into
# target/release/<channel>.json. The base URL is where the DMG will be hosted;
# upload the JSON as releases.kepelet.com/pilus/<channel>.json beside it.
manifest channel base_url:
    @case "{{channel}}" in stable|preview) ;; *) echo "channel must be stable or preview" >&2; exit 1;; esac
    ./scripts/write-manifest.sh "target/release/{{channel}}.json" "{{base_url}}" \
        "target/release/pilus-v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)-macos-arm64.dmg"

# Build the preview payload against the public releases URL, without uploading.
release-preview-build:
    PILUS_PREVIEW_BASE_URL="https://releases.kepelet.com/pilus/preview" ./scripts/preview-feed.sh

# Publish a preview build to releases.kepelet.com: the `preview` channel serves
# it to clients (see docs/ota.md). Needs `bunx wrangler login` in infra/ and
# PILUS_RELEASES_BUCKET (env or infra/.env).
release-preview: release-preview-build
    ./scripts/publish-release.sh preview

# Publish the stable release in the Cargo.toml version to releases.kepelet.com.
# Rebuilds the DMG (a preview build reuses the same filename), then refuses to
# replace a version that is already live: bump Cargo.toml first.
release-stable: package-dmg
    just manifest stable https://releases.kepelet.com/pilus
    ./scripts/publish-release.sh stable

# Serve the preview payload on the port the preview channel expects.
serve-preview:
    python3 -m http.server 8000 --directory target/release/preview-feed

# Pin the Luvus release pilus installs and caps daemon updates at, after
# trying pilus against it. Downloads every platform archive to hash it.
pin-luvus version:
    ./scripts/pin-luvus.sh {{version}}

# Check the OTA release source without opening a window.
check-updates *args:
    cargo run -p pilus --no-default-features -- --check-updates {{args}}

# Build the landing page and docs into site/dist.
site-build:
    cd site && bun install --frozen-lockfile && bun run build

# Run the landing page and docs locally with live reload.
site-dev:
    cd site && bun install --frozen-lockfile && bun run dev

# Preview what `infra-deploy` would change on Cloudflare (site + releases bucket).
infra-plan: site-build
    cd infra && bun install --frozen-lockfile && dotenvx run -- bunx alchemy plan --stage prod

# Apply infra/alchemy.run.ts: the usepilus.dev site and releases.kepelet.com.
infra-deploy: site-build
    cd infra && bun install --frozen-lockfile && dotenvx run -- bunx alchemy deploy --stage prod
