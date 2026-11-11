//! Where the app keeps its tokens: one for each server it has paired with,
//! in the desktop's keychain (the Secret Service: GNOME Keyring, KeePassXC,
//! KWallet), under the server's address. Never in a file of the app's own:
//! without a keychain the app doesn't pair, and the server's password login
//! works in the window as it does in a browser.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use coversync_proxy::Token;

/// What a keychain call comes to: its answer, or why it has none, in words
/// for people.
pub type Answer<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send>>;

/// A place for the servers' tokens, by server URL
/// ([`Upstream::url`](coversync_proxy::Upstream::url)).
pub trait Tokens: Send + Sync {
    /// Whether tokens can be kept at all: asked before a pairing code is
    /// spent, so it isn't spent for a token there's nowhere to put. May ask
    /// the user to unlock the keychain.
    fn ready(&self) -> Answer<()>;
    /// The token kept for `server`, if any.
    fn load(&self, server: &str) -> Answer<Option<Token>>;
    /// Keeps `token` for `server`, in place of any it had.
    fn save(&self, server: &str, token: &Token) -> Answer<()>;
    /// Removes `server`'s token. Having none to remove is fine.
    fn forget(&self, server: &str) -> Answer<()>;
}

/// The desktop's keychain, through the Secret Service (or, in a sandbox,
/// the portal's keyring file).
pub struct Keychain {
    /// The app's ID, kept with each token so the keychain's own tools show
    /// whose it is.
    app_id: String,
}

impl Keychain {
    pub fn new(app_id: &str) -> Self {
        Self {
            app_id: app_id.to_owned(),
        }
    }

    /// What a token is filed under.
    fn attributes(&self, server: &str) -> [(&'static str, String); 3] {
        [
            ("application", self.app_id.clone()),
            ("type", "device-token".to_owned()),
            ("server", server.to_owned()),
        ]
    }
}

/// The keychain, unlocked.
async fn open() -> Result<oo7::Keyring, String> {
    let keyring = oo7::Keyring::new().await.map_err(explain)?;
    keyring.unlock().await.map_err(explain)?;
    Ok(keyring)
}

fn explain(err: oo7::Error) -> String {
    use oo7::dbus::{Error as Bus, ServiceError};
    use oo7::zbus::{Error as Wire, fdo};

    // No one owns the service's name and nothing starts it, or there is no
    // session bus to ask.
    let absent = |wire: &Wire| match wire {
        Wire::FDO(fdo) => matches!(
            **fdo,
            fdo::Error::ServiceUnknown(_)
                | fdo::Error::NameHasNoOwner(_)
                | fdo::Error::SpawnFailed(_)
        ),
        Wire::Address(_) | Wire::InputOutput(_) => true,
        _ => false,
    };
    match &err {
        oo7::Error::DBus(Bus::Dismissed) => "The keychain wasn't unlocked.".to_owned(),
        oo7::Error::DBus(Bus::ZBus(wire) | Bus::Service(ServiceError::ZBus(wire)))
            if absent(wire) =>
        {
            "This desktop has no keychain to keep the token in. GNOME Keyring, KeePassXC and \
             KWallet each provide one (the Secret Service)."
                .to_owned()
        }
        _ => format!("The keychain didn't work: {err}."),
    }
}

impl Tokens for Keychain {
    fn ready(&self) -> Answer<()> {
        Box::pin(async { open().await.map(|_| ()) })
    }

    fn load(&self, server: &str) -> Answer<Option<Token>> {
        let attributes = self.attributes(server);
        Box::pin(async move {
            let keyring = open().await?;
            let items = keyring.search_items(&attributes).await.map_err(explain)?;
            let Some(item) = items.first() else {
                return Ok(None);
            };
            let secret = item.secret().await.map_err(explain)?;
            Ok(std::str::from_utf8(secret.as_bytes())
                .ok()
                .and_then(Token::new))
        })
    }

    fn save(&self, server: &str, token: &Token) -> Answer<()> {
        let attributes = self.attributes(server);
        let label = format!("coversync: this computer's token for {server}");
        let secret = oo7::Secret::text(token.expose());
        Box::pin(async move {
            let keyring = open().await?;
            keyring
                .create_item(&label, &attributes, secret, true)
                .await
                .map_err(explain)
        })
    }

    fn forget(&self, server: &str) -> Answer<()> {
        let attributes = self.attributes(server);
        Box::pin(async move {
            let keyring = open().await?;
            keyring.delete(&attributes).await.map_err(explain)
        })
    }
}

/// Tokens kept in memory, for tests: gone with the process.
#[derive(Default)]
pub struct Memory {
    tokens: Mutex<HashMap<String, Token>>,
    /// Why nothing can be kept, when a test wants a desktop without a
    /// keychain.
    broken: Mutex<Option<String>>,
    /// Why nothing new can be kept, when a test wants a keychain that opens
    /// and then refuses.
    full: Mutex<Option<String>>,
}

impl Memory {
    /// Makes every call fail with `why` from now on, or work again.
    pub fn fail_with(&self, why: Option<&str>) {
        *self.broken.lock().expect("tokens lock") = why.map(str::to_owned);
    }

    /// Makes saving fail with `why` from now on, while the rest works.
    pub fn refuse_saves_with(&self, why: Option<&str>) {
        *self.full.lock().expect("tokens lock") = why.map(str::to_owned);
    }

    /// The servers that have a token.
    pub fn servers(&self) -> Vec<String> {
        let mut servers: Vec<String> = self
            .tokens
            .lock()
            .expect("tokens lock")
            .keys()
            .cloned()
            .collect();
        servers.sort();
        servers
    }

    fn answer<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut HashMap<String, Token>) -> T,
    ) -> Answer<T> {
        let broken = self.broken.lock().expect("tokens lock").clone();
        let answer = match broken {
            Some(why) => Err(why),
            None => Ok(work(&mut self.tokens.lock().expect("tokens lock"))),
        };
        Box::pin(async move { answer })
    }
}

impl Tokens for Memory {
    fn ready(&self) -> Answer<()> {
        self.answer(|_| ())
    }

    fn load(&self, server: &str) -> Answer<Option<Token>> {
        self.answer(|tokens| tokens.get(server).cloned())
    }

    fn save(&self, server: &str, token: &Token) -> Answer<()> {
        if let Some(why) = self.full.lock().expect("tokens lock").clone() {
            return Box::pin(async move { Err(why) });
        }
        self.answer(|tokens| {
            tokens.insert(server.to_owned(), token.clone());
        })
    }

    fn forget(&self, server: &str) -> Answer<()> {
        self.answer(|tokens| {
            tokens.remove(server);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_a_token_under_its_server_and_the_app() {
        let keychain = Keychain::new("io.github.brpjerry.coversync");
        let attributes = keychain.attributes("https://music.example/coversync/");
        assert_eq!(
            attributes,
            [
                ("application", "io.github.brpjerry.coversync".to_owned()),
                ("type", "device-token".to_owned()),
                ("server", "https://music.example/coversync/".to_owned()),
            ]
        );
    }
}
