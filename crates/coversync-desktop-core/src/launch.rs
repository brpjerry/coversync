//! A launch of the app: the server it asks for, launches that arrive before
//! the app has started, and what it logs.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use url::Url;

/// A server for this run only, like `--server`. Neither is saved.
pub const SERVER_ENV: &str = "COVERSYNC_SERVER";

/// `--server <url>` or `--server=<url>`, from the arguments after the
/// program's name.
pub fn server_arg(args: impl IntoIterator<Item = String>) -> Option<String> {
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

/// A pairing link, as the server's Add device makes it and the desktop
/// hands it to the app as an argument:
/// `coversync://pair?server=https%3A%2F%2Fn5pro.internal%2Fcoversync%2F&code=K7QM4ZTD`.
///
/// It can come from anywhere (a web page can make the desktop open one), so
/// it only ever leads to a question: the app names the server and asks
/// before it pairs. Its `ca=`, for apps that bring their own certificate
/// store, is ignored: this app trusts the system's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairLink {
    /// The server's address, as the link gives it: not checked yet.
    pub server: String,
    pub code: String,
}

impl PairLink {
    /// Reads one argument. `None` for anything but a pairing link with a
    /// server and a code; `coversync://app/…`, the window's own pages, is
    /// none.
    pub fn parse(arg: &str) -> Option<Self> {
        let url = Url::parse(arg.trim()).ok()?;
        let plain = url.username().is_empty() && url.password().is_none() && url.port().is_none();
        if url.scheme() != "coversync"
            || url.host_str() != Some("pair")
            || !matches!(url.path(), "" | "/")
            || !plain
        {
            return None;
        }
        let value = |name: &str| {
            let mut values = url.query_pairs().filter(|(key, _)| key == name);
            let value = values.next()?.1.trim().to_owned();
            // Given twice, it's unclear which is meant.
            (values.next().is_none() && !value.is_empty()).then_some(value)
        };
        Some(Self {
            server: value("server")?,
            code: value("code")?,
        })
    }

    /// The first pairing link among a launch's arguments, after the
    /// program's name.
    pub fn among(args: impl IntoIterator<Item = String>) -> Option<Self> {
        args.into_iter().find_map(|arg| Self::parse(&arg))
    }
}

/// A later launch's arguments, with its `COVERSYNC_SERVER` as `--server`
/// when it gives no `--server` of its own: the app, which is handed a
/// launch's environment, keeps both together while it waits to start.
pub fn with_env_server(mut args: Vec<String>, env_server: Option<String>) -> Vec<String> {
    if let Some(server) = env_server.filter(|server| !server.is_empty())
        && server_arg(args.iter().skip(1).cloned()).is_none()
    {
        args.push(format!("--server={server}"));
    }
    args
}

/// Launches that came in before the app had started: set up, with its first
/// page on the way (after the one cache clear, if due). They're handled once
/// it has.
/// A launch is whatever the app keeps of one: its arguments, or those with
/// the token that lets its window take the focus.
pub struct EarlyLaunches<T = Vec<String>> {
    started: bool,
    waiting: Vec<T>,
}

impl<T> Default for EarlyLaunches<T> {
    fn default() -> Self {
        Self {
            started: false,
            waiting: Vec::new(),
        }
    }
}

impl<T> EarlyLaunches<T> {
    /// Keeps `launch` for later until the app has started; then hands it
    /// back.
    pub fn hold(&mut self, launch: T) -> Option<T> {
        if self.started {
            return Some(launch);
        }
        self.waiting.push(launch);
        None
    }

    /// The app has started: the launches held until now.
    pub fn release(&mut self) -> Vec<T> {
        self.started = true;
        std::mem::take(&mut self.waiting)
    }
}

