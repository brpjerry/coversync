//! `coversync`, the command for a computer that keeps a copy of the Library.
//!
//! `coversync run` is the sync agent: a background service that runs the
//! sync engine (`coversync-replica`, the same one every client runs)
//! whenever the server has something for this computer's folder. It needs
//! no app open. [`agent`] is its loop, [`system`] what it learns from the
//! desktop (a metered network, battery power, waking from sleep), and
//! [`config`] the three things kept on this computer: the server, the
//! folder and where the token is. Every other setting is the server's, set
//! in Sync setup.

pub mod agent;
pub mod config;
pub mod notify;
pub mod system;
pub mod tokens;

pub use coversync_desktop_core::APP_ID;
