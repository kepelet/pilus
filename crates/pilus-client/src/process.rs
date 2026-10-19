//! What this process is costing: CPU time, memory and threads, read from the
//! operating system.
//!
//! The Process page samples [`usage`] and [`threads`] once a second and turns
//! two samples into rates with [`percent`]. Only macOS is implemented, through
//! `proc_pidinfo`, which reports a task's counters without stopping it; elsewhere
//! both return nothing and the page says so. The one `unsafe` block is the FFI
//! call itself, behind a buffer the right size for the flavor asked for.

use std::time::Duration;

/// The process as a whole at one moment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    /// Physical memory in use, in bytes.
    pub resident: u64,
    /// Address space reserved, in bytes. Large on 64-bit systems and not a cost.
    pub virtual_size: u64,
    /// CPU time spent since launch, user and system together.
    pub cpu: Duration,
    /// Threads alive.
    pub threads: u32,
}

/// One thread at one moment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Thread {
    /// The system's id for it, stable for the thread's life.
    pub id: u64,
    /// The name it gave itself, empty if it never did.
    pub name: String,
    /// CPU time it has spent since it started.
    pub cpu: Duration,
    /// Whether it was running on a core when sampled.
    pub running: bool,
}

/// Share of one core `later` used since `earlier`, over `elapsed` wall time, as a
/// percentage: 100 is one core's worth, and a busy multi-threaded process can
/// exceed it. Zero when no time passed or the counter went backwards.
pub fn percent(earlier: Duration, later: Duration, elapsed: Duration) -> f32 {
    if elapsed.is_zero() {
        return 0.0;
    }
    let used = later.saturating_sub(earlier);
    (used.as_secs_f64() / elapsed.as_secs_f64() * 100.0) as f32
}

/// The whole process's counters, or `None` where they cannot be read.
pub fn usage() -> Option<Usage> {
    imp::usage()
}

/// Every thread's counters, or nothing where they cannot be read.
pub fn threads() -> Vec<Thread> {
    imp::threads()
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code, deprecated)]
mod imp {
    use std::ffi::{CStr, c_int, c_void};
    use std::mem::{size_of, zeroed};
    use std::sync::OnceLock;
    use std::time::Duration;

    use super::{Thread, Usage};

    /// `proc_pidinfo` flavors that `libc` does not name.
    const PROC_PIDTHREADID64INFO: c_int = 15;
    const PROC_PIDLISTTHREADIDS: c_int = 28;

    /// How many nanoseconds one tick of the system's clock is, as a ratio.
    /// Task and thread times are in these ticks, which on Apple silicon are not
    /// nanoseconds.
    fn ticks() -> (u64, u64) {
        static RATIO: OnceLock<(u64, u64)> = OnceLock::new();
        *RATIO.get_or_init(|| {
            // SAFETY: an all-zero `mach_timebase_info` is a valid value, and the
            // call only fills it in.
            let mut info: libc::mach_timebase_info = unsafe { zeroed() };
            // SAFETY: `info` is a live, writable `mach_timebase_info`.
            let status = unsafe { libc::mach_timebase_info(&mut info) };
            if status != 0 || info.denom == 0 {
                (1, 1)
            } else {
                (u64::from(info.numer), u64::from(info.denom))
            }
        })
    }

