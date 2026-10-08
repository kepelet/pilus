//! The relay choice stored under `"relay"` in `~/.pilus/settings.json`.
//!
//! The window owns that file and rewrites it from its typed `Settings`, so the
//! relay block has to be a real field there or a save would drop it. The
//! agent reads the same file on its own, through [`RelaySettings::load`],
//! without depending on the window crate.

use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};

/// Where the host endpoint dials out, and how it proves it may.
///
/// A managed relay needs both halves: `url` says where it is, `secret` (the
/// Iroh Services project API secret) lets the endpoint mint the relay token the
/// server checks. A secret with no URL keeps the n0 public relays.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelaySettings {
    /// Iroh Services project API secret.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Dedicated or self-hosted relay URLs; empty uses the n0 public relays.
    /// A single string is accepted for the one-relay case.
    #[serde(
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub url: Vec<String>,
}

// The secret must never reach a log, so Debug names only whether it is set.
impl std::fmt::Debug for RelaySettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelaySettings")
            .field("url", &self.url)
            .field("secret", &self.secret.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl RelaySettings {
    /// Read the `relay` block of `<root>/settings.json`.
    ///
    /// A missing file or block is the default. A file that exists but cannot be
    /// read or parsed is an error, so a typo does not silently fall back to the
    /// public relays the user meant to replace.
    pub fn load(root: &Path) -> Result<Self, String> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct File {
            relay: RelaySettings,
        }

        let path = root.join("settings.json");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("could not read {}: {error}", path.display())),
        };
        let file: File = serde_json::from_str(&text)
            .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
        Ok(file.relay.cleaned())
    }

    /// Split what a user typed into relay URLs: comma or whitespace separated,
    /// each an `http(s)` URL. The agent only learns a URL is bad when it binds,
    /// which for a background service is out of sight, so the window checks
    /// here first.
    pub fn parse_urls(text: &str) -> Result<Vec<String>, String> {
        let urls: Vec<String> = text
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|url| !url.is_empty())
            .map(str::to_string)
            .collect();
        match urls
            .iter()
            .find(|url| !url.starts_with("https://") && !url.starts_with("http://"))
        {
            Some(bad) => Err(format!("`{bad}` is not an http(s) URL")),
            None => Ok(urls),
        }
    }

    /// Drop blank entries, so `"url": [""]` and `"secret": ""` mean "unset".
    fn cleaned(mut self) -> Self {
        self.url = self
            .url
            .into_iter()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
            .collect();
        self.secret = self
            .secret
            .map(|secret| secret.trim().to_string())
            .filter(|secret| !secret.is_empty());
        self
    }
}

fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Urls {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Urls::deserialize(deserializer)? {
        Urls::One(url) => vec![url],
        Urls::Many(urls) => urls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str, settings: Option<&str>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-relay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        if let Some(text) = settings {
            std::fs::write(dir.join("settings.json"), text).expect("write");
        }
        dir
    }

    #[test]
    fn reads_the_relay_block_and_ignores_the_rest() {
        let dir = root(
            "block",
            Some(
                r#"{"terminal_font":"Menlo","relay":{"secret":"abc","url":["https://a.example","https://b.example"]}}"#,
            ),
        );
        let relay = RelaySettings::load(&dir).expect("load");
        assert_eq!(relay.secret.as_deref(), Some("abc"));
        assert_eq!(relay.url, ["https://a.example", "https://b.example"]);
    }

    #[test]
    fn blank_entries_mean_unset() {
        let dir = root("blank", Some(r#"{"relay":{"secret":" ","url":[""]}}"#));
        let relay = RelaySettings::load(&dir).expect("load");
        assert_eq!(relay, RelaySettings::default());
    }

    #[test]
    fn a_single_string_is_one_relay() {
        let dir = root("one", Some(r#"{"relay":{"url":"https://a.example"}}"#));
        assert_eq!(
            RelaySettings::load(&dir).expect("load").url,
            ["https://a.example"]
        );
    }

    #[test]
    fn missing_file_or_block_is_the_default() {
        assert_eq!(
            RelaySettings::load(&root("none", None)).expect("load"),
            RelaySettings::default()
        );
        assert_eq!(
            RelaySettings::load(&root("empty", Some("{}"))).expect("load"),
            RelaySettings::default()
        );
    }

    #[test]
    fn a_broken_file_is_an_error_not_a_silent_default() {
        assert!(RelaySettings::load(&root("bad", Some("{not json"))).is_err());
    }

    #[test]
    fn parse_urls_splits_on_commas_and_whitespace() {
        assert_eq!(
            RelaySettings::parse_urls(" https://a.example,https://b.example  http://c.example ")
                .expect("parse"),
            ["https://a.example", "https://b.example", "http://c.example"]
        );
        assert!(RelaySettings::parse_urls("  ").expect("blank").is_empty());
        assert!(RelaySettings::parse_urls("relay.example").is_err());
    }

    #[test]
    fn debug_hides_the_secret() {
        let relay = RelaySettings {
            secret: Some("super-secret-value".to_string()),
            url: Vec::new(),
        };
        let shown = format!("{relay:?}");
        assert!(!shown.contains("super-secret-value"));
        assert!(shown.contains("<set>"));
    }
}
