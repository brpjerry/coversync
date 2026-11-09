//! The app's window and what drives it: the first launch sets it up, later
//! launches bring it up (and may switch servers), and the fallback page's
//! commands, the proxies' reports and a crashed web process all end up
//! changing what it shows.
//!
//! GTK runs on the main thread; the proxies, and the checks of a server, on
//! the tokio runtime. Work on the runtime that changes the window sends a
//! [`UiEvent`] back over a channel, which the main loop drains in order.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use coversync_desktop_core::{
    ConfigLock, EarlyLaunches, Problem, Save, Shell, Store, config, gstreamer, server_arg,
};
use coversync_proxy::{Cookies, Upstream};
use gtk::prelude::*;
use serde::Serialize;
use tokio::runtime::Handle;
use url::Url;
use webkit::prelude::*;
use webkit::{gio, glib};

use crate::bridge::{self, Reply, Request};
use crate::pacing;
use crate::scheme::{self, FALLBACK};

pub const APP_ID: &str = "io.github.brpjerry.coversync";

/// What work on the runtime asks of the window.
enum UiEvent {
    /// Load this page: a proxy's entry URL.
    Open(String),
    /// Show the fallback page, saying what went wrong.
    Fallback(Problem),
}

/// What the fallback page shows.
#[derive(Serialize)]
struct Status {
    /// The server's URL, if one is set.
    server: Option<String>,
    /// Why the server's UI isn't showing.
    problem: Option<Problem>,
}

pub struct App {
    runtime: Handle,
    shell: Arc<Shell>,
    window: gtk::ApplicationWindow,
    view: webkit::WebView,
    events: async_channel::Sender<UiEvent>,
    early: RefCell<EarlyLaunches>,
    /// Held for the whole run: see [`ConfigLock`].
    _lock: ConfigLock,
}

/// The app's settings and cookies (`~/.config/io.github.brpjerry.coversync`),
/// where the Windows app keeps its.
fn config_dir() -> PathBuf {
    glib::user_config_dir().join(APP_ID)
}

/// The webview's data: local storage, its own cookies, and its HTTP cache
/// (`~/.local/share/io.github.brpjerry.coversync`). Earlier builds of the
/// app (WebKitGTK 4.1, through Tauri) kept both there too, so a server's
/// settings in local storage carry over.
fn data_dir() -> PathBuf {
    glib::user_data_dir().join(APP_ID)
}

impl App {
    /// The first launch: takes the settings' lock, opens the window and its
    /// first page. `env_server` is the launch's `COVERSYNC_SERVER`.
    pub fn start(
        gtk_app: &gtk::Application,
        runtime: Handle,
        args: Vec<String>,
        env_server: Option<String>,
    ) -> Result<Rc<Self>, String> {
        if gstreamer::autoaudiosink_missing() {
            tracing::warn!(
                "GStreamer's autoaudiosink isn't installed (gst-plugins-good): WebKitGTK crashes \
                 when audio starts playing"
            );
        }
        let config_dir = config_dir();
        // Before anything reads the settings or the cookies.
        let lock = ConfigLock::take(&config_dir)?;
        let config = Store::open(config_dir.clone());
        let clear_cache = config.get().needs_cache_clear();
        let saved = config.get();
        let server = server_arg(args.into_iter().skip(1))
            .or(env_server)
            .or(saved.server);

        let (events, received) = async_channel::unbounded();
        let shell = Arc::new(Shell::new(
            config,
            Cookies::open(Some(config_dir.join("cookies.json"))),
            Url::parse(FALLBACK).expect("the fallback page's URL is valid"),
            Arc::new({
                let events = events.clone();
                // The proxy logs it.
                move |unreachable| {
                    let _ = events.try_send(UiEvent::Fallback(Problem::unreachable(unreachable)));
                }
            }),
        ));
        // Straight to the server's UI when there's a server; the fallback
        // page takes over if it can't be reached.
        let first = match server.as_deref().map(|url| (url, Upstream::parse(url))) {
            Some((_, Ok(upstream))) => match runtime.block_on(shell.proxy_for(&upstream)) {
                Ok(proxy) => {
                    shell.show(proxy.clone());
                    proxy.entry_url()
                }
                Err(err) => {
                    shell.set_problem(Some(Problem {
                        server: Some(upstream.url()),
                        ..Problem::new("Couldn't start the app's proxy", err.to_string())
                    }));
                    FALLBACK.to_owned()
                }
            },
            Some((address, Err(err))) => {
                shell.set_problem(Some(Problem::invalid(err, address)));
                FALLBACK.to_owned()
            }
            None => FALLBACK.to_owned(),
        };

        let session = webkit::NetworkSession::new(
            Some(&data_dir().to_string_lossy()),
            Some(&data_dir().to_string_lossy()),
        );
        let app = Rc::new_cyclic(|this: &Weak<Self>| {
            let view = web_view(&session, this.clone());
            let offload = gtk::GraphicsOffload::new(Some(&view));
            if let Some(content) = view.user_content_manager() {
                pacing::install(&offload, &content);
            }
            let window = gtk::ApplicationWindow::builder()
                .application(gtk_app)
                .title("coversync")
                .default_width(1280)
                .default_height(860)
                // Without the offload, GTK draws each of the view's frames
                // into the window itself, and they reach the display at 60 a
                // second at most; with it, at the display's refresh rate.
                .child(&offload)
                .build();
            window.set_size_request(720, 480);
            Self {
                runtime,
                shell,
                window,
                view,
                events,
                early: RefCell::new(EarlyLaunches::default()),
                _lock: lock,
            }
        });
        app.drain(received);
        app.window.present();
        if clear_cache {
            app.clear_http_cache_then(&session, first);
        } else {
            app.view.load_uri(&first);
            app.started();
        }
        Ok(app)
    }

