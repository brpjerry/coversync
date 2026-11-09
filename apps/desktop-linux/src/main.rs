//! The coversync desktop app for Linux: one GTK 4 window onto a coversync
//! server's UI, in WebKitGTK 6.0.
//!
//! The UI comes from the server, through a proxy on 127.0.0.1
//! (`coversync-proxy`) that streams everything both ways and keeps the
//! server's path prefix; each server gets its own (see
//! [`coversync_desktop_core::shell`]). When no server is set, it can't be
//! reached, or the page crashes, the window shows the app's own fallback
//! page ([`scheme`]), where the server can be retried or changed. One
//! instance runs at a time: launching it again brings its window up.
//!
//! Windows has an app of its own (`apps/desktop`, Tauri). This one exists
//! because WebKitGTK 4.1, which Tauri uses on Linux, draws at 60 frames a
//! second at most; this one follows the display (120 Hz and up).

mod app;
mod bridge;
mod pacing;
mod scheme;

use std::cell::RefCell;
use std::rc::Rc;

use coversync_desktop_core::{SERVER_ENV, log_filter};
use gtk::gio::ApplicationFlags;
use gtk::glib;
use gtk::prelude::*;

use app::{APP_ID, App};

/// WebKit's switch for painting with Skia's deferred display lists.
const SKIA_DDL_ENV: &str = "WEBKIT_SKIA_ENABLE_DDL";

fn main() -> glib::ExitCode {
    // With deferred display lists (WebKit's default), tiles are recorded on
    // worker threads but drawn, and the images in them uploaded, on the
    // thread that composites each frame; scrolling into new rows of covers
    // then ran that thread past the frame. Without, GPU worker threads paint
    // the tiles. Two-finger drags over the album grid on a 120 Hz laptop:
    // 8-14% of frames late with them, 0-1% without. The CPU time is the same;
    // the web process holds about 400 MiB more GPU memory (770-810 MiB).
    // Set before anything else runs, so the web processes inherit it.
    if std::env::var_os(SKIA_DDL_ENV).is_none() {
        // SAFETY: no other thread has started yet.
        unsafe { std::env::set_var(SKIA_DDL_ENV, "0") };
    }
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(&std::env::var("RUST_LOG").unwrap_or_default()))
        .with_writer(std::io::stderr)
        .init();
    // The proxies and the servers' checks run here; GTK on the main thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("coversync-proxy")
        .enable_all()
        .build()
        .expect("the app's runtime");

    // One instance: a second launch hands its arguments (and environment)
    // to the first over D-Bus and ends.
    let gtk_app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(ApplicationFlags::HANDLES_COMMAND_LINE | ApplicationFlags::SEND_ENVIRONMENT)
        .build();
    let app: Rc<RefCell<Option<Rc<App>>>> = Rc::default();
    let handle = runtime.handle().clone();
    gtk_app.connect_command_line(move |gtk_app, line| {
        let args: Vec<String> = line
            .arguments()
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let env_server = line
            .getenv(SERVER_ENV)
            .map(String::from)
            .filter(|server| !server.is_empty());
        let running = app.borrow().clone();
        if let Some(running) = running {
            running.launched_again(args, env_server);
            return glib::ExitCode::SUCCESS;
        }
        match App::start(gtk_app, handle.clone(), args, env_server) {
            Ok(started) => {
                *app.borrow_mut() = Some(started);
                glib::ExitCode::SUCCESS
            }
            Err(message) => {
                tracing::error!("{message}");
                app::show_error(gtk_app, &message);
                glib::ExitCode::FAILURE
            }
        }
    });
    gtk_app.run()
}
