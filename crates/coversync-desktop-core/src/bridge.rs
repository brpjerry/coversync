//! What the app offers a server's UI: the commands behind the proxies'
//! `/__coversync-app/v1/` path (see [`coversync_proxy::AppCommands`], which
//! decides who may ask).
//!
//! The interface is the same whichever app implements it, and it only grows:
//! a command or a feature is added, never changed, so an older app works
//! with a newer UI and the other way round. The UI asks `capabilities` once
//! and uses only what's listed; an app asked for a command it doesn't have
//! says so (`unknown_command`).
//!
//! Every command has to be safe for any script on the server's page: see
//! [`coversync_proxy::AppCommands`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use coversync_proxy::{AppAnswer, AppCommands, AppError, Upstream};
use serde::Serialize;
use serde_json::{Value, json};

/// The interface's version: `capabilities` says it, and the path carries it.
pub const VERSION: u32 = 1;

/// Something the app can do that a browser can't, or doesn't unasked. The
/// UI shows what depends on one only when `capabilities` lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Feature {
    /// The page's notifications (the web's `Notification`) are shown as the
    /// desktop's, without asking the user first.
    Notify,
}

/// The app's features, once it knows them.
pub type Features = Pin<Box<dyn Future<Output = Vec<Feature>> + Send>>;

/// The app around the window, as the commands see it.
pub trait Host: Send + Sync {
    /// What this app can do now, on this desktop.
    fn features(&self) -> Features;
}

/// The commands, for an app that is `host`.
pub struct Commands {
    host: Arc<dyn Host>,
}

impl Commands {
    pub fn new(host: Arc<dyn Host>) -> Self {
        Self { host }
    }
}

impl AppCommands for Commands {
    fn call(&self, _server: &Upstream, command: &str, _args: Value) -> AppAnswer {
        match command {
            "capabilities" => {
                let features = self.host.features();
                Box::pin(
                    async move { Ok(json!({ "version": VERSION, "features": features.await })) },
                )
            }
            _ => Box::pin(async { Err(AppError::Unknown) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<Feature>);

    impl Host for Fixed {
        fn features(&self) -> Features {
            let features = self.0.clone();
            Box::pin(async move { features })
        }
    }

    fn ask(features: Vec<Feature>, command: &str) -> Result<Value, AppError> {
        let commands = Commands::new(Arc::new(Fixed(features)));
        let server = Upstream::parse("https://music.example/coversync/").unwrap();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(commands.call(&server, command, json!({})))
    }

    #[test]
    fn capabilities_lists_the_version_and_the_features() {
        assert_eq!(
            ask(vec![Feature::Notify], "capabilities"),
            Ok(json!({ "version": 1, "features": ["notify"] }))
        );
        // An app that can't notify here, or an older one.
        assert_eq!(
            ask(Vec::new(), "capabilities"),
            Ok(json!({ "version": 1, "features": [] }))
        );
    }

    #[test]
    fn the_fallback_pages_commands_arent_among_them() {
        for command in ["connect", "status", "pair", "upload"] {
            assert_eq!(
                ask(vec![Feature::Notify], command),
                Err(AppError::Unknown),
                "{command}"
            );
        }
    }
}
