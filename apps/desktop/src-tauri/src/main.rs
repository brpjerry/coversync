//! The coversync desktop app for Windows (Tauri): one window onto a coversync
//! server's UI. Linux has an app of its own, `apps/desktop-linux` (GTK 4 and
//! WebKitGTK 6.0): WebKitGTK 4.1, which Tauri uses there, draws at 60 frames
//! a second at most. This one still builds and runs on Linux.
//!
//! The UI comes from the server, through a proxy on 127.0.0.1
//! (`coversync-proxy`) that streams everything both ways and keeps the
//! server's path prefix; each server gets its own (see
//! [`coversync_desktop_core::shell`]). When no
//! server is set or it can't be reached, or (on Linux) the page crashes, the
//! window shows the bundled fallback page (`../ui`), where the server can be
//! retried or changed. One instance runs at a time: launching it again
//! brings its window up.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::error::Error;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
use coversync_desktop_core::gstreamer;
use coversync_desktop_core::{
    ConfigLock, EarlyLaunches, Problem, SERVER_ENV, Save, Shell, Store, config, links, log_filter,
    server_arg,
};
use coversync_proxy::{Cookies, Proxy};
use serde::Serialize;
use tauri::{AppHandle, Manager, State, Url, WebviewUrl, WebviewWindowBuilder};

/// The window's label, which the capabilities name.
const WINDOW: &str = "main";
/// The bundled fallback page.
const FALLBACK: &str = "index.html";

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

/// The app has started (see [`EarlyLaunches`]): handles the launches that
/// waited for it.
fn started(app: &AppHandle) {
    let waiting = app
        .state::<Mutex<EarlyLaunches>>()
        .lock()
        .expect("early launches lock")
        .release();
    for args in waiting {
        launched_again(app, args);
    }
}

