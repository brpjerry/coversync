//! The coversync desktop app: one window onto a coversync server's UI.
//!
//! The UI comes from the server, through a proxy on 127.0.0.1
//! (`coversync-proxy`) that streams everything both ways and keeps the
//! server's path prefix. When no server is set, or it can't be reached, the
//! window shows the bundled fallback page (`../ui`), where the server can be
//! retried or changed.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod config;

use std::error::Error;
use std::sync::Mutex;

use coversync_proxy::{Proxy, Unreachable, Upstream};
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
    problem: Mutex<Option<String>>,
}

impl Shell {
    fn set_problem(&self, problem: Option<String>) {
        *self.problem.lock().expect("problem lock") = problem;
    }
}

/// What the fallback page shows.
#[derive(Serialize)]
struct Status {
    /// The server's URL, if one is set.
    server: Option<String>,
    /// Why the server's UI isn't showing.
    problem: Option<String>,
}

#[tauri::command]
fn status(shell: State<'_, Shell>) -> Status {
    Status {
        server: shell.proxy.upstream().map(|upstream| upstream.url()),
        problem: shell.problem.lock().expect("problem lock").clone(),
    }
}

/// Connects to `server` and saves it as the server to use, or with `None`
/// tries the current one again. Opens the server's UI once it answers.
#[tauri::command]
async fn connect(
    app: AppHandle,
    shell: State<'_, Shell>,
    server: Option<String>,
) -> Result<(), String> {
    let upstream = match server {
        Some(url) => {
            let upstream = Upstream::parse(&url).map_err(|err| err.to_string())?;
            shell
                .config
                .update(|config| config.server = Some(upstream.url()))
                .map_err(|err| format!("Couldn't save the server: {err}"))?;
            shell.proxy.set_upstream(Some(upstream.clone()));
            upstream
        }
        None => shell.proxy.upstream().ok_or("No server is set.")?,
    };
    if let Err(unreachable) = shell.proxy.check(&upstream).await {
        let problem = unreachable.to_string();
        shell.set_problem(Some(problem.clone()));
        return Err(problem);
    }
    shell.set_problem(None);
    open_ui(&app, &shell.proxy).map_err(|err| err.to_string())
}

/// Sends the window through the proxy's entry URL to the server's UI.
fn open_ui(app: &AppHandle, proxy: &Proxy) -> tauri::Result<()> {
    let url = Url::parse(&proxy.entry_url()).expect("the entry URL is valid");
    match app.get_webview_window(WINDOW) {
        Some(window) => window.navigate(url),
        None => Ok(()),
    }
}

/// Shows the fallback page when a page load can't reach the server.
fn show_fallback(app: &AppHandle, unreachable: Unreachable) {
    tracing::warn!("{unreachable}");
    let Some(shell) = app.try_state::<Shell>() else {
        return;
    };
    shell.set_problem(Some(unreachable.to_string()));
    if let Some(window) = app.get_webview_window(WINDOW)
        && let Err(err) = window.navigate(shell.fallback.clone())
    {
        tracing::error!("couldn't show the fallback page: {err}");
    }
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
    let config = Store::open(app.path().app_config_dir()?);
    let saved = config.get();
    let server = server_arg()
        .or_else(|| std::env::var(SERVER_ENV).ok().filter(|s| !s.is_empty()))
        .or(saved.server);
    let mut problem = None;
    let upstream = match server.as_deref().map(Upstream::parse) {
        Some(Ok(upstream)) => Some(upstream),
        Some(Err(err)) => {
            problem = Some(err.to_string());
            None
        }
        None => None,
    };

    let handle = app.handle().clone();
    let proxy = tauri::async_runtime::block_on(
        Proxy::builder()
            .upstream(upstream)
            .preferred_port(saved.port)
            .on_unreachable(move |unreachable| show_fallback(&handle, unreachable))
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
    WebviewWindowBuilder::new(app, WINDOW, url)
        .title("coversync")
        .inner_size(1280.0, 860.0)
        .min_inner_size(720.0, 480.0)
        .build()?;
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
