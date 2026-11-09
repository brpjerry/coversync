//! What the coversync desktop apps share, apart from their windows and
//! webviews: the settings ([`config`]), the servers and a proxy for each
//! ([`shell`]), and how a launch picks a server ([`launch`]).
//!
//! The Linux app (`apps/desktop-linux`, GTK 4 and WebKitGTK 6.0) and the
//! Windows one (`apps/desktop`, Tauri) each wrap these in their own window.

pub mod config;
pub mod gstreamer;
pub mod launch;
pub mod shell;

pub use config::{ConfigLock, Store};
pub use launch::{EarlyLaunches, SERVER_ENV, log_filter, server_arg};
pub use shell::{Problem, Save, Shell};
