//! Getting an image the person pasted or dropped onto a pane to a path that
//! pane's shell can read.
//!
//! A terminal has no way to take pixels, but the programs in it (an agent, an
//! image viewer, `open`) take a path. So an image is stored on the machine that
//! owns the pane and its absolute path is what the window pastes. Where the
//! pane runs decides how it gets there, the same split [`crate::upload_path`]
//! makes: a local pane is read straight from disk, a remote one is piped over
//! the saved SSH profile, so a screenshot taken on this Mac lands on the
//! remote host the agent is running on.
//!
//! A file already on the local disk is not copied: its own path is the answer,
//! as it is when a file is dropped on any terminal. Only pixels (a screenshot
//! on the clipboard) and files bound for a remote host are written, under
//! `~/.pilus/paste` there, which is owner-only because a screenshot is as
//! private as anything on the screen. Files there are cleared after
//! [`KEEP_DAYS`] days so the folder does not grow without bound.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::discovery::pilus_config_dir;
use crate::git::sh_quote;
use crate::transport::SshTarget;

/// Largest image that is stored, in bytes. A paste is read into memory and, for
/// a remote host, sent over the link in one go, so a huge file is refused
/// instead of stalling the window's worker.
pub const MAX_PASTED_BYTES: u64 = 32 * 1024 * 1024;

/// How long a stored image is kept, in days.
pub const KEEP_DAYS: u32 = 7;

/// The folder under the pilus state root that holds stored images.
const FOLDER: &str = "paste";

/// Disambiguates two images stored in the same millisecond, as a multi-file
/// drop does.
static SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// An image headed for a pane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PastedImage {
    /// Encoded pixels from the clipboard, such as a screenshot.
    Pixels {
        /// The encoded image.
        bytes: Vec<u8>,
        /// Its file extension, without the dot, so the stored file opens by
        /// type.
        extension: &'static str,
    },
    /// A file on this machine, such as one dropped from Finder.
    File(PathBuf),
}

/// Whether `path` names a picture a pane can be handed, by extension.
pub fn is_image_file(path: &Path) -> bool {
    image_extension(path).is_some()
}

/// Store `image` where the pane on `target`'s host (or this machine, for
/// `None`) can read it, and return its absolute path there.
pub fn stash_image(target: Option<&SshTarget>, image: &PastedImage) -> Result<String, String> {
    match image {
        PastedImage::Pixels { bytes, extension } => {
            if bytes.len() as u64 > MAX_PASTED_BYTES {
                return Err(too_big("The clipboard image"));
            }
            let name = stored_name("image", extension);
            match target {
                None => stash_local(&pilus_config_dir().join(FOLDER), &name, bytes),
                Some(target) => send(target, &name, Cursor::new(bytes)),
            }
        }
        PastedImage::File(path) => {
            let label = path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            let extension = image_extension(path)
                .ok_or_else(|| format!("{label} is not an image pilus can paste"))?;
            let meta = fs::metadata(path).map_err(|error| format!("{label}: {error}"))?;
            if !meta.is_file() {
                return Err(format!("{label} is not a file"));
            }
            let Some(target) = target else {
                // Already where the pane can read it.
                return path
                    .to_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{label} has a path pilus cannot paste"));
            };
            if meta.len() > MAX_PASTED_BYTES {
                return Err(too_big(&label));
            }
            let stem = path
                .file_stem()
                .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
            let input = File::open(path).map_err(|error| format!("{label}: {error}"))?;
            send(target, &stored_name(&stem, extension), input)
        }
    }
}

/// `path` as one shell word, with a backslash before anything the shell would
/// split or expand. Backslash escapes read the same in sh, zsh and fish, which
/// is how a terminal pastes a dropped path.
pub fn shell_word(path: &str) -> String {
    let mut word = String::with_capacity(path.len());
    for ch in path.chars() {
        if !(ch.is_alphanumeric() || "_-./:@%+=,~".contains(ch)) {
            word.push('\\');
        }
        word.push(ch);
    }
    word
}

/// The extension of an image `path`, lowercased and normalised, or `None` for
/// anything else.
fn image_extension(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "gif" => "gif",
        "webp" => "webp",
        "bmp" => "bmp",
        "tif" | "tiff" => "tiff",
        "heic" => "heic",
        _ => return None,
    })
}

