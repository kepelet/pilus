//! A local index of the coding-agent sessions on this machine and on the saved
//! remote ones, so a window can search them and resume one.
//!
//! The data flow is one way: [`scan`] lists each agent's transcript files and
//! reads a few evidence lines from the ones that changed (on this machine
//! directly, on a remote one in two `ssh` round trips), [`parse`] turns that
//! evidence into a title and metadata, and [`Index`] keeps the result in a
//! SQLite file. The index is a cache of what the agents wrote, not a source of
//! truth: it holds no message text, a schema change drops and rebuilds it, and
//! [`wipe`] removes it entirely when the person withdraws consent.
//!
//! Nothing here asks whether it may run. The caller owns that decision
//! (`Settings::index_agents`) and must not call [`refresh`] before it is made.

#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod error;
mod model;
mod parse;
mod refresh;
mod scan;
mod store;

pub use error::{Error, Result};
pub use model::{Agent, Session};
pub use refresh::{Report, Source, refresh};
pub use store::{Filter, Index, wipe};
