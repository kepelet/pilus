//! Where the window's warnings and crashes go.
//!
//! The app reports trouble with `tracing` (a failed save, a refused URL, a
//! daemon that answered nonsense), but a bundle launched from Finder has no
//! terminal, so without this every report was dropped. Warnings and errors are
//! appended to `<pilus dir>/logs/pilus.log` (owner-only, since a message can
//! name a file or a host), which is cut over to `pilus.log.1` once it passes
//! [`MAX_BYTES`], so it never grows past twice that. `PILUS_LOG=info` (or
//! `debug`, `trace`) says more; a run from a terminal also writes to stderr.
//!
//! A panic is written there too, with its thread, location and a backtrace,
//! before the default hook runs, so a tester has something to send.

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, filter::LevelFilter};

/// How large the log may grow before it is cut over.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// The variable that sets how much is logged.
const LEVEL_ENV: &str = "PILUS_LOG";

/// Start logging to `<root>/logs/pilus.log` and record panics there. Safe to
/// call more than once; only the first call installs anything.
pub fn init(root: &Path) {
    let level = level_from(std::env::var(LEVEL_ENV).ok().as_deref());
    let file = RotatingFile::open(&root.join("logs").join("pilus.log"));
    let to_file = file.clone().map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_writer(file)
            .with_filter(level)
    });
    let to_terminal = io::stderr().is_terminal().then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(io::stderr)
            .with_filter(level)
    });
    // Another subscriber already set (a test harness, GPUI's profiler) wins.
    let _ = tracing_subscriber::registry()
        .with(to_file)
        .with(to_terminal)
        .try_init();
    install_panic_hook(file);
}

/// `PILUS_LOG`'s level, warnings when it is unset or unreadable.
fn level_from(value: Option<&str>) -> LevelFilter {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("off") => LevelFilter::OFF,
        Some("error") => LevelFilter::ERROR,
        Some("info") => LevelFilter::INFO,
        Some("debug") => LevelFilter::DEBUG,
        Some("trace") => LevelFilter::TRACE,
        _ => LevelFilter::WARN,
    }
}

fn install_panic_hook(file: Option<RotatingFile>) {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let name = thread.name().unwrap_or("unnamed");
            let backtrace = std::backtrace::Backtrace::force_capture();
            let report = format!(
                "{} pilus {} panicked on thread {name}: {info}\n{backtrace}\n",
                timestamp(),
                env!("CARGO_PKG_VERSION"),
            );
            // Written straight to the file: the subscriber may be what panicked.
            if let Some(file) = &file {
                let _ = file.make_writer().write_all(report.as_bytes());
            }
            previous(info);
        }));
    });
}

/// Seconds since the epoch: enough to put a crash beside the log lines around
/// it without a calendar dependency.
fn timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| format!("[{}]", elapsed.as_secs()))
        .unwrap_or_default()
}

/// An append-only log file that is cut over to `<name>.1` when it grows past
/// [`MAX_BYTES`]. Writes from every thread share one handle.
#[derive(Clone)]
struct RotatingFile {
    inner: Arc<Mutex<Rotating>>,
}

struct Rotating {
    path: PathBuf,
    file: File,
    written: u64,
    limit: u64,
}

impl RotatingFile {
    fn open(path: &Path) -> Option<Self> {
        Self::with_limit(path, MAX_BYTES)
    }

    fn with_limit(path: &Path, limit: u64) -> Option<Self> {
        if let Some(dir) = path.parent() {
            pilus_db::private_dir(dir).ok()?;
        }
        let file = open_private(path).ok()?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Some(Self {
            inner: Arc::new(Mutex::new(Rotating {
                path: path.to_path_buf(),
                file,
                written,
                limit,
            })),
        })
    }
}

fn open_private(path: &Path) -> io::Result<File> {
    pilus_db::create_private(path)?;
    OpenOptions::new().append(true).open(path)
}

impl Rotating {
    fn rotate(&mut self) -> io::Result<()> {
        let older = self.path.with_extension("log.1");
        std::fs::rename(&self.path, older)?;
        self.file = open_private(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

/// One write's handle on the shared file.
struct Writing<'a>(&'a Mutex<Rotating>);

impl Write for Writing<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut log = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if log.written + buf.len() as u64 > log.limit && log.written > 0 {
            // A log that cannot be cut over keeps growing rather than losing lines.
            let _ = log.rotate();
        }
        let written = log.file.write(buf)?;
        log.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut log = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        log.file.flush()
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = Writing<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        Writing(&self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_is_warnings_unless_pilus_log_says_otherwise() {
        assert_eq!(level_from(None), LevelFilter::WARN);
        assert_eq!(level_from(Some("nonsense")), LevelFilter::WARN);
        assert_eq!(level_from(Some(" Debug ")), LevelFilter::DEBUG);
        assert_eq!(level_from(Some("off")), LevelFilter::OFF);
    }

    #[test]
    fn the_log_is_owner_only_and_cut_over_when_it_grows() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pilus-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("logs").join("pilus.log");
        let file = RotatingFile::with_limit(&path, 64).expect("open");
        for line in 0..10 {
            // One write per line, as the formatter and the panic hook make them.
            file.make_writer()
                .write_all(format!("line {line} of the log\n").as_bytes())
                .expect("write");
        }
        let current = std::fs::read_to_string(&path).expect("current");
        let older = std::fs::read_to_string(path.with_extension("log.1")).expect("older");
        assert!(current.len() <= 64, "{current:?}");
        assert!(current.ends_with("line 9 of the log\n"));
        assert!(older.contains("line"), "{older:?}");
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
