//! `coversync`: see the library crate.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use coversync_cli::agent::{self, Settings, Shows, Stop};
use coversync_cli::config::{self, Config};
use coversync_cli::notify::Notifier;
use coversync_cli::system::System;
use coversync_cli::tokens::Source;
use coversync_desktop_core::config::Device;
use coversync_desktop_core::{ConfigLock, Store};
use coversync_proxy::{AboutApp, Upstream};
use coversync_replica::{CancellationToken, Options, WatchOptions};
use tokio::signal::unix::{SignalKind, signal};

/// The settings can't be used as they are: nothing a restart mends.
const EXIT_SETUP: u8 = 2;
/// This computer isn't paired with the server, or not any more.
const EXIT_UNPAIRED: u8 = 3;

/// How long the server gets to answer a pairing.
const PAIR_TIMEOUT: Duration = Duration::from_secs(30);

/// Keeps a copy of a coversync server's Library on this computer.
#[derive(Parser, Debug)]
#[command(name = "coversync", version)]
struct Args {
    /// The settings file (default:
    /// $XDG_CONFIG_HOME/io.github.brpjerry.coversync/sync.json).
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Sync in the background until stopped: the sync agent.
    ///
    /// Syncs the folder whenever the server's Library changes, after the
    /// quiet period set for this device in Sync setup, and at once for Sync
    /// now. The server, the folder and the token are read from the settings
    /// file unless given here. It is what the coversync-agent user service
    /// runs.
    Run {
        /// The server's address, such as https://n5pro.internal/coversync.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
        /// The folder the copy of the Library is in.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
        /// A file holding this computer's token, instead of the keychain.
        #[arg(long, value_name = "FILE")]
        token_file: Option<PathBuf>,
    },
    /// Show what's kept on this computer, or change it.
    ///
    /// Without options, shows the server and the folder. The folder is
    /// never sent to the server.
    Setup {
        /// The server to sync with.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
        /// The folder to keep the copy of the Library in. It must exist.
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
        /// Read the token from this file: for a computer with no keychain.
        #[arg(long, value_name = "FILE", conflicts_with = "keychain")]
        token_file: Option<PathBuf>,
        /// Read the token from the keychain again.
        #[arg(long)]
        keychain: bool,
    },
    /// Pair this computer with a server, without the app.
    ///
    /// Takes a code from Add device in the server's Sync setup. A computer
    /// the coversync app has paired needs no pairing of its own: the agent
    /// uses the app's token.
    Pair {
        /// The server's address.
        server: String,
        /// The code Add device shows.
        code: String,
        /// Keep the token in this file instead of the keychain.
        #[arg(long, value_name = "FILE")]
        token_file: Option<PathBuf>,
    },
}

/// The status, to systemd and the log.
struct Status(Notifier);

impl Shows for Status {
    fn status(&self, text: &str) {
        tracing::debug!("{text}");
        self.0.status(text);
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "coversync=info,coversync_cli=info,coversync_replica=info".into());
    let log = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    // The journal stamps each line itself.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        log.without_time().init();
    } else {
        log.init();
    }
    let file = args.config.clone().unwrap_or_else(config::file);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => return fail(EXIT_SETUP, &format!("Couldn't start: {e}.")),
    };
    let done = runtime.block_on(async {
        match args.command {
            Command::Run {
                server,
                root,
                token_file,
            } => run(&file, server, root, token_file).await,
            Command::Setup {
                server,
                root,
                token_file,
                keychain,
            } => setup(&file, server, root, token_file, keychain),
            Command::Pair {
                server,
                code,
                token_file,
            } => pair(&file, &server, &code, token_file).await,
        }
    });
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, why)) => fail(code, &why),
    }
}

fn fail(code: u8, why: &str) -> ExitCode {
    eprintln!("coversync: {why}");
    ExitCode::from(code)
}

type Done = Result<(), (u8, String)>;

fn setup_error(why: String) -> (u8, String) {
    (EXIT_SETUP, why)
}

