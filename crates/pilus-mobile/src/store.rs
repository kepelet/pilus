//! On-device persistence: the iroh identity key and the enrollment record.
//!
//! The device key is a 32-byte Ed25519 secret stored owner-only; it *is* the
//! device's authorization, since the connector allows peers by `EndpointId`.
//! `enrollment.json` records the host ticket and delegated token so the app
//! can resume without re-entering anything.

use std::path::{Path, PathBuf};

use iroh::SecretKey;
use serde_json::{Value, json};

use crate::client::Error;

/// What the device remembers about one enrolled host.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Enrollment {
    /// The `EndpointTicket` the device dialed.
    pub ticket: String,
    /// The host's `EndpointId`, verified at connect time.
    pub endpoint_id: String,
    /// The delegated UHP token handed out after enrollment.
    pub token: String,
}

impl Enrollment {
    /// Load the saved enrollment under `dir`, if one exists and is complete.
    pub fn load(dir: &Path) -> Option<Self> {
        let bytes = std::fs::read(state_path(dir)).ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        let ticket = value.get("ticket")?.as_str()?.to_string();
        let token = value.get("token")?.as_str()?.to_string();
        Some(Self {
            ticket,
            endpoint_id: value
                .get("endpoint_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            token,
        })
    }

    /// Persist the enrollment under `dir`, owner-only, refusing to corrupt an
    /// existing record: a failed write leaves the old file untouched.
    pub fn save(&self, dir: &Path) -> Result<(), Error> {
        std::fs::create_dir_all(dir)?;
        let payload = json!({
            "ticket": self.ticket,
            "endpoint_id": self.endpoint_id,
            "token": self.token,
        });
        write_private_replace(&state_path(dir), payload.to_string().as_bytes())
    }

    /// Delete the saved enrollment (device unpair/reset).
    pub fn forget(dir: &Path) {
        let _ = std::fs::remove_file(state_path(dir));
    }
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("enrollment.json")
}

/// Load the device identity at `dir/device.key`, creating it on first run.
pub fn load_or_create_key(dir: &Path) -> Result<SecretKey, Error> {
    let path = dir.join("device.key");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let array: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::Store("device.key is not 32 bytes".to_string()))?;
            Ok(SecretKey::from_bytes(&array))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            let key = SecretKey::generate();
            write_private_new(&path, &key.to_bytes())?;
            Ok(key)
        }
        Err(error) => Err(Error::Io(error)),
    }
}

/// Write `bytes` to `path`, owner-only, refusing to overwrite an identity.
fn write_private_new(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    Ok(())
}

/// Write `bytes` to `path` atomically through a same-directory temporary file,
/// owner-only, so a crash mid-write never leaves a truncated token file.
fn write_private_replace(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "enrollment.json".to_string());
    let temp = parent.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<(), Error> {
        write_private_new(&temp, bytes)?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn identity_key_persists_and_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("pilus-mobile-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested/device.key");

        let first = load_or_create_key(path.parent().expect("parent")).expect("create");
        let second = load_or_create_key(&dir.join("nested")).expect("load");
        assert_eq!(first.to_bytes(), second.to_bytes());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enrollment_round_trips_and_forgets() {
        let dir = std::env::temp_dir().join(format!("pilus-mobile-enroll-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(Enrollment::load(&dir).is_none());

        let enrollment = Enrollment {
            ticket: "endpointaaa".to_string(),
            endpoint_id: "host".to_string(),
            token: "luv_tok_x".to_string(),
        };
        enrollment.save(&dir).expect("save");
        assert_eq!(Enrollment::load(&dir), Some(enrollment));

        Enrollment::forget(&dir);
        assert!(Enrollment::load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_reads_as_absent() {
        let dir = std::env::temp_dir().join(format!("pilus-mobile-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("enrollment.json"), b"not json").expect("write");
        assert!(Enrollment::load(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