    /// Carries out what the runtime's work asks of the window, in the order
    /// it asks.
    fn drain(self: &Rc<Self>, received: async_channel::Receiver<UiEvent>) {
        let this = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = received.recv().await {
                let Some(app) = this.upgrade() else { break };
                match event {
                    UiEvent::Open(url) => app.view.load_uri(&url),
                    UiEvent::Fallback(problem) => app.show_fallback(problem),
                }
            }
        });
    }

    /// Shows the fallback page, saying what went wrong.
    fn show_fallback(&self, problem: Problem) {
        self.shell.set_problem(Some(problem));
        self.view.load_uri(FALLBACK);
    }

    /// The app has started (see [`EarlyLaunches`]): handles the launches that
    /// waited for it.
    fn started(&self) {
        let waiting = self.early.borrow_mut().release();
        for args in waiting {
            self.launched_again(args, None);
        }
    }

    /// Another launch of the app: brings the window up, and shows the server
    /// it was given with `--server` (or `COVERSYNC_SERVER`), if any, for this
    /// run. GTK hands the launch's activation token to the window, so the
    /// compositor may raise and focus it (GNOME and KDE do; Hyprland only
    /// with `focus_on_activate`).
    pub fn launched_again(&self, mut args: Vec<String>, env_server: Option<String>) {
        if let Some(server) = env_server
            && server_arg(args.iter().skip(1).cloned()).is_none()
        {
            args.push(format!("--server={server}"));
        }
        let Some(args) = self.early.borrow_mut().hold(args) else {
            return;
        };
        self.window.present();
        let Some(server) = server_arg(args.into_iter().skip(1)) else {
            return;
        };
        let (shell, events) = (self.shell.clone(), self.events.clone());
        self.runtime.spawn(async move {
            let _navigation = shell.navigation.lock().await;
            let event = match shell.switch(Some(server), Save::No).await {
                Ok(proxy) => UiEvent::Open(proxy.entry_url()),
                Err(problem) => UiEvent::Fallback(problem),
            };
            let _ = events.send(event).await;
        });
    }

    /// A command from the fallback page. Only the app's own pages may use
    /// them: the bridge is in a script world a server's pages don't get, and
    /// this checks the page regardless.
    fn on_request(&self, request: Request, reply: Reply) {
        let page = self.view.uri().map(String::from).unwrap_or_default();
        if !scheme::is_app_page(&page) {
            reply.err(Problem::new(
                "Not allowed",
                "A server's pages can't use the app's own commands.",
            ));
            return;
        }
        match request {
            Request::Status {} => reply.ok(Status {
                server: self
                    .shell
                    .current()
                    .and_then(|proxy| proxy.upstream())
                    .map(|upstream| upstream.url()),
                problem: self.shell.problem(),
            }),
            Request::Connect { server } => {
                let (shell, events) = (self.shell.clone(), self.events.clone());
                let switched = self.runtime.spawn(async move {
                    let _navigation = shell.navigation.lock().await;
                    let proxy = shell.switch(server, Save::Yes).await?;
                    // Under the lock, so a later switch can't open first.
                    let _ = events.send(UiEvent::Open(proxy.entry_url())).await;
                    Ok::<_, Problem>(())
                });
                glib::spawn_future_local(async move {
                    match switched.await {
                        Ok(Ok(())) => reply.ok(()),
                        Ok(Err(problem)) => reply.err(problem),
                        Err(err) => {
                            reply.err(Problem::new("Something went wrong", err.to_string()))
                        }
                    }
                });
            }
        }
    }

    /// The first builds served every server from one port, so the webview's
    /// HTTP cache may hold several servers' answers under that origin.
    /// Settings from before [`config::VERSION`] 1 get it cleared before the
    /// first page loads; local storage and cookies stay. Only a clear that
    /// succeeds is recorded: one that fails is tried again at the next start.
    /// Either way the first page opens, and then the launches that waited are
    /// handled.
    fn clear_http_cache_then(self: &Rc<Self>, session: &webkit::NetworkSession, first: String) {
        let (done, cleared) = async_channel::bounded(1);
        match session.website_data_manager() {
            Some(manager) => manager.clear(
                webkit::WebsiteDataTypes::DISK_CACHE | webkit::WebsiteDataTypes::MEMORY_CACHE,
                glib::TimeSpan(0),
                None::<&gio::Cancellable>,
                move |result| {
                    match &result {
                        Ok(()) => tracing::info!("cleared the HTTP cache the servers shared"),
                        Err(err) => tracing::warn!("couldn't clear the HTTP cache: {err}"),
                    }
                    let _ = done.try_send(result.is_ok());
                },
            ),
            None => {
                tracing::warn!("couldn't clear the HTTP cache: the session has no data manager");
                let _ = done.try_send(false);
            }
        }
        let this = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let cleared = cleared.recv().await.unwrap_or(false);
            let Some(app) = this.upgrade() else { return };
            if cleared
                && let Err(err) = app
                    .shell
                    .config
                    .update(|settings| settings.version = config::VERSION)
            {
                tracing::warn!("couldn't record the cleared cache: {err}");
            }
            app.view.load_uri(&first);
            app.started();
        });
    }
}

