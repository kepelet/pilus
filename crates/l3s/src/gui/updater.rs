//! Observable OTA state for the GPUI window.
//!
//! [`Updater`] drives the blocking [`UpdateEngine`] on the background
//! executor, so the GUI thread never blocks on network or disk. Views read
//! [`Updater::status`] in `render` and call [`Updater::check`] /
//! [`Updater::download_and_install`] from menu actions and the status badge.
//!
//! Mirrors the `gpui-updater-pre` shape without its Rust 1.98 floor, so this
//! workspace (1.96) can build it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gpui::{Context, Task};
use gpui_updater_core::{Release, UpdateEngine, UpdateSource, UpdateStatus};

use crate::update::UpdateRequest;

type BoxedEngine = UpdateEngine<Box<dyn UpdateSource>>;

/// A GPUI entity that checks for, downloads, and stages updates.
///
/// Hold it as `Entity<Updater>` in `L3sView`. All work is explicit — no
/// background polling — so checks happen on launch (when enabled) and on
/// `Check for Updates…`. Dropping the entity cancels its foreground task, not
/// an in-flight blocking download.
pub struct Updater {
    status: UpdateStatus,
    available: Option<Release>,
    engine: Arc<BoxedEngine>,
    task: Option<Task<()>>,
}

impl Updater {
    /// Build from an [`UpdateRequest`]; the engine is fixed for the entity.
    pub fn from_request(request: &UpdateRequest, _cx: &mut Context<Self>) -> Self {
        Self {
            status: UpdateStatus::Idle,
            available: None,
            engine: Arc::new(crate::update::engine(request)),
            task: None,
        }
    }

    /// Swap the engine after the channel or feed URL changed.
    ///
    /// Cancels any in-flight foreground task, so a check that raced the
    /// settings change cannot land an answer for the old source.
    pub fn reconfigure(&mut self, request: &UpdateRequest, cx: &mut Context<Self>) {
        self.task = None;
        self.engine = Arc::new(crate::update::engine(request));
        self.available = None;
        self.set_status(UpdateStatus::Idle, cx);
    }

    /// Current status; read this in `render`.
    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    /// The release found by the last successful check, if newer.
    pub fn available(&self) -> Option<&Release> {
        self.available.as_ref()
    }

    fn set_status(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    /// Check the source for a newer release. No-op while busy.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        if self.status.is_busy() {
            return;
        }
        self.set_status(UpdateStatus::Checking, cx);
        let engine = self.engine.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { engine.check() })
                .await;
            this.update(cx, |this, cx| {
                this.task = None;
                match result {
                    Ok(Some(release)) => {
                        let version = release.version.clone();
                        this.available = Some(release);
                        this.set_status(UpdateStatus::Available(version), cx);
                    }
                    Ok(None) => this.set_status(UpdateStatus::UpToDate, cx),
                    Err(error) => this.set_status(UpdateStatus::Errored(error.to_string()), cx),
                }
            })
            .ok();
        }));
    }

    /// Download, verify, and swap in the available update. No-op unless a
    /// release is [`available`](Self::available) and nothing is in flight.
    ///
    /// On success the status becomes [`UpdateStatus::Staged`] and the app's
    /// restart path points at the new bundle; call [`Updater::restart`].
    pub fn download_and_install(&mut self, cx: &mut Context<Self>) {
        if self.status.is_busy() {
            return;
        }
        let Some(release) = self.available.clone() else {
            return;
        };
        let engine = self.engine.clone();
        self.set_status(
            UpdateStatus::Downloading {
                downloaded: 0,
                total: None,
            },
            cx,
        );
        self.task = Some(cx.spawn(async move |this, cx| {
            let downloaded = Arc::new(AtomicU64::new(0));
            let total = Arc::new(AtomicU64::new(0));
            let done = Arc::new(AtomicBool::new(false));

            let dl_task = {
                let (engine, release) = (engine.clone(), release.clone());
                let (got, tot, fin) = (downloaded.clone(), total.clone(), done.clone());
                cx.background_executor().spawn(async move {
                    let result = engine.download(&release, |got_bytes, tot_bytes| {
                        got.store(got_bytes, Ordering::Relaxed);
                        tot.store(tot_bytes.unwrap_or(0), Ordering::Relaxed);
                    });
                    fin.store(true, Ordering::Relaxed);
                    result
                })
            };

            loop {
                let got = downloaded.load(Ordering::Relaxed);
                let tot = total.load(Ordering::Relaxed);
                this.update(cx, |this, cx| {
                    this.set_status(
                        UpdateStatus::Downloading {
                            downloaded: got,
                            total: (tot != 0).then_some(tot),
                        },
                        cx,
                    );
                })
                .ok();
                if done.load(Ordering::Relaxed) {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
            }

            let artifact = match dl_task.await {
                Ok(path) => path,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.task = None;
                        this.set_status(UpdateStatus::Errored(error.to_string()), cx);
                    })
                    .ok();
                    return;
                }
            };

            this.update(cx, |this, cx| this.set_status(UpdateStatus::Installing, cx))
                .ok();

            let installed = {
                let engine = engine.clone();
                cx.background_executor()
                    .spawn(async move { engine.install(&artifact) })
                    .await
            };
            this.update(cx, |this, cx| {
                this.task = None;
                match installed {
                    Ok(installed) => {
                        if let Some(path) = &installed.restart_path {
                            cx.set_restart_path(path.clone());
                        }
                        let version = release.version.clone();
                        this.set_status(UpdateStatus::Staged(version), cx);
                    }
                    Err(error) => this.set_status(UpdateStatus::Errored(error.to_string()), cx),
                }
            })
            .ok();
        }));
    }

    /// Relaunch into the staged update.
    pub fn restart(&self, cx: &mut Context<Self>) {
        cx.restart();
    }
}
