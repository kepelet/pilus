//! Process-wide lifecycle for the host connector.
//!
//! "Enable this node" is a setting, not a window action: once on, the
//! connector keeps running while pilus is open even if the Remote Machines
//! window is closed. One global owns the child so every window observes the
//! same state, and the last window to close tears it down.

use std::sync::{Mutex, MutexGuard, OnceLock};

use pilus_client::{Connector, ConnectorOptions, ConnectorStatus};

static CONNECTOR: OnceLock<Mutex<Option<Connector>>> = OnceLock::new();

fn lock() -> MutexGuard<'static, Option<Connector>> {
    CONNECTOR
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The options the toggle enables: the bounded control surface, default
/// session, and the default relays.
pub fn default_options() -> ConnectorOptions {
    ConnectorOptions {
        control: true,
        ..ConnectorOptions::default()
    }
}

/// Start the connector for this node if it is not already running.
pub fn start(options: &ConnectorOptions) -> Result<(), String> {
    let mut guard = lock();
    if guard.is_some() {
        return Ok(());
    }
    let connector = Connector::start(options).map_err(|error| error.to_string())?;
    *guard = Some(connector);
    Ok(())
}

/// Stop the connector and reap its child.
pub fn stop() {
    if let Some(mut connector) = lock().take() {
        connector.stop();
    }
}

/// The connector's current status, or a stopped default.
pub fn status() -> ConnectorStatus {
    match lock().as_mut() {
        Some(connector) => connector.status(),
        None => ConnectorStatus::default(),
    }
}

/// Whether a connector child is present.
pub fn running() -> bool {
    lock().is_some()
}
