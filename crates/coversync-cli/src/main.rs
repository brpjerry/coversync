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
    let server = server.or(config.server).ok_or_else(|| {
        setup_error(
            "No server is set. Turn sync on in the coversync app's settings, or run \
             `coversync setup --server <url> --root <folder>`."
                .into(),
        )
    })?;
    let server = config::server_url(&server).map_err(setup_error)?;
    let root = root.or(config.root).ok_or_else(|| {
        setup_error(
            "No folder is set. Choose one in the coversync app's settings, or run \
             `coversync setup --root <folder>`."
                .into(),
        )
    })?;
    // Not checked for being there: a folder on a disk that isn't mounted
    // yet is waited for.
    let root = std::path::absolute(&root)
        .map_err(|e| setup_error(format!("{} isn't a usable path: {e}.", root.display())))?;
    let tokens = token_file
        .or(config.token_file)
        .map_or(Source::Keychain, Source::File);
    tracing::info!(server, root = %root.display(), "the sync agent is starting");

    let status = Arc::new(Status(Notifier::from_env()));
    let shutdown = CancellationToken::new();
    let mut signals = [SignalKind::terminate(), SignalKind::interrupt()]
        .map(|kind| signal(kind).map_err(|e| setup_error(format!("Couldn't start: {e}."))));
    let [Ok(terminate), Ok(interrupt)] = &mut signals else {
        let [terminate, interrupt] = signals;
        return Err(terminate.and(interrupt).unwrap_err());
    };
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

fn setup(
    file: &Path,
    server: Option<String>,
    root: Option<PathBuf>,
    token_file: Option<PathBuf>,
    keychain: bool,
) -> Done {
    let mut config = Config::load(file).map_err(setup_error)?;
    let changing = server.is_some() || root.is_some() || token_file.is_some() || keychain;
    if let Some(server) = server {
        config.server = Some(config::server_url(&server).map_err(setup_error)?);
    }
    if let Some(root) = root {
        config.root = Some(config::folder(&root).map_err(setup_error)?);
    }
    if let Some(token_file) = token_file {
        let path = std::path::absolute(&token_file).map_err(|e| {
            setup_error(format!(
                "{} isn't a usable path: {e}.",
                token_file.display()
            ))
        })?;
        config.token_file = Some(path);
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
    let token_file =
        match token_file {
            Some(path) => Some(std::path::absolute(&path).map_err(|e| {
                setup_error(format!("{} isn't a usable path: {e}.", path.display()))
            })?),
            None => config.token_file.clone(),
        };
    let tokens = token_file.clone().map_or(Source::Keychain, Source::File);
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
