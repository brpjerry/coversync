//! What drives the app's window: the first launch sets it up, later
//! launches bring it up (and may switch servers), and the fallback page's
//! commands, the proxies' reports and a page whose process ended all end up
//! changing what it shows.
//!
//! Qt runs on the main thread ([`crate::ffi`]); the proxies, the checks of a
//! server and the later launches, on the tokio runtime. Either side may ask
//! the window for something: the requests reach it in the order they're
//! made.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use coversync_desktop_core::bridge::{Commands, Feature, Features, Host};
use coversync_desktop_core::tokens::Keychain;
use coversync_desktop_core::{
    ConfigLock, EarlyLaunches, PairLink, Problem, Save, Shell, Store, config, server_arg,
    with_env_server,
};
use coversync_proxy::{AboutApp, Cookies, Upstream};
use serde::Serialize;
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use url::Url;

use crate::bridge::{self, Reply, Request};
use crate::ffi::qt;
use crate::instance::Launch;
use crate::pages::FALLBACK;
use crate::paths;

pub const APP_ID: &str = "io.github.brpjerry.coversync";

/// Qt's `RenderProcessTerminationStatus`, for a page's process that ended
/// on its own.
const TERMINATED_NORMALLY: i32 = 0;
const TERMINATED_KILLED: i32 = 3;

static APP: OnceLock<App> = OnceLock::new();

/// The app, as the commands for the servers' pages see it
/// ([`coversync_desktop_core::bridge`]).
struct Desktop {
    /// Whether the desktop has anything to show notifications with, once
    /// the window knows.
    notifications: watch::Receiver<Option<bool>>,
}

impl Host for Desktop {
    /// Waits for the window to find out whether the desktop shows
    /// notifications, however long that takes: the page asks once and keeps
    /// the answer, so an early "no" would stand until it's reloaded. The
    /// window always finds out; a desktop that doesn't answer is a "no"
    /// once the call to it times out.
    fn features(&self) -> Features {
        let mut notifications = self.notifications.clone();
        Box::pin(async move {
            let known = notifications
                .wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|known| *known);
            let mut features = Vec::new();
            if known == Some(true) {
                features.push(Feature::Notify);
            }
            features
        })
    }
}

/// What the fallback page shows.
#[derive(Serialize)]
struct Status {
    /// The server's URL, if one is set.
    server: Option<String>,
    /// Why the server's UI isn't showing.
    problem: Option<Problem>,
    /// The servers the app keeps something of, which the page can forget.
    remembered: Vec<String>,
}

/// What the window opens with.
pub struct Start {
    /// The first page.
    pub first_url: String,
    /// Clear the web view's HTTP cache before it loads.
    pub clear_cache: bool,
}

pub struct App {
    runtime: Handle,
    shell: Arc<Shell>,
    early: Mutex<EarlyLaunches<Launch>>,
    /// The pairing link the app was started with, until the window is there
    /// to ask about it.
    first_link: Mutex<Option<PairLink>>,
    /// The questions the window is asking for the app, each waiting for its
    /// answer.
    questions: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    next_question: AtomicU64,
    /// Whether the desktop shows notifications: `None` until the window has
    /// found out.
    notifications: watch::Sender<Option<bool>>,
    /// Held for the whole run: see [`ConfigLock`].
    _lock: ConfigLock,
}