/// A name that is unique to this store and safe in a shell and a path: the
/// source's own stem, cleaned, then the time and a sequence number.
fn stored_name(stem: &str, extension: &str) -> String {
    let cleaned: String = stem
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let cleaned = cleaned.trim_matches('-');
    let stem = if cleaned.is_empty() { "image" } else { cleaned };
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{stem}-{millis}-{sequence}.{extension}")
}

fn too_big(what: &str) -> String {
    format!(
        "{what} is over {} MB, which is too large to paste",
        MAX_PASTED_BYTES / (1024 * 1024)
    )
}

/// Write `bytes` as `name` in `dir`, owner-only, and return its path.
fn stash_local(dir: &Path, name: &str, bytes: &[u8]) -> Result<String, String> {
    use std::io::Write;

    pilus_db::private_dir(dir)
        .map_err(|error| format!("couldn't make {}: {error}", dir.display()))?;
    prune(
        dir,
        Duration::from_secs(u64::from(KEEP_DAYS) * 24 * 60 * 60),
    );
    let path = dir.join(name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("couldn't store {name}: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("couldn't store {name}: {error}"))?;
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| "the paste folder has a path pilus cannot paste".to_string())
}

/// Remove the files in `dir` last written more than `keep` ago. Best effort:
/// a file that will not go is left for the next paste.
fn prune(dir: &Path, keep: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .ok()
            .filter(|meta| meta.is_file())
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > keep);
        if old {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The script that stores stdin as `name` on a host and prints where. The
/// folder hangs off the host's own `$HOME`, which only the host knows, so the
/// script reports the absolute path back.
fn remote_script(name: &str) -> String {
    let name = sh_quote(name);
    // `set -C` makes the redirect refuse an existing file, and the umask makes
    // the folder and the file owner-only from the first byte.
    format!(
        "umask 077; dir=\"$HOME/.pilus/{FOLDER}\"; mkdir -p \"$dir\" || exit 1; \
         find \"$dir\" -type f -mtime +{KEEP_DAYS} -exec rm -f {{}} + 2>/dev/null; \
         set -C; cat > \"$dir\"/{name} || exit 1; printf '%s\\n' \"$dir\"/{name}"
    )
}

/// Pipe `input` into the host's paste folder as `name` and return its path.
fn send(target: &SshTarget, name: &str, mut input: impl Read) -> Result<String, String> {
    let mut child = target
        .shell(&remote_script(name))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("couldn't reach the host: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A host that refuses the write closes the pipe early; its reason is on
        // stderr, which is the more useful error.
        let _ = io::copy(&mut input, &mut stdin);
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("couldn't store {name}: {error}"))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if reason.is_empty() {
            format!("couldn't store {name} on the host")
        } else {
            reason
        });
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.starts_with('/') {
        Ok(path)
    } else {
        Err(format!("the host did not say where it stored {name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pilus-paste-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    #[test]
    fn only_pictures_are_images() {
        assert!(is_image_file(Path::new("/a/shot.PNG")));
        assert!(is_image_file(Path::new("photo.jpeg")));
        assert!(!is_image_file(Path::new("notes.txt")));
        assert!(!is_image_file(Path::new("logo.svg")));
        assert!(!is_image_file(Path::new("png")));
    }

    #[test]
    fn a_stored_name_is_safe_and_unique() {
        let a = stored_name("My Shot (2)", "png");
        let b = stored_name("My Shot (2)", "png");
        assert_ne!(a, b);
        assert!(a.starts_with("My-Shot--2-"), "{a}");
        assert!(a.ends_with(".png"));
        assert!(
            a.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch))
        );
        assert!(stored_name("   ", "png").starts_with("image-"));
        assert!(stored_name(&"x".repeat(200), "png").len() < 80);
    }

    #[test]
    fn a_path_is_escaped_as_one_shell_word() {
        assert_eq!(shell_word("/tmp/a-b_c.png"), "/tmp/a-b_c.png");
        assert_eq!(
            shell_word("/Users/a b/s (1).png"),
            "/Users/a\\ b/s\\ \\(1\\).png"
        );
        assert_eq!(shell_word("/it's.png"), "/it\\'s.png");
    }

    #[test]
    fn pixels_are_stored_privately_and_named_by_path() {
        let dir = scratch("pixels").join("paste");
        let path = stash_local(&dir, "image-1.png", b"png bytes").expect("stash");
        assert_eq!(Path::new(&path), dir.join("image-1.png"));
        assert_eq!(fs::read(&path).expect("read"), b"png bytes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o077, 0, "a stored screenshot is owner-only");
        }
    }

    #[test]
    fn a_stored_image_is_never_replaced() {
        let dir = scratch("clobber").join("paste");
        stash_local(&dir, "a.png", b"old").expect("stash");
        assert!(stash_local(&dir, "a.png", b"new").is_err());
        assert_eq!(fs::read(dir.join("a.png")).expect("read"), b"old");
    }

    #[test]
    fn old_images_are_pruned_and_recent_ones_stay() {
        let dir = scratch("prune");
        fs::write(dir.join("keep.png"), b"k").expect("write");
        fs::write(dir.join("gone.png"), b"g").expect("write");
        // Nothing is older than a day, so everything stays; a zero horizon
        // clears anything already written.
        prune(&dir, Duration::from_secs(24 * 60 * 60));
        assert!(dir.join("keep.png").exists() && dir.join("gone.png").exists());
        std::thread::sleep(Duration::from_millis(20));
        prune(&dir, Duration::ZERO);
        assert!(!dir.join("keep.png").exists() && !dir.join("gone.png").exists());
    }

    #[test]
    fn a_local_file_is_pasted_by_its_own_path() {
        let root = scratch("file");
        let source = root.join("shot.png");
        fs::write(&source, b"x").expect("write");
        let path = stash_image(None, &PastedImage::File(source.clone())).expect("stash");
        assert_eq!(Path::new(&path), source);
        assert_eq!(fs::read_dir(&root).expect("dir").count(), 1, "no copy made");
    }

    #[test]
    fn a_file_that_is_not_an_image_is_refused() {
        let root = scratch("text");
        let source = root.join("notes.txt");
        fs::write(&source, b"x").expect("write");
        let error = stash_image(None, &PastedImage::File(source)).expect_err("not an image");
        assert_eq!(error, "notes.txt is not an image pilus can paste");
    }

    #[test]
    fn a_folder_named_like_an_image_is_refused() {
        let root = scratch("dirimg");
        let source = root.join("shots.png");
        fs::create_dir_all(&source).expect("dir");
        assert!(stash_image(None, &PastedImage::File(source)).is_err());
    }

    #[test]
    fn an_oversized_clipboard_image_is_refused() {
        let image = PastedImage::Pixels {
            bytes: vec![0; MAX_PASTED_BYTES as usize + 1],
            extension: "png",
        };
        let error = stash_image(None, &image).expect_err("too big");
        assert!(error.contains("too large"), "{error}");
    }

    /// Run the remote script under `sh` against a throwaway `$HOME`: the same
    /// words a host would run, minus the SSH hop.
    #[cfg(unix)]
    fn run_script(home: &Path, name: &str, stdin: &[u8]) -> std::process::Output {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(remote_script(name))
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin)
            .expect("write");
        child.wait_with_output().expect("output")
    }

    #[cfg(unix)]
    #[test]
    fn the_remote_script_stores_privately_and_reports_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let home = scratch("remote-home");
        let output = run_script(&home, "my shot.png", b"pixels");
        assert!(output.status.success(), "{output:?}");
        let reported = String::from_utf8(output.stdout).expect("utf8");
        let path = PathBuf::from(reported.trim());
        assert_eq!(path, home.join(".pilus/paste/my shot.png"));
        assert_eq!(fs::read(&path).expect("read"), b"pixels");
        let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("dir")), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn the_remote_script_never_replaces_a_file() {
        let home = scratch("remote-clobber");
        assert!(run_script(&home, "a.png", b"old").status.success());
        let again = run_script(&home, "a.png", b"new");
        assert!(!again.status.success());
        assert!(again.stdout.is_empty(), "no path is reported for a refusal");
        assert_eq!(
            fs::read(home.join(".pilus/paste/a.png")).expect("read"),
            b"old"
        );
    }

    #[test]
    fn the_remote_script_quotes_the_name_and_keeps_the_folder_private() {
        let script = remote_script("a'b.png");
        assert!(script.starts_with("umask 077;"));
        assert!(
            script.contains("set -C; cat > \"$dir\"/'a'\\''b.png'"),
            "{script}"
        );
        assert!(script.contains("-mtime +7"));
    }
}
