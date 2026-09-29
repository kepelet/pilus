//! OTA release source and policy for `l3s`.
//!
//! The GUI never talks to the release host directly. It drives
//! [`gui::updater::Updater`], which owns an [`UpdateEngine`] built here, so the
//! source selection (channel manifest vs. explicit feed URL) and the
//! protocol-compatibility policy stay in one testable place. The headless
//! `l3s --check-updates` path builds the same engine, so a broken feed fails
//! there before it fails in a window.
//!
//! Data flow: `update::engine()` → `Updater` entity (background executor) →
//! `L3sView` status badge → `cx.restart()` into the staged bundle.

use gpui_updater_core::{
    EngineConfig, StaticManifestSource, UpdateEngine, UpdateSource, Verification, Version,
};

/// Manifest the stable channel reads.
pub const STABLE_FEED_URL: &str = "https://releases.kepelet.com/l3s/stable.json";

/// Manifest the preview/nightly channels read.
pub const PREVIEW_FEED_URL: &str = "https://releases.kepelet.com/l3s/preview.json";

/// Name of the env var that overrides the update channel.
///
/// Lets a dev session run `nightly` without editing `settings.json`, e.g.
/// `L3S_UPDATE_CHANNEL=preview cargo run -p l3s`.
pub const CHANNEL_ENV: &str = "L3S_UPDATE_CHANNEL";

/// Name of the env var that overrides the feed with a static manifest URL.
///
/// Useful for staging and for local testing
/// (`python3 -m http.server 8000 --directory /tmp/feed`).
pub const FEED_URL_ENV: &str = "L3S_UPDATE_FEED_URL";

/// Minisign public key that verifies release artifacts.
///
/// `None` until the first signed release is published. While unset the engine
/// runs [`Verification::BestEffort`] (checksum when present, nothing when
/// not). Once set, switch the default policy to [`Verification::Signature`].
pub const MINISIGN_PUBLIC_KEY: Option<&str> = None;

/// Supported update channel.
///
/// Each channel reads a manifest on the release host; `beta`, `nightly`,
/// `canary`, and `preview` all map to the preview manifest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UpdateChannel {
    /// Production releases from [`STABLE_FEED_URL`].
    #[default]
    Stable,
    /// Pre-release builds from [`PREVIEW_FEED_URL`].
    Preview,
}

impl UpdateChannel {
    /// Parse a channel name, falling back to stable.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "preview" | "nightly" | "beta" | "canary" | "dev" | "pre" | "prerelease"
            | "pre-release" => Self::Preview,
            _ => Self::Stable,
        }
    }

    /// Canonical name written to settings and shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Preview => "preview",
        }
    }

    /// Manifest used when no explicit URL is configured.
    pub fn default_feed_url(self) -> &'static str {
        match self {
            Self::Stable => STABLE_FEED_URL,
            Self::Preview => PREVIEW_FEED_URL,
        }
    }
}

/// What to check, and how strictly to verify it.
pub struct UpdateRequest {
    /// Release channel; selects the default source.
    pub channel: UpdateChannel,
    /// Explicit manifest URL; `None` uses the channel default.
    pub feed_url: Option<String>,
    /// Minisign public key; defaults to [`MINISIGN_PUBLIC_KEY`].
    pub minisign_key: Option<String>,
}

impl UpdateRequest {
    /// Build from persisted settings, then apply the env overrides.
    ///
    /// For each knob the env var wins: `L3S_UPDATE_CHANNEL` over
    /// `update_channel`, `L3S_UPDATE_FEED_URL` over `update_feed_url`. A blank
    /// value is ignored.
    pub fn from_settings(channel: &str, feed_url: Option<String>) -> Self {
        let channel = std::env::var(CHANNEL_ENV)
            .ok()
            .filter(|raw| !raw.trim().is_empty())
            .map_or_else(
                || UpdateChannel::parse(channel),
                |raw| UpdateChannel::parse(&raw),
            );
        let feed_url = std::env::var(FEED_URL_ENV)
            .ok()
            .filter(|url| !url.trim().is_empty())
            .or_else(|| feed_url.filter(|url| !url.trim().is_empty()));
        Self {
            channel,
            feed_url,
            minisign_key: MINISIGN_PUBLIC_KEY.map(str::to_string),
        }
    }

