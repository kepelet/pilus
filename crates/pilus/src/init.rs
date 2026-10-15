//! First-run alignment of the daemon's config with the surface pilus draws.
//!
//! The daemon owns pane gaps, title rows, and the theme, so pilus cannot show
//! its intended chrome until those few keys match. This runs once per install:
//! it waits for the local control socket (the window's own attach may be
//! starting the daemon), patches the keys pilus assumes, and reports success so
//! the caller can record the fact in `settings.json` and skip the round trip
//! on every later launch.

use std::path::Path;
use std::time::{Duration, Instant};

use pilus_client::ControlClient;

/// How long to wait for the daemon's control socket before giving up and
/// letting a later launch try again.
const SOCKET_WAIT: Duration = Duration::from_secs(20);
/// Delay between connection attempts while the daemon finishes starting.
const RETRY: Duration = Duration::from_millis(250);

/// The daemon keys first-run pilus sets: flush panes with no gap, no title
/// rows, and the near-black theme the native chrome is tuned against.
fn default_patch() -> serde_json::Value {
    serde_json::json!({
        "patch": {
            "layout": { "col_gap": 0, "row_gap": 0, "show_titles": false },
            "theme": "noir",
        }
    })
}

/// Patch the daemon behind `socket` once. Returns whether the patch landed; a
/// `false` leaves the caller's `init` flag unset so a later launch retries.
pub fn patch_luvus_defaults(socket: &Path) -> bool {
    let deadline = Instant::now() + SOCKET_WAIT;
    loop {
        match ControlClient::connect(socket) {
            Ok(mut client) => return client.request("config.patch", default_patch()).is_ok(),
            Err(_) if Instant::now() < deadline => std::thread::sleep(RETRY),
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_patch_flushes_panes_and_selects_noir() {
        let patch = default_patch();
        let patch = &patch["patch"];
        assert_eq!(patch["layout"]["col_gap"], 0);
        assert_eq!(patch["layout"]["row_gap"], 0);
        assert_eq!(patch["layout"]["show_titles"], false);
        assert_eq!(patch["theme"], "noir");
    }
}
