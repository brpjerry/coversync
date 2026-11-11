//! The app's state, apart from the window: which server it shows, a proxy
//! for each server it has used, and what went wrong.
//!
//! Each server gets a proxy of its own, on a port of its own that's kept
//! between runs, so each has an origin of its own in the webview: its HTTP
//! cache and the UI's local storage (sort, sidebar) aren't shared with
//! another server's. The proxies share one cookie jar, which keeps each
//! server's cookies apart, and the app's commands for the servers' pages
//! ([`crate::bridge`]).
//!
//! A server the app has paired with gets the app's token with every request
//! ([`Shell::pair`]), from the keychain ([`crate::tokens`]). One it hasn't is
//! used as in a browser, with the server's own login.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use coversync_proxy::{
    AboutApp, AppCommands, Cookies, PairError, Proxy, Unreachable, Upstream, UpstreamError,
};
use serde::Serialize;
use url::Url;

use crate::config::{Device, Store};
use crate::tokens::Tokens;

pub type UnreachableHandler = Arc<dyn Fn(Unreachable) + Send + Sync>;
/// Hears of a server that refused the app's token: see [`Shell::rejected`].
pub type RejectedHandler = Arc<dyn Fn(Upstream) + Send + Sync>;

pub struct Shell {
    pub config: Store,
    cookies: Cookies,
    /// The fallback page's URL.
    pub fallback: Url,
    on_unreachable: UnreachableHandler,
    /// What the servers' pages may ask of the app, through their proxies.
    commands: Option<Arc<dyn AppCommands>>,
    /// Where the servers' tokens are kept, and what the app says of itself
    /// as it pairs. Without them the app can't pair.
    pairing: Option<Pairing>,
    /// A proxy for each server used in this run, by its URL.
    proxies: Mutex<HashMap<String, Proxy>>,
    /// The server the window shows. A proxy's report reaches the window only
    /// while its server is this one, checked and sent under this lock.
    current: Arc<Mutex<Option<Proxy>>>,
    /// Why the fallback page is showing, when something went wrong.
    problem: Mutex<Option<Problem>>,
    /// Held from a connection attempt's check to the window's navigation
    /// (the fallback page's Connect, another launch's `--server`), so an
    /// older attempt can't overtake a newer one.
    pub navigation: tokio::sync::Mutex<()>,
}

struct Pairing {
    tokens: Arc<dyn Tokens>,
    about: AboutApp,
    on_rejected: RejectedHandler,
}

/// An address that passed the check: a coversync server the app can use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Probe {
    /// Its URL, as the app writes it.
    pub server: String,
    /// Whether this computer is paired with it already.
    pub paired: bool,
}

/// Something that went wrong, as the fallback page shows it.
#[derive(Clone, Debug, Serialize)]
pub struct Problem {
    pub title: String,
    pub message: String,
    /// The error underneath, in small print, when it adds something.
    pub detail: Option<String>,
    /// The address it's about: the page shows it, and puts it in the field.
    pub server: Option<String>,
    /// The way on is to pair this computer with `server`: the page offers
    /// that right away.
    pub pair: bool,
}

impl Problem {
    pub fn new(title: &str, message: impl Into<String>) -> Self {
        Self {
            title: title.to_owned(),
            message: message.into(),
            detail: None,
            server: None,
            pair: false,
        }
    }

    pub fn unreachable(unreachable: Unreachable) -> Self {
        Self {
            detail: unreachable.detail,
            server: Some(unreachable.server),
            ..Self::new("Can't open the server", unreachable.reason)
        }
    }

    pub fn invalid(err: UpstreamError, address: &str) -> Self {
        Self {
            server: Some(address.to_owned()),
            ..Self::new("That address won't work", err.to_string())
        }
    }

    /// About `server`, the address it concerns.
    fn about(self, server: &Upstream) -> Self {
        Self {
            server: Some(server.url()),
            ..self
        }
    }

    fn no_proxy(err: std::io::Error, server: &Upstream) -> Self {
        Self::new("Couldn't start the app's proxy", err.to_string()).about(server)
    }

    fn pairing(err: PairError) -> Self {
        Self {
            detail: err.detail,
            ..Self::new("Couldn't pair with the server", err.message)
        }
    }

    /// The server took this computer off its list of devices (or its
    /// token stopped working some other way).
    pub fn removed(server: &Upstream) -> Self {
        Self {
            pair: true,
            ..Self::new(
                "This computer was removed from the server",
                "The server no longer takes this computer's token. Pair it again with a \
                 code from Add device (to keep its settings there, choose Replace an \
                 existing device), or sign in with the server's password.",
            )
        }
        .about(server)
    }
}

/// Whether a server chosen now is saved as the one to use from then on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Save {
    Yes,
    /// Shown for this run only.
    No,
}

impl Shell {
    pub fn new(
        config: Store,
        cookies: Cookies,
        fallback: Url,
        on_unreachable: UnreachableHandler,
    ) -> Self {
        Self {
            config,
            cookies,
            fallback,
            on_unreachable,
            commands: None,
            pairing: None,
            proxies: Mutex::new(HashMap::new()),
            current: Arc::new(Mutex::new(None)),
            problem: Mutex::new(None),
            navigation: tokio::sync::Mutex::new(()),
        }
    }

    /// Gives the servers' pages the app's commands: every proxy started from
    /// now on answers them (see [`crate::bridge::Commands`]).
    pub fn with_commands(mut self, commands: Arc<dyn AppCommands>) -> Self {
        self.commands = Some(commands);
        self
    }

    /// Lets the app pair with servers: their tokens are kept in `tokens`,
    /// `about` is what a server is told of the app, and `on_rejected` hears
    /// when a server refuses a token it gave (it should call
    /// [`Shell::rejected`]).
    pub fn with_pairing(
        mut self,
        tokens: Arc<dyn Tokens>,
        about: AboutApp,
        on_rejected: RejectedHandler,
    ) -> Self {
        self.pairing = Some(Pairing {
            tokens,
            about,
            on_rejected,
        });
        self
    }

    pub fn current(&self) -> Option<Proxy> {
        self.current.lock().expect("current lock").clone()
    }

    pub fn problem(&self) -> Option<Problem> {
        self.problem.lock().expect("problem lock").clone()
    }

    pub fn set_problem(&self, problem: Option<Problem>) {
        *self.problem.lock().expect("problem lock") = problem;
    }