    /// The URL the next check reads: explicit override, else the channel
    /// default.
    pub fn resolved_feed_url(&self) -> String {
        self.feed_url
            .clone()
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| self.channel.default_feed_url().to_string())
    }
}

/// The running app version.
///
/// `scripts/preview-feed.sh` stamps the synthetic preview version into the
/// binary through `build.rs`, so an installed preview reports exactly what its
/// feed manifest advertises and a re-check of the same feed settles as up to
/// date. Stable builds embed nothing and fall back to the Cargo version.
pub fn current_version() -> Version {
    let raw = build_version().unwrap_or(env!("CARGO_PKG_VERSION"));
    // Cargo guarantees a valid semver version; a bad stamp should not panic a
    // release build.
    Version::parse(raw).unwrap_or(Version::new(0, 0, 0))
}

/// The build-time preview stamp, when one was set while packaging.
fn build_version() -> Option<&'static str> {
    let raw = include_str!(concat!(env!("OUT_DIR"), "/preview_version")).trim();
    (!raw.is_empty()).then_some(raw)
}

/// One-line description of where a request reads releases from.
///
/// Printed by `--check-updates`, so a misconfigured channel is visible before
/// a check runs.
pub fn describe_source(request: &UpdateRequest) -> String {
    format!("manifest {}", request.resolved_feed_url())
}

/// Build the update source for a request.
///
/// Each channel reads its manifest on the release host, or the explicit feed
/// URL when one is configured, with per-asset `os`/`arch`/`format` fields.
/// Releases ship Apple Silicon only, so `arch` is always `arm64`: an Intel Mac
/// matches nothing and gets a readable "no release asset" error instead of a
/// wrong binary.
pub fn build_source(request: &UpdateRequest) -> Box<dyn UpdateSource> {
    Box::new(
        StaticManifestSource::new(request.resolved_feed_url())
            .os("macos")
            .arch("arm64")
            .format("dmg"),
    )
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
    fn current_version_uses_the_build_stamp_when_present() {
        let expected = build_version().unwrap_or(env!("CARGO_PKG_VERSION"));
        assert_eq!(current_version().to_string(), expected);
    }

    #[test]
    fn channel_parses_aliases_leniently() {
        for preview in ["preview", "BETA", "nightly", "canary", "pre-release"] {
            assert_eq!(
                UpdateChannel::parse(preview),
                UpdateChannel::Preview,
                "{preview}"
            );
        }
        assert_eq!(UpdateChannel::parse("stable"), UpdateChannel::Stable);
        assert_eq!(UpdateChannel::parse("nonsense"), UpdateChannel::Stable);
        assert_eq!(UpdateChannel::parse("  "), UpdateChannel::Stable);
    }

    #[test]
    fn stable_defaults_to_the_stable_manifest() {
        let request = UpdateRequest {
            channel: UpdateChannel::Stable,
            feed_url: None,
            minisign_key: None,
        };
        assert_eq!(request.resolved_feed_url(), STABLE_FEED_URL);
        assert_eq!(
            describe_source(&request),
            format!("manifest {STABLE_FEED_URL}")
        );
    }

    #[test]
    fn preview_defaults_to_the_preview_manifest() {
        let request = UpdateRequest {
            channel: UpdateChannel::Preview,
            feed_url: None,
            minisign_key: None,
        };
        assert_eq!(request.resolved_feed_url(), PREVIEW_FEED_URL);
    }

    #[test]
    fn explicit_feed_url_wins_over_the_channel_default() {
        let request = UpdateRequest {
            channel: UpdateChannel::Preview,
            feed_url: Some("https://dl.example.com/preview/latest.json".to_string()),
            minisign_key: None,
        };
        assert_eq!(
            request.resolved_feed_url(),
            "https://dl.example.com/preview/latest.json"
        );
    }

    #[test]
    fn blank_settings_feed_falls_back_to_the_channel_default() {
        let request = UpdateRequest::from_settings("preview", Some("   ".to_string()));
        assert_eq!(request.resolved_feed_url(), PREVIEW_FEED_URL);
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
