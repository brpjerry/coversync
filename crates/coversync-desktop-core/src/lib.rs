//! The coversync desktop app, apart from its window and web view: the
//! settings ([`config`]), the servers and a proxy for each ([`shell`]), how a
//! launch picks a server ([`launch`]), which links go to the browser
//! ([`links`]), what the app offers a server's UI ([`bridge`]), and where
//! its tokens are kept ([`tokens`]).
//!
//! The app (`apps/desktop-linux`, Qt 6 and Qt WebEngine) wraps these in its
//! window. They are here, in the repository's workspace, so they build and
//! are tested without Qt.

pub mod bridge;
pub mod config;
pub mod launch;
pub mod links;
pub mod shell;
pub mod tokens;

pub use config::{ConfigLock, Store};
pub use launch::{EarlyLaunches, PairLink, SERVER_ENV, log_filter, server_arg, with_env_server};
pub use shell::{Probe, Problem, Save, Shell};