    /// The proxy for `upstream`'s server: the one already running, or a new
    /// one on the port kept for that server. A server without one, or whose
    /// port is taken, gets a random port that's no other server's and was
    /// never anyone's.
    pub async fn proxy_for(&self, upstream: &Upstream) -> std::io::Result<Proxy> {
        let url = upstream.url();
        if let Some(proxy) = self.proxies.lock().expect("proxies lock").get(&url) {
            return Ok(proxy.clone());
        }
        let config = self.config.get();
        let (current, on_unreachable) = (self.current.clone(), self.on_unreachable.clone());
        let mut builder = Proxy::builder();
        if let Some(commands) = &self.commands {
            builder = builder.app_commands(commands.clone());
        }
        if let Some(pairing) = &self.pairing {
            let on_rejected = pairing.on_rejected.clone();
            builder = builder.on_rejected(move |upstream| on_rejected(upstream));
        }
        let proxy = builder
            .upstream(Some(upstream.clone()))
            .preferred_port(config.ports.get(&url).copied())
            .avoid_ports(config.taken_except(&url))
            .cookies(self.cookies.clone())
            .on_unreachable(move |unreachable| {
                // A late failure from a server no longer shown stays quiet.
                let shown = current.lock().expect("current lock");
                let is_shown = shown
                    .as_ref()
                    .and_then(Proxy::upstream)
                    .is_some_and(|upstream| upstream.url() == unreachable.server);
                if is_shown {
                    on_unreachable(unreachable);
                }
            })
            .start()
            .await?;
        Ok(self
            .proxies
            .lock()
            .expect("proxies lock")
            .entry(url)
            .or_insert(proxy)
            .clone())
    }

    /// Shows `proxy`'s server from now on, and keeps its port for it. A
    /// server that drops out of the kept ports loses its cookies too, unless
    /// a kept server shares its origin.
    pub fn show(&self, proxy: Proxy) {
        if let Some(upstream) = proxy.upstream() {
            let (url, port) = (upstream.url(), proxy.addr().port());
            match self.config.update(|config| config.shown(&url, port)) {
                Ok(dropped) => self.forget(&dropped),
                Err(err) => tracing::warn!("couldn't save the server's port: {err}"),
            }
        }
        *self.current.lock().expect("current lock") = Some(proxy);
    }

    /// Forgets the cookies of `servers`, which have no port kept, unless a
    /// kept server shares an origin with one.
    fn forget(&self, servers: &[String]) {
        let origin = |server: &String| {
            Upstream::parse(server)
                .ok()
                .map(|up| up.origin().to_owned())
        };
        let kept: Vec<String> = self.config.get().ports.keys().filter_map(origin).collect();
        for server in servers {
            if let Some(origin) = origin(server)
                && !kept.contains(&origin)
            {
                tracing::info!("forgetting {server}'s cookies: no port is kept for it");
                self.cookies.forget(&origin);
            }
        }
    }

    /// Shows `server` without checking it or saving it: a launch's `--server`
    /// or `COVERSYNC_SERVER`, which may name a server the check would refuse
    /// (one behind a login of its own, say). If it can't be reached, its
    /// page load says so.
    pub async fn open(&self, server: &str) -> Result<Proxy, Problem> {
        let result = async {
            let upstream = Upstream::parse(server).map_err(|err| Problem::invalid(err, server))?;
            let upstream = self.with_token(upstream).await?;
            let proxy = self.started_proxy(&upstream).await?;
            self.show(proxy.clone());
            self.set_problem(None);
            Ok(proxy)
        }
        .await;
        if let Err(problem) = &result {
            log_refusal(Some(server), problem);
        }
        result
    }

    /// Whether the window shows `server`'s UI, with nothing wrong: a launch
    /// that asks for it again only needs the window brought up.
    pub fn shows(&self, server: &str) -> bool {
        let Ok(asked) = Upstream::parse(server) else {
            return false;
        };
        self.problem().is_none()
            && self
                .current()
                .and_then(|proxy| proxy.upstream())
                .is_some_and(|shown| shown.url() == asked.url())
    }

    /// [`Shell::proxy_for`], with a failure to start it as the page's
    /// problem. The proxy sends `upstream`'s token from now on, or none if
    /// it has none.
    async fn started_proxy(&self, upstream: &Upstream) -> Result<Proxy, Problem> {
        let proxy = self
            .proxy_for(upstream)
            .await
            .map_err(|err| Problem::no_proxy(err, upstream))?;
        if proxy.upstream().as_ref() != Some(upstream) {
            proxy.set_upstream(Some(upstream.clone()));
        }
        Ok(proxy)
    }

    /// `upstream` with this computer's token for it, if it's paired with
    /// that server. A pairing whose token the keychain no longer has (a new
    /// keychain, say) is forgotten: the server's own login shows, and the
    /// computer can be paired again.
    async fn with_token(&self, upstream: Upstream) -> Result<Upstream, Problem> {
        let url = upstream.url();
        let Some(pairing) = &self.pairing else {
            return Ok(upstream);
        };
        if !self.config.get().paired.contains_key(&url) {
            return Ok(upstream);
        }
        let token = pairing.tokens.load(&url).await.map_err(|why| {
            Problem::new("Can't get this computer's token for the server", why).about(&upstream)
        })?;
        match token {
            Some(token) => Ok(upstream.with_authorization(Some(token.authorization()))),
            None => {
                tracing::warn!("the keychain has no token for {url}: no longer paired with it");
                self.unpair(&url);
                Ok(upstream)
            }
        }
    }

    /// Takes `url`'s server off the list of those this computer is paired
    /// with.
    fn unpair(&self, url: &str) {
        if let Err(err) = self.config.update(|config| config.paired.remove(url)) {
            tracing::warn!("couldn't save the settings: {err}");
        }
    }

    /// Whether `server` is a coversync server the app can use, and whether
    /// this computer is paired with it. Nothing changes: the window goes on
    /// showing what it shows. A refused address's cookies (the check may
    /// have stored some) are forgotten.
    pub async fn probe(&self, server: &str) -> Result<Probe, Problem> {
        let result = async {
            let upstream = Upstream::parse(server).map_err(|err| Problem::invalid(err, server))?;
            self.checked(&upstream).await?;
            let url = upstream.url();
            Ok(Probe {
                paired: self.config.get().paired.contains_key(&url),
                server: url,
            })
        }
        .await;
        if let Err(problem) = &result {
            log_refusal(Some(server), problem);
        }
        result
    }

    /// The proxy for `upstream`, once its server has answered as coversync.
    /// The proxy is left as it is: one that sends a token goes on sending
    /// it while the address is only looked at.
    async fn checked(&self, upstream: &Upstream) -> Result<Proxy, Problem> {
        let proxy = self
            .proxy_for(upstream)
            .await
            .map_err(|err| Problem::no_proxy(err, upstream))?;
        if let Err(unreachable) = proxy.check(upstream).await {
            self.forget(&[upstream.url()]);
            return Err(Problem::unreachable(unreachable));
        }
        Ok(proxy)
    }

    /// Pairs this computer with `server`, using a code from its Add device:
    /// the token it gets goes to the keychain and, from then on, with every
    /// request to that server. The server is shown and saved, as after
    /// [`Shell::switch`]. Nothing is changed unless all of it worked.
    pub async fn pair(&self, server: &str, code: &str) -> Result<Proxy, Problem> {
        let result = self.try_pair(server, code).await;
        if let Err(problem) = &result {
            log_refusal(Some(server), problem);
        }
        result
    }