async fn run(
    file: &Path,
    server: Option<String>,
    root: Option<PathBuf>,
    token_file: Option<PathBuf>,
) -> Done {
    let config = Config::load(file).map_err(setup_error)?;
    let Resolved {
        server,
        root,
        tokens,
    } = resolve(config, server, root, token_file).map_err(setup_error)?;
    tracing::info!(server, root = %root.display(), "the sync agent is starting");

    let status = Arc::new(Status(Notifier::from_env()));
    let shutdown = CancellationToken::new();
    let listening = |kind| signal(kind).map_err(|e| setup_error(format!("Couldn't start: {e}.")));
    let mut terminate = listening(SignalKind::terminate())?;
    let mut interrupt = listening(SignalKind::interrupt())?;
    let settings = Settings {
        server,
        root,
        tokens,
        state_file: Some(config::state_file()),
        watch: WatchOptions::default(),
        options: Options::default(),
        look_again: agent::LOOK_AGAIN,
    };
    status.0.ready();
    status.status("Starting.");
    let agent = agent::run(
        settings,
        System::desktop(),
        status.clone(),
        shutdown.clone(),
    );
    tokio::pin!(agent);
    let ended = loop {
        tokio::select! {
            ended = &mut agent => break ended,
            _ = terminate.recv() => shutdown.cancel(),
            _ = interrupt.recv() => shutdown.cancel(),
        }
        status.0.stopping();
        status.status("Stopping.");
    };
    match ended {
        Ok(()) => {
            tracing::info!("the sync agent stopped");
            Ok(())
        }
        Err(stop) => {
            status.status(&stop.to_string());
            let code = match stop {
                Stop::NotPaired | Stop::Refused => EXIT_UNPAIRED,
                Stop::Setup(_) => EXIT_SETUP,
            };
            Err((code, stop.to_string()))
        }
    }
}

/// Keeps the app out while a pairing replaces the keychain's token, which
/// is the app's too. An app that's open still holds the token being
/// replaced, and would take the server's refusal of that one as reason to
/// remove the new one. So the app's own lock on its settings folder is
/// held for the pairing, and an app that's open means no pairing here. A
/// token file is nobody else's: no lock.
///
/// Where locks don't work at all (the app goes on without one there too),
/// whether the app is open can't be told: the pairing goes ahead, and says
/// what to do if it was.
fn apps_lock(tokens: &Source, settings: Option<&Path>) -> Result<Option<ConfigLock>, String> {
    let (Source::Keychain, Some(settings)) = (tokens, settings) else {
        return Ok(None);
    };
    let lock = ConfigLock::take(settings).map_err(|_| {
        "The coversync app is open. Pair in its window instead, or close it first: it would \
         undo a pairing made behind its back."
            .to_owned()
    })?;
    if !lock.held() {
        eprintln!(
            "coversync: couldn't check whether the coversync app is open ({} can't be locked). \
             If it is, close it and pair again, or pair in its window.",
            settings.display()
        );
    }
    Ok(Some(lock))
}

/// Tells the app, in its settings, of a pairing that put a token in the
/// keychain for `url`. A token that manages is one the app signs in with:
/// the server is listed as paired, as if the app had paired itself. A
/// sync-only token isn't (the app would be refused everything it asks):
/// the server is taken off the list, and the app signs in by password.
fn note_for_app(
    settings: &Path,
    url: &str,
    paired: &coversync_proxy::Paired,
) -> std::io::Result<()> {
    let store = Store::open(settings.to_owned());
    store.update(|app| {
        if paired.manages {
            let device = Device {
                id: paired.device_id,
                name: paired.name.clone(),
            };
            app.paired.insert(url.to_owned(), device);
        } else {
            app.paired.remove(url);
        }
    })
}

/// What the agent runs with, once the command's options and the settings
/// file are put together.
#[derive(Debug, PartialEq)]
struct Resolved {
    server: String,
    root: PathBuf,
    tokens: Source,
}

