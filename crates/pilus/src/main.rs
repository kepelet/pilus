//! `pilus` — a native desktop client for a Luvus daemon.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod cli;
#[cfg(feature = "gui")]
mod gui;
#[cfg(feature = "gui")]
mod luvus_update;
mod settings;
mod update;

fn main() -> std::process::ExitCode {
    match cli::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("pilus: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
