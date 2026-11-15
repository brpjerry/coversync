//! Where the agent finds this computer's token for a server: in the
//! desktop's keychain, where the app put it when it paired (so the app and
//! the agent are one device), or in a file only the user can read, for a
//! computer with no keychain.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use coversync_desktop_core::tokens::{Keychain, Tokens};
use coversync_proxy::Token;

use crate::APP_ID;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Keychain,
    File(PathBuf),
}

impl Source {
    /// Whether a token can be kept: asked before a pairing code is spent.
    pub async fn ready(&self) -> Result<(), String> {
        match self {
            Source::Keychain => Keychain::new(APP_ID).ready().await,
            Source::File(file) => writable(file),
        }
    }

    /// The token for `server` ([`crate::config::server_url`]'s form), if
    /// there is one. A file holds one token, whichever server it's for.
    pub async fn load(&self, server: &str) -> Result<Option<Token>, String> {
        match self {
            Source::Keychain => Keychain::new(APP_ID).load(server).await,
            Source::File(file) => {
                let text = match fs::read_to_string(file) {
                    Ok(text) => text,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(format!("{} can't be read: {e}.", file.display())),
                };
                if let Ok(meta) = fs::metadata(file)
                    && meta.permissions().mode() & 0o077 != 0
                {
                    tracing::warn!(
                        "{} holds this computer's token and others can read it: chmod 600 it",
                        file.display()
                    );
                }
                Ok(Token::new(text.trim()))
            }
        }
    }

    /// Keeps `token` for `server`, in place of any it had.
    pub async fn save(&self, server: &str, token: &Token) -> Result<(), String> {
        match self {
            Source::Keychain => Keychain::new(APP_ID).save(server, token).await,
            Source::File(file) => {
                let failed = |e: io::Error| format!("{} can't be written: {e}.", file.display());
                if let Some(dir) = file.parent() {
                    fs::create_dir_all(dir).map_err(failed)?;
                }
                // Made for the user alone, then put in place whole.
                let mut name = file.file_name().unwrap_or_default().to_owned();
                name.push(".new");
                let tmp = file.with_file_name(name);
                let _ = fs::remove_file(&tmp);
                let mut out = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp)
                    .map_err(failed)?;
                out.write_all(token.expose().as_bytes()).map_err(failed)?;
                out.write_all(b"\n").map_err(failed)?;
                out.sync_all().map_err(failed)?;
                fs::rename(&tmp, file).map_err(failed)
            }
        }
    }
}

/// Whether a token could be written to `file`: its folder can be made and
/// written in, and nothing but a file is in its place. Tried with a file
/// of another name, so a token already there stays as it is.
fn writable(file: &std::path::Path) -> Result<(), String> {
    let cant = |why: String| format!("A token can't be kept in {}: {why}.", file.display());
    if file.is_dir() {
        return Err(cant("it's a folder".into()));
    }
    let dir = file
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    fs::create_dir_all(dir).map_err(|e| cant(e.to_string()))?;
    let mut name = file.file_name().unwrap_or_default().to_owned();
    name.push(format!(".{}.try", std::process::id()));
    let probe = file.with_file_name(name);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
        .map_err(|e| cant(e.to_string()))?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[tokio::test]
    async fn a_token_file_that_couldnt_be_written_is_known_before_pairing() {
        let dir = std::env::temp_dir().join(format!("coversync-cli-ready-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Somewhere it can go, with the folder still to make: ready, and
        // nothing left behind but the folder.
        let fine = Source::File(dir.join("new/token"));
        assert_eq!(fine.ready().await, Ok(()));
        assert_eq!(fs::read_dir(dir.join("new")).unwrap().count(), 0);
        // A token already there is left alone.
        let token = Token::new("cst_kept").unwrap();
        fine.save("https://a/", &token).await.unwrap();
        assert_eq!(fine.ready().await, Ok(()));
        assert_eq!(fine.load("https://a/").await.unwrap(), Some(token));
        // A folder where the file would be, a file where its folder would
        // be, and a folder that can't be written in.
        assert!(Source::File(dir.join("new")).ready().await.is_err());
        let under_a_file = Source::File(dir.join("new/token/deeper"));
        assert!(under_a_file.ready().await.is_err());
        let locked = dir.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
        let refused = Source::File(locked.join("token")).ready().await;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(refused.is_err());
    }

    // Bugs caught: 0, False positives: 0
    #[tokio::test]
    async fn a_token_file_is_the_users_alone_and_read_back() {
        let dir = std::env::temp_dir().join(format!("coversync-cli-tokens-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("sub/token");
        let source = Source::File(file.clone());
        assert_eq!(source.load("https://a/").await.unwrap(), None);
        let token = Token::new("cst_example").unwrap();
        source.save("https://a/", &token).await.unwrap();
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(source.load("https://a/").await.unwrap(), Some(token));
        // A second save replaces it, and leaves nothing beside it.
        let other = Token::new("cst_other").unwrap();
        source.save("https://a/", &other).await.unwrap();
        assert_eq!(source.load("https://a/").await.unwrap(), Some(other));
        assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
        // An empty file is no token.
        fs::write(&file, b"\n").unwrap();
        assert_eq!(source.load("https://a/").await.unwrap(), None);
    }
}