/// Puts `run`'s options over the settings file's: an option given wins.
fn resolve(
    config: Config,
    server: Option<String>,
    root: Option<PathBuf>,
    token_file: Option<PathBuf>,
) -> Result<Resolved, String> {
    let given = server.as_deref().map(config::server_url).transpose()?;
    // The settings' token file is the token of the settings' server: sent
    // to another one, it would hand that server this computer's sign-in
    // to the first. Another server needs its own named.
    if let (Some(given), Some(kept), Some(file), None) =
        (&given, &config.server, &config.token_file, &token_file)
        && given != kept
    {
        return Err(format!(
            "The token file in the settings ({}) is this computer's token for {kept}, not for \
             {given}. Name {given}'s with --token-file, or pair with it: coversync pair.",
            file.display()
        ));
    }
    let server = given.or(config.server).ok_or(
        "No server is set. Turn sync on in the coversync app's settings, or run `coversync \
         setup --server <url> --root <folder>`.",
    )?;
    let server = config::server_url(&server)?;
    let root = root.or(config.root).ok_or(
        "No folder is set. Choose one in the coversync app's settings, or run `coversync setup \
         --root <folder>`.",
    )?;
    // Not checked for being there: a folder on a disk that isn't mounted
    // yet is waited for.
    let root = config::path(&root)?;
    let tokens = token_file
        .or(config.token_file)
        .map_or(Source::Keychain, Source::File);
    Ok(Resolved {
        server,
        root,
        tokens,
    })
}

fn setup(
    file: &Path,
    server: Option<String>,
    root: Option<PathBuf>,
    token_file: Option<PathBuf>,
    keychain: bool,
) -> Done {
    let mut config = Config::load(file).map_err(setup_error)?;
    let changing = server.is_some() || root.is_some() || token_file.is_some() || keychain;
    // A token file is one server's token. Moved to another server without
    // a token file named for it, the old file is let go of (it would be
    // sent to the new server otherwise) and the keychain is looked in.
    let mut let_go = None;
    if let Some(server) = server {
        let server = config::server_url(&server).map_err(setup_error)?;
        let moved = config.server.as_ref().is_some_and(|kept| *kept != server);
        if moved && token_file.is_none() {
            let_go = config.token_file.take();
        }
        config.server = Some(server);
    }
    if let Some(root) = root {
        config.root = Some(config::folder(&root).map_err(setup_error)?);
    }
    if let Some(token_file) = token_file {
        config.token_file = Some(config::path(&token_file).map_err(setup_error)?);
    }
    if keychain {
        config.token_file = None;
    }
    if changing {
        config.save(file).map_err(setup_error)?;
    }
    let or_unset = |value: Option<String>| value.unwrap_or_else(|| "not set".into());
    println!("Server: {}", or_unset(config.server.clone()));
    println!(
        "Folder: {}",
        or_unset(config.root.as_ref().map(|p| p.display().to_string()))
    );
    println!(
        "Token:  {}",
        config.token_file.as_ref().map_or_else(
            || "in the keychain".to_owned(),
            |p| format!("in {}", p.display())
        )
    );
    if let Some(old) = let_go {
        println!(
            "The token file {} was the other server's and is no longer used. Pair with this \
             one (coversync pair), or name its token file with --token-file.",
            old.display()
        );
    }
    if changing {
        println!("Saved to {}.", file.display());
        println!(
            "A running agent takes it up when restarted: systemctl --user restart coversync-agent"
        );
    }
    Ok(())
}