    async fn try_pair(&self, server: &str, code: &str) -> Result<Proxy, Problem> {
        let upstream = Upstream::parse(server).map_err(|err| Problem::invalid(err, server))?;
        let url = upstream.url();
        let Some(pairing) = &self.pairing else {
            return Err(
                Problem::new("Couldn't pair with the server", "This app can't pair.")
                    .about(&upstream),
            );
        };
        let code = code.trim();
        if code.is_empty() || code.len() > MAX_CODE || !code.chars().all(is_code_char) {
            return Err(Problem::new(
                "That isn't a pairing code",
                "A pairing code has 8 letters and digits, like K7QM-4ZTD. Add device, in the \
                 server's Sync setup, makes one.",
            )
            .about(&upstream));
        }
        let proxy = self.checked(&upstream).await?;
        // Before the code is spent: it works once.
        pairing.tokens.ready().await.map_err(|why| {
            Problem::new("Can't keep a token on this computer", why).about(&upstream)
        })?;
        let paired = proxy
            .pair(&upstream, code, &pairing.about)
            .await
            .map_err(|err| Problem::pairing(err).about(&upstream))?;
        let listed = if paired.name.is_empty() {
            "the device this code added".to_owned()
        } else {
            format!("“{}”", paired.name)
        };
        if !paired.manages {
            return Err(Problem::new(
                "That code is for syncing only",
                format!(
                    "The app needs a code made for Management and sync, to show the \
                     library. Make one in Add device, and remove {listed} from the \
                     server's devices."
                ),
            )
            .about(&upstream));
        }
        pairing
            .tokens
            .save(&url, &paired.token)
            .await
            .map_err(|why| {
                Problem {
                    detail: Some(why),
                    ..Problem::new(
                        "Paired, but couldn't keep the token",
                        format!(
                            "The keychain didn't take this computer's token, so it's lost. \
                             Remove {listed} from the server's devices, and pair again."
                        ),
                    )
                }
                .about(&upstream)
            })?;
        let device = Device {
            id: paired.device_id,
            name: paired.name,
        };
        if let Err(err) = self.config.update(|config| {
            config.paired.insert(url.clone(), device);
            config.server = Some(url.clone());
        }) {
            tracing::warn!("couldn't save the pairing: {err}");
        }
        proxy.set_upstream(Some(
            upstream.with_authorization(Some(paired.token.authorization())),
        ));
        self.show(proxy.clone());
        self.set_problem(None);
        tracing::info!("paired with {url}");
        Ok(proxy)
    }

    /// A server refused the app's token (`refused` is the server as the
    /// request went to it): the device was removed there. The token is
    /// forgotten, and the server is used without one from now on. Returns
    /// the problem to show if the window shows that server; `None` if it
    /// shows another, or the token refused wasn't the one in use any more
    /// (an answer that came late, after pairing again).
    pub async fn rejected(&self, refused: Upstream) -> Option<Problem> {
        let url = refused.url();
        let proxy = self
            .proxies
            .lock()
            .expect("proxies lock")
            .get(&url)
            .cloned()?;
        let now = proxy.upstream()?;
        if now.authorization().is_none() || now.authorization() != refused.authorization() {
            return None;
        }
        proxy.set_upstream(Some(now.clone().with_authorization(None)));
        self.unpair(&url);
        if let Some(pairing) = &self.pairing
            && let Err(why) = pairing.tokens.forget(&url).await
        {
            tracing::warn!("couldn't remove {url}'s token from the keychain: {why}");
        }
        let shown = self
            .current()
            .and_then(|proxy| proxy.upstream())
            .is_some_and(|shown| shown.url() == url);
        shown.then(|| {
            let problem = Problem::removed(&now);
            self.set_problem(Some(problem.clone()));
            problem
        })
    }

    /// Forgets `server`: its token leaves the keychain, and its cookies,
    /// its port and its place as the saved server go. If the window shows
    /// it, it shows no server from now on. Everything that can be removed
    /// is, even if the keychain can't be reached; that is then the error.
    pub async fn forget_server(&self, server: &str) -> Result<(), Problem> {
        let upstream = Upstream::parse(server).map_err(|err| Problem::invalid(err, server))?;
        let url = upstream.url();
        let token = match &self.pairing {
            Some(pairing) => pairing.tokens.forget(&url).await,
            None => Ok(()),
        };
        if let Err(err) = self.config.update(|config| config.forget(&url)) {
            tracing::warn!("couldn't save the settings: {err}");
        }
        self.forget(std::slice::from_ref(&url));
        if let Some(proxy) = self.proxies.lock().expect("proxies lock").remove(&url) {
            proxy.set_upstream(None);
        }
        let mut current = self.current.lock().expect("current lock");
        let shown = current
            .as_ref()
            .and_then(Proxy::upstream)
            .is_none_or(|shown| shown.url() == url);
        if shown {
            *current = None;
        }
        drop(current);
        // What was wrong with it goes with it.
        let about_it = self
            .problem()
            .is_some_and(|problem| shown || problem.server.as_deref() == Some(url.as_str()));
        if about_it {
            self.set_problem(None);
        }
        tracing::info!("forgot {url}");
        token.map_err(|why| {
            Problem {
                detail: Some(why),
                ..Problem::new(
                    "Forgotten, except in the keychain",
                    "This computer's token for the server couldn't be removed from the \
                     keychain. It can be deleted there by hand.",
                )
            }
            .about(&upstream)
        })
    }

    /// Picks `server`, or with `None` tries the current one again, and checks
    /// that it's a coversync server. Only then is a new server shown (and,
    /// with [`Save::Yes`], saved), so a typo can't replace an address that
    /// works. A failed retry of the current server becomes the page's
    /// problem. Refusals are logged, and a refused new address's cookies
    /// (the check may have stored some) are forgotten.
    pub async fn switch(&self, server: Option<String>, save: Save) -> Result<Proxy, Problem> {
        let address = server.clone();
        let result = self.try_switch(server, save).await;
        if let Err(problem) = &result {
            log_refusal(address.as_deref(), problem);
        }
        result
    }

    async fn try_switch(&self, server: Option<String>, save: Save) -> Result<Proxy, Problem> {
        let (upstream, new) = match server {
            Some(url) => {
                let upstream = Upstream::parse(&url).map_err(|err| Problem::invalid(err, &url))?;
                (self.with_token(upstream).await?, true)
            }
            None => match self.current().and_then(|proxy| proxy.upstream()) {
                Some(upstream) => (upstream, false),
                None => return Err(Problem::new("No server is set", "Enter its address.")),
            },
        };
        let proxy = self.started_proxy(&upstream).await?;
        if let Err(unreachable) = proxy.check(&upstream).await {
            self.forget(&[upstream.url()]);
            let problem = Problem::unreachable(unreachable);
            if !new {
                self.set_problem(Some(problem.clone()));
            }
            return Err(problem);
        }
        if new
            && save == Save::Yes
            && let Err(err) = self
                .config
                .update(|config| config.server = Some(upstream.url()))
        {
            tracing::warn!("couldn't save the server: {err}");
        }
        self.show(proxy.clone());
        self.set_problem(None);
        Ok(proxy)
    }

