//! Where the app keeps its files, by the XDG base directory rules.

use std::env;
use std::path::PathBuf;

use crate::app::APP_ID;

/// `$<var>` if it's an absolute path, or else `fallback` under the home
/// directory, as the XDG base directory specification has it.
fn base(var: &str, fallback: &str, get: impl Fn(&str) -> Option<String>) -> PathBuf {
    get(var)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| PathBuf::from(get("HOME").unwrap_or_default()).join(fallback))
}

fn from_env(var: &str) -> Option<String> {
    env::var(var).ok().filter(|value| !value.is_empty())
}

/// The app's settings and cookies (`~/.config/io.github.brpjerry.coversync`).
pub fn config_dir() -> PathBuf {
    base("XDG_CONFIG_HOME", ".config", from_env).join(APP_ID)
}

/// The web view's data: local storage and the like
/// (`~/.local/share/io.github.brpjerry.coversync/webengine`). The builds on
/// WebKitGTK kept theirs in the folder above, in WebKit's own format, which
/// Qt WebEngine can't read: a server's settings in local storage start anew.
pub fn storage_dir() -> PathBuf {
    base("XDG_DATA_HOME", ".local/share", from_env)
        .join(APP_ID)
        .join("webengine")
}

/// The web view's HTTP cache
/// (`~/.cache/io.github.brpjerry.coversync/webengine`).
pub fn cache_dir() -> PathBuf {
    base("XDG_CACHE_HOME", ".cache", from_env)
        .join(APP_ID)
        .join("webengine")
}

/// Where a socket for this session can go: `$XDG_RUNTIME_DIR`, or the
/// system's temporary folder without one.
pub fn runtime_dir() -> PathBuf {
    from_env("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_xdg_variables_and_falls_back_to_the_home_directory() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |var: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == var)
                    .map(|(_, value)| value.to_string())
            }
        };
        let home = env(&[("HOME", "/home/a")]);
        assert_eq!(
            base("XDG_CONFIG_HOME", ".config", home),
            PathBuf::from("/home/a/.config")
        );
        let set = env(&[("HOME", "/home/a"), ("XDG_CONFIG_HOME", "/elsewhere")]);
        assert_eq!(
            base("XDG_CONFIG_HOME", ".config", set),
            PathBuf::from("/elsewhere")
        );
        // A relative path there is ignored, as the specification says.
        let relative = env(&[("HOME", "/home/a"), ("XDG_CONFIG_HOME", "config")]);
        assert_eq!(
            base("XDG_CONFIG_HOME", ".config", relative),
            PathBuf::from("/home/a/.config")
        );
    }
}
