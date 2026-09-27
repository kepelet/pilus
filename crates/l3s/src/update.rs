//! OTA release source and policy for `l3s`.
//!
//! The GUI never talks to GitHub or a feed directly. It drives
//! [`gui::updater::Updater`], which owns an [`UpdateEngine`] built here, so
//! the source selection (GitHub Releases vs. a static manifest) and the
//! protocol-compatibility policy stay in one testable place.
//!
//! Data flow: `update::engine()` → `Updater` entity (background executor) →
//! `L3sView` status badge → `cx.restart()` into the staged bundle.

use gpui_updater_core::{
    EngineConfig, GitHubSource, StaticManifestSource, UpdateEngine, UpdateSource, Verification,
    Version,
};

/// GitHub repo that publishes the `.dmg` releases.
pub const GITHUB_OWNER: &str = "RizRiyz";
/// GitHub repo that publishes the `.dmg` releases.
pub const GITHUB_REPO: &str = "l3s";

/// Name of the env var that overrides the feed with a static manifest URL.
///
/// Useful for staging (`https://dl.example.com/channels/beta/latest.json`)
/// and for local testing (`python3 -m http.server` + a hand-written manifest).
pub const FEED_URL_ENV: &str = "L3S_UPDATE_FEED_URL";

/// Minisign public key that verifies release artifacts.
///
/// `None` until the first signed release is published. While unset the engine
/// runs [`Verification::BestEffort`] (checksum when present, nothing when
/// not). Once set, switch the default policy to [`Verification::Signature`].
pub const MINISIGN_PUBLIC_KEY: Option<&str> = None;

/// Supported update channel. The channel selects the manifest path; the
/// GitHub path ignores it and always reads `releases/latest`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UpdateChannel {
    /// Production releases.
    #[default]
    Stable,
    /// Pre-release builds.
    Beta,
}

impl UpdateChannel {
    /// Parse `stable`/`beta`, falling back to stable.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "beta" | "pre" | "prerelease" | "pre-release" => Self::Beta,
            _ => Self::Stable,
        }
    }
}

/// What to check, and how strictly to verify it.
pub struct UpdateRequest {
    /// Release channel (matters for static-manifest feeds).
    pub channel: UpdateChannel,
    /// Explicit manifest URL; overrides the GitHub source when set.
    pub feed_url: Option<String>,
    /// Minisign public key; defaults to [`MINISIGN_PUBLIC_KEY`].
    pub minisign_key: Option<String>,
}

impl UpdateRequest {
    /// Build from settings: explicit feed URL wins, else GitHub.
    pub fn from_settings(channel: &str, feed_url: Option<String>) -> Self {
        let from_env = std::env::var(FEED_URL_ENV)
            .ok()
            .filter(|url| !url.trim().is_empty());
        Self {
            channel: UpdateChannel::parse(channel),
            feed_url: feed_url.filter(|url| !url.trim().is_empty()).or(from_env),
            minisign_key: MINISIGN_PUBLIC_KEY.map(str::to_string),
        }
    }

    /// Whether this check reads a static manifest instead of GitHub.
    pub fn uses_manifest(&self) -> bool {
        self.feed_url
            .as_ref()
            .is_some_and(|url| !url.trim().is_empty())
    }
}

/// The running app version, parsed from `Cargo.toml`.
pub fn current_version() -> Version {
    // Cargo guarantees a valid semver version string.
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version::new(0, 0, 0))
}

/// Arch substring the release asset name must contain.
///
/// Releases ship Apple Silicon only, so this is always `arm64`. An Intel Mac
/// matches nothing and gets a readable "no release asset" error instead of
/// a wrong binary.
pub fn arch_pattern() -> &'static str {
    "arm64"
}

/// Build the update source for a request.
///
/// GitHub needs no extra infrastructure: the workflow publishes
/// `l3s-*-macos-*.dmg` + `SHA256SUMS` (+ `.minisig` once signing lands) and
/// the client reads `releases/latest`. A feed URL switches to a static
/// manifest (R2/S3/any HTTPS host) with per-asset `os`/`arch`/`format` fields.
pub fn build_source(request: &UpdateRequest) -> Box<dyn UpdateSource> {
    if let Some(url) = request.feed_url.clone() {
        Box::new(
            StaticManifestSource::new(url)
                .os("macos")
                .arch(arch_normalized())
                .format("dmg"),
        )
    } else {
        let mut source = GitHubSource::new(GITHUB_OWNER, GITHUB_REPO)
            .asset_contains("macos")
            .asset_contains(arch_pattern())
            .asset_contains(".dmg")
            .with_checksums("SHA256SUMS");
        if request.channel == UpdateChannel::Beta {
            source = source.include_prereleases(true);
        }
        if request.minisign_key.is_some() {
            source = source.with_minisig();
        }
        Box::new(source)
    }
}

/// Arch value for manifest `arch` fields: Apple Silicon only (see above).
fn arch_normalized() -> &'static str {
    "arm64"
}

/// Verification policy for a request.
///
/// With no public key configured there is nothing to enforce, so stay
/// best-effort. The moment a key exists, fail closed on signature.
pub fn verification_for(request: &UpdateRequest) -> Verification {
    if request.minisign_key.is_some() {
        Verification::Signature
    } else {
        Verification::BestEffort
    }
}

/// Build a blocking [`UpdateEngine`] for a request.
pub fn engine(request: &UpdateRequest) -> UpdateEngine<Box<dyn UpdateSource>> {
    let mut config = EngineConfig::new(current_version()).verification(verification_for(request));
    if let Some(key) = request.minisign_key.clone() {
        config = config.minisign_public_key(key);
    }
    UpdateEngine::new(build_source(request), config)
}

/// One-line policy note shown in the update UI.
///
/// The daemon handshake already rejects a protocol mismatch with a readable
/// error, so OTA needs no separate protocol gate: a client that updated past
/// its daemon simply fails to attach until `luvus` is updated too.
pub const PROTOCOL_NOTE: &str = "If the update needs a newer daemon, update `luvus` as well.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_matches_cargo() {
        let version = current_version();
        assert_eq!(version.to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn channel_parses_leniently() {
        assert_eq!(UpdateChannel::parse("stable"), UpdateChannel::Stable);
        assert_eq!(UpdateChannel::parse("BETA"), UpdateChannel::Beta);
        assert_eq!(UpdateChannel::parse("pre-release"), UpdateChannel::Beta);
        assert_eq!(UpdateChannel::parse("nonsense"), UpdateChannel::Stable);
    }

    #[test]
    fn explicit_feed_url_wins() {
        let request = UpdateRequest::from_settings(
            "stable",
            Some("https://dl.example.com/stable/latest.json".to_string()),
        );
        assert!(request.uses_manifest());
    }

    #[test]
    fn unsigned_builds_stay_best_effort() {
        let request = UpdateRequest {
            channel: UpdateChannel::Stable,
            feed_url: None,
            minisign_key: None,
        };
        assert_eq!(verification_for(&request), Verification::BestEffort);
    }

    #[test]
    fn signed_builds_fail_closed() {
        let request = UpdateRequest {
            channel: UpdateChannel::Stable,
            feed_url: None,
            minisign_key: Some("RWQ…".to_string()),
        };
        assert_eq!(verification_for(&request), Verification::Signature);
    }
}