async fn pair(file: &Path, server: &str, code: &str, token_file: Option<PathBuf>) -> Done {
    let mut config = Config::load(file).map_err(setup_error)?;
    let upstream = Upstream::parse(server).map_err(|e| setup_error(e.to_string()))?;
    let url = upstream.url();
    // Before the code is spent: a path the settings can't hold would leave
    // a token nobody finds again.
    let token_file = match token_file {
        Some(path) => Some(config::path(&path).map_err(setup_error)?),
        None => config.token_file.clone(),
    };
    let tokens = token_file.clone().map_or(Source::Keychain, Source::File);
    let _apps_lock = apps_lock(&tokens, config::file().parent()).map_err(setup_error)?;
    // Before the code is spent: there has to be somewhere for the token.
    tokens.ready().await.map_err(setup_error)?;
    let about = AboutApp {
        platform: std::env::consts::OS.to_owned(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        hostname: std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|name| name.trim().to_owned())
            .unwrap_or_default(),
    };
    let paired = coversync_proxy::pair(&upstream, code.trim(), &about, PAIR_TIMEOUT)
        .await
        .map_err(|e| {
            let detail = e
                .detail
                .as_ref()
                .map_or(String::new(), |d| format!(" ({d})"));
            (EXIT_UNPAIRED, format!("{e}{detail}"))
        })?;
    tokens.save(&url, &paired.token).await.map_err(|why| {
        setup_error(format!(
            "Paired, but the token couldn't be kept: {why} Remove \"{}\" in Sync setup and pair \
             again.",
            paired.name
        ))
    })?;
    // The keychain's token is the app's too, and the app goes by its own
    // list of the servers it's paired with: kept in step, under the lock
    // still held.
    if let (Source::Keychain, Some(settings)) = (&tokens, config::file().parent())
        && let Err(e) = note_for_app(settings, &url, &paired)
    {
        eprintln!(
            "coversync: paired, but the app's settings in {} couldn't be updated ({e}). The \
             app may ask to pair again.",
            settings.display()
        );
    }
    config.server = Some(url.clone());
    config.token_file = token_file;
    config.save(file).map_err(setup_error)?;
    println!("Paired with {url} as \"{}\".", paired.name);
    match &config.root {
        Some(root) => println!("It syncs into {}.", root.display()),
        None => println!("Choose its folder next: coversync setup --root <folder>"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn an_option_given_wins_over_the_settings_file() {
        let saved = || Config {
            server: Some("https://saved.example/".into()),
            root: Some("/saved".into()),
            token_file: Some("/saved/token".into()),
            ..Default::default()
        };
        // Nothing given: the file's, with the token where the file says.
        assert_eq!(
            resolve(saved(), None, None, None),
            Ok(Resolved {
                server: "https://saved.example/".into(),
                root: "/saved".into(),
                tokens: Source::File("/saved/token".into()),
            })
        );
        // Each option alone replaces just its own setting, and a server is
        // read as the app reads one.
        let given = resolve(
            saved(),
            Some("given.example/music".into()),
            Some("/given".into()),
            Some("/given/token".into()),
        )
        .unwrap();
        assert_eq!(given.server, "https://given.example/music/");
        assert_eq!(given.root, Path::new("/given"));
        assert_eq!(given.tokens, Source::File("/given/token".into()));
        let one = resolve(saved(), None, Some("/given".into()), None).unwrap();
        assert_eq!(one.server, "https://saved.example/");
        assert_eq!(one.root, Path::new("/given"));

        // Another server than the settings' one, with only the settings'
        // token file to go by: refused, not sent that token.
        let other = resolve(saved(), Some("https://other.example".into()), None, None);
        let why = other.unwrap_err();
        assert!(why.contains("not for https://other.example/"), "{why}");
        // The same server spelled another way is the same server.
        assert!(resolve(saved(), Some("saved.example".into()), None, None).is_ok());

        // No token file anywhere: the keychain.
        let keychain = Config {
            token_file: None,
            ..saved()
        };
        assert_eq!(
            resolve(keychain, None, None, None).unwrap().tokens,
            Source::Keychain
        );
        // Nothing set up says what's missing.
        let missing = resolve(Config::default(), None, None, None).unwrap_err();
        assert!(missing.contains("No server is set"), "{missing}");
        let no_folder = Config {
            root: None,
            ..saved()
        };
        let missing = resolve(no_folder, None, None, None).unwrap_err();
        assert!(missing.contains("No folder is set"), "{missing}");
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_keychain_isnt_paired_behind_the_open_apps_back() {
        let dir = std::env::temp_dir().join(format!("coversync-cli-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // No app open: the pairing holds the lock itself, for as long as
        // it takes.
        let held = apps_lock(&Source::Keychain, Some(&dir)).unwrap();
        assert!(held.is_some());
        // The app (here, whoever holds its lock) is open: not now.
        let refused = apps_lock(&Source::Keychain, Some(&dir))
            .err()
            .expect("refused while the lock is held");
        assert!(refused.contains("app is open"), "{refused}");
        // A token file is the command's own business either way.
        let file = Source::File(dir.join("token"));
        assert!(apps_lock(&file, Some(&dir)).unwrap().is_none());
        drop(held);
        assert!(apps_lock(&Source::Keychain, Some(&dir)).unwrap().is_some());
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn switching_between_a_token_file_and_the_keychain_is_kept() {
        let dir = std::env::temp_dir().join(format!("coversync-cli-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sync.json");
        let token = dir.join("token");
        setup(&file, None, Some(dir.clone()), Some(token.clone()), false).unwrap();
        let saved = Config::load(&file).unwrap();
        assert_eq!(saved.token_file.as_deref(), Some(token.as_path()));
        assert_eq!(saved.root.as_deref(), Some(dir.as_path()));
        // Back to the keychain: the folder stays.
        setup(&file, None, None, None, true).unwrap();
        let saved = Config::load(&file).unwrap();
        assert_eq!(saved.token_file, None);
        assert_eq!(saved.root.as_deref(), Some(dir.as_path()));
        // A token file is one server's: moving to another server lets go
        // of it, unless that server's is named in the same breath.
        let a = Some("https://a.example".to_owned());
        let b = Some("https://b.example".to_owned());
        setup(&file, a.clone(), None, Some(token.clone()), false).unwrap();
        setup(&file, a.clone(), None, None, false).unwrap();
        let saved = Config::load(&file).unwrap();
        assert_eq!(
            saved.token_file.as_deref(),
            Some(token.as_path()),
            "the same server"
        );
        setup(&file, b.clone(), None, None, false).unwrap();
        let saved = Config::load(&file).unwrap();
        assert_eq!(saved.server.as_deref(), Some("https://b.example/"));
        assert_eq!(saved.token_file, None, "a's token would have gone to b");
        let other = dir.join("b-token");
        setup(&file, a.clone(), None, Some(other.clone()), false).unwrap();
        let saved = Config::load(&file).unwrap();
        assert_eq!(saved.token_file.as_deref(), Some(other.as_path()));
        setup(&file, None, None, None, true).unwrap();

        // Only looking changes nothing.
        let before = std::fs::read(&file).unwrap();
        setup(&file, None, None, None, false).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), before);
        // A folder that isn't there is refused, and the settings stay.
        assert!(setup(&file, None, Some(dir.join("missing")), None, false).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_pairing_into_the_keychain_is_told_to_the_app() {
        let dir = std::env::temp_dir().join(format!("coversync-cli-note-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let url = "https://music.example/";
        let paired = |manages| coversync_proxy::Paired {
            device_id: 12,
            name: "Laptop".into(),
            manages,
            token: coversync_proxy::Token::new("cst_example").unwrap(),
        };
        // A token that manages: the app finds the server paired.
        note_for_app(&dir, url, &paired(true)).unwrap();
        let app = Store::open(dir.clone()).get();
        assert_eq!(
            app.paired.get(url).map(|d| (d.id, d.name.as_str())),
            Some((12, "Laptop"))
        );
        // Paired again as sync-only: the app mustn't sign in with that
        // one, and nothing else of its settings goes.
        Store::open(dir.clone())
            .update(|app| app.server = Some(url.to_owned()))
            .unwrap();
        note_for_app(&dir, url, &paired(false)).unwrap();
        let app = Store::open(dir.clone()).get();
        assert!(app.paired.is_empty());
        assert_eq!(app.server.as_deref(), Some(url));
    }
}
