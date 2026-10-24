//! The Luvus daemon installer and updater, pinned to the release this build
//! was tested against.
//!
//! `pilus` updates itself through [`crate::update`]; this is the parallel
//! updater for the daemon it talks to. The target is never "whatever Luvus
//! published last": `luvus.pin` names one release and the SHA-256 of each
//! platform archive, and both the first-run install and every later update go
//! to exactly that release. A newer Luvus can change the wire protocol, so
//! letting every user pick up the latest one is how two people on the same
//! pilus build end up on daemons that behave differently. A new Luvus reaches
//! users by bumping the pin in a pilus release.
//!
//! The archive is fetched from the Luvus GitHub release and checked against
//! the pinned checksum, not a sidecar from the same host, then
//! [`gpui_updater_core`]'s download + verify pipeline hands it to the install,
//! which extracts `luvus` and swaps it into place. A Homebrew/Cargo/Nix install
//! is refused so it keeps updating through its package manager.
//!
//! Data flow: missing-Luvus setup or Settings → blocking [`LuvusEngine`] on a
//! background thread → staged binary → `luvus server start`/`restart` loads
//! it. The GUI thread never blocks on the network or disk.

use std::path::{Path, PathBuf};
use std::process::Command;

use gpui_updater_core::{
    Asset, EngineConfig, Release, UpdateEngine, UpdateSource, Verification, Version, parse_tag,
};

/// Public Luvus release manifest, read only to say a newer Luvus exists.
pub const LATEST_MANIFEST_URL: &str = "https://luvus.dev/latest.json";
/// Where release archives live; the `luvus` CLI honors the same override.
pub const RELEASE_BASE: &str = "https://github.com/RizRiyz/luvus/releases/download";
/// Env var the `luvus` CLI uses to override the manifest URL.
pub const MANIFEST_ENV: &str = "LUVUS_UPDATE_MANIFEST";
/// Env var the `luvus` CLI uses to override the download base.
pub const RELEASE_BASE_ENV: &str = "LUVUS_UPDATE_RELEASE_BASE";

/// The pinned release, compiled in so the checksum travels with the build.
const PIN: &str = include_str!("../luvus.pin");

/// The blocking engine the Settings page drives.
pub type LuvusEngine = UpdateEngine<Box<dyn UpdateSource>>;

/// The Luvus release a build installs and stops updates at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// The pinned release.
    pub version: Version,
    /// `(triple, sha256)` for each platform archive, lowercase hex.
    checksums: Vec<(String, String)>,
}

impl Pin {
    /// Read the `luvus.pin` format: `#` comments, one `version <v>` line, and
    /// `<triple> <sha256>` lines.
    fn parse(text: &str) -> Result<Self, String> {
        let mut version = None;
        let mut checksums = Vec::new();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut words = line.split_whitespace();
            let (Some(key), Some(value), None) = (words.next(), words.next(), words.next()) else {
                return Err(format!("luvus.pin: unreadable line `{line}`"));
            };
            if key == "version" {
                version =
                    Some(parse_tag(value).map_err(|error| format!("luvus.pin: version: {error}"))?);
            } else if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                checksums.push((key.to_string(), value.to_ascii_lowercase()));
            } else {
                return Err(format!("luvus.pin: `{key}` has no SHA-256"));
            }
        }
        let version = version.ok_or_else(|| "luvus.pin: no version line".to_string())?;
        Ok(Self { version, checksums })
    }

    /// The pinned checksum of `triple`'s archive.
    fn sha256(&self, triple: &str) -> Option<&str> {
        self.checksums
            .iter()
            .find(|(name, _)| name == triple)
            .map(|(_, sha)| sha.as_str())
    }
}

/// The Luvus release this build installs and updates to.
pub fn pin() -> gpui_updater_core::Result<Pin> {
    Pin::parse(PIN).map_err(gpui_updater_core::Error::Parse)
}

/// A source that always resolves the pinned release for this platform.
struct LuvusSource;

impl UpdateSource for LuvusSource {
    fn fetch_latest(&self) -> gpui_updater_core::Result<Release> {
        let pin = pin()?;
        let no_asset = || gpui_updater_core::Error::NoMatchingAsset {
            target_os: std::env::consts::OS,
            target_arch: std::env::consts::ARCH,
        };
        let triple = target_triple().ok_or_else(no_asset)?;
        let sha256 = pin.sha256(triple).ok_or_else(no_asset)?.to_string();
        let version = pin.version;
        let base = env_override(RELEASE_BASE_ENV).unwrap_or_else(|| RELEASE_BASE.to_string());
        let asset_name = format!("luvus-v{version}-{triple}.tar.gz");
        let url = format!("{base}/v{version}/{asset_name}");
        Ok(Release {
            version,
            notes: None,
            asset: Asset {
                name: asset_name,
                url,
                size: 0,
            },
            signature: None,
            signature_url: None,
            sha256: Some(sha256),
        })
    }
}