/// The view, set up to draw at the display's refresh rate and to answer the
/// fallback page. `app` is the app it's for, once it exists.
fn web_view(session: &webkit::NetworkSession, app: Weak<App>) -> webkit::WebView {
    let settings = webkit::Settings::new();
    prefer_display_refresh_rate(&settings);
    // Right-click, Inspect Element; and pages' console messages on stdout.
    #[cfg(debug_assertions)]
    {
        settings.set_enable_developer_extras(true);
        settings.set_enable_write_console_messages_to_stdout(true);
    }

    let content = webkit::UserContentManager::new();
    bridge::install(&content, {
        let app = app.clone();
        move |request, reply| match app.upgrade() {
            Some(app) => app.on_request(request, reply),
            None => reply.err(Problem::new("Not ready", "The app is still starting.")),
        }
    });

    let context = webkit::WebContext::default().expect("WebKit's default web context");
    scheme::register(&context);
    let view = webkit::WebView::builder()
        .web_context(&context)
        .network_session(session)
        .settings(&settings)
        .user_content_manager(&content)
        .build();
    // When the web process that draws the page crashes, the view would keep
    // its last frame and never come back.
    view.connect_web_process_terminated(move |_, reason| {
        let message = match reason {
            webkit::WebProcessTerminationReason::Crashed => crash_message(),
            webkit::WebProcessTerminationReason::ExceededMemoryLimit => {
                "The web process that shows the page used too much memory and was stopped."
                    .to_owned()
            }
            // The app closing its own view.
            _ => return,
        };
        tracing::error!("{message}");
        if let Some(app) = app.upgrade() {
            app.show_fallback(Problem::new("The page stopped working", message));
        }
    });
    view
}

/// WebKit renders pages at about 60 frames a second by default, half a
/// 120 Hz display's rate: `PreferPageRenderingUpdatesNear60FPS`. Off, they
/// follow the display. Settings only reach a view made with them, so this
/// comes before the view.
fn prefer_display_refresh_rate(settings: &webkit::Settings) {
    const NEAR_60: &str = "PreferPageRenderingUpdatesNear60FPS";
    let feature = webkit::Settings::all_features().and_then(|features| {
        (0..features.length())
            .filter_map(|i| features.get(i))
            .find(|feature| feature.identifier().as_deref() == Some(NEAR_60))
    });
    match feature {
        Some(feature) => settings.set_feature_enabled(&feature, false),
        None => tracing::error!(
            "WebKit has no {NEAR_60} feature to turn off: pages may render at 60 frames a second"
        ),
    }
}

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

/// Shows `message` in a dialog of its own, with no window behind it, and
/// lets the app end once it's closed.
pub fn show_error(gtk_app: &gtk::Application, message: &str) {
    let hold = gtk_app.hold();
    gtk::AlertDialog::builder()
        .message("coversync")
        .detail(message)
        .buttons(["Close"])
        .modal(true)
        .build()
        .choose(None::<&gtk::Window>, None::<&gio::Cancellable>, move |_| {
            drop(hold)
        });
}
