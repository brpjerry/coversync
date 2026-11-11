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
        /// The app's icon as a file, for notifications on a desktop that has
        /// no icon by the app's ID; empty if there is none.
        icon_file: String,
    }

    /// A two-finger movement, as the window saw it: when (the device's time
    /// and the window's, in seconds) and how far, in angle units.
    #[derive(Clone, Copy)]
    struct Sample {
        time: f64,
        arrived: f64,
        x: f64,
        y: f64,
    }

    /// Momentum's settings (`cpp/momentum.h`), in seconds.
    struct Tuning {
        /// The coast's speed falls to 1/e in this time.
        tau: f64,
        /// How far back the speed on screen is weighted.
        recent: f64,
        /// The movement a lift's last report is taken to hold; 0 ignores its
        /// size.
        lift_lag: f64,
        /// How fast the coast moves from the speed on screen to the fingers'.
        blend: f64,
    }

    /// Whether a coast starts, from which speed on screen, towards which
    /// speed of the fingers'.
    struct CoastStart {
        starts: bool,
        shown_x: f64,
        shown_y: f64,
        fingers_x: f64,
        fingers_y: f64,
    }

    /// A coast under way: the fingers' speed, slowing; what the speed on
    /// screen had over or under it, fading; and the fraction of an angle unit
    /// not sent yet.
    struct CoastState {
        velocity_x: f64,
        velocity_y: f64,
        excess_x: f64,
        excess_y: f64,
        carry_x: f64,
        carry_y: f64,
    }

    /// What one step of a coast scrolls by, and whether it was the last.
    struct CoastStep {
        dx: i32,
        dy: i32,
        done: bool,
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

        /// Asks the user something over the page, in the window's own
        /// dialog: `message`, with a button that says `accept` and, unless
        /// `cancel` is empty, one that says `cancel`. `answered(id, …)`
        /// follows.
        fn ask(id: u64, message: &str, accept: &str, cancel: &str);

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

        /// The user answered the question `id` (see `ask`): with its accept
        /// button, or not.
        fn answered(id: u64, accepted: bool);

        /// Whether the page at `origin` may show notifications: asked when
        /// it asks for the permission.
        fn may_notify(origin: &str) -> bool;

        /// Whether a notification from the page at `origin` is shown now.
        /// `window_active` says whether the window has the focus.
        fn shows_notification(origin: &str, window_active: bool) -> bool;

        /// Whether the desktop has anything to show notifications with:
        /// said once, soon after the window opens.
        fn notifications_found(available: bool);

        // Momentum's arithmetic: see `crate::momentum`.

        fn momentum_looks_like_lift_flush(samples: &[Sample], dx: f64, dy: f64, now: f64) -> bool;
        /// `holding` says whether `(held_x, held_y)` is a report held back.
        fn momentum_coast_start(
            samples: &[Sample],
            holding: bool,
            held_x: f64,
            held_y: f64,
            now: f64,
            tuning: &Tuning,
        ) -> CoastStart;
        fn momentum_coast_state(start: &CoastStart) -> CoastState;
        fn momentum_coast_step(state: &mut CoastState, dt: f64, tuning: &Tuning) -> CoastStep;
    }
}

use crate::app::App;
use crate::{momentum, pages};

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

fn answered(id: u64, accepted: bool) {
    if let Some(app) = App::get() {
        app.answered(id, accepted);
    }
}

fn may_notify(origin: &str) -> bool {
    App::get().is_some_and(|app| app.may_notify(origin))
}

fn shows_notification(origin: &str, window_active: bool) -> bool {
    App::get().is_some_and(|app| app.shows_notification(origin, window_active))
}

fn notifications_found(available: bool) {
    if let Some(app) = App::get() {
        app.notifications_found(available);
    }
}

fn momentum_looks_like_lift_flush(samples: &[qt::Sample], dx: f64, dy: f64, now: f64) -> bool {
    momentum::looks_like_lift_flush(samples, dx, dy, now)
}

fn momentum_coast_start(
    samples: &[qt::Sample],
    holding: bool,
    held_x: f64,
    held_y: f64,
    now: f64,
    tuning: &qt::Tuning,
) -> qt::CoastStart {
    momentum::coast_start(samples, holding.then_some((held_x, held_y)), now, tuning)
}

fn momentum_coast_state(start: &qt::CoastStart) -> qt::CoastState {
    momentum::coast_state(start)
}

fn momentum_coast_step(state: &mut qt::CoastState, dt: f64, tuning: &qt::Tuning) -> qt::CoastStep {
    momentum::coast_step(state, dt, tuning)
}
