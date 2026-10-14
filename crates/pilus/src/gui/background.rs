//! Keeping the menu bar item alive across a quit.
//!
//! Quitting pilus ends its process, and the status item with it. When the menu
//! bar is on, a quit therefore hands off to a menu-bar-only copy of pilus
//! (`pilus --menubar`): no window and no Dock icon, just the item and the
//! watcher behind it. Opening a window from it, by a click or by launching
//! pilus again, turns it back into the full app. Choosing Quit from the item
//! itself, turning the setting off, or restarting for an update ends pilus
//! outright. A process that is not the helper retires a stale one at launch,
//! so two items never stand at once. Other platforms have no menu bar item,
//! so they do nothing.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::App;
use pilus_client::ConnectTarget;

/// Set once the user chose to end pilus for good, so the quit hook leaves no
/// helper behind.
static FULL_QUIT: AtomicBool = AtomicBool::new(false);

/// Quit and leave nothing running.
pub fn quit_fully(cx: &mut App) {
    skip_hand_off();
    cx.quit();
}

/// Make the coming quit final, for callers that quit by other means.
pub fn skip_hand_off() {
    FULL_QUIT.store(true, Ordering::SeqCst);
}

/// Whether a quit now should leave the menu bar helper behind.
pub fn hand_off_wanted(menu_bar: bool) -> bool {
    cfg!(target_os = "macos") && menu_bar && !FULL_QUIT.load(Ordering::SeqCst)
}

/// Where the helper records its process id.
fn pid_file(root: &Path) -> std::path::PathBuf {
    root.join("menubar.pid")
}

/// Start the menu-bar-only helper, detached so it outlives this process.
pub fn spawn_helper(root: &Path, target: &ConnectTarget) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(exe);
    command.arg("--menubar");
    if let Some(socket) = &target.socket {
        command.arg("--socket").arg(socket);
    } else if let Some(session) = &target.session {
        command.arg("--session").arg(session);
    }
    // Its own process group, so closing the terminal that started pilus does
    // not take the helper with it.
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    if let Ok(child) = child {
        std::fs::write(pid_file(root), child.id().to_string()).ok();
    }
}

/// End a helper left from an earlier quit, so this process owns the item.
pub fn retire_helper(root: &Path) {
    use std::process::Command;

    let path = pid_file(root);
    let Some(pid) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
    else {
        return;
    };
    std::fs::remove_file(&path).ok();
    if pid == std::process::id() {
        return;
    }
    // The pid may have been reused since, so only a process that was started
    // as the helper is touched.
    let is_helper = || {
        Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains("--menubar"))
    };
    if !is_helper() {
        return;
    }
    Command::new("kill").arg(pid.to_string()).status().ok();
    // The old item leaves with its process; wait for that so it does not
    // overlap the one this process is about to install.
    for _ in 0..20 {
        if !is_helper() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Run without a Dock icon or app menu: the helper, until it opens a window.
#[cfg(target_os = "macos")]
pub fn set_accessory(accessory: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    NSApplication::sharedApplication(mtm).setActivationPolicy(if accessory {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    });
}

#[cfg(not(target_os = "macos"))]
pub fn set_accessory(_accessory: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_off_needs_the_menu_bar_on() {
        assert!(!hand_off_wanted(false));
    }

    #[test]
    fn a_stale_pid_file_is_cleared_without_touching_other_processes() {
        let root = std::env::temp_dir().join(format!("pilus-bg-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("dir");
        // This test process was not started with `--menubar`, so it is left
        // alone even though its pid is on file.
        std::fs::write(pid_file(&root), std::process::id().to_string()).expect("write");
        retire_helper(&root);
        assert!(!pid_file(&root).exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
