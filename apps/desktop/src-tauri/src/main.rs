//! The coversync desktop app: one window onto a coversync server's UI.
//!
//! The UI comes from the server, through a proxy on 127.0.0.1
//! (`coversync-proxy`) that streams everything both ways and keeps the
//! server's path prefix; each server gets its own (see [`shell`]). When no
//! server is set, it can't be reached, or the page crashes, the window shows
//! the bundled fallback page (`../ui`), where the server can be retried or
//! changed. One instance runs at a time: launching it again brings its
//! window up.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
#[cfg(target_os = "linux")]
mod gstreamer;
mod shell;

use std::error::Error;
use std::sync::{Arc, Mutex};

use coversync_proxy::{Cookies, Proxy, Upstream};
use serde::Serialize;
use tauri::{AppHandle, Manager, State, Url, WebviewUrl, WebviewWindowBuilder};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

use config::{ConfigLock, Store};
use shell::{Problem, Save, Shell};

/// The window's label, which the capabilities name.
const WINDOW: &str = "main";
/// The bundled fallback page.
const FALLBACK: &str = "index.html";
/// A server for this run only, like `--server`. Neither is saved.
const SERVER_ENV: &str = "COVERSYNC_SERVER";

/// What the fallback page shows.
#[derive(Serialize)]
struct Status {
    /// The server's URL, if one is set.
    server: Option<String>,
    /// Why the server's UI isn't showing.
    problem: Option<Problem>,
}

/// The commands are for the bundled pages. Tauri already treats the
/// proxies' origins as remote and the capability lists no remote URLs, so a
/// server's UI can't call them; this refuses them there regardless, whatever
/// the build.
fn bundled_only(webview: &tauri::Webview, shell: &Shell) -> Result<(), Problem> {
    let page = webview
        .url()
        .map_err(|err| Problem::new("Couldn't tell which page asked", err.to_string()))?;
    if shell.is_proxied(&page) {
        return Err(Problem::new(
            "Not allowed",
            "A server's pages can't use the app's own commands.",
        ));
    }
    Ok(())
}

#[tauri::command]
fn status(webview: tauri::Webview, shell: State<'_, Shell>) -> Result<Status, Problem> {
    bundled_only(&webview, &shell)?;
    Ok(Status {
        server: shell
            .current()
            .and_then(|proxy| proxy.upstream())
            .map(|upstream| upstream.url()),
        problem: shell.problem(),
    })
}

/// Connects to `server`, or with `None` tries the current one again, and
/// opens its UI once it's checked (see [`Shell::switch`]).
#[tauri::command]
async fn connect(
    app: AppHandle,
    webview: tauri::Webview,
    shell: State<'_, Shell>,
    server: Option<String>,
) -> Result<(), Problem> {
    bundled_only(&webview, &shell)?;
    let _navigation = shell.navigation.lock().await;
    let proxy = shell.switch(server, Save::Yes).await?;
    open_ui(&app, &proxy)
        .map_err(|err| Problem::new("Couldn't open the server's UI", err.to_string()))
}

/// Sends the window through the proxy's entry URL to its server's UI.
fn open_ui(app: &AppHandle, proxy: &Proxy) -> tauri::Result<()> {
    let url = Url::parse(&proxy.entry_url()).expect("the entry URL is valid");
    match app.get_webview_window(WINDOW) {
        Some(window) => window.navigate(url),
        None => Ok(()),
    }
}

/// Shows the fallback page, saying what went wrong.
fn show_fallback(app: &AppHandle, problem: Problem) {
    let Some(shell) = app.try_state::<Shell>() else {
        return;
    };
    shell.set_problem(Some(problem));
    if let Some(window) = app.get_webview_window(WINDOW)
        && let Err(err) = window.navigate(shell.fallback.clone())
    {
        tracing::error!("couldn't show the fallback page: {err}");
    }
}

/// Launches that came in before setup finished, to handle once it has.
#[derive(Default)]
struct EarlyLaunches {
    /// Set once the shell is managed; until then, launches wait here.
    ready: bool,
    waiting: Vec<Vec<String>>,
}

