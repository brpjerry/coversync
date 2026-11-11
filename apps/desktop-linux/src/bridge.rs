//! How the fallback page calls the app's commands.
//!
//! The page side is `ui/bridge.js`: it sends each command as an event. A script of the app's own
//! (`relay.js`), which the window runs in a script world apart from the
//! page's, passes them on as POSTs to the app's own origin
//! ([`crate::pages::BRIDGE`]) and the answers back. Only the app's pages can
//! send those: the scheme is local, so a server's pages can't reach it, and
//! [`allowed`] checks each request's origin and the page the window shows
//! regardless.

use serde::{Deserialize, Serialize};

use coversync_desktop_core::Problem;

use crate::ffi::qt;
use crate::pages;

/// A command from the fallback page.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", content = "args", rename_all = "snake_case")]
pub enum Request {
    Status {},
    Connect { server: Option<String> },
}

impl Request {
    pub fn read(body: &[u8]) -> Result<Self, Problem> {
        serde_json::from_slice(body)
            .map_err(|err| Problem::new("Not a command the app knows", err.to_string()))
    }
}

/// Whether a command may run: sent from the app's own origin, while the
/// window shows one of the app's pages.
pub fn allowed(initiator: &str, page: &str) -> bool {
    initiator.trim_end_matches('/') == pages::ORIGIN && pages::is_app_page(page)
}

/// Where a command's answer goes: the page's pending call. Any thread.
pub struct Reply {
    id: u64,
}

impl Reply {
    pub fn new(id: u64) -> Self {
        Self { id }
    }

    pub fn ok(self, value: impl Serialize) {
        self.send(&serde_json::json!({ "ok": true, "value": value }));
    }

    pub fn err(self, problem: Problem) {
        self.send(&serde_json::json!({ "ok": false, "error": problem }));
    }

    fn send(self, answer: &serde_json::Value) {
        qt::bridge_reply(self.id, answer.to_string().as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_pages_commands() {
        let read = |json: &str| Request::read(json.as_bytes());
        assert!(matches!(
            read(r#"{"cmd":"status","args":{}}"#),
            Ok(Request::Status {})
        ));
        assert!(matches!(
            read(r#"{"cmd":"connect","args":{"server":null}}"#),
            Ok(Request::Connect { server: None })
        ));
        assert!(matches!(
            read(r#"{"cmd":"connect","args":{"server":"https://a/"}}"#),
            Ok(Request::Connect { server: Some(s) }) if s == "https://a/"
        ));
        assert!(read(r#"{"cmd":"rm","args":{}}"#).is_err());
        assert!(read("").is_err());
    }

    #[test]
    fn only_the_apps_own_pages_may_send_commands() {
        let page = "coversync://app/index.html";
        assert!(allowed("coversync://app", page));
        assert!(allowed("coversync://app/", page));
        // A server's page, whatever the window shows.
        assert!(!allowed("http://127.0.0.1:40000", page));
        assert!(!allowed("https://coversync.example.com", page));
        assert!(!allowed("", page));
        assert!(!allowed("null", page));
        assert!(!allowed("coversync://other", page));
        // The app's origin, but the window has moved on to a server's page.
        assert!(!allowed("coversync://app", "http://127.0.0.1:40000/"));
    }
}
