//! What the agent keeps on this computer: see
//! [`coversync_desktop_core::sync`], where the app's settings page finds it
//! too.

pub use coversync_desktop_core::sync::{
    Config, VERSION, file, folder, path, same_server, server_url, state_file, temporary,
    write_whole,
};