/// Another launch of the app: bring the window up, and show the server it
/// was given with `--server`, if any, for this run.
fn launched_again(app: &AppHandle, args: Vec<String>) {
    {
        let early = app.state::<Mutex<EarlyLaunches>>();
        let mut early = early.lock().expect("early launches lock");
        if !early.ready {
            early.waiting.push(args);
            return;
        }
    }
    raise_window(app, activation_token(&args));
    let Some(server) = server_arg(args.into_iter().skip(1)) else {
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(shell) = app.try_state::<Shell>() else {
            return;
        };
        let _navigation = shell.navigation.lock().await;
        match shell.switch(Some(server), Save::No).await {
            Ok(proxy) => {
                if let Err(err) = open_ui(&app, &proxy) {
                    tracing::error!("couldn't open the server's UI: {err}");
                }
            }
            Err(problem) => show_fallback(&app, problem),
        }
    });
}

/// The argument main() adds to carry a launch's activation token (see
/// [`carry_activation_token`]).
const ACTIVATION_ARG: &str = "--activation-token=";

fn activation_token(args: &[String]) -> Option<String> {
    args.iter()
        .find_map(|arg| arg.strip_prefix(ACTIVATION_ARG))
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

/// Brings the window up. With the launch's activation token (Linux), the
/// compositor can raise and focus it: GNOME and KDE do. Without one, or
/// where the compositor won't (Hyprland with `focus_on_activate` off), it
/// may only be marked as wanting attention.
fn raise_window(app: &AppHandle, token: Option<String>) {
    let Some(window) = app.get_webview_window(WINDOW) else {
        return;
    };
    let _ = window.unminimize();
    let _ = window.show();
    #[cfg(target_os = "linux")]
    if let Some(token) = token {
        let window = window.clone();
        let shown = app.run_on_main_thread(move || {
            use gtk::prelude::GtkWindowExt;
            match window.gtk_window() {
                Ok(gtk_window) => {
                    gtk_window.set_startup_id(&token);
                    gtk_window.present();
                }
                Err(err) => tracing::warn!("couldn't raise the window: {err}"),
            }
        });
        if shown.is_ok() {
            return;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = token;
    let _ = window.set_focus();
}

/// The single-instance plugin hands a second launch's arguments to the
/// running app, but not its environment, where the desktop puts the
/// launch's activation token (`XDG_ACTIVATION_TOKEN` on Wayland,
/// `DESKTOP_STARTUP_ID` on X11). Without the token, the running app may not
/// raise its window. So a launch with a token starts itself again with the
/// token as an argument too, before anything else runs.
#[cfg(target_os = "linux")]
fn carry_activation_token() {
    use std::os::unix::process::CommandExt;

    let token = std::env::var("XDG_ACTIVATION_TOKEN")
        .or_else(|_| std::env::var("DESKTOP_STARTUP_ID"))
        .ok()
        .filter(|token| !token.is_empty());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(token) = token else {
        return;
    };
    if activation_token(&args).is_some() {
        return;
    }
    let Ok(program) = std::env::current_exe() else {
        return;
    };
    let err = std::process::Command::new(program)
        .args(&args)
        .arg(format!("{ACTIVATION_ARG}{token}"))
        .exec();
    tracing::warn!("couldn't pass the activation token on: {err}");
}

/// Watches WebKitGTK's web process, which draws the page: when it crashes,
/// the window would otherwise keep its last frame and never come back.
#[cfg(target_os = "linux")]
fn watch_web_process(window: &tauri::WebviewWindow) -> tauri::Result<()> {
    use webkit2gtk::{WebProcessTerminationReason, WebViewExt};

    let app = window.app_handle().clone();
    window.with_webview(move |webview| {
        webview
            .inner()
            .connect_web_process_terminated(move |_, reason| {
                let message = match reason {
                    WebProcessTerminationReason::Crashed => crash_message(),
                    WebProcessTerminationReason::ExceededMemoryLimit => {
                        "The web process that shows the page used too much memory and was \
                         stopped."
                            .to_owned()
                    }
                    // The app closing its own webview.
                    _ => return,
                };
                tracing::error!("{message}");
                show_fallback(&app, Problem::new("The page stopped working", message));
            });
    })
}

/// WebView2 and WKWebView aren't watched yet.
#[cfg(not(target_os = "linux"))]
fn watch_web_process(_window: &tauri::WebviewWindow) -> tauri::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn crash_message() -> String {
    let mut message = String::from("The web process that shows the page crashed.");
    if gstreamer::autoaudiosink_missing() {
        message.push_str(
            " GStreamer's autoaudiosink isn't installed, and WebKitGTK crashes without it \
             when audio starts playing. Install GStreamer's good plugins (gst-plugins-good \
             on Arch, gstreamer1.0-plugins-good on Debian and Ubuntu), then try again.",
        );
    }
    message
}

/// Tauri serves the bundled files from `tauri://localhost`
/// (`http://tauri.localhost` on Windows), or from its dev server under
/// `tauri dev`.
fn fallback_url(app: &tauri::App) -> Result<Url, Box<dyn Error>> {
    let base = match &app.config().build.dev_url {
        Some(url) if tauri::is_dev() => url.clone(),
        _ if cfg!(windows) => Url::parse("http://tauri.localhost/")?,
        _ => Url::parse("tauri://localhost/")?,
    };
    Ok(base.join(FALLBACK)?)
}

/// `--server <url>` or `--server=<url>`, from the arguments after the
/// program's name.
fn server_arg(args: impl IntoIterator<Item = String>) -> Option<String> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--server" {
            return args.next();
        }
        if let Some(url) = arg.strip_prefix("--server=") {
            return Some(url.to_owned());
        }
    }
    None
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "linux")]
    if gstreamer::autoaudiosink_missing() {
        tracing::warn!(
            "GStreamer's autoaudiosink isn't installed (gst-plugins-good): WebKitGTK crashes \
             when audio starts playing"
        );
    }

    let config_dir = app.path().app_config_dir()?;
    // Before anything reads the settings or the cookies.
    let lock = match ConfigLock::take(&config_dir) {
        Ok(lock) => lock,
        Err(message) => {
            tracing::error!("{message}");
            show_error(&message);
            std::process::exit(1);
        }
    };
    app.manage(lock);
    let config = Store::open(config_dir.clone());
    let clear_cache = config.had_shared_port();
    let saved = config.get();
    let server = server_arg(std::env::args().skip(1))
        .or_else(|| std::env::var(SERVER_ENV).ok().filter(|s| !s.is_empty()))
        .or(saved.server);

    let handle = app.handle().clone();
    let shell = Shell::new(
        config,
        Cookies::open(Some(config_dir.join("cookies.json"))),
        fallback_url(app)?,
        // The proxy logs it.
        Arc::new(move |unreachable| show_fallback(&handle, Problem::unreachable(unreachable))),
    );
    // Straight to the server's UI when there's a server; the fallback page
    // takes over if it can't be reached.
    let url = match server.as_deref().map(|url| (url, Upstream::parse(url))) {
        Some((_, Ok(upstream))) => {
            let proxy = tauri::async_runtime::block_on(shell.proxy_for(&upstream))?;
            shell.show(proxy.clone());
            WebviewUrl::External(Url::parse(&proxy.entry_url())?)
        }
        Some((address, Err(err))) => {
            shell.set_problem(Some(Problem::invalid(err, address)));
            WebviewUrl::App(FALLBACK.into())
        }
        None => WebviewUrl::App(FALLBACK.into()),
    };
    app.manage(shell);
    let first = if clear_cache {
        WebviewUrl::External(Url::parse("about:blank")?)
    } else {
        url.clone()
    };
    let window = WebviewWindowBuilder::new(app, WINDOW, first)
        .title("coversync")
        .inner_size(1280.0, 860.0)
        .min_inner_size(720.0, 480.0)
        .build()?;
    watch_web_process(&window)?;
    if clear_cache {
        clear_http_cache_then(&window, url);
    }

    // Launches that arrived while this ran.
    let waiting = {
        let early = app.state::<Mutex<EarlyLaunches>>();
        let mut early = early.lock().expect("early launches lock");
        early.ready = true;
        std::mem::take(&mut early.waiting)
    };
    for args in waiting {
        launched_again(app.handle(), args);
    }
    Ok(())
}

