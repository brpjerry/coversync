//! The app's state, apart from the window: which server it shows, a proxy
//! for each server it has used, and what went wrong.
//!
//! Each server gets a proxy of its own, on a port of its own that's kept
//! between runs, so each has an origin of its own in the webview: its HTTP
//! cache and the UI's local storage (sort, sidebar) aren't shared with
//! another server's. The proxies share one cookie jar, which keeps each
//! server's cookies apart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use coversync_proxy::{Cookies, Proxy, Unreachable, Upstream, UpstreamError};
use serde::Serialize;
use url::Url;

use crate::config::Store;

pub type UnreachableHandler = Arc<dyn Fn(Unreachable) + Send + Sync>;

pub struct Shell {
    pub config: Store,
    cookies: Cookies,
    /// The fallback page's URL.
    pub fallback: Url,
    on_unreachable: UnreachableHandler,
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

/// Something that went wrong, as the fallback page shows it.
#[derive(Clone, Debug, Serialize)]
pub struct Problem {
    pub title: String,
    pub message: String,
    /// The error underneath, in small print, when it adds something.
    pub detail: Option<String>,
    /// The address it's about: the page shows it, and puts it in the field.
    pub server: Option<String>,
}

impl Problem {
    pub fn new(title: &str, message: impl Into<String>) -> Self {
        Self {
            title: title.to_owned(),
            message: message.into(),
            detail: None,
            server: None,
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
            proxies: Mutex::new(HashMap::new()),
            current: Arc::new(Mutex::new(None)),
            problem: Mutex::new(None),
            navigation: tokio::sync::Mutex::new(()),
        }
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
        let proxy = Proxy::builder()
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

    /// [`Shell::proxy_for`], with a failure to start it as the page's problem.
    async fn started_proxy(&self, upstream: &Upstream) -> Result<Proxy, Problem> {
        self.proxy_for(upstream).await.map_err(|err| Problem {
            server: Some(upstream.url()),
            ..Problem::new("Couldn't start the app's proxy", err.to_string())
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
            Some(url) => (
                Upstream::parse(&url).map_err(|err| Problem::invalid(err, &url))?,
                true,
            ),
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
        for bundled in [
            "coversync://app/index.html",
            "tauri://localhost/index.html",
            "http://tauri.localhost/index.html",
        ] {
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

    /// Opens `proxy`'s entry URL, then loads its server's page through it, as
    /// the window does, over plain sockets.
    fn page_load(proxy: &Proxy) {
        let addr = proxy.addr();
        let entry = proxy.entry_url();
        let entry = &entry[entry.find("/?k=").unwrap()..];
        let send = |request: String| {
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
            let mut answer = String::new();
            let _ = stream.read_to_string(&mut answer);
            answer
        };
        let request = |lines: &[String]| format!("{}\r\n\r\n", lines.join("\r\n"));
        let answer = send(request(&[
            format!("GET {entry} HTTP/1.1"),
            format!("Host: {addr}"),
            "Connection: close".to_owned(),
        ]));
        let cookie = answer
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("set-cookie")
                    .then(|| value.trim().to_owned())
            })
            .unwrap();
        let cookie = cookie.split(';').next().unwrap();
        send(request(&[
            "GET / HTTP/1.1".to_owned(),
            format!("Host: {addr}"),
            format!("Cookie: {cookie}"),
            "Sec-Fetch-Dest: document".to_owned(),
            "Accept: text/html".to_owned(),
            "Connection: close".to_owned(),
        ]));
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