    fn duration(ticks_used: u64) -> Duration {
        let (numer, denom) = ticks();
        let nanos = u128::from(ticks_used) * u128::from(numer) / u128::from(denom);
        Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    fn pid() -> c_int {
        std::process::id().try_into().unwrap_or(0)
    }

    /// Fill `out` with the `flavor` of this process (or of the thread `arg`),
    /// and say whether the system filled all of it.
    fn info<T>(flavor: c_int, arg: u64, out: &mut T) -> bool {
        let Ok(size) = c_int::try_from(size_of::<T>()) else {
            return false;
        };
        // SAFETY: `out` is a live, exclusively borrowed `T` and `size` is its
        // exact size, so the call cannot write past it; `proc_pidinfo` reads no
        // memory of ours.
        let written = unsafe {
            libc::proc_pidinfo(
                pid(),
                flavor,
                arg,
                std::ptr::from_mut(out).cast::<c_void>(),
                size,
            )
        };
        written == size
    }

    pub fn usage() -> Option<Usage> {
        // SAFETY: `proc_taskinfo` is plain integers, for which all-zero is valid.
        let mut task: libc::proc_taskinfo = unsafe { zeroed() };
        if !info(libc::PROC_PIDTASKINFO, 0, &mut task) {
            return None;
        }
        Some(Usage {
            resident: task.pti_resident_size,
            virtual_size: task.pti_virtual_size,
            cpu: duration(task.pti_total_user.saturating_add(task.pti_total_system)),
            threads: u32::try_from(task.pti_threadnum).unwrap_or(0),
        })
    }

    pub fn threads() -> Vec<Thread> {
        // Room for more threads than a healthy pilus has; a longer list is cut.
        let mut ids = [0u64; 512];
        let Ok(size) = c_int::try_from(size_of_val(&ids)) else {
            return Vec::new();
        };
        // SAFETY: `ids` is a live array of exactly `size` bytes, which is all the
        // call may write.
        let written = unsafe {
            libc::proc_pidinfo(
                pid(),
                PROC_PIDLISTTHREADIDS,
                0,
                ids.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        let count = usize::try_from(written).unwrap_or(0) / size_of::<u64>();
        ids.iter()
            .take(count)
            .filter_map(|&id| {
                // SAFETY: `proc_threadinfo` is plain integers and a byte array,
                // for which all-zero is valid.
                let mut thread: libc::proc_threadinfo = unsafe { zeroed() };
                // A thread that exited since the list was read is skipped.
                if !info(PROC_PIDTHREADID64INFO, id, &mut thread) {
                    return None;
                }
                // SAFETY: the system wrote a NUL-terminated name into the array,
                // and zero-initialising it first guarantees a NUL even if not.
                let name = unsafe { CStr::from_ptr(thread.pth_name.as_ptr()) };
                Some(Thread {
                    id,
                    name: name.to_string_lossy().into_owned(),
                    cpu: duration(thread.pth_user_time.saturating_add(thread.pth_system_time)),
                    running: thread.pth_run_state == libc::TH_STATE_RUNNING,
                })
            })
            .collect()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{Thread, Usage};

    pub fn usage() -> Option<Usage> {
        None
    }

    pub fn threads() -> Vec<Thread> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_core_fully_used_is_a_hundred_percent_and_two_are_two_hundred() {
        let second = Duration::from_secs(1);
        assert!((percent(Duration::ZERO, second, second) - 100.0).abs() < 0.01);
        assert!((percent(second, second * 3, second) - 200.0).abs() < 0.01);
        assert!((percent(Duration::ZERO, Duration::from_millis(5), second) - 0.5).abs() < 0.01);
    }

    #[test]
    fn no_time_passing_or_a_counter_going_backwards_is_zero() {
        let second = Duration::from_secs(1);
        assert_eq!(percent(Duration::ZERO, second, Duration::ZERO), 0.0);
        assert_eq!(percent(second, Duration::ZERO, second), 0.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_process_reports_memory_cpu_and_a_thread_it_named() {
        let (stop, wait) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::Builder::new()
            .name("pilus-process-test".to_string())
            .spawn(move || {
                let _ = wait.recv();
            })
            .expect("spawn");
        // Burn a little CPU so the counter has moved off zero.
        let mut sum = 0u64;
        for n in 0..5_000_000u64 {
            sum = sum.wrapping_add(n * n);
        }
        assert_ne!(sum, 1);

        let usage = usage().expect("usage");
        assert!(usage.resident > 1 << 20, "{usage:?}");
        assert!(usage.cpu > Duration::ZERO, "{usage:?}");
        assert!(usage.threads >= 2, "{usage:?}");

        let threads = threads();
        assert!(
            threads
                .iter()
                .any(|thread| thread.name == "pilus-process-test"),
            "{threads:?}"
        );
        let total: Duration = threads.iter().map(|thread| thread.cpu).sum();
        assert!(total > Duration::ZERO);
        drop(stop);
        worker.join().expect("join");
    }
}
