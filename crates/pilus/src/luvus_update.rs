//! The Luvus daemon updater: check the public release manifest and install the
//! platform archive.
//!
//! `pilus` updates itself through [`crate::update`]; this is the parallel
//! updater for the daemon it talks to. It reads
//! `https://luvus.dev/latest.json`, resolves the platform `.tar.gz` and its
//! `.sha256` sidecar, and reuses [`gpui_updater_core`]'s download + verify
//! pipeline. The install extracts the `luvus` binary and swaps it into place,
//! refusing a Homebrew/Cargo/Nix install so those keep updating through their
//! package manager.
//!
//! Data flow: Settings → blocking [`LuvusEngine`] on a background thread →
//! staged binary → `luvus server restart` loads it. The GUI thread never
//! blocks on the network or disk.

use std::path::{Path, PathBuf};
use std::process::Command;

use gpui_updater_core::{
    Asset, EngineConfig, Release, UpdateEngine, UpdateSource, Verification, Version, parse_tag,
};

/// Public Luvus release manifest.
pub const LATEST_MANIFEST_URL: &str = "https://luvus.dev/latest.json";
/// Where release archives live; the `luvus` CLI honors the same override.
pub const RELEASE_BASE: &str = "https://github.com/RizRiyz/luvus/releases/download";
/// Env var the `luvus` CLI uses to override the manifest URL.
pub const MANIFEST_ENV: &str = "LUVUS_UPDATE_MANIFEST";
/// Env var the `luvus` CLI uses to override the download base.
pub const RELEASE_BASE_ENV: &str = "LUVUS_UPDATE_RELEASE_BASE";

/// The blocking engine the Settings page drives.
pub type LuvusEngine = UpdateEngine<Box<dyn UpdateSource>>;

/// A source that resolves the Luvus release archive for this platform.
struct LuvusSource;

impl UpdateSource for LuvusSource {
    fn fetch_latest(&self) -> gpui_updater_core::Result<Release> {
        let manifest_url =
            env_override(MANIFEST_ENV).unwrap_or_else(|| LATEST_MANIFEST_URL.to_string());
        let manifest = get_string(&manifest_url)?;
        let value: serde_json::Value = serde_json::from_str(&manifest)
            .map_err(|error| gpui_updater_core::Error::Parse(error.to_string()))?;
        let version_raw = value
            .get("version")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                gpui_updater_core::Error::Parse("manifest has no version".to_string())
            })?;
        let version = parse_tag(version_raw)?;
        let notes = value
            .get("notes")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let triple = target_triple().ok_or(gpui_updater_core::Error::NoMatchingAsset {
            target_os: std::env::consts::OS,
            target_arch: std::env::consts::ARCH,
        })?;
        let base = env_override(RELEASE_BASE_ENV).unwrap_or_else(|| RELEASE_BASE.to_string());
        let stem = format!("luvus-v{version}-{triple}");
        let asset_name = format!("{stem}.tar.gz");
        let url = format!("{base}/v{version}/{asset_name}");
        // The sidecar is `shasum -a 256` output: `<hex>  <archive name>`.
        let sha256 = get_string(&format!("{base}/v{version}/{stem}.sha256"))?
            .split_whitespace()
            .next()
            .map(str::to_string);
        Ok(Release {
            version,
            notes,
            asset: Asset {
                name: asset_name,
                url,
                size: 0,
            },
            signature: None,
            signature_url: None,
            sha256,
        })
    }
}

/// The installed `luvus` binary's version, from `luvus --version`.
///
/// A binary the CLI cannot identify still parses here; the version is what the
/// check compares, not the install channel.
pub fn installed_version() -> Option<Version> {
    let binary = pilus_client::luvus_binary()?;
    let output = Command::new(binary).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .find_map(|token| Version::parse(token.trim_start_matches('v')).ok())
}

/// Build the blocking engine for a check or update.
///
/// `BestEffort` is enough: the download is checked against the published
/// sidecar whenever one exists, and the archive is served over HTTPS.
pub fn engine() -> LuvusEngine {
    engine_with(Verification::BestEffort)
}

/// The engine for a first install, which fails closed on a checksum mismatch.
///
/// An update replaces a binary the user already runs; a first install has
/// nothing else vouching for what lands in `~/.local/bin`.
pub fn first_install_engine() -> LuvusEngine {
    engine_with(Verification::Checksum)
}

fn engine_with(verification: Verification) -> LuvusEngine {
    UpdateEngine::new(
        Box::new(LuvusSource) as Box<dyn UpdateSource>,
        EngineConfig::new(installed_version().unwrap_or_else(|| Version::new(0, 0, 0)))
            .verification(verification),
    )
}

/// The latest release, when it is newer than the installed binary.
pub fn check(engine: &LuvusEngine) -> gpui_updater_core::Result<Option<Release>> {
    engine.check()
}

/// Download, verify, and install the release, returning the new binary path.
pub fn install(
    engine: &LuvusEngine,
    release: &Release,
    progress: impl FnMut(u64, Option<u64>),
) -> gpui_updater_core::Result<PathBuf> {
    let artifact = engine.download(release, progress)?;
    let target = install_target()?;
    extract_and_replace(&artifact, &target)?;
    Ok(target)
}

