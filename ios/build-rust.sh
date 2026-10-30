#!/bin/sh
# Builds libpilus_mobile.a for whatever SDK Xcode is targeting. Xcode runs
# build phases without the user profile, so cargo and rustup are put on PATH
# explicitly.

set -eu

export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"

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

cargo build --manifest-path "$PROJECT_DIR/../Cargo.toml" \
    -p pilus-mobile --target "$TARGET" $PROFILE
