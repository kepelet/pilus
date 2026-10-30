#!/bin/sh
# Builds libpilus_mobile.a for whatever SDK Xcode is targeting. Xcode runs
# build phases without the user profile, so cargo and rustup are put on PATH
# explicitly.

set -eu

export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

# Use the rustup proxy with the toolchain pinned in .mise.toml. Calling a
# toolchain's own cargo directly (or whatever `stable` a CI image preinstalls)
# skips the proxy and the iOS targets installed for the pinned version.
RUST_VERSION=$(sed -n 's/^rust = "\(.*\)"/\1/p' "$PROJECT_DIR/../.mise.toml")
if [ -n "$RUST_VERSION" ]; then
    export RUSTUP_TOOLCHAIN="$RUST_VERSION"
fi

case "$PLATFORM_NAME" in
    iphoneos)
        TARGET=aarch64-apple-ios
        ;;
    iphonesimulator)
        case "${ARCHS:-}" in
            *x86_64*) TARGET=x86_64-apple-ios ;;
            *) TARGET=aarch64-apple-ios-sim ;;
        esac
        ;;
    *)
        echo "build-rust.sh: unsupported platform ${PLATFORM_NAME:-?}" >&2
        exit 1
        ;;
esac

# Release (archive, TestFlight) links the optimized library; project.yml points
# LIBRARY_SEARCH_PATHS at the matching target subdirectory.
PROFILE=""
if [ "${CONFIGURATION:-Debug}" = "Release" ]; then
    PROFILE="--release"
fi

# No-op when the target is already installed; saves a "can't find crate for
# `core`" failure on a fresh machine.
rustup target add "$TARGET"

cargo build --manifest-path "$PROJECT_DIR/../Cargo.toml" \
    -p pilus-mobile --target "$TARGET" $PROFILE
