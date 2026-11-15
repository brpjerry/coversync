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
    /// The app has a settings page of its own (where this computer's copy
    /// of the Library is kept, say), which `open-settings` shows over the
    /// server's page.
    Settings,
}

/// The app's features, once it knows them.
pub type Features = Pin<Box<dyn Future<Output = Vec<Feature>> + Send>>;

/// The app around the window, as the commands see it.
pub trait Host: Send + Sync {
    /// What this app can do now, on this desktop.
    fn features(&self) -> Features;

    /// Shows the app's own settings page, over the server's. The page
    /// asking learns nothing from it and can't reach into it: what's on it
    /// is for the app's user alone.
    fn open_settings(&self);
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
            "open-settings" => {
                self.host.open_settings();
                Box::pin(async { Ok(json!({})) })
            }
            _ => Box::pin(async { Err(AppError::Unknown) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An app with these features, counting how often its settings were
    /// opened.
    struct Fixed(Vec<Feature>, AtomicUsize);

    impl Host for Fixed {
        fn features(&self) -> Features {
            let features = self.0.clone();
            Box::pin(async move { features })
        }

        fn open_settings(&self) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn ask(features: Vec<Feature>, command: &str) -> Result<Value, AppError> {
        ask_app(&Arc::new(Fixed(features, AtomicUsize::new(0))), command)
    }

    fn ask_app(app: &Arc<Fixed>, command: &str) -> Result<Value, AppError> {
        let commands = Commands::new(app.clone());
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

    // Bugs caught: 0, False positives: 0
    #[test]
    fn open_settings_shows_the_page_and_tells_the_asker_nothing() {
        let app = Arc::new(Fixed(
            vec![Feature::Notify, Feature::Settings],
            AtomicUsize::new(0),
        ));
        assert_eq!(
            ask_app(&app, "capabilities"),
            Ok(json!({ "version": 1, "features": ["notify", "settings"] }))
        );
        assert_eq!(app.1.load(Ordering::Relaxed), 0);
        assert_eq!(ask_app(&app, "open-settings"), Ok(json!({})));
        assert_eq!(app.1.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_fallback_pages_commands_arent_among_them() {
        for command in [
            "connect",
            "status",
            "pair",
            "upload",
            "sync_status",
            "sync_turn",
            "sync_choose_folder",
        ] {
            assert_eq!(
                ask(vec![Feature::Notify], command),
                Err(AppError::Unknown),
                "{command}"
            );
        }
    }
}
