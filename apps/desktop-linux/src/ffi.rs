//! The bridge between the app (Rust) and its window (Qt, in `cpp/`).
//!
//! Qt runs on the main thread, inside [`run`], which returns when the window
//! closes. The functions under "any thread" may be called from the runtime's
//! threads: each hands its work to the main thread, in the order called.
//! What Qt calls back (`extern "Rust"`) always runs on the main thread.

#[cxx::bridge]
pub mod qt {
    /// What the window starts with.
    struct Options {
        /// The app's ID: the window's Wayland app ID, which a desktop
        /// environment matches to the app's desktop entry.
        app_id: String,
        /// The first page.
        first_url: String,
        /// Where the web view keeps local storage and the like.
        storage_dir: String,
        /// Where it keeps its HTTP cache.
        cache_dir: String,
        /// Clear the HTTP cache before the first page loads.
        clear_cache: bool,
        /// Print pages' console messages (debug builds).
        console: bool,
        /// The window, in QML.
        qml: String,
        /// The script that passes the fallback page's commands on.
        relay_script: String,
        /// The Content-Security-Policy of the app's own pages.
        csp: String,
        /// The scheme of the app's own pages.
        scheme: String,
        /// Their origin.
        origin: String,
        /// Where the fallback page's commands are sent.
        bridge_url: String,
    }

    unsafe extern "C++" {
        include!("shell.h");

        /// Opens the window and runs Qt until it closes. Main thread.
        fn run(options: &Options) -> i32;

        /// Shows `message` in a window of its own, and returns once it's
        /// closed. For a launch that can't open the app's window; main
        /// thread, and not alongside [`run`].
        fn show_error(app_id: &str, message: &str);

        // Any thread.

        /// Loads `url` in the window.
        fn load(url: &str);

        /// Brings the window up, with the launch's activation token if it
        /// had one (empty otherwise).
        fn raise(activation_token: &str);

        /// Answers the fallback page's command `id` with `body` (JSON).
        fn bridge_reply(id: u64, body: &[u8]);

        /// Closes the window, which ends [`run`].
        fn quit();
    }

    extern "Rust" {
        /// The content type of the app's own page at `path` (`/index.html`),
        /// or nothing if there's no such page.
        fn app_page_type(path: &str) -> &'static str;

        /// That page's bytes.
        fn app_page_body(path: &str) -> &'static [u8];

        /// A command from a page (`body`, JSON), to answer with
        /// `bridge_reply(id, …)`. `initiator` is the origin that sent it and
        /// `page` the address the window shows.
        fn bridge_request(id: u64, initiator: &str, page: &str, body: &[u8]);

        /// The process that draws the page ended: `status` is Qt's
        /// `RenderProcessTerminationStatus`.
        fn render_terminated(status: i32, exit_code: i32);

        /// The address to hand to the default browser for a link that would
        /// open a new window, or nothing if it isn't one for the browser.
        fn browser_url(uri: &str) -> String;

        /// The HTTP cache clear that [`Options::clear_cache`] asked for has
        /// ended.
        fn cache_cleared(cleared: bool);

        /// The first page is on its way.
        fn started();
    }
}

use crate::app::App;
use crate::pages;

fn app_page_type(path: &str) -> &'static str {
    pages::find(path).map_or("", |page| page.content_type)
}

fn app_page_body(path: &str) -> &'static [u8] {
    pages::find(path).map_or(&[], |page| page.body)
}

fn bridge_request(id: u64, initiator: &str, page: &str, body: &[u8]) {
    match App::get() {
        Some(app) => app.on_request(id, initiator, page, body),
        None => crate::bridge::Reply::new(id).err(coversync_desktop_core::Problem::new(
            "Not ready",
            "The app is still starting.",
        )),
    }
}

fn render_terminated(status: i32, exit_code: i32) {
    if let Some(app) = App::get() {
        app.render_terminated(status, exit_code);
    }
}

fn browser_url(uri: &str) -> String {
    match coversync_desktop_core::links::for_browser(uri) {
        Some(url) => url.into(),
        None => {
            tracing::info!("not opening {uri}: only web links go to the browser");
            String::new()
        }
    }
}

fn cache_cleared(cleared: bool) {
    if let Some(app) = App::get() {
        app.cache_cleared(cleared);
    }
}

fn started() {
    if let Some(app) = App::get() {
        app.started();
    }
}