/// Where a self-managed install should land.
///
/// A package-manager install is refused so it keeps updating through its
/// manager. Everything else lands in place when it is a real install, or at
/// `~/.local/bin/luvus` when the only `luvus` is the remote-provisioning cache
/// (which is versioned and must not be replaced in place).
fn install_target() -> gpui_updater_core::Result<PathBuf> {
    if let Some(binary) = pilus_client::luvus_binary() {
        // Resolve a package-manager symlink (Homebrew) before deciding, so a
        // `/usr/local/bin/luvus` link is not replaced with a regular file.
        let resolved = std::fs::canonicalize(&binary).unwrap_or_else(|_| binary.clone());
        let path = resolved.display().to_string();
        if let Some(manager) = managed_by(&path) {
            return Err(gpui_updater_core::Error::Install(format!(
                "this luvus is managed by {manager}; update it with that package manager"
            )));
        }
        if !path.contains("/luvus/remote/") {
            return Ok(binary);
        }
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| gpui_updater_core::Error::Install("HOME is not set".to_string()))?;
    Ok(PathBuf::from(home).join(".local/bin/luvus"))
}

/// The package manager that owns `path`, when one does.
fn managed_by(path: &str) -> Option<&'static str> {
    const MANAGERS: &[(&str, &str)] = &[
        ("/opt/homebrew/", "Homebrew"),
        ("/usr/local/Cellar/", "Homebrew"),
        ("/home/linuxbrew/", "Homebrew"),
        ("/.cargo/bin/", "Cargo"),
        ("/.nix-profile/", "Nix"),
        ("/nix/store/", "Nix"),
    ];
    MANAGERS
        .iter()
        .find(|(prefix, _)| path.contains(prefix))
        .map(|(_, name)| *name)
}

/// Extract `luvus` from the archive and atomically replace `target`.
fn extract_and_replace(archive: &Path, target: &Path) -> gpui_updater_core::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        gpui_updater_core::Error::Install("install path has no parent".to_string())
    })?;
    std::fs::create_dir_all(parent)?;
    let staging = unique_dir(parent, "pilus-luvus")?;
    let result = (|| {
        let output = Command::new("tar")
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(&staging)
            .output()?;
        if !output.status.success() {
            return Err(gpui_updater_core::Error::Install(format!(
                "tar extract failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let extracted = staging.join("luvus");
        if !extracted.is_file() {
            return Err(gpui_updater_core::Error::Install(
                "archive did not contain `luvus`".to_string(),
            ));
        }
        let temp = parent.join(format!(".luvus.new-{}", std::process::id()));
        std::fs::copy(&extracted, &temp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&temp, target)?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// A unique staging directory on the same volume as `parent`.
fn unique_dir(parent: &Path, prefix: &str) -> gpui_updater_core::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = parent.join(format!(".{prefix}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The release triple for this build.
fn target_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        _ => None,
    }
}

/// A non-blank environment override, if one is set.
fn env_override(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Fetch a small text body.
fn get_string(url: &str) -> gpui_updater_core::Result<String> {
    let mut response = ureq::get(url)
        .call()
        .map_err(|error| gpui_updater_core::Error::Http(format!("{url}: {error}")))?;
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|error| gpui_updater_core::Error::Http(format!("{url}: {error}")))?;
    String::from_utf8(bytes).map_err(|error| gpui_updater_core::Error::Parse(error.to_string()))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn target_triple_covers_macos_and_linux() {
        // The running host is one of these; the point is that neither arm is
        // `None` on a supported platform.
        if cfg!(target_os = "macos") {
            assert!(target_triple().is_some());
        }
    }

    #[test]
    fn managed_by_flags_package_prefixes() {
        assert_eq!(managed_by("/opt/homebrew/bin/luvus"), Some("Homebrew"));
        assert_eq!(managed_by("/Users/x/.cargo/bin/luvus"), Some("Cargo"));
        assert_eq!(managed_by("/nix/store/abc/bin/luvus"), Some("Nix"));
        assert_eq!(managed_by("/Users/x/.local/bin/luvus"), None);
        assert_eq!(
            managed_by("/Users/x/.local/share/luvus/remote/v1/luvus"),
            None
        );
    }

    #[test]
    #[ignore = "hits the public Luvus release host"]
    fn checks_the_public_manifest() {
        let engine = engine();
        // Either an available release or up to date; both prove the manifest,
        // platform asset, and sidecar all resolved.
        if let Some(release) = check(&engine).expect("check the manifest") {
            assert!(!release.asset.name.is_empty());
            assert!(release.sha256.is_some(), "a sidecar checksum is published");
        }
    }

    #[test]
    fn extracts_luvus_from_a_tar_gz() {
        let dir = std::env::temp_dir().join(format!("pilus-luvus-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let payload = dir.join("payload");
        std::fs::create_dir_all(&payload).expect("payload dir");
        std::fs::write(payload.join("luvus"), b"new binary").expect("write payload");
        let archive = dir.join("luvus.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&payload)
            .arg("luvus")
            .status()
            .expect("tar");
        assert!(status.success());
        let target = dir.join("bin/luvus");
        extract_and_replace(&archive, &target).expect("install");
        assert_eq!(std::fs::read(&target).expect("read"), b"new binary");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
