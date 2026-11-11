//! One instance of the app at a time: a later launch hands what it was asked
//! (its arguments, its `COVERSYNC_SERVER`, the token that lets the window
//! come to the front) to the first over a socket, and ends.
//!
//! The socket belongs to a settings folder, like [`ConfigLock`]: an app
//! started with other settings (`XDG_CONFIG_HOME`) is an instance of its own.
//!
//! [`ConfigLock`]: coversync_desktop_core::ConfigLock

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::runtime::Handle;

use crate::app::APP_ID;
use crate::paths;

/// How long a launch waits for the first instance to take it, and the first
/// instance for a launch to say what it wants.
const TIMEOUT: Duration = Duration::from_secs(5);
/// More than a launch's arguments ever come to.
const MAX_LINE: u64 = 64 * 1024;

/// What a launch asks of the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Launch {
    /// Its arguments, the program's name first.
    pub args: Vec<String>,
    /// Its `COVERSYNC_SERVER`.
    pub env_server: Option<String>,
    /// Its `XDG_ACTIVATION_TOKEN`: what the desktop gave it so that its
    /// window may take the focus.
    pub activation_token: Option<String>,
}

/// The first instance's end of the socket. The file goes when this does.
pub struct Listening {
    listener: Option<UnixListener>,
    path: PathBuf,
}

pub enum Claim {
    /// No instance was running: this is it.
    First(Listening),
    /// One is: hand the launch over.
    Running(UnixStream),
}

/// The socket of the instance that uses `config_dir`, in the session's
/// runtime folder, which is the user's alone; without one there is no
/// socket, since anywhere else another user could be listening at its name.
/// The name carries a hash of the folder rather than the folder: a socket's
/// path can't be long.
pub fn socket_path(config_dir: &Path) -> Option<PathBuf> {
    Some(socket_in(&paths::runtime_dir()?, config_dir))
}

fn socket_in(runtime_dir: &Path, config_dir: &Path) -> PathBuf {
    // FNV-1a.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in config_dir.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    runtime_dir.join(format!("{APP_ID}-{hash:016x}.sock"))
}

/// Becomes the instance at `path`, or reaches the one that is.
pub fn claim(path: &Path) -> io::Result<Claim> {
    for _ in 0..2 {
        match UnixListener::bind(path) {
            Ok(listener) => {
                // The runtime folder is the user's alone already; this is for
                // one that isn't as it should be.
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                return Ok(Claim::First(Listening {
                    listener: Some(listener),
                    path: path.to_owned(),
                }));
            }
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {}
            Err(err) => return Err(err),
        }
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(Claim::Running(stream)),
            // Left behind by an instance that didn't get to remove it.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                let _ = std::fs::remove_file(path);
            }
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "another launch took the socket meanwhile",
    ))
}

/// Hands `launch` to the running instance, and waits until it has it.
pub fn hand_over(mut stream: UnixStream, launch: &Launch) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut line = serde_json::to_vec(launch)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    if answer.trim() == "ok" {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the running app didn't take the launch",
        ))
    }
}

impl Listening {
    /// Passes each later launch to `on_launch`, on `runtime`.
    pub fn serve(&mut self, runtime: &Handle, on_launch: impl Fn(Launch) + Send + Sync + 'static) {
        let Some(listener) = self.listener.take() else {
            return;
        };
        let on_launch = Arc::new(on_launch);
        runtime.spawn(async move {
            let listener = match listener
                .set_nonblocking(true)
                .and_then(|()| tokio::net::UnixListener::from_std(listener))
            {
                Ok(listener) => listener,
                Err(err) => {
                    tracing::warn!("later launches won't reach this window: {err}");
                    return;
                }
            };
            loop {
                let stream = match listener.accept().await {
                    Ok((stream, _)) => stream,
                    Err(err) => {
                        tracing::warn!("couldn't take a later launch: {err}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        continue;
                    }
                };
                let on_launch = on_launch.clone();
                tokio::spawn(async move {
                    match tokio::time::timeout(TIMEOUT, receive(stream)).await {
                        Ok(Ok((launch, mut stream))) => {
                            on_launch(launch);
                            let _ = stream.write_all(b"ok\n").await;
                        }
                        Ok(Err(err)) => tracing::warn!("couldn't read a later launch: {err}"),
                        Err(_) => tracing::warn!("a later launch didn't say what it wanted"),
                    }
                });
            }
        });
    }
}

async fn receive(stream: tokio::net::UnixStream) -> io::Result<(Launch, tokio::net::UnixStream)> {
    use tokio::io::AsyncReadExt;
    let mut reader = tokio::io::BufReader::new(stream).take(MAX_LINE);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line).await?;
    let launch = serde_json::from_slice(&line)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    Ok((launch, reader.into_inner().into_inner()))
}

impl Drop for Listening {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(server: &str) -> Launch {
        Launch {
            args: vec!["coversync-desktop".into(), format!("--server={server}")],
            env_server: None,
            activation_token: Some("token".into()),
        }
    }

    #[test]
    fn each_settings_folder_has_a_socket_of_its_own_with_a_short_path() {
        let socket = |config: &Path| socket_in(Path::new("/run/user/1000"), config);
        let a = socket(Path::new("/home/a/.config/io.github.brpjerry.coversync"));
        let b = socket(Path::new("/home/b/.config/io.github.brpjerry.coversync"));
        assert_ne!(a, b);
        assert_eq!(
            a,
            socket(Path::new("/home/a/.config/io.github.brpjerry.coversync"))
        );
        assert!(a.starts_with("/run/user/1000"));
        let long = socket(&Path::new("/tmp").join("x".repeat(400)));
        let name = long.file_name().unwrap().to_string_lossy().into_owned();
        // sockaddr_un's path holds 108 bytes; the folder takes some.
        assert!(name.len() < 60, "{name}");
    }

    #[test]
    fn a_later_launch_reaches_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.sock");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        let Claim::First(mut first) = claim(&path).unwrap() else {
            panic!("nothing was running");
        };
        let (sent, received) = std::sync::mpsc::channel();
        first.serve(runtime.handle(), move |launch| {
            let _ = sent.send(launch);
        });

        for server in ["https://a/", "https://b/"] {
            let Claim::Running(stream) = claim(&path).unwrap() else {
                panic!("the first instance is running");
            };
            hand_over(stream, &launch(server)).unwrap();
            assert_eq!(
                received.recv_timeout(Duration::from_secs(5)).unwrap(),
                launch(server)
            );
        }

        drop(first);
        assert!(!path.exists(), "the socket goes with the instance");
    }

    #[test]
    fn a_socket_left_behind_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.sock");
        // Bound, then closed without removing the file: as after a crash.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        assert!(matches!(claim(&path).unwrap(), Claim::First(_)));
    }

    #[test]
    fn a_launch_that_says_nothing_sensible_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.sock");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let Claim::First(mut first) = claim(&path).unwrap() else {
            panic!("nothing was running");
        };
        first.serve(runtime.handle(), |_| panic!("not a launch"));

        let mut stream = UnixStream::connect(&path).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        stream.write_all(b"not json\n").unwrap();
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer).unwrap();
        assert_eq!(answer, "", "closed without an ok");
    }
}