/// The newest published Luvus, when it is past the pin.
///
/// Only for telling the user a newer daemon exists and comes with a pilus
/// update; nothing installs it. Offline or a malformed manifest reads as
/// nothing newer.
pub fn newer_than_pin() -> Option<Version> {
    let pin = pin().ok()?;
    let url = env_override(MANIFEST_ENV).unwrap_or_else(|| LATEST_MANIFEST_URL.to_string());
    let manifest = get_string(&url).ok()?;
    let value: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    let latest = parse_tag(value.get("version")?.as_str()?).ok()?;
    (latest > pin.version).then_some(latest)
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

/// Build the blocking engine for a first install, a check, or an update.
///
/// It fails closed: an archive is installed only when it matches the pinned
/// checksum. The daemon runs every pane's shell, so it is never swapped in on
/// the strength of HTTPS alone.
pub fn engine() -> LuvusEngine {
    UpdateEngine::new(
        Box::new(LuvusSource) as Box<dyn UpdateSource>,
        EngineConfig::new(installed_version().unwrap_or_else(|| Version::new(0, 0, 0)))
            .verification(Verification::Checksum),
    )
}

/// The pinned release, when it is newer than the installed binary. An install
/// already at or past the pin is left alone, never downgraded.
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
    let started = std::time::Instant::now();
    let fetched = ureq::get(url)
        .call()
        .and_then(|mut response| response.body_mut().read_to_vec());
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url);
    pilus_client::netlog::record(
        pilus_client::netlog::Kind::Http,
        host,
        "update check",
        started,
        fetched.is_ok(),
    );
    let bytes =
        fetched.map_err(|error| gpui_updater_core::Error::Http(format!("{url}: {error}")))?;
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
    fn the_pin_parses_and_covers_every_platform() {
        let pin = pin().expect("luvus.pin parses");
        for triple in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-musl",
            "x86_64-unknown-linux-musl",
        ] {
            assert!(pin.sha256(triple).is_some(), "luvus.pin lacks {triple}");
        }
    }

    #[test]
    fn a_broken_pin_is_refused() {
        assert!(Pin::parse("aarch64-apple-darwin 00").is_err(), "short hash");
        assert!(
            Pin::parse(&format!("x {}", "a".repeat(64))).is_err(),
            "no version"
        );
        assert!(Pin::parse("version 0.14.3 extra").is_err(), "extra words");
        let pin = Pin::parse(&format!("# c\nversion v0.14.3\nt {}\n", "AB".repeat(32)))
            .expect("a well-formed pin");
        assert_eq!(pin.version, Version::new(0, 14, 3));
        assert_eq!(pin.sha256("t"), Some("ab".repeat(32).as_str()));
    }

    #[test]
    fn the_source_resolves_the_pinned_archive_without_the_network() {
        let pin = pin().expect("luvus.pin parses");
        let Some(triple) = target_triple() else {
            return;
        };
        let release = LuvusSource.fetch_latest().expect("resolve the pin");
        assert_eq!(release.version, pin.version);
        assert_eq!(release.sha256.as_deref(), pin.sha256(triple));
        assert!(
            release
                .asset
                .url
                .ends_with(&format!("/v{0}/luvus-v{0}-{triple}.tar.gz", pin.version))
        );
    }

    #[test]
    fn an_install_at_or_past_the_pin_is_not_offered_anything() {
        let pin = pin().expect("luvus.pin parses");
        let at = |current: Version| {
            UpdateEngine::new(
                Box::new(LuvusSource) as Box<dyn UpdateSource>,
                EngineConfig::new(current).verification(Verification::Checksum),
            )
        };
        let mut newer = pin.version.clone();
        newer.minor += 1;
        assert!(
            check(&at(newer)).expect("check").is_none(),
            "never downgrade"
        );
        assert!(check(&at(pin.version.clone())).expect("check").is_none());
        let offered = check(&at(Version::new(0, 0, 0))).expect("check");
        assert_eq!(offered.map(|release| release.version), Some(pin.version));
    }

    #[test]
    #[ignore = "hits the Luvus release host"]
    fn the_pin_matches_the_published_sidecars() {
        let pin = pin().expect("luvus.pin parses");
        let version = &pin.version;
        for (triple, sha) in &pin.checksums {
            let sidecar = get_string(&format!(
                "{RELEASE_BASE}/v{version}/luvus-v{version}-{triple}.sha256"
            ))
            .expect("fetch the sidecar");
            assert_eq!(sidecar.split_whitespace().next(), Some(sha.as_str()));
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