/// Warnings from everything, with `RUST_LOG`'s directives on top: so
/// `RUST_LOG=coversync_proxy=debug` adds the proxy's requests without hiding
/// the app's own warnings. A level of its own in `RUST_LOG` (`debug`)
/// replaces the default.
pub fn log_filter(rust_log: &str) -> EnvFilter {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_pairing_link() {
        let link =
            "coversync://pair?server=https%3A%2F%2Fn5pro.internal%2Fcoversync%2F&code=K7QM4ZTD";
        let read = PairLink::parse(link).unwrap();
        assert_eq!(read.server, "https://n5pro.internal/coversync/");
        assert_eq!(read.code, "K7QM4ZTD");
        // With the certificate's hash for other apps, which this one ignores.
        let with_ca = PairLink::parse(&format!("{link}&ca=sha256%3Aabcd")).unwrap();
        assert_eq!(with_ca, read);
        assert_eq!(
            PairLink::parse("coversync://pair/?code=K7QM-4ZTD&server=http://127.0.0.1:8770/"),
            Some(PairLink {
                server: "http://127.0.0.1:8770/".into(),
                code: "K7QM-4ZTD".into(),
            })
        );
    }

    #[test]
    fn only_a_whole_pairing_link_is_one() {
        for not_one in [
            "",
            "--server",
            "https://n5pro.internal/coversync/",
            // The window's own pages.
            "coversync://app/index.html",
            "coversync://app/__bridge?server=https://a/&code=K7QM4ZTD",
            // Another host, a path, a port or a user in the pairing link's place.
            "coversync://pairing?server=https://a/&code=K7QM4ZTD",
            "coversync://pair/now?server=https://a/&code=K7QM4ZTD",
            "coversync://pair:1?server=https://a/&code=K7QM4ZTD",
            "coversync://user@pair?server=https://a/&code=K7QM4ZTD",
            "coversyncs://pair?server=https://a/&code=K7QM4ZTD",
            // A part missing, empty, or given twice.
            "coversync://pair",
            "coversync://pair?server=https://a/",
            "coversync://pair?code=K7QM4ZTD",
            "coversync://pair?server=&code=K7QM4ZTD",
            "coversync://pair?server=https://a/&code=",
            "coversync://pair?server=https://a/&server=https://b/&code=K7QM4ZTD",
            "coversync://pair?server=https://a/&code=K7QM4ZTD&code=00000000",
        ] {
            assert_eq!(PairLink::parse(not_one), None, "{not_one}");
        }
    }

    #[test]
    fn finds_the_link_among_a_launchs_arguments() {
        let args = |list: &[&str]| list.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let link = "coversync://pair?server=https://a/&code=K7QM4ZTD";
        assert!(PairLink::among(args(&[link])).is_some());
        assert!(PairLink::among(args(&["--server", "https://b/", link])).is_some());
        assert_eq!(PairLink::among(args(&["--server", "https://b/"])), None);
        assert_eq!(PairLink::among(args(&[])), None);
    }

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
    fn launches_wait_until_the_app_has_started() {
        let mut early = EarlyLaunches::<Vec<String>>::default();
        let launch = |n: &str| vec!["app".to_owned(), format!("--server=http://{n}/")];
        // While setup or the cache clear runs.
        assert_eq!(early.hold(launch("a")), None);
        assert_eq!(early.hold(launch("b")), None);
        assert_eq!(early.release(), [launch("a"), launch("b")]);
        assert_eq!(early.hold(launch("c")), Some(launch("c")));
        assert!(early.release().is_empty());
    }

    #[test]
    fn a_launchs_environment_names_a_server_its_arguments_dont() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let env = |server: &str| Some(server.to_owned());
        assert_eq!(
            with_env_server(args(&["app"]), env("https://env/")),
            args(&["app", "--server=https://env/"])
        );
        let given = args(&["app", "--server", "https://arg/"]);
        assert_eq!(with_env_server(given.clone(), env("https://env/")), given);
        assert_eq!(with_env_server(args(&["app"]), env("")), args(&["app"]));
        assert_eq!(with_env_server(args(&["app"]), None), args(&["app"]));
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
