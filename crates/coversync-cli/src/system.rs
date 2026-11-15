//! What the agent learns from the desktop, over the system bus: whether the
//! network is metered (NetworkManager), whether the computer runs on its
//! battery (UPower), when it wakes from sleep (logind) and when the network
//! comes back or changes (NetworkManager).
//!
//! A computer without one of these services is taken to be on an unmetered
//! network and on mains power: a server or a desktop with systemd-networkd
//! has nothing to tell, and nothing to save.

use std::future::pending;

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use zbus::{Connection, OwnedObjectPath, Proxy};

/// What auto sync may depend on (Sync setup's "unmetered networks only"
/// and "while charging").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Conditions {
    pub metered: bool,
    pub on_battery: bool,
}

/// Something happened that the server's event stream may have missed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nudge {
    /// The computer woke from sleep.
    Woke,
    /// The network came back, or is another one now.
    NetworkBack,
}

/// The desktop, as the agent hears it.
pub struct System {
    pub conditions: watch::Receiver<Conditions>,
    pub nudges: mpsc::UnboundedReceiver<Nudge>,
}

/// The other ends of a [`System`], for whoever tells it things.
pub struct Feed {
    pub conditions: watch::Sender<Conditions>,
    pub nudges: mpsc::UnboundedSender<Nudge>,
}

impl System {
    /// A system told by hand: by tests, and by [`System::desktop`]'s tasks.
    pub fn manual() -> (System, Feed) {
        let (conditions_tx, conditions) = watch::channel(Conditions::default());
        let (nudges_tx, nudges) = mpsc::unbounded_channel();
        (
            System { conditions, nudges },
            Feed {
                conditions: conditions_tx,
                nudges: nudges_tx,
            },
        )
    }

    /// The desktop's services, listened to from here on.
    pub fn desktop() -> System {
        let (system, feed) = System::manual();
        tokio::spawn(async move {
            let Feed { conditions, nudges } = feed;
            match Connection::system().await {
                Ok(bus) => {
                    let heard = tokio::join!(
                        network(&bus, &conditions, &nudges),
                        power(&bus, &conditions),
                        sleep(&bus, &nudges),
                    );
                    for (what, result) in [
                        ("NetworkManager", heard.0),
                        ("UPower", heard.1),
                        ("logind", heard.2),
                    ] {
                        if let Err(e) = result {
                            tracing::info!("not listening to {what}: {e}");
                        }
                    }
                }
                Err(e) => tracing::info!(
                    "no system bus ({e}): taking the network as unmetered and the power as mains"
                ),
            }
            // The agent goes on without them, on what was last heard.
            let _keep = (conditions, nudges);
            pending::<()>().await
        });
        system
    }
}

/// NetworkManager's `Metered`: yes, or guessed to be.
fn is_metered(value: u32) -> bool {
    matches!(value, 1 | 3)
}

/// NetworkManager's `State`: connected, if only to the local network (the
/// server may well be on it).
fn is_connected(state: u32) -> bool {
    state >= 50
}

/// Tells a network that came back, or changed, from one that's the same as
/// before.
#[derive(Default)]
struct Link {
    connected: Option<bool>,
    primary: Option<String>,
}

impl Link {
    /// NetworkManager's state is now `state`: whether that's the network
    /// coming back.
    fn state(&mut self, state: u32) -> bool {
        let now = is_connected(state);
        let back = now && self.connected == Some(false);
        self.connected = Some(now);
        back
    }

    /// The connection the computer mainly uses is now `path` (`/` for
    /// none): whether that's another network than before.
    fn primary(&mut self, path: &str) -> bool {
        let other = path != "/" && self.primary.as_deref().is_some_and(|old| old != path);
        if path != "/" {
            self.primary = Some(path.to_owned());
        }
        other
    }
}