    /// Whether `page` is one of the proxies' pages, a server's UI, rather
    /// than the app's bundled page.
    pub fn is_proxied(&self, page: &Url) -> bool {
        self.proxies
            .lock()
            .expect("proxies lock")
            .values()
            .any(|proxy| {
                Url::parse(proxy.origin()).is_ok_and(|origin| origin.origin() == page.origin())
            })
    }
}

/// More than a pairing code comes to, however it's written.
const MAX_CODE: usize = 32;

/// What a pairing code is written with: the server reads it leniently
/// (`k7qm 4ztd`, `K7QM-4ZTD`), so only what could never be one is refused
/// here.
fn is_code_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == ' '
}

fn log_refusal(address: Option<&str>, problem: &Problem) {
    tracing::warn!(
        "not showing {}: {}{}",
        problem
            .server
            .as_deref()
            .or(address)
            .unwrap_or("the server"),
        problem.message,
        problem
            .detail
            .as_ref()
            .map(|detail| format!(" ({detail})"))
            .unwrap_or_default()
    );
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::path::PathBuf;

    use super::*;

    /// A server that answers every request with `status`, and coversync's
    /// `/api/hello` body when it's a success.
    fn server(status: &'static str) -> SocketAddr {
        server_by_path(move |_| status)
    }

    /// The same, with the status `status_for` gives each request's path.
    fn server_by_path(status_for: impl Fn(&str) -> &'static str + Send + 'static) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = [0; 4096];
                let read = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]);
                let status = status_for(request.split(' ').nth(1).unwrap_or("/"));
                let body = if status.starts_with('2') {
                    r#"{"app":"coversync","name":"test","version":"0.1.0","api":["1"]}"#
                } else {
                    ""
                };
                // Every server sets a cookie, so the jar has something of it.
                let answer = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Set-Cookie: session=1; Path=/; Max-Age=3600\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
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

    fn url(addr: SocketAddr) -> String {
        format!("http://{addr}/")
    }

    struct Fixture {
        shell: Shell,
        dir: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        /// A shell showing the server at `current`, saved as the one to use.
        fn new(name: &str, current: SocketAddr) -> Self {
            let dir =
                std::env::temp_dir().join(format!("coversync-app-{name}-{}", std::process::id()));
            let config = Store::open(dir.clone());
            config
                .update(|config| config.server = Some(url(current)))
                .unwrap();
            let shell = Shell::new(
                config,
                Cookies::default(),
                Url::parse("coversync://app/index.html").unwrap(),
                Arc::new(|unreachable| REPORTS.lock().unwrap().push(unreachable.server)),
            );
            let upstream = Upstream::parse(&url(current)).unwrap();
            let proxy = block(shell.proxy_for(&upstream)).unwrap();
            shell.show(proxy);
            Self { shell, dir }
        }

        fn saved_server(&self) -> Option<String> {
            Store::open(self.dir.clone()).get().server
        }

        fn shown(&self) -> String {
            self.shell.current().unwrap().upstream().unwrap().url()
        }
    }

    /// What reached the window, from every test's shell.
    static REPORTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// One runtime for every test, as the apps have: a proxy started on it
    /// keeps running between the calls that use it.
    fn block<F: std::future::Future>(future: F) -> F::Output {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RUNTIME
            .get_or_init(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
            })
            .block_on(future)
    }

    #[test]
    fn saves_a_new_server_once_it_answers() {
        let (old, new) = (server("200 OK"), server("200 OK"));
        let fixture = Fixture::new("switch", old);
        block(fixture.shell.switch(Some(url(new)), Save::Yes)).unwrap();
        assert_eq!(fixture.saved_server(), Some(url(new)));
        assert_eq!(fixture.shown(), url(new));
    }

    #[test]
    fn a_server_for_this_run_isnt_saved() {
        let (old, new) = (server("200 OK"), server("200 OK"));
        let fixture = Fixture::new("one-run", old);
        block(fixture.shell.switch(Some(url(new)), Save::No)).unwrap();
        assert_eq!(fixture.saved_server(), Some(url(old)));
        assert_eq!(fixture.shown(), url(new));
    }

    #[test]
    fn a_launchs_server_opens_unchecked_and_unsaved() {
        let saved = server("200 OK");
        let fixture = Fixture::new("open", saved);
        // No /api/hello, as behind a login of its own.
        let guarded = server("401 Unauthorized");
        fixture
            .shell
            .set_problem(Some(Problem::new("Can't open the server", "earlier")));
        block(fixture.shell.open(&url(guarded))).unwrap();
        assert_eq!(fixture.shown(), url(guarded));
        assert!(fixture.shell.problem().is_none());
        assert_eq!(fixture.saved_server(), Some(url(saved)));

        let invalid = block(fixture.shell.open("not a :// url")).unwrap_err();
        assert_eq!(invalid.server.as_deref(), Some("not a :// url"));
        assert_eq!(fixture.shown(), url(guarded), "still shown");
    }

    #[test]
    fn knows_the_server_it_shows() {
        let shown = server("200 OK");
        let fixture = Fixture::new("shows", shown);
        assert!(fixture.shell.shows(&url(shown)));
        // The same server, written another way.
        assert!(fixture.shell.shows(&format!("http://{shown}")));
        assert!(!fixture.shell.shows(&url(server("200 OK"))));
        assert!(!fixture.shell.shows("not a :// url"));
        // The fallback page is up instead.
        fixture
            .shell
            .set_problem(Some(Problem::new("Can't open the server", "down")));
        assert!(!fixture.shell.shows(&url(shown)));
    }

    #[test]
    fn keeps_the_old_server_when_the_new_one_fails() {
        let old = server("200 OK");
        let fixture = Fixture::new("keep", old);
        for bad in [
            url(closed()),
            url(server("404 Not Found")),
            "not a :// url".to_owned(),
        ] {
            let problem = block(fixture.shell.switch(Some(bad.clone()), Save::Yes)).unwrap_err();
            assert!(!problem.message.is_empty(), "{bad}");
            assert_eq!(fixture.saved_server(), Some(url(old)), "{bad}");
            assert_eq!(fixture.shown(), url(old), "{bad}");
            assert!(
                fixture.shell.problem().is_none(),
                "a new server's failure is shown, not kept"
            );
        }
    }

    #[test]
    fn a_failed_retry_becomes_the_problem() {
        let fixture = Fixture::new("retry", closed());
        let problem = block(fixture.shell.switch(None, Save::Yes)).unwrap_err();
        assert_eq!(problem.title, "Can't open the server");
        assert!(fixture.shell.problem().is_some());
    }

    #[test]
    fn each_server_keeps_an_origin_of_its_own() {
        let (a, b) = (server("200 OK"), server("200 OK"));
        let fixture = Fixture::new("origins", a);
        let port_a = fixture.shell.current().unwrap().addr().port();
        let proxy_b = block(fixture.shell.switch(Some(url(b)), Save::Yes)).unwrap();
        let port_b = proxy_b.addr().port();
        assert_ne!(port_a, port_b, "another server, another origin");

        let ports = Store::open(fixture.dir.clone()).get().ports;
        assert_eq!(ports.get(&url(a)), Some(&port_a));
        assert_eq!(ports.get(&url(b)), Some(&port_b));

        // Back to A: the same proxy, so the same origin.
        let again = block(fixture.shell.switch(Some(url(a)), Save::Yes)).unwrap();
        assert_eq!(again.addr().port(), port_a);
    }

    #[test]
    fn only_the_proxies_pages_are_proxied() {
        let fixture = Fixture::new("gate", server("200 OK"));
        let proxy = fixture.shell.current().unwrap();
        let page = |s: &str| Url::parse(s).unwrap();
        assert!(
            fixture
                .shell
                .is_proxied(&page(&format!("{}/coversync/", proxy.origin())))
        );
        for bundled in ["coversync://app/index.html", "coversync://app/fallback.js"] {
            assert!(!fixture.shell.is_proxied(&page(bundled)), "{bundled}");
        }
        let other_port = page(&format!("http://127.0.0.1:{}/", proxy.addr().port() ^ 1));
        assert!(
            !fixture.shell.is_proxied(&other_port),
            "the dev server, say"
        );
    }

    #[test]
    fn a_refusal_names_the_address() {
        let fixture = Fixture::new("named", server("200 OK"));
        let typo = url(server("404 Not Found"));
        let problem = block(fixture.shell.switch(Some(typo.clone()), Save::Yes)).unwrap_err();
        assert_eq!(problem.server.as_deref(), Some(typo.as_str()));
        let invalid = block(
            fixture
                .shell
                .switch(Some("not a :// url".into()), Save::Yes),
        )
        .unwrap_err();
        assert_eq!(invalid.server.as_deref(), Some("not a :// url"));
    }

    #[test]
    fn servers_long_unused_lose_their_ports_and_cookies() {
        let fixture = Fixture::new("lru", server("200 OK"));
        let servers: Vec<SocketAddr> = (0..10).map(|_| server("200 OK")).collect();
        for addr in &servers {
            block(fixture.shell.switch(Some(url(*addr)), Save::No)).unwrap();
        }
        let config = Store::open(fixture.dir.clone()).get();
        assert_eq!(config.ports.len(), 1 + crate::config::KEEP_RECENT);
        let origin = |addr: SocketAddr| format!("http://{addr}");
        let origins = fixture.shell.cookies.origins();
        for dropped in &servers[..2] {
            assert!(!config.ports.contains_key(&url(*dropped)));
            assert!(!origins.contains(&origin(*dropped)), "its cookies go too");
        }
        for kept in &servers[2..] {
            assert!(config.ports.contains_key(&url(*kept)));
            assert!(origins.contains(&origin(*kept)));
        }
    }

    #[test]
    fn a_refused_address_leaves_no_cookies() {
        let fixture = Fixture::new("refused-cookies", server("200 OK"));
        // It sets a cookie as it refuses.
        let refused = server("404 Not Found");
        block(fixture.shell.switch(Some(url(refused)), Save::No)).unwrap_err();
        let origins = fixture.shell.cookies.origins();
        assert!(
            !origins.contains(&format!("http://{refused}")),
            "{origins:?}"
        );
    }

    #[test]
    fn a_kept_server_keeps_its_origins_cookies() {
        let fixture = Fixture::new("shared-origin", server("200 OK"));
        // One host: coversync under /a/, nothing under /b/.
        let host = server_by_path(|path| {
            if path.starts_with("/a/") {
                "200 OK"
            } else {
                "404 Not Found"
            }
        });
        let (a, b) = (format!("http://{host}/a/"), format!("http://{host}/b/"));
        block(fixture.shell.switch(Some(a), Save::No)).unwrap();
        block(fixture.shell.switch(Some(b), Save::No)).unwrap_err();
        let origins = fixture.shell.cookies.origins();
        assert!(
            origins.contains(&format!("http://{host}")),
            "/a/'s session stays: {origins:?}"
        );
    }

    /// Sends `lines` to `proxy` as one request over a plain socket, and
    /// returns the whole answer.
    fn send(proxy: &Proxy, lines: &[String], body: &str) -> String {
        let mut stream = std::net::TcpStream::connect(proxy.addr()).unwrap();
        let request = format!("{}\r\n\r\n{body}", lines.join("\r\n"));
        stream.write_all(request.as_bytes()).unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        answer
    }

    /// Opens `proxy`'s entry URL, as the window does, and returns the
    /// cookie it gets: `name=value`.
    fn enter(proxy: &Proxy) -> String {
        let addr = proxy.addr();
        let entry = proxy.entry_url();
        let entry = &entry[entry.find("/?k=").unwrap()..];
        let answer = send(
            proxy,
            &[
                format!("GET {entry} HTTP/1.1"),
                format!("Host: {addr}"),
                "Connection: close".to_owned(),
            ],
            "",
        );
        let cookie = answer
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("set-cookie")
                    .then(|| value.trim().to_owned())
            })
            .unwrap();
        cookie.split(';').next().unwrap().to_owned()
    }

    /// Opens `proxy`'s entry URL, then loads its server's page through it, as
    /// the window does, over plain sockets.
    fn page_load(proxy: &Proxy) {
        let cookie = enter(proxy);
        send(
            proxy,
            &[
                "GET / HTTP/1.1".to_owned(),
                format!("Host: {}", proxy.addr()),
                format!("Cookie: {cookie}"),
                "Sec-Fetch-Dest: document".to_owned(),
                "Accept: text/html".to_owned(),
                "Connection: close".to_owned(),
            ],
            "",
        );
    }

    /// What a pairing server saw of a request.
    #[derive(Clone, Debug)]
    struct Asked {
        path: String,
        authorization: Option<String>,
        body: String,
    }

    /// A coversync server with pairing: `/api/pair` gives the code
    /// `K7QM4ZTD` the token `cst_first` (and `K7QM-4ZTD`, written the long
    /// way, `cst_second`), `SYNC0NLY` a sync-only one, and refuses any
    /// other; `/api/hello` is open; everything else wants a token it gave
    /// and hasn't revoked, or none at all (as if signed in by cookie).
    struct PairingServer {
        addr: SocketAddr,
        asked: Arc<Mutex<Vec<Asked>>>,
        revoked: Arc<Mutex<Vec<String>>>,
    }

    impl PairingServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let asked: Arc<Mutex<Vec<Asked>>> = Arc::default();
            let revoked: Arc<Mutex<Vec<String>>> = Arc::default();
            let (log, gone) = (asked.clone(), revoked.clone());
            std::thread::spawn(move || {
                for mut stream in listener.incoming().flatten() {
                    let Some(request) = read_request(&mut stream) else {
                        continue;
                    };
                    log.lock().unwrap().push(request.clone());
                    let (status, body) = answer(&request, &gone.lock().unwrap());
                    let answer = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                         Set-Cookie: session=1; Path=/; Max-Age=3600\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(answer.as_bytes());
                }
            });
            Self {
                addr,
                asked,
                revoked,
            }
        }

        fn url(&self) -> String {
            url(self.addr)
        }

        fn asked(&self, path: &str) -> Vec<Asked> {
            let asked = self.asked.lock().unwrap();
            asked.iter().filter(|a| a.path == path).cloned().collect()
        }

        fn revoke(&self, token: &str) {
            self.revoked.lock().unwrap().push(token.to_owned());
        }
    }

    /// Reads one request whole: its head, then as much body as it says.
    fn read_request(stream: &mut std::net::TcpStream) -> Option<Asked> {
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        let (head_end, length) = loop {
            let read = stream.read(&mut chunk).ok().filter(|read| *read > 0)?;
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                let length = head
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break (end + 4, length);
            }
        };
        while bytes.len() < head_end + length {
            let read = stream.read(&mut chunk).ok().filter(|read| *read > 0)?;
            bytes.extend_from_slice(&chunk[..read]);
        }
        let head = String::from_utf8_lossy(&bytes[..head_end]).into_owned();
        let header = |wanted: &str| {
            head.lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
                .map(|(_, value)| value.trim().to_owned())
        };
        Some(Asked {
            path: head.split(' ').nth(1).unwrap_or("/").to_owned(),
            authorization: header("authorization"),
            body: String::from_utf8_lossy(&bytes[head_end..]).into_owned(),
        })
    }

    fn answer(request: &Asked, revoked: &[String]) -> (&'static str, String) {
        let hello = r#"{"app":"coversync","name":"test","version":"0.1.0","api":["1"]}"#;
        let paired = |token: &str, scope: &str| {
            (
                "200 OK",
                format!(r#"{{"device_id":7,"token":"{token}","name":"Laptop","scope":"{scope}"}}"#),
            )
        };
        match request.path.as_str() {
            "/api/hello" => ("200 OK", hello.to_owned()),
            "/api/pair" => {
                let sent: serde_json::Value =
                    serde_json::from_str(&request.body).unwrap_or_default();
                match sent["code"].as_str() {
                    Some("K7QM4ZTD") => paired("cst_first", "manage"),
                    Some("K7QM-4ZTD") => paired("cst_second", "manage"),
                    Some("SYNC0NLY") => paired("cst_sync", "sync"),
                    _ => (
                        "400 Bad Request",
                        r#"{"error":"That pairing code isn't right.","code":"wrong_code"}"#
                            .to_owned(),
                    ),
                }
            }
            _ => match &request.authorization {
                Some(sent)
                    if !sent.starts_with("Bearer cst_")
                        || revoked.iter().any(|token| sent.ends_with(token.as_str())) =>
                {
                    (
                        "401 Unauthorized",
                        r#"{"error":"This app's token isn't valid; pair it again.","code":"invalid_token"}"#
                            .to_owned(),
                    )
                }
                _ => ("200 OK", hello.to_owned()),
            },
        }
    }

    /// A fixture whose shell can pair, with its keychain and what its
    /// proxies reported as refused.
    struct Pairing {
        fixture: Fixture,
        tokens: Arc<crate::tokens::Memory>,
        refused: Arc<Mutex<Vec<Upstream>>>,
    }

    impl Pairing {
        fn new(name: &str) -> Self {
            let tokens = Arc::new(crate::tokens::Memory::default());
            let refused: Arc<Mutex<Vec<Upstream>>> = Arc::default();
            let mut fixture = Fixture::new(name, server("200 OK"));
            fixture.shell = Self::shell(&fixture.dir, &tokens, &refused);
            Self {
                fixture,
                tokens,
                refused,
            }
        }

        /// A shell over the settings in `dir`, as after a restart.
        fn shell(
            dir: &std::path::Path,
            tokens: &Arc<crate::tokens::Memory>,
            refused: &Arc<Mutex<Vec<Upstream>>>,
        ) -> Shell {
            let refused = refused.clone();
            Shell::new(
                Store::open(dir.join("pairing")),
                Cookies::default(),
                Url::parse("coversync://app/index.html").unwrap(),
                Arc::new(|_| {}),
            )
            .with_pairing(
                tokens.clone(),
                AboutApp {
                    platform: "linux".into(),
                    app_version: "0.1.0".into(),
                    hostname: "thinkpad".into(),
                },
                Arc::new(move |upstream| refused.lock().unwrap().push(upstream)),
            )
        }

        fn restarted(&self) -> Shell {
            Self::shell(&self.fixture.dir, &self.tokens, &self.refused)
        }

        fn settings(&self) -> crate::config::Config {
            Store::open(self.fixture.dir.join("pairing")).get()
        }

        fn settings_text(&self) -> String {
            std::fs::read_to_string(self.fixture.dir.join("pairing").join("config.json"))
                .unwrap_or_default()
        }
    }

    /// Asks for `path` through `proxy`, as a page of its UI would, and
    /// returns the answer's first line.
    fn fetch(proxy: &Proxy, path: &str) -> String {
        let cookie = enter(proxy);
        let answer = send(
            proxy,
            &[
                format!("GET {path} HTTP/1.1"),
                format!("Host: {}", proxy.addr()),
                format!("Cookie: {cookie}"),
                "Connection: close".to_owned(),
            ],
            "",
        );
        answer.lines().next().unwrap_or_default().to_owned()
    }

    fn token_of(proxy: &Proxy) -> Option<String> {
        let upstream = proxy.upstream()?;
        let header = upstream.authorization()?.to_str().ok()?.to_owned();
        Some(header.trim_start_matches("Bearer ").to_owned())
    }

    #[test]
    fn pairing_gets_a_token_keeps_it_in_the_keychain_and_sends_it() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair");
        let shell = &pairing.fixture.shell;

        assert_eq!(
            block(shell.probe(&server.url())).unwrap(),
            Probe {
                server: server.url(),
                paired: false
            }
        );
        let proxy = block(shell.pair(&server.url(), " K7QM4ZTD ")).unwrap();

        // The server heard the code, and of this computer.
        let sent: serde_json::Value =
            serde_json::from_str(&server.asked("/api/pair")[0].body).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({
                "code": "K7QM4ZTD", "platform": "linux", "app_version": "0.1.0",
                "hostname": "thinkpad",
            })
        );
        // The token is in the keychain, under the server, and nowhere in the settings.
        assert_eq!(pairing.tokens.servers(), [server.url()]);
        let settings = pairing.settings();
        assert_eq!(
            settings.server,
            Some(server.url()),
            "saved, as after Connect"
        );
        assert_eq!(
            settings.paired.get(&server.url()),
            Some(&Device {
                id: 7,
                name: "Laptop".into()
            })
        );
        assert!(!pairing.settings_text().contains("cst_"));
        // It's the server shown, and its requests carry the token.
        assert_eq!(shell.current().unwrap().addr(), proxy.addr());
        assert!(fetch(&proxy, "/api/status").contains("200"));
        assert_eq!(
            server.asked("/api/status")[0].authorization.as_deref(),
            Some("Bearer cst_first")
        );
        assert!(block(shell.probe(&server.url())).unwrap().paired);
    }

    #[test]
    fn a_paired_server_opens_with_its_token_after_a_restart() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-restart");
        block(pairing.fixture.shell.pair(&server.url(), "K7QM4ZTD")).unwrap();

        for way in ["open", "switch"] {
            let shell = pairing.restarted();
            let proxy = match way {
                "open" => block(shell.open(&server.url())),
                _ => block(shell.switch(Some(server.url()), Save::Yes)),
            }
            .unwrap();
            assert_eq!(token_of(&proxy).as_deref(), Some("cst_first"), "{way}");
        }
        // A server it never paired with gets none.
        let other = url(super::tests::server("200 OK"));
        let proxy = block(pairing.restarted().open(&other)).unwrap();
        assert_eq!(token_of(&proxy), None);
    }

    #[test]
    fn a_code_that_is_refused_changes_nothing() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-wrong");
        let shell = &pairing.fixture.shell;
        let before = pairing.settings_text();

        let refused = block(shell.pair(&server.url(), "00000000")).unwrap_err();
        assert_eq!(refused.title, "Couldn't pair with the server");
        assert_eq!(refused.message, "That pairing code isn't right.");
        assert_eq!(refused.server, Some(server.url()));
        // What could never be a code isn't sent at all.
        for junk in ["", "   ", "K7QM4ZTD; rm -rf", &"A".repeat(40), "ｋ７ｑｍ"] {
            let refused = block(shell.pair(&server.url(), junk)).unwrap_err();
            assert_eq!(refused.title, "That isn't a pairing code", "{junk:?}");
        }
        assert_eq!(server.asked("/api/pair").len(), 1);

        assert!(pairing.tokens.servers().is_empty());
        assert_eq!(pairing.settings_text(), before);
        assert!(shell.current().is_none(), "nothing is shown that wasn't");
        assert!(shell.problem().is_none());
    }

    #[test]
    fn an_address_that_isnt_coversync_never_gets_the_code() {
        let pairing = Pairing::new("pair-elsewhere");
        let shell = &pairing.fixture.shell;
        let refused = block(shell.pair(&url(closed()), "K7QM4ZTD")).unwrap_err();
        assert_eq!(refused.title, "Can't open the server");
        let invalid = block(shell.pair("not a :// url", "K7QM4ZTD")).unwrap_err();
        assert_eq!(invalid.title, "That address won't work");
        assert!(pairing.tokens.servers().is_empty());
    }

    #[test]
    fn without_a_keychain_no_code_is_spent() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-no-keychain");
        pairing
            .tokens
            .fail_with(Some("This desktop has no keychain."));
        let refused = block(pairing.fixture.shell.pair(&server.url(), "K7QM4ZTD")).unwrap_err();
        assert_eq!(refused.title, "Can't keep a token on this computer");
        assert_eq!(refused.message, "This desktop has no keychain.");
        assert!(server.asked("/api/pair").is_empty(), "the code still works");
        assert!(pairing.settings().paired.is_empty());
    }

    #[test]
    fn a_token_the_keychain_wont_take_isnt_used() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-full");
        pairing
            .tokens
            .refuse_saves_with(Some("The keychain is full."));
        let shell = &pairing.fixture.shell;
        let refused = block(shell.pair(&server.url(), "K7QM4ZTD")).unwrap_err();
        assert_eq!(refused.title, "Paired, but couldn't keep the token");
        assert!(refused.message.contains("“Laptop”"), "{}", refused.message);
        assert_eq!(refused.detail.as_deref(), Some("The keychain is full."));
        assert!(pairing.settings().paired.is_empty());
        assert!(shell.current().is_none());
    }

    #[test]
    fn a_sync_only_code_isnt_kept() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-sync");
        let refused = block(pairing.fixture.shell.pair(&server.url(), "SYNC0NLY")).unwrap_err();
        assert_eq!(refused.title, "That code is for syncing only");
        assert!(refused.message.contains("“Laptop”"), "{}", refused.message);
        assert!(pairing.tokens.servers().is_empty());
        assert!(pairing.settings().paired.is_empty());
    }

    #[test]
    fn pairing_again_replaces_the_token_and_a_failed_try_keeps_it() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-again");
        let shell = &pairing.fixture.shell;
        let proxy = block(shell.pair(&server.url(), "K7QM4ZTD")).unwrap();

        // Looking at the address, or a wrong code, leaves the token in use.
        block(shell.probe(&server.url())).unwrap();
        block(shell.pair(&server.url(), "00000000")).unwrap_err();
        assert_eq!(token_of(&proxy).as_deref(), Some("cst_first"));

        let again = block(shell.pair(&server.url(), "K7QM-4ZTD")).unwrap();
        assert_eq!(
            again.addr(),
            proxy.addr(),
            "the same proxy, so the same origin"
        );
        assert_eq!(token_of(&proxy).as_deref(), Some("cst_second"));
        assert_eq!(pairing.tokens.servers(), [server.url()]);
    }

    #[test]
    fn a_token_the_keychain_lost_unpairs_the_server() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-lost");
        block(pairing.fixture.shell.pair(&server.url(), "K7QM4ZTD")).unwrap();
        // A new keychain: nothing in it.
        block(pairing.tokens.forget(&server.url())).unwrap();

        let proxy = block(pairing.restarted().open(&server.url())).unwrap();
        assert_eq!(token_of(&proxy), None, "the server's own login shows");
        assert!(pairing.settings().paired.is_empty());
    }

    #[test]
    fn a_keychain_that_cant_be_read_is_a_problem_to_show() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-locked");
        block(pairing.fixture.shell.pair(&server.url(), "K7QM4ZTD")).unwrap();
        pairing
            .tokens
            .fail_with(Some("The keychain wasn't unlocked."));

        let shell = pairing.restarted();
        let problem = block(shell.open(&server.url())).unwrap_err();
        assert_eq!(
            problem.title,
            "Can't get this computer's token for the server"
        );
        assert_eq!(problem.server, Some(server.url()));
        assert!(
            pairing.settings().paired.contains_key(&server.url()),
            "still paired"
        );
        // Unlocked: it opens.
        pairing.tokens.fail_with(None);
        assert_eq!(
            token_of(&block(shell.open(&server.url())).unwrap()).as_deref(),
            Some("cst_first")
        );
    }

    #[test]
    fn a_token_the_server_refuses_is_forgotten_and_said() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-removed");
        let shell = &pairing.fixture.shell;
        let proxy = block(shell.pair(&server.url(), "K7QM4ZTD")).unwrap();

        // The device is removed on the server: the page's next request is refused.
        server.revoke("cst_first");
        assert!(fetch(&proxy, "/api/status").contains("401"));
        let refused = pairing.refused.lock().unwrap().clone();
        assert_eq!(refused.len(), 1, "the proxy told the app");

        let problem = block(shell.rejected(refused[0].clone())).unwrap();
        assert_eq!(problem.title, "This computer was removed from the server");
        assert_eq!(problem.server, Some(server.url()));
        assert_eq!(shell.problem().map(|p| p.title), Some(problem.title));
        assert!(pairing.tokens.servers().is_empty());
        assert!(pairing.settings().paired.is_empty());
        assert_eq!(token_of(&proxy), None);
        assert_eq!(
            pairing.settings().server,
            Some(server.url()),
            "still the server"
        );

        // The same refusal, heard again from another request: nothing more to do.
        assert!(block(shell.rejected(refused[0].clone())).is_none());
        // Signed in with the password instead, the page works.
        assert!(fetch(&proxy, "/api/status").contains("200"));
    }

    #[test]
    fn a_late_refusal_of_an_old_token_doesnt_undo_a_new_pairing() {
        let server = PairingServer::start();
        let pairing = Pairing::new("pair-late");
        let shell = &pairing.fixture.shell;
        let proxy = block(shell.pair(&server.url(), "K7QM4ZTD")).unwrap();
        let old = proxy.upstream().unwrap();
        block(shell.pair(&server.url(), "K7QM-4ZTD")).unwrap();

        assert!(block(shell.rejected(old)).is_none());
        assert_eq!(token_of(&proxy).as_deref(), Some("cst_second"));
        assert_eq!(pairing.tokens.servers(), [server.url()]);
    }

    #[test]
    fn a_refusal_from_a_server_not_shown_stays_quiet() {
        let (a, b) = (PairingServer::start(), PairingServer::start());
        let pairing = Pairing::new("pair-quiet");
        let shell = &pairing.fixture.shell;
        let proxy_a = block(shell.pair(&a.url(), "K7QM4ZTD")).unwrap();
        block(shell.switch(Some(b.url()), Save::No)).unwrap();

        assert!(block(shell.rejected(proxy_a.upstream().unwrap())).is_none());
        assert!(shell.problem().is_none(), "B's page stays");
        assert!(
            pairing.tokens.servers().is_empty(),
            "A's token is gone all the same"
        );
    }

    #[test]
    fn forgetting_a_server_removes_its_token_cookies_and_port() {
        let (a, b) = (PairingServer::start(), PairingServer::start());
        let pairing = Pairing::new("forget");
        let shell = &pairing.fixture.shell;
        block(shell.pair(&b.url(), "K7QM4ZTD")).unwrap();
        let proxy = block(shell.pair(&a.url(), "K7QM4ZTD")).unwrap();
        let port = proxy.addr().port();
        let origin = format!("http://{}", a.addr);
        assert!(
            shell.cookies.origins().contains(&origin),
            "the check's cookie"
        );

        // Whatever was wrong with it is said no more once it's forgotten.
        shell.set_problem(Some(Problem::removed(&proxy.upstream().unwrap())));
        block(shell.forget_server(&a.url())).unwrap();
        assert_eq!(pairing.tokens.servers(), [b.url()], "B keeps its own");
        let settings = pairing.settings();
        assert_eq!(settings.server, None);
        assert!(!settings.paired.contains_key(&a.url()));
        assert!(!settings.ports.contains_key(&a.url()));
        assert!(settings.taken_except(&b.url()).contains(&port));
        assert!(!shell.cookies.origins().contains(&origin));
        assert!(shell.current().is_none(), "the window shows no server");
        assert!(proxy.upstream().is_none(), "its proxy serves nothing");
        assert!(shell.problem().is_none());
        assert!(settings.paired.contains_key(&b.url()));

        // Again, or a server it never knew: nothing to do, and no error.
        block(shell.forget_server(&a.url())).unwrap();
        assert_eq!(
            block(shell.forget_server("not a :// url"))
                .unwrap_err()
                .title,
            "That address won't work"
        );
    }

    #[test]
    fn forgetting_goes_as_far_as_it_can_without_the_keychain() {
        let server = PairingServer::start();
        let pairing = Pairing::new("forget-no-keychain");
        let shell = &pairing.fixture.shell;
        block(shell.pair(&server.url(), "K7QM4ZTD")).unwrap();
        pairing
            .tokens
            .fail_with(Some("The keychain wasn't unlocked."));

        let problem = block(shell.forget_server(&server.url())).unwrap_err();
        assert_eq!(problem.title, "Forgotten, except in the keychain");
        assert_eq!(
            problem.detail.as_deref(),
            Some("The keychain wasn't unlocked.")
        );
        assert!(pairing.settings().paired.is_empty());
        assert_eq!(pairing.settings().server, None);
    }

    #[test]
    fn every_servers_proxy_answers_the_apps_commands() {
        use crate::bridge::{Commands, Feature, Features, Host};

        struct Notifies;
        impl Host for Notifies {
            fn features(&self) -> Features {
                Box::pin(async { vec![Feature::Notify] })
            }
        }

        let (a, b) = (server("200 OK"), server("200 OK"));
        let mut fixture = Fixture::new("commands", a);
        // As the app sets its shell up: before any server is shown.
        let dir = fixture.dir.join("with-commands");
        fixture.shell = Shell::new(
            Store::open(dir),
            Cookies::default(),
            Url::parse("coversync://app/index.html").unwrap(),
            Arc::new(|_| {}),
        )
        .with_commands(Arc::new(Commands::new(Arc::new(Notifies))));

        for addr in [a, b] {
            let proxy = block(fixture.shell.switch(Some(url(addr)), Save::No)).unwrap();
            let cookie = enter(&proxy);
            let answer = send(
                &proxy,
                &[
                    "POST /__coversync-app/v1/capabilities HTTP/1.1".to_owned(),
                    format!("Host: {}", proxy.addr()),
                    format!("Cookie: {cookie}"),
                    format!("Origin: {}", proxy.origin()),
                    "Sec-Fetch-Site: same-origin".to_owned(),
                    "Content-Type: application/json".to_owned(),
                    "Content-Length: 2".to_owned(),
                    "Connection: close".to_owned(),
                ],
                "{}",
            );
            assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
            let body = answer.split_once("\r\n\r\n").unwrap().1;
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(body).unwrap(),
                serde_json::json!({ "version": 1, "features": ["notify"] })
            );
        }
    }

    #[test]
    fn only_the_shown_server_reaches_the_window() {
        // A is down; B is up.
        let (a, b) = (closed(), server("200 OK"));
        let fixture = Fixture::new("stale", a);
        let proxy_a = fixture.shell.current().unwrap();
        let reports_of_a = || {
            REPORTS
                .lock()
                .unwrap()
                .iter()
                .filter(|s| **s == url(a))
                .count()
        };

        page_load(&proxy_a);
        assert_eq!(reports_of_a(), 1, "while A is shown, its failure shows");
        block(fixture.shell.switch(Some(url(b)), Save::No)).unwrap();
        page_load(&proxy_a);
        assert_eq!(
            reports_of_a(),
            1,
            "once B is shown, A's late failure doesn't"
        );
    }
}
