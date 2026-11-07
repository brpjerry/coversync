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
use tracing_subscriber::filter::LevelFilter;

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

    /// What `connect` does short of moving the window: picks `server`, or
    /// with `None` the current one, and checks that it shows a UI. A new
    /// server is saved and used only then, so a typo can't replace an
    /// address that works. A failed retry of the current server becomes the
    /// page's problem.
    async fn switch(&self, server: Option<String>) -> Result<(), Problem> {
        let (upstream, new) = match server {
            Some(url) => (Upstream::parse(&url).map_err(Problem::invalid)?, true),
            None => match self.proxy.upstream() {
                Some(upstream) => (upstream, false),
                None => return Err(Problem::new("No server is set", "Enter its address.")),
            },
        };
        if let Err(unreachable) = self.proxy.check(&upstream).await {
            let problem = Problem::unreachable(unreachable);
            if !new {
                self.set_problem(Some(problem.clone()));
            }
            return Err(problem);
        }
        if new {
            if let Err(err) = self
                .config
                .update(|config| config.server = Some(upstream.url()))
            {
                tracing::warn!("couldn't save the server: {err}");
            }
            self.proxy.set_upstream(Some(upstream));
        }
        self.set_problem(None);
        Ok(())
    }

    /// The commands are for the bundled pages. Tauri already treats the
    /// proxy's origin as remote and the capability lists no remote URLs, so
    /// the server's UI can't call them; this refuses them there regardless,
    /// whatever the build.
    fn bundled_only(&self, webview: &tauri::Webview) -> Result<(), Problem> {
        let url = webview
            .url()
            .map_err(|err| Problem::new("Couldn't tell which page asked", err.to_string()))?;
        let proxy = Url::parse(self.proxy.origin()).expect("the proxy's origin is a URL");
        if url.origin() == proxy.origin() {
            return Err(Problem::new(
                "Not allowed",
                "The server's pages can't use the app's own commands.",
            ));
        }
        Ok(())
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
fn status(webview: tauri::Webview, shell: State<'_, Shell>) -> Result<Status, Problem> {
    shell.bundled_only(&webview)?;
    Ok(Status {
        server: shell.proxy.upstream().map(|upstream| upstream.url()),
        problem: shell.problem.lock().expect("problem lock").clone(),
    })
}

/// Connects to `server`, or with `None` tries the current one again, and
/// opens its UI once it answers (see [`Shell::switch`]).
#[tauri::command]
async fn connect(
    app: AppHandle,
    webview: tauri::Webview,
    shell: State<'_, Shell>,
    server: Option<String>,
) -> Result<(), Problem> {
    shell.bundled_only(&webview)?;
    shell.switch(server).await?;
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
    let config = Store::open(config_dir.clone());
    let saved = config.get();
    let server = server_arg(std::env::args().skip(1))
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
            .cookie_file(Some(config_dir.join("cookies.json")))
            // The proxy logs it.
            .on_unreachable(move |unreachable| {
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
    tracing_subscriber::fmt()
        .with_env_filter(log_filter(&std::env::var("RUST_LOG").unwrap_or_default()))
        .with_writer(std::io::stderr)
        .init();
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![status, connect])
        .setup(setup)
        .run(tauri::generate_context!())
        .expect("the app failed to start");
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};

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

    /// A server that answers every request with `status`.
    fn server(status: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = [0; 4096];
                let _ = stream.read(&mut request);
                let answer =
                    format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = stream.write_all(answer.as_bytes());
            }
        });
        addr
    }

    /// A port nothing listens on.
    fn closed() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    struct Fixture {
        shell: Shell,
        dir: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn shell(name: &str, current: SocketAddr) -> Fixture {
        let dir = std::env::temp_dir().join(format!("coversync-app-{name}-{}", std::process::id()));
        let config = Store::open(dir.clone());
        let current = format!("http://{current}/");
        config
            .update(|config| config.server = Some(current.clone()))
            .unwrap();
        let proxy = tauri::async_runtime::block_on(
            Proxy::builder()
                .upstream(Some(Upstream::parse(&current).unwrap()))
                .start(),
        )
        .unwrap();
        let shell = Shell {
            proxy,
            config,
            fallback: Url::parse("tauri://localhost/index.html").unwrap(),
            problem: Mutex::new(None),
        };
        Fixture { shell, dir }
    }

    fn saved(fixture: &Fixture) -> Option<String> {
        Store::open(fixture.dir.clone()).get().server
    }

    #[test]
    fn saves_a_new_server_once_it_answers() {
        let (old, new) = (server("200 OK"), server("200 OK"));
        let fixture = shell("switch", old);
        let new_url = format!("http://{new}/");
        tauri::async_runtime::block_on(fixture.shell.switch(Some(new_url.clone()))).unwrap();
        assert_eq!(saved(&fixture), Some(new_url.clone()));
        assert_eq!(fixture.shell.proxy.upstream().unwrap().url(), new_url);
    }

    #[test]
    fn keeps_the_old_server_when_the_new_one_fails() {
        let old = server("200 OK");
        let fixture = shell("keep", old);
        let old_url = format!("http://{old}/");
        for bad in [
            format!("http://{}/", closed()),
            format!("http://{}/", server("404 Not Found")),
            "not a :// url".to_owned(),
        ] {
            let problem = tauri::async_runtime::block_on(fixture.shell.switch(Some(bad.clone())))
                .unwrap_err();
            assert!(!problem.message.is_empty(), "{bad}");
            assert_eq!(saved(&fixture), Some(old_url.clone()), "{bad}");
            assert_eq!(
                fixture.shell.proxy.upstream().unwrap().url(),
                old_url,
                "{bad}"
            );
            assert!(
                fixture.shell.problem.lock().unwrap().is_none(),
                "a new server's failure is shown, not kept"
            );
        }
    }

    #[test]
    fn a_failed_retry_becomes_the_problem() {
        let fixture = shell("retry", closed());
        let problem = tauri::async_runtime::block_on(fixture.shell.switch(None)).unwrap_err();
        assert_eq!(problem.title, "Can't reach the server");
        assert!(fixture.shell.problem.lock().unwrap().is_some());
    }
}