async fn network(
    bus: &Connection,
    conditions: &watch::Sender<Conditions>,
    nudges: &mpsc::UnboundedSender<Nudge>,
) -> zbus::Result<()> {
    let nm = Proxy::new(
        bus,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await?;
    let mut metered = nm.receive_property_changed::<u32>("Metered").await;
    let mut state = nm.receive_property_changed::<u32>("State").await;
    let mut primary = nm
        .receive_property_changed::<OwnedObjectPath>("PrimaryConnection")
        .await;
    // Asked once as well: the streams only tell of what the service says
    // from here on when it has said nothing yet.
    let now = nm.get_property::<u32>("Metered").await?;
    conditions.send_if_modified(|c| {
        std::mem::replace(&mut c.metered, is_metered(now)) != is_metered(now)
    });
    let mut link = Link::default();
    loop {
        tokio::select! {
            Some(change) = metered.next() => {
                if let Ok(value) = change.get().await {
                    let now = is_metered(value);
                    conditions.send_if_modified(|c| std::mem::replace(&mut c.metered, now) != now);
                }
            }
            Some(change) = state.next() => {
                if let Ok(value) = change.get().await
                    && link.state(value)
                {
                    let _ = nudges.send(Nudge::NetworkBack);
                }
            }
            Some(change) = primary.next() => {
                if let Ok(path) = change.get().await
                    && link.primary(path.as_str())
                {
                    let _ = nudges.send(Nudge::NetworkBack);
                }
            }
            else => return Ok(()),
        }
    }
}

async fn power(bus: &Connection, conditions: &watch::Sender<Conditions>) -> zbus::Result<()> {
    let upower = Proxy::new(
        bus,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "org.freedesktop.UPower",
    )
    .await?;
    let mut on_battery = upower.receive_property_changed::<bool>("OnBattery").await;
    let now = upower.get_property::<bool>("OnBattery").await?;
    conditions.send_if_modified(|c| std::mem::replace(&mut c.on_battery, now) != now);
    while let Some(change) = on_battery.next().await {
        if let Ok(now) = change.get().await {
            conditions.send_if_modified(|c| std::mem::replace(&mut c.on_battery, now) != now);
        }
    }
    Ok(())
}

async fn sleep(bus: &Connection, nudges: &mpsc::UnboundedSender<Nudge>) -> zbus::Result<()> {
    let logind = Proxy::new(
        bus,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    let mut sleeps = logind.receive_signal("PrepareForSleep").await?;
    while let Some(signal) = sleeps.next().await {
        // True before sleeping, false on waking.
        if signal.body().deserialize::<bool>() == Ok(false) {
            let _ = nudges.send(Nudge::Woke);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn metered_is_yes_or_guessed_yes() {
        // NetworkManager: 0 unknown, 1 yes, 2 no, 3 guessed yes, 4 guessed no.
        assert_eq!(
            [0, 1, 2, 3, 4, 9].map(is_metered),
            [false, true, false, true, false, false]
        );
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_network_is_back_only_after_being_away() {
        let mut link = Link::default();
        // What it is when the agent starts is nothing new.
        assert!(!link.state(70));
        assert!(!link.primary("/org/freedesktop/NetworkManager/ActiveConnection/1"));
        // Still connected, in another way of saying it.
        assert!(!link.state(60));
        // Away (disconnected, connecting), then back, even only locally.
        assert!(!link.state(20));
        assert!(!link.state(40));
        assert!(link.state(50));
        assert!(!link.state(70));
        // No connection is no network; the same one again is nothing new;
        // another one is.
        assert!(!link.primary("/"));
        assert!(!link.primary("/org/freedesktop/NetworkManager/ActiveConnection/1"));
        assert!(link.primary("/org/freedesktop/NetworkManager/ActiveConnection/2"));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_network_found_after_starting_without_one_is_back() {
        let mut link = Link::default();
        assert!(!link.state(20));
        assert!(!link.primary("/"));
        assert!(link.state(70));
        // The first connection named isn't another one.
        assert!(!link.primary("/org/freedesktop/NetworkManager/ActiveConnection/5"));
    }
}
