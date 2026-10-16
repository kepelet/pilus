//! Stamps an optional preview version and update channel into the binary.
//!
//! `scripts/preview-feed.sh` sets `PILUS_BUILD_VERSION` when it packages a
//! preview payload, so the built app reports the same synthetic version the
//! feed manifest advertises. Stable builds leave it unset and fall back to the
//! Cargo package version.
//!
//! It also sets `PILUS_BUILD_CHANNEL` there, which pins the app to that update
//! channel: the Nightly bundle sits beside stable and shares its settings file,
//! so the channel cannot be left to a setting either app can change.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    // Cargo re-runs this script, and rustc re-reads the stamp through
    // `include_str!`, whenever the variable changes — so stable and preview
    // builds restamp cleanly in either direction.
    println!("cargo:rerun-if-env-changed=PILUS_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=PILUS_BUILD_CHANNEL");
    if let Some(out) = env::var_os("OUT_DIR") {
        let out = PathBuf::from(out);
        let _ = fs::write(out.join("preview_version"), stamp("PILUS_BUILD_VERSION"));
        let _ = fs::write(out.join("build_channel"), stamp("PILUS_BUILD_CHANNEL"));
    }
}

/// A trimmed env value, or an empty string when unset.
fn stamp(name: &str) -> String {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}
