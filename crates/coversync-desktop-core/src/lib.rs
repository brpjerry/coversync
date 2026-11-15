//! The coversync desktop app, apart from its window and web view: the
//! settings ([`config`]), the servers and a proxy for each ([`shell`]), how a
//! launch picks a server ([`launch`]), which links go to the browser
//! ([`links`]), what the app offers a server's UI ([`bridge`]), where its
//! tokens are kept ([`tokens`]), and background sync as the app and the sync
//! agent share it ([`sync`]).
//!
//! The app (`apps/desktop-linux`, Qt 6 and Qt WebEngine) wraps these in its
//! window. They are here, in the repository's workspace, so they build and
//! are tested without Qt.

pub mod bridge;
pub mod config;
pub mod launch;
pub mod links;
pub mod local;
pub mod shell;
pub mod sync;
pub mod tokens;

/// The app's ID: what its settings folder is called and whose its tokens
/// are in the keychain. The sync agent (`coversync`) uses both, so the app
/// and the agent are one device on a server.
pub const APP_ID: &str = "io.github.brpjerry.coversync";

pub use config::{ConfigLock, Store};
pub use launch::{EarlyLaunches, PairLink, SERVER_ENV, log_filter, server_arg, with_env_server};
pub use shell::{Probe, Problem, Save, Shell};
