//! The coversync desktop app: one window onto a coversync server's UI.
//!
//! The UI comes from the server, through a proxy on 127.0.0.1
//! (`coversync-proxy`) that streams everything both ways and keeps the
//! server's path prefix. When no server is set, it can't be reached, or the
//! page crashes, the window shows the bundled fallback page (`../ui`), where
//! the server can be retried or changed.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;
#[cfg(target_os = "linux")]
mod gstreamer;

use std::error::Error;
use std::sync::Mutex;

use coversync_proxy::{Proxy, Unreachable, Upstream, UpstreamError};
use serde::Serialize;
use tauri::{AppHandle, Manager, State, Url, WebviewUrl, WebviewWindowBuilder};
use tracing_subscriber::EnvFilter;

use config::Store;

/// The window's label, which the capabilities name.
const WINDOW: &str = "main";
/// The bundled fallback page.
const FALLBACK: &str = "index.html";
/// A server for this run only, like `--server`. Neither is saved.
const SERVER_ENV: &str = "COVERSYNC_SERVER";

struct Shell {
    proxy: Proxy,
    config: Store,
    /// The fallback page's URL.
    fallback: Url,
    /// Why the fallback page is showing, when something went wrong.
    problem: Mutex<Option<Problem>>,
}

impl Shell {
    fn set_problem(&self, problem: Option<Problem>) {
        *self.problem.lock().expect("problem lock") = problem;
    }
}

/// Something that went wrong, as the fallback page shows it.
#[derive(Clone, Debug, Serialize)]
struct Problem {
    title: String,
    message: String,
    /// The error underneath, in small print, when it adds something.
    detail: Option<String>,
}

impl Problem {
    fn new(title: &str, message: impl Into<String>) -> Self {
        Self {
            title: title.to_owned(),
            message: message.into(),
            detail: None,
        }
    }

    fn unreachable(unreachable: Unreachable) -> Self {
        Self {
            detail: unreachable.detail,
            ..Self::new("Can't reach the server", unreachable.reason)
        }
    }

    fn invalid(err: UpstreamError) -> Self {
        Self::new("That address won't work", err.to_string())
    }
}

/// What the fallback page shows.
#[derive(Serialize)]
struct Status {
    /// The server's URL, if one is set.
    server: Option<String>,
    /// Why the server's UI isn't showing.
    problem: Option<Problem>,
}

#[tauri::command]
fn status(shell: State<'_, Shell>) -> Status {
    Status {
        server: shell.proxy.upstream().map(|upstream| upstream.url()),
        problem: shell.problem.lock().expect("problem lock").clone(),
    }
}

/// Connects to `server`, or with `None` tries the current one again, and
/// opens its UI once it answers. A new server is saved only then, so a typo
/// can't replace an address that works.
#[tauri::command]
async fn connect(
    app: AppHandle,
    shell: State<'_, Shell>,
    server: Option<String>,
) -> Result<(), Problem> {
    let (upstream, new) = match server {
        Some(url) => (Upstream::parse(&url).map_err(Problem::invalid)?, true),
        None => match shell.proxy.upstream() {
            Some(upstream) => (upstream, false),
            None => return Err(Problem::new("No server is set", "Enter its address.")),
        },
    };
    if let Err(unreachable) = shell.proxy.check(&upstream).await {
        let problem = Problem::unreachable(unreachable);
        if !new {
            shell.set_problem(Some(problem.clone()));
        }
        return Err(problem);
    }
    if new {
        if let Err(err) = shell
            .config
            .update(|config| config.server = Some(upstream.url()))
        {
            tracing::warn!("couldn't save the server: {err}");
        }
        shell.proxy.set_upstream(Some(upstream));
    }
    shell.set_problem(None);
    open_ui(&app, &shell.proxy)
        .map_err(|err| Problem::new("Couldn't open the server's UI", err.to_string()))
}

/// Sends the window through the proxy's entry URL to the server's UI.
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

/// `--server <url>` or `--server=<url>`.
fn server_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
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

    let config = Store::open(app.path().app_config_dir()?);
    let saved = config.get();
    let server = server_arg()
        .or_else(|| std::env::var(SERVER_ENV).ok().filter(|s| !s.is_empty()))
        .or(saved.server);
    let mut problem = None;
    let upstream = match server.as_deref().map(Upstream::parse) {
        Some(Ok(upstream)) => Some(upstream),
        Some(Err(err)) => {
            problem = Some(Problem::invalid(err));
            None
        }
        None => None,
    };

    let handle = app.handle().clone();
    let proxy = tauri::async_runtime::block_on(
        Proxy::builder()
            .upstream(upstream)
            .preferred_port(saved.port)
            .on_unreachable(move |unreachable| {
                tracing::warn!("{unreachable}");
                show_fallback(&handle, Problem::unreachable(unreachable));
            })
            .start(),
    )?;
    let port = proxy.addr().port();
    if saved.port != Some(port)
        && let Err(err) = config.update(|config| config.port = Some(port))
    {
        tracing::warn!("couldn't save the proxy's port: {err}");
    }

    // Straight to the server's UI when there's a server; the fallback page
    // takes over if it can't be reached.
    let url = if proxy.upstream().is_some() {
        WebviewUrl::External(Url::parse(&proxy.entry_url())?)
    } else {
        WebviewUrl::App(FALLBACK.into())
    };
    app.manage(Shell {
        proxy,
        config,
        fallback: fallback_url(app)?,
        problem: Mutex::new(problem),
    });
    let window = WebviewWindowBuilder::new(app, WINDOW, url)
        .title("coversync")
        .inner_size(1280.0, 860.0)
        .min_inner_size(720.0, 480.0)
        .build()?;
    watch_web_process(&window)?;
    Ok(())
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![status, connect])
        .setup(setup)
        .run(tauri::generate_context!())
        .expect("the app failed to start");
}
