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
use tauri::Url;

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
    /// The server the window shows.
    current: Mutex<Option<Proxy>>,
    /// Why the fallback page is showing, when something went wrong.
    problem: Mutex<Option<Problem>>,
}

/// Something that went wrong, as the fallback page shows it.
#[derive(Clone, Debug, Serialize)]
pub struct Problem {
    pub title: String,
    pub message: String,
    /// The error underneath, in small print, when it adds something.
    pub detail: Option<String>,
}

impl Problem {
    pub fn new(title: &str, message: impl Into<String>) -> Self {
        Self {
            title: title.to_owned(),
            message: message.into(),
            detail: None,
        }
    }

    pub fn unreachable(unreachable: Unreachable) -> Self {
        Self {
            detail: unreachable.detail,
            ..Self::new("Can't reach the server", unreachable.reason)
        }
    }

    pub fn invalid(err: UpstreamError) -> Self {
        Self::new("That address won't work", err.to_string())
    }
}

/// Whether a server chosen now is saved as the one to use from then on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Save {
    Yes,
    /// `--server` and `COVERSYNC_SERVER` pick a server for one run.
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
            current: Mutex::new(None),
            problem: Mutex::new(None),
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
    /// one on the port kept for that server. A server without one gets a
    /// random port that's no other server's.
    pub async fn proxy_for(&self, upstream: &Upstream) -> std::io::Result<Proxy> {
        let url = upstream.url();
        if let Some(proxy) = self.proxies.lock().expect("proxies lock").get(&url) {
            return Ok(proxy.clone());
        }
        let ports = self.config.get().ports;
        let others = ports
            .iter()
            .filter(|(server, _)| **server != url)
            .map(|(_, port)| *port);
        let on_unreachable = self.on_unreachable.clone();
        let proxy = Proxy::builder()
            .upstream(Some(upstream.clone()))
            .preferred_port(ports.get(&url).copied())
            .avoid_ports(others)
            .cookies(self.cookies.clone())
            .on_unreachable(move |unreachable| on_unreachable(unreachable))
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

    /// Shows `proxy`'s server from now on, and keeps its port for it.
    pub fn show(&self, proxy: Proxy) {
        if let Some(upstream) = proxy.upstream() {
            let (url, port) = (upstream.url(), proxy.addr().port());
            if self.config.get().ports.get(&url) != Some(&port)
                && let Err(err) = self.config.update(|config| {
                    config.ports.insert(url, port);
                })
            {
                tracing::warn!("couldn't save the server's port: {err}");
            }
        }
        *self.current.lock().expect("current lock") = Some(proxy);
    }

    /// Picks `server`, or with `None` tries the current one again, and checks
    /// that it's a coversync server. Only then is a new server shown (and,
    /// with [`Save::Yes`], saved), so a typo can't replace an address that
    /// works. A failed retry of the current server becomes the page's
    /// problem.
    pub async fn switch(&self, server: Option<String>, save: Save) -> Result<Proxy, Problem> {
        let (upstream, new) = match server {
            Some(url) => (Upstream::parse(&url).map_err(Problem::invalid)?, true),
            None => match self.current().and_then(|proxy| proxy.upstream()) {
                Some(upstream) => (upstream, false),
                None => return Err(Problem::new("No server is set", "Enter its address.")),
            },
        };
        let proxy = self
            .proxy_for(&upstream)
            .await
            .map_err(|err| Problem::new("Couldn't start the app's proxy", err.to_string()))?;
        if let Err(unreachable) = proxy.check(&upstream).await {
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

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::path::PathBuf;

    use super::*;

    /// A server that answers every request with `status`, and coversync's
    /// `/api/hello` body when it's a success.
    fn server(status: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = [0; 4096];
                let _ = stream.read(&mut request);
                let body = if status.starts_with('2') {
                    r#"{"app":"coversync","name":"test","version":"0.1.0","api":["1"]}"#
                } else {
                    ""
                };
                let answer = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
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
                Url::parse("tauri://localhost/index.html").unwrap(),
                Arc::new(|_| {}),
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

    fn block<F: std::future::Future>(future: F) -> F::Output {
        tauri::async_runtime::block_on(future)
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
        assert_eq!(problem.title, "Can't reach the server");
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
        assert!(
            !fixture
                .shell
                .is_proxied(&page("tauri://localhost/index.html"))
        );
        assert!(
            !fixture
                .shell
                .is_proxied(&page("http://tauri.localhost/index.html"))
        );
        let other_port = page(&format!("http://127.0.0.1:{}/", proxy.addr().port() ^ 1));
        assert!(
            !fixture.shell.is_proxied(&other_port),
            "the dev server, say"
        );
    }
}