impl App {
    /// The app, once [`App::start`] has set it up.
    pub fn get() -> Option<&'static App> {
        APP.get()
    }

    /// The first launch: takes the settings' lock and works out the first
    /// page. `env_server` is the launch's `COVERSYNC_SERVER`.
    pub fn start(
        runtime: Handle,
        args: Vec<String>,
        env_server: Option<String>,
    ) -> Result<Start, String> {
        let config_dir = paths::config_dir();
        // Before anything reads the settings or the cookies.
        let lock = ConfigLock::take(&config_dir)?;
        let config = Store::open(config_dir.clone());
        let saved = config.get();
        let clear_cache = saved.needs_cache_clear();
        let first_link = PairLink::among(args.iter().skip(1).cloned());
        let server = server_arg(args.into_iter().skip(1))
            .or(env_server)
            .or(saved.server);

        let (notifications, found) = watch::channel(None);
        let desktop = Desktop {
            notifications: found,
        };
        let shell = Arc::new(
            Shell::new(
                config,
                Cookies::open(Some(config_dir.join("cookies.json"))),
                Url::parse(FALLBACK).expect("the fallback page's URL is valid"),
                // The proxy logs it.
                Arc::new(|unreachable| {
                    if let Some(app) = App::get() {
                        app.show_fallback(Problem::unreachable(unreachable));
                    }
                }),
            )
            .with_commands(Arc::new(Commands::new(Arc::new(desktop))))
            .with_pairing(
                Arc::new(Keychain::new(APP_ID)),
                AboutApp {
                    platform: "linux".to_owned(),
                    app_version: env!("CARGO_PKG_VERSION").to_owned(),
                    hostname: hostname(),
                },
                Arc::new(|refused| {
                    if let Some(app) = App::get() {
                        app.token_refused(refused);
                    }
                }),
            ),
        );
        // Straight to the server's UI when there's a server; the fallback
        // page takes over if it can't be reached.
        let first_url = match server.map(|server| runtime.block_on(shell.open(&server))) {
            Some(Ok(proxy)) => proxy.entry_url(),
            Some(Err(problem)) => {
                shell.set_problem(Some(problem));
                FALLBACK.to_owned()
            }
            None => FALLBACK.to_owned(),
        };

        let app = App {
            runtime,
            shell,
            early: Mutex::new(EarlyLaunches::default()),
            first_link: Mutex::new(first_link),
            questions: Mutex::new(HashMap::new()),
            next_question: AtomicU64::new(1),
            notifications,
            _lock: lock,
        };
        if APP.set(app).is_err() {
            return Err("The app was started twice.".to_owned());
        }
        Ok(Start {
            first_url,
            clear_cache,
        })
    }

    /// Shows the fallback page, saying what went wrong. Any thread.
    fn show_fallback(&self, problem: Problem) {
        self.shell.set_problem(Some(problem));
        qt::load(FALLBACK);
    }

    /// The first builds served every server from one port, so a web view's
    /// HTTP cache may hold several servers' answers under that origin.
    /// Settings from before [`config::VERSION`] 1 get it cleared before the
    /// first page loads. Only a clear that succeeds is recorded: one that
    /// fails is tried again at the next start.
    pub fn cache_cleared(&self, cleared: bool) {
        if !cleared {
            tracing::warn!("couldn't clear the HTTP cache");
            return;
        }
        tracing::info!("cleared the HTTP cache the servers shared");
        if let Err(err) = self
            .shell
            .config
            .update(|settings| settings.version = config::VERSION)
        {
            tracing::warn!("couldn't record the cleared cache: {err}");
        }
    }

    /// The app has started (see [`EarlyLaunches`]): handles the launches that
    /// waited for it.
    pub fn started(&'static self) {
        let waiting = self.early.lock().unwrap().release();
        if let Some(link) = self.first_link.lock().unwrap().take() {
            self.pair_from_link(link);
        }
        for launch in waiting {
            self.launched_again(launch);
        }
    }

    /// A server refused this computer's token (the device was removed
    /// there): the token is forgotten, and if the window shows that server,
    /// the fallback page says so and offers to pair again. Any thread.
    fn token_refused(&'static self, refused: Upstream) {
        self.runtime.spawn(async move {
            // After a pairing or a switch under way, and before the next: a
            // new pairing's token isn't the one refused, and mustn't be
            // removed from the keychain along with it.
            let _navigation = self.shell.navigation.lock().await;
            if let Some(problem) = self.shell.rejected(refused).await {
                self.show_fallback(problem);
            }
        });
    }

    /// Asks the user `message` in the window's own dialog, with a button
    /// saying `accept` and, unless `cancel` is empty, one saying `cancel`.
    /// `false` also when the window closes before an answer.
    async fn ask(&self, message: &str, accept: &str, cancel: &str) -> bool {
        let id = self.next_question.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        self.questions.lock().unwrap().insert(id, answer);
        qt::ask(id, message, accept, cancel);
        answered.await.unwrap_or(false)
    }

    /// The window's answer to a question of [`App::ask`]'s.
    pub fn answered(&self, id: u64, accepted: bool) {
        if let Some(answer) = self.questions.lock().unwrap().remove(&id) {
            let _ = answer.send(accepted);
        }
    }

    /// A pairing link was opened (`coversync://pair?server=…&code=…`, from
    /// the server's Add device): pairs this computer with its server, once
    /// the user has said so. A link can come from anywhere, so nothing
    /// happens, and nothing is sent, before that. What goes wrong is said
    /// in a dialog, and the page the window shows stays.
    fn pair_from_link(&'static self, link: PairLink) {
        self.runtime.spawn(async move {
            // As the app reads the address, which is what it would pair with.
            let server = match Upstream::parse(&link.server) {
                Ok(upstream) => upstream.url(),
                Err(err) => {
                    tracing::warn!("ignoring a pairing link: {err}");
                    let message = format!("That pairing link doesn't work.\n\n{err}");
                    self.ask(&message, "OK", "").await;
                    return;
                }
            };
            tracing::info!("a pairing link for {server}: asking");
            let question = format!(
                "Pair this computer with {server}?\n\nThe app will show that server from now \
                 on, signed in as a device of its own. Go on only if you made this link \
                 yourself, in that server's Add device."
            );
            if !self.ask(&question, "Pair", "Cancel").await {
                tracing::info!("not pairing with {server}: declined");
                return;
            }
            let _navigation = self.shell.navigation.lock().await;
            match self.shell.pair(&server, &link.code).await {
                Ok(proxy) => qt::load(&proxy.entry_url()),
                Err(problem) => {
                    let message = format!("{}\n\n{}", problem.title, problem.message);
                    self.ask(&message, "OK", "").await;
                }
            }
        });
    }

    /// Another launch of the app: brings the window up, and shows the server
    /// it was given with `--server` (or `COVERSYNC_SERVER`), if any, for this
    /// run, unchecked as at the first launch. The launch's activation token
    /// goes to the window, so the compositor may raise and focus it (GNOME
    /// and KDE do; Hyprland only with `focus_on_activate`). Any thread.
    pub fn launched_again(&'static self, launch: Launch) {
        // Whole, token and all: a launch that waits can still bring the
        // window to the front once it is handled.
        let Some(launch) = self.early.lock().unwrap().hold(launch) else {
            return;
        };
        qt::raise(launch.activation_token.as_deref().unwrap_or_default());
        // A pairing link is the whole of what its launch asks.
        if let Some(link) = PairLink::among(launch.args.iter().skip(1).cloned()) {
            return self.pair_from_link(link);
        }
        let args = with_env_server(launch.args, launch.env_server);
        let Some(server) = server_arg(args.into_iter().skip(1)) else {
            return;
        };
        self.runtime.spawn(async move {
            let _navigation = self.shell.navigation.lock().await;
            // Already shown, once earlier launches are through: the page, and
            // whatever it's playing, stays.
            if self.shell.shows(&server) {
                return;
            }
            match self.shell.open(&server).await {
                Ok(proxy) => qt::load(&proxy.entry_url()),
                Err(problem) => self.show_fallback(problem),
            }
        });
    }

    /// A command from the fallback page. Only the app's own pages may use
    /// them: see [`bridge::allowed`].
    pub fn on_request(&'static self, id: u64, initiator: &str, page: &str, body: &[u8]) {
        let reply = Reply::new(id);
        if !bridge::allowed(initiator, page) {
            tracing::warn!("refused a command from {initiator} while showing {page}");
            reply.err(Problem::new(
                "Not allowed",
                "A server's pages can't use the app's own commands.",
            ));
            return;
        }
        let request = match Request::read(body) {
            Ok(request) => request,
            Err(problem) => return reply.err(problem),
        };
        match request {
            Request::Status {} => reply.ok(Status {
                server: self
                    .shell
                    .current()
                    .and_then(|proxy| proxy.upstream())
                    .map(|upstream| upstream.url()),
                problem: self.shell.problem(),
                remembered: self.shell.remembered(),
            }),
            Request::Connect {
                server,
                without_token,
            } => {
                self.runtime.spawn(async move {
                    let _navigation = self.shell.navigation.lock().await;
                    let switched = match (server, without_token) {
                        (Some(server), true) => {
                            self.shell.switch_without_token(server, Save::Yes).await
                        }
                        (server, _) => self.shell.switch(server, Save::Yes).await,
                    };
                    match switched {
                        Ok(proxy) => {
                            // Under the lock, so a later switch can't open first.
                            qt::load(&proxy.entry_url());
                            reply.ok(());
                        }
                        Err(problem) => reply.err(problem),
                    }
                });
            }
            Request::Check { server } => {
                self.runtime.spawn(async move {
                    match self.shell.probe(&server).await {
                        Ok(probe) => reply.ok(probe),
                        Err(problem) => reply.err(problem),
                    }
                });
            }
            Request::Pair { server, code } => {
                self.runtime.spawn(async move {
                    let _navigation = self.shell.navigation.lock().await;
                    match self.shell.pair(&server, &code).await {
                        Ok(proxy) => {
                            qt::load(&proxy.entry_url());
                            reply.ok(());
                        }
                        Err(problem) => reply.err(problem),
                    }
                });
            }
            Request::Forget { server } => {
                self.runtime.spawn(async move {
                    let _navigation = self.shell.navigation.lock().await;
                    match self.shell.forget_server(&server).await {
                        Ok(()) => reply.ok(()),
                        Err(problem) => reply.err(problem),
                    }
                });
            }
        }
    }

    /// Whether the page at `origin` may show notifications: a server's UI,
    /// from one of the app's proxies. Any other page the window comes to
    /// show (a link followed out of the UI) may not.
    pub fn may_notify(&self, origin: &str) -> bool {
        Url::parse(origin).is_ok_and(|origin| self.shell.is_proxied(&origin))
    }

    /// Whether a notification from the page at `origin` is shown now: only
    /// while the window is in the background. In front, the page's own
    /// messages are in view.
    pub fn shows_notification(&self, origin: &str, window_active: bool) -> bool {
        let shown = !window_active && self.may_notify(origin);
        if !shown {
            let why = if window_active {
                "the window has the focus"
            } else {
                "it isn't a server's page"
            };
            tracing::debug!("not showing a notification from {origin}: {why}");
        }
        shown
    }

    /// The window has found out whether the desktop shows notifications.
    pub fn notifications_found(&self, available: bool) {
        if !available {
            tracing::info!("no notifications: the desktop has nothing that shows them");
        }
        self.notifications.send_replace(Some(available));
    }

    /// The process that draws the page ended (`status` is Qt's
    /// `RenderProcessTerminationStatus`): the view would keep its last frame,
    /// or a blank one, and never come back.
    pub fn render_terminated(&self, status: i32, exit_code: i32) {
        let message = match status {
            // The app closing its own view.
            TERMINATED_NORMALLY => return,
            TERMINATED_KILLED => {
                "The process that shows the page was stopped by the system. It may have used \
                 too much memory."
            }
            _ => "The process that shows the page crashed.",
        };
        tracing::error!("{message} (status {status}, exit code {exit_code})");
        self.show_fallback(Problem::new("The page stopped working", message));
    }
}

/// This computer's name, for the server's list of devices; empty if it has
/// none to tell.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_features_wait_for_the_window_to_find_out() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let features = |found: Option<bool>| async move {
                let (notifications, heard) = watch::channel(None);
                let desktop = Desktop {
                    notifications: heard,
                };
                let asked = desktop.features();
                tokio::pin!(asked);
                // However long the page has waited, there's no answer until
                // the window has one: the page keeps what it's told.
                let early = tokio::time::timeout(Duration::from_millis(50), &mut asked).await;
                assert!(early.is_err(), "answered before the window knew");
                match found {
                    Some(found) => drop(notifications.send(Some(found))),
                    // The window is gone without ever finding out.
                    None => drop(notifications),
                }
                asked.await
            };
            assert_eq!(features(Some(true)).await, [Feature::Notify]);
            assert_eq!(features(Some(false)).await, []);
            assert_eq!(features(None).await, []);
        });
    }
}