/// Another launch of the app: bring the window up, and show the server it
/// was given with `--server`, if any, for this run, unchecked as at the first
/// launch. (The plugin passes on arguments only, not `COVERSYNC_SERVER`.)
fn launched_again(app: &AppHandle, args: Vec<String>) {
    let held = app
        .state::<Mutex<EarlyLaunches>>()
        .lock()
        .expect("early launches lock")
        .hold(args);
    let Some(args) = held else {
        return;
    };
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
        // Already shown, once earlier launches are through: the page, and
        // whatever it's playing, stays.
        if shell.shows(&server) {
            return;
        }
        match shell.open(&server).await {
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
        .filter_map(|arg| arg.strip_prefix(ACTIVATION_ARG))
        .find(|token| !token.is_empty())
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
/// token as an argument too ([`carried`]), before anything else runs.
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
        .args(carried(&token, args))
        .exec();
    tracing::warn!("couldn't pass the activation token on: {err}");
}

/// The arguments a launch starts itself again with: the token first, so it
/// can't be taken for the value of a bare `--server` at the end of `args`.
#[cfg(any(target_os = "linux", test))]
fn carried(token: &str, args: Vec<String>) -> Vec<String> {
    std::iter::once(format!("{ACTIVATION_ARG}{token}"))
        .chain(args)
        .collect()
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
    let saved = config.get();
    let clear_cache = saved.needs_cache_clear();
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
    let url = match server.map(|server| tauri::async_runtime::block_on(shell.open(&server))) {
        Some(Ok(proxy)) => WebviewUrl::External(Url::parse(&proxy.entry_url())?),
        Some(Err(problem)) => {
            shell.set_problem(Some(problem));
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
        // The app has one window: links that would open another
        // (`target="_blank"`) go to the default browser.
        .on_new_window(|url, _| {
            match links::for_browser(url.as_str()) {
                Some(url) => {
                    if let Err(err) = open::that_detached(url.as_str()) {
                        tracing::warn!("couldn't open a link in the browser: {err}");
                    }
                }
                None => tracing::info!("not opening {url}: only web links go to the browser"),
            }
            tauri::webview::NewWindowResponse::Deny
        })
        // Tauri's own handler would take files dropped on the window, which
        // WebView2 then never passes to the page (the import's artwork).
        .disable_drag_drop_handler()
        .build()?;
    watch_web_process(&window)?;
    if clear_cache {
        clear_http_cache_then(&window, url);
    } else {
        started(app.handle());
    }
    Ok(())
}

/// The first builds served every server from one port, so the webview's
/// HTTP cache may hold several servers' answers under that origin. Settings
/// from before [`config::VERSION`] 1 (theirs, and the builds' since that
/// kept no version) get it cleared before the first page loads; local
/// storage and cookies stay. Only a clear that succeeds is recorded: one
/// that fails is tried again at the next start. Either way the first page
/// opens, and then the launches that waited are handled. WebKitGTK only: no
/// other webview ran those builds.
fn clear_http_cache_then(window: &tauri::WebviewWindow, url: WebviewUrl) {
    let go = {
        let window = window.clone();
        Arc::new(move |cleared: bool| {
            let app = window.app_handle();
            let shell = app.try_state::<Shell>();
            if let Some(shell) = &shell
                && cleared
                && let Err(err) = shell
                    .config
                    .update(|settings| settings.version = config::VERSION)
            {
                tracing::warn!("couldn't record the cleared cache: {err}");
            }
            let target = match &url {
                WebviewUrl::External(url) => Some(url.clone()),
                _ => shell.map(|shell| shell.fallback.clone()),
            };
            if let Some(target) = target
                && let Err(err) = window.navigate(target)
            {
                tracing::error!("couldn't open the first page: {err}");
            }
            started(app);
        })
    };
    #[cfg(target_os = "linux")]
    {
        use webkit2gtk::{WebViewExt, WebsiteDataManagerExtManual, WebsiteDataTypes};

        let then = go.clone();
        let cleared =
            window.with_webview(
                move |webview| match webview.inner().website_data_manager() {
                    Some(manager) => manager.clear(
                        WebsiteDataTypes::DISK_CACHE | WebsiteDataTypes::MEMORY_CACHE,
                        webkit2gtk::glib::TimeSpan(0),
                        None::<&webkit2gtk::gio::Cancellable>,
                        move |result| {
                            match &result {
                                Ok(()) => {
                                    tracing::info!("cleared the HTTP cache the servers shared")
                                }
                                Err(err) => tracing::warn!("couldn't clear the HTTP cache: {err}"),
                            }
                            then(result.is_ok());
                        },
                    ),
                    None => {
                        tracing::warn!(
                            "couldn't clear the HTTP cache: the webview has no data manager"
                        );
                        then(false);
                    }
                },
            );
        if let Err(err) = cleared {
            tracing::warn!("couldn't clear the HTTP cache: {err}");
            go(false);
        }
    }
    #[cfg(not(target_os = "linux"))]
    go(true);
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

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(&std::env::var("RUST_LOG").unwrap_or_default()))
        .with_writer(std::io::stderr)
        .init();
    #[cfg(target_os = "linux")]
    carry_activation_token();
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
        assert_eq!(
            activation_token(&args(&[
                "app",
                "--activation-token=",
                "--activation-token=abc"
            ]))
            .as_deref(),
            Some("abc"),
            "an empty one doesn't hide the one carried"
        );
        assert_eq!(activation_token(&args(&["app"])), None);
    }

    #[test]
    fn the_carried_token_isnt_a_servers_value() {
        let args = carried("abc", vec!["--server".to_owned()]);
        assert_eq!(activation_token(&args).as_deref(), Some("abc"));
        assert_eq!(server_arg(args.into_iter()), None);
        let args = carried("abc", vec!["--server".to_owned(), "https://a/".to_owned()]);
        assert_eq!(server_arg(args.into_iter()).as_deref(), Some("https://a/"));
    }
}
