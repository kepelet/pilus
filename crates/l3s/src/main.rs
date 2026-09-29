//! `l3s` — a native desktop client for a Luvus daemon.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod cli;
#[cfg(feature = "gui")]
mod gui;
mod settings;
mod update;

fn main() -> std::process::ExitCode {
    match cli::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("l3s: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
