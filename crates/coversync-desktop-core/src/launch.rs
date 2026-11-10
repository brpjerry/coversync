//! A launch of the app: the server it asks for, launches that arrive before
//! the app has started, and what it logs.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;

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
#[derive(Default)]
pub struct EarlyLaunches {
    started: bool,
    waiting: Vec<Vec<String>>,
}

impl EarlyLaunches {
    /// Keeps `args` for later until the app has started; then hands them
    /// back.
    pub fn hold(&mut self, args: Vec<String>) -> Option<Vec<String>> {
        if self.started {
            return Some(args);
        }
        self.waiting.push(args);
        None
    }

    /// The app has started: the launches held until now.
    pub fn release(&mut self) -> Vec<Vec<String>> {
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
        let mut early = EarlyLaunches::default();
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