/// The first builds served every server from one port, so the webview's
/// HTTP cache may hold several servers' answers under that origin. It's
/// cleared once, when their settings are read, before the first page loads;
/// local storage and cookies stay. WebKitGTK only: no other webview ran
/// those builds.
fn clear_http_cache_then(window: &tauri::WebviewWindow, url: WebviewUrl) {
    let go = {
        let window = window.clone();
        move || {
            let target = match &url {
                WebviewUrl::External(url) => Some(url.clone()),
                _ => window
                    .app_handle()
                    .try_state::<Shell>()
                    .map(|shell| shell.fallback.clone()),
            };
            if let Some(target) = target
                && let Err(err) = window.navigate(target)
            {
                tracing::error!("couldn't open the first page: {err}");
            }
        }
    };
    #[cfg(target_os = "linux")]
    {
        use webkit2gtk::{WebViewExt, WebsiteDataManagerExtManual, WebsiteDataTypes};

        let cleared =
            window.with_webview(
                move |webview| match webview.inner().website_data_manager() {
                    Some(manager) => manager.clear(
                        WebsiteDataTypes::DISK_CACHE | WebsiteDataTypes::MEMORY_CACHE,
                        webkit2gtk::glib::TimeSpan(0),
                        None::<&webkit2gtk::gio::Cancellable>,
                        move |result| {
                            match result {
                                Ok(()) => {
                                    tracing::info!("cleared the HTTP cache the servers shared")
                                }
                                Err(err) => tracing::warn!("couldn't clear the HTTP cache: {err}"),
                            }
                            go();
                        },
                    ),
                    None => go(),
                },
            );
        if let Err(err) = cleared {
            tracing::warn!("couldn't clear the HTTP cache: {err}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    go();
}

/// Shows `message` in a dialog of its own, before any window: on Linux,
/// with GTK. Elsewhere it's only logged.
fn show_error(message: &str) {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::{DialogExt, GtkWindowExt};

        let dialog = gtk::MessageDialog::new(
            None::<&gtk::Window>,
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Error,
            gtk::ButtonsType::Close,
            message,
        );
        dialog.set_title("coversync");
        dialog.run();
        dialog.close();
        while gtk::events_pending() {
            gtk::main_iteration();
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = message;
}

/// Warnings from everything, with `RUST_LOG`'s directives on top: so
/// `RUST_LOG=coversync_proxy=debug` adds the proxy's requests without hiding
/// the app's own warnings. A level of its own in `RUST_LOG` (`debug`)
/// replaces the default.
fn log_filter(rust_log: &str) -> EnvFilter {
    let directives: Vec<&str> = rust_log
        .split(',')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .collect();
    let has_level = directives.iter().any(|d| d.parse::<LevelFilter>().is_ok());
    let all = if has_level {
        directives.join(",")
    } else {
        std::iter::once("warn")
            .chain(directives)
            .collect::<Vec<_>>()
            .join(",")
    };
    EnvFilter::try_new(all).unwrap_or_else(|_| EnvFilter::new("warn"))
}

fn main() {
    #[cfg(target_os = "linux")]
    carry_activation_token();
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(&std::env::var("RUST_LOG").unwrap_or_default()))
        .with_writer(std::io::stderr)
        .init();
    tauri::Builder::default()
        .manage(Mutex::new(EarlyLaunches::default()))
        // First, so a second launch hands over before setting anything up.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            launched_again(app, args)
        }))
        .invoke_handler(tauri::generate_handler![status, connect])
        .setup(setup)
        .run(tauri::generate_context!())
        .expect("the app failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_warnings_whatever_rust_log_adds() {
        let filter = |env: &str| log_filter(env).to_string();
        assert_eq!(filter(""), "warn");
        let proxy = filter("coversync_proxy=debug");
        assert!(
            proxy.contains("coversync_proxy=debug") && proxy.contains("warn"),
            "{proxy}"
        );
        assert_eq!(filter("debug"), "debug");
        assert_eq!(filter("not a [directive"), "warn");
    }

    #[test]
    fn reads_the_activation_token() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            activation_token(&args(&["app", "--server=x", "--activation-token=abc"])).as_deref(),
            Some("abc")
        );
        assert_eq!(
            activation_token(&args(&["app", "--activation-token="])),
            None
        );
        assert_eq!(activation_token(&args(&["app"])), None);
    }

    #[test]
    fn reads_the_server_argument() {
        let args = |list: &[&str]| server_arg(list.iter().map(|s| s.to_string()));
        assert_eq!(
            args(&["--server", "https://a/"]).as_deref(),
            Some("https://a/")
        );
        assert_eq!(
            args(&["--server=https://b/"]).as_deref(),
            Some("https://b/")
        );
        assert_eq!(
            args(&["-v", "--server", "https://c/", "x"]).as_deref(),
            Some("https://c/")
        );
        assert_eq!(args(&["--server"]), None, "no value");
        assert_eq!(args(&["--serverx=y", "https://d/"]), None);
        assert_eq!(args(&[]), None);
    }
}
