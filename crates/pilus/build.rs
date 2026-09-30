//! Stamps an optional preview version into the binary.
//!
//! `scripts/preview-feed.sh` sets `PILUS_BUILD_VERSION` when it packages a
//! preview payload, so the built app reports the same synthetic version the
//! feed manifest advertises. Stable builds leave it unset and fall back to the
//! Cargo package version.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    // Cargo re-runs this script, and rustc re-reads the stamp through
    // `include_str!`, whenever the variable changes — so stable and preview
    // builds restamp cleanly in either direction.
    println!("cargo:rerun-if-env-changed=PILUS_BUILD_VERSION");
    let version = env::var("PILUS_BUILD_VERSION")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_default();
    if let Some(out) = env::var_os("OUT_DIR") {
        let _ = fs::write(PathBuf::from(out).join("preview_version"), version);
    }
}
