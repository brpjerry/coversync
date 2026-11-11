//! The coversync desktop app for Linux: one Qt 6 window onto a coversync
//! server's UI, in Qt WebEngine (Chromium).
//!
//! The UI comes from the server, through a proxy on 127.0.0.1
//! (`coversync-proxy`) that streams everything both ways and keeps the
//! server's path prefix; each server gets its own (see
//! [`coversync_desktop_core::shell`]). When no server is set, it can't be
//! reached, or the page's process ends, the window shows the app's own
//! fallback page ([`pages`]), where the server can be retried or changed.
//! One instance runs at a time: launching it again brings its window up
//! ([`instance`]).
//!
//! The window itself is Qt's, in C++ and QML (`cpp/`, `window.qml`); Rust
//! reaches it through [`ffi`].

mod app;
mod bridge;
mod ffi;
mod instance;
mod momentum;
mod pages;
mod paths;

use std::process::ExitCode;

use coversync_desktop_core::{SERVER_ENV, log_filter};

use app::{APP_ID, App};
use ffi::qt;
use instance::{Claim, Launch};

/// Chromium's command line, as Qt WebEngine takes it.
const CHROMIUM_FLAGS_ENV: &str = "QTWEBENGINE_CHROMIUM_FLAGS";
/// `unlimited` takes Chromium's own frame pacing away: see [`chromium_flags`].
const FRAME_RATE_ENV: &str = "COVERSYNC_FRAME_RATE";
const NO_FRAME_LIMIT: &str = "--disable-frame-rate-limit";

/// Qt WebEngine never tells Chromium the display's refresh rate, so
/// Chromium paces pages at 60 frames a second whatever the display does
/// (QTBUG-123640). A Qt WebEngine patched to pass the rate on
/// (`patches/qt6-webengine-use-the-screens-refresh-rate.patch`) draws at the
/// display's; that is what the app is meant to run on.
///
/// On an unpatched one, `COVERSYNC_FRAME_RATE=unlimited` takes Chromium's
/// limit away: frames are then drawn as the window asks for them, at the
/// display's rate (on a 120 Hz laptop the album grid scrolls at 110-121
/// frames a second, against 52-61). It is for trying scrolling only: with
/// it, a view transition never finishes and the page stops being redrawn,
/// which is what opening an album does; and Chromium draws as fast as it
/// can while a page animates (about 120% of a core during a scroll, against
/// 70%).
fn chromium_flags(set: Option<String>, frame_rate: Option<String>) -> Option<String> {
    let set = set.unwrap_or_default();
    if frame_rate.as_deref() != Some("unlimited") || set.contains(NO_FRAME_LIMIT) {
        return None;
    }
    Some(format!("{set} {NO_FRAME_LIMIT}").trim().to_owned())
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn main() -> ExitCode {
    // Before anything else runs, so Qt WebEngine's processes get it.
    if let Some(flags) = chromium_flags(env(CHROMIUM_FLAGS_ENV), env(FRAME_RATE_ENV)) {
        // SAFETY: no other thread has started yet.
        unsafe { std::env::set_var(CHROMIUM_FLAGS_ENV, flags) };
    }
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(&std::env::var("RUST_LOG").unwrap_or_default()))
        .with_writer(std::io::stderr)
        .init();

    // Lossily: an argument that isn't UTF-8 (the program's own path, say)
    // mustn't keep the app from starting.
    let args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let env_server = env(SERVER_ENV);

    // One instance: a later launch hands its arguments (and environment) to
    // the first and ends.
    let socket = instance::socket_path(&paths::config_dir());
    let claim = match &socket {
        Some(socket) => instance::claim(socket),
        None => Err(std::io::Error::other(
            "no XDG_RUNTIME_DIR for the socket that hands launches over",
        )),
    };
    let mut listening = match claim {
        Ok(Claim::First(listening)) => Some(listening),
        Ok(Claim::Running(stream)) => {
            let launch = Launch {
                args,
                env_server,
                activation_token: env("XDG_ACTIVATION_TOKEN"),
            };
            return match instance::hand_over(stream, &launch) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    let message =
                        format!("coversync is already open, but didn't take this launch: {err}");
                    tracing::error!("{message}");
                    qt::show_error(APP_ID, &message);
                    ExitCode::FAILURE
                }
            };
        }
        // Without it the settings' lock still keeps a second instance out.
        Err(err) => {
            tracing::warn!("later launches won't reach this window: {err}");
            None
        }
    };

    // The proxies, the servers' checks and the later launches run here; Qt on
    // the main thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("coversync-proxy")
        .enable_all()
        .build()
        .expect("the app's runtime");

    let start = match App::start(runtime.handle().clone(), args, env_server) {
        Ok(start) => start,
        Err(message) => {
            tracing::error!("{message}");
            drop(listening);
            qt::show_error(APP_ID, &message);
            return ExitCode::FAILURE;
        }
    };
    if let (Some(listening), Some(app)) = (listening.as_mut(), App::get()) {
        listening.serve(runtime.handle(), |launch| app.launched_again(launch));
    }
    // Asked to end (a session logging out, Ctrl+C): close the window, so the
    // run ends as it does when the user closes it.
    runtime.spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
            signal(SignalKind::hangup()),
        ) else {
            return;
        };
        loop {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
                _ = hup.recv() => {}
            }
            // Kept until the window is there to close, if it isn't yet.
            qt::quit();
        }
    });

    let options = qt::Options {
        app_id: APP_ID.to_owned(),
        first_url: start.first_url,
        storage_dir: paths::storage_dir().to_string_lossy().into_owned(),
        cache_dir: paths::cache_dir().to_string_lossy().into_owned(),
        clear_cache: start.clear_cache,
        // Pages' console messages on stdout.
        console: cfg!(debug_assertions),
        qml: include_str!("window.qml").to_owned(),
        relay_script: include_str!("relay.js").to_owned(),
        csp: pages::CSP.to_owned(),
        scheme: pages::SCHEME.to_owned(),
        origin: pages::ORIGIN.to_owned(),
        bridge_url: pages::BRIDGE.to_owned(),
        icon_file: pages::icon_file(&paths::icon_file())
            .map(|file| file.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let status = qt::run(&options);
    drop(listening);
    ExitCode::from(u8::try_from(status).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_chromiums_frame_limit_away_only_when_asked() {
        let flags = |set: Option<&str>, rate: Option<&str>| {
            chromium_flags(set.map(String::from), rate.map(String::from))
        };
        assert_eq!(flags(None, None), None);
        assert_eq!(flags(Some("--enable-logging"), None), None);
        assert_eq!(flags(None, Some("display")), None);
        assert_eq!(
            flags(None, Some("unlimited")).as_deref(),
            Some(NO_FRAME_LIMIT)
        );
        assert_eq!(
            flags(Some("--enable-logging"), Some("unlimited")).as_deref(),
            Some("--enable-logging --disable-frame-rate-limit")
        );
        // Already there: the variable stays as it is.
        assert_eq!(
            flags(Some("--disable-frame-rate-limit"), Some("unlimited")),
            None
        );
    }
}
