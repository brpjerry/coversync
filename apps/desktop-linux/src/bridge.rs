//! How the fallback page calls the app's commands.
//!
//! The message handler "coversync" lives in a script world of its own,
//! where only the app's script (`bridge.js`) runs, and only on the app's own
//! pages: a server's pages, in the page's own world, can't see either. The
//! script passes the page's requests on and the answers back (the page side
//! is `apps/desktop/ui/bridge.js`, shared with the Windows app).

use serde::{Deserialize, Serialize};
use webkit::javascriptcore;

use coversync_desktop_core::Problem;

const HANDLER: &str = "coversync";
const WORLD: &str = "coversync";
const SCRIPT: &str = include_str!("bridge.js");

/// A command from the fallback page.
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", content = "args", rename_all = "snake_case")]
pub enum Request {
    Status {},
    Connect { server: Option<String> },
}

/// Where a command's answer goes: the page's pending call.
pub struct Reply {
    reply: webkit::ScriptMessageReply,
    context: Option<javascriptcore::Context>,
}

impl Reply {
    pub fn ok(self, value: impl Serialize) {
        self.send(&serde_json::json!({ "ok": true, "value": value }));
    }

    pub fn err(self, problem: Problem) {
        self.send(&serde_json::json!({ "ok": false, "error": problem }));
    }

    fn send(self, answer: &serde_json::Value) {
        match &self.context {
            Some(context) => {
                let value = javascriptcore::Value::from_json(context, &answer.to_string());
                self.reply.return_value(&value);
            }
            None => self.reply.return_error_message("The page went away."),
        }
    }
}

/// Sets up `content` (the view's) for the fallback page: `on_request` gets
/// each command it sends, with where to answer.
pub fn install(
    content: &webkit::UserContentManager,
    on_request: impl Fn(Request, Reply) + 'static,
) {
    if !content.register_script_message_handler_with_reply(HANDLER, Some(WORLD)) {
        tracing::error!("couldn't register the fallback page's message handler");
    }
    content.connect_script_message_with_reply_received(Some(HANDLER), move |_, value, reply| {
        let reply = Reply {
            reply: reply.clone(),
            context: value.context(),
        };
        match serde_json::from_str::<Request>(&value.to_str()) {
            Ok(request) => on_request(request, reply),
            Err(err) => reply.err(Problem::new("Not a command the app knows", err.to_string())),
        }
        true
    });
    content.add_script(&webkit::UserScript::for_world(
        SCRIPT,
        webkit::UserContentInjectedFrames::TopFrame,
        webkit::UserScriptInjectionTime::Start,
        WORLD,
        &["coversync://*/*"],
        &[],
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_pages_commands() {
        let read = |json: &str| serde_json::from_str::<Request>(json);
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
    }
}
