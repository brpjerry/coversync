//! What the agent learns from the desktop, over the system bus: whether the
//! network is metered (NetworkManager), whether the computer runs on its
//! battery (UPower), when it wakes from sleep (logind) and when the network
//! comes back or changes (NetworkManager).
//!
//! A computer without one of these services is taken to be on an unmetered
//! network and on mains power: a server or a desktop with systemd-networkd
//! has nothing to tell, and nothing to save. Until a service has answered,
//! or shown that it isn't there, what it would say isn't known, and the
//! agent takes nothing for granted.

use std::future::pending;

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use zbus::{Connection, OwnedObjectPath, Proxy};

/// What auto sync may depend on (Sync setup's "unmetered networks only"
/// and "while charging"). None: the desktop hasn't said yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Conditions {
    pub metered: Option<bool>,
    pub on_battery: Option<bool>,
}

impl Conditions {
    /// Nothing heard yet.
    pub const UNKNOWN: Self = Self {
        metered: None,
        on_battery: None,
    };
    /// An unmetered network and mains power: nothing in the way.
    pub const FINE: Self = Self::known(false, false);

    pub const fn known(metered: bool, on_battery: bool) -> Self {
        Self {
            metered: Some(metered),
            on_battery: Some(on_battery),
        }
    }
}

/// How long a service gets to say how things are before the agent goes on
/// as if it weren't there.
const ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
/// How often a service that wasn't there, or went away, is looked for
/// again.
const LISTEN_AGAIN: std::time::Duration = std::time::Duration::from_secs(60);

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
    /// It starts out as `conditions`.
    pub fn manual(conditions: Conditions) -> (System, Feed) {
        let (conditions_tx, conditions) = watch::channel(conditions);
        let (nudges_tx, nudges) = mpsc::unbounded_channel();
        (
            System { conditions, nudges },
            Feed {
                conditions: conditions_tx,
                nudges: nudges_tx,
            },
        )
    }

    /// The desktop's services, listened to from here on. Nothing is known
    /// until each has answered or turned out not to be there.
    pub fn desktop() -> System {
        let (system, feed) = System::manual(Conditions::UNKNOWN);
        tokio::spawn(async move {
            let Feed { conditions, nudges } = feed;
            // A service that neither answers nor fails is given up on.
            let give_up = async {
                tokio::time::sleep(ANSWER_WAIT).await;
                if conditions.send_if_modified(|c| settle(c, true, true)) {
                    tracing::info!(
                        "the desktop didn't say in time: taking what isn't known as an unmetered \
                         network and mains power"
                    );
                }
            };
            let listen = async {
                match Connection::system().await {
                    Ok(bus) => {
                        // Each for itself: one that isn't there mustn't
                        // keep the other's answer waiting. And each asked
                        // again now and then: a service that starts after
                        // the agent has things to say from then on, and one
                        // that goes away takes what it said with it.
                        let network = async {
                            let mut said = false;
                            loop {
                                let ended = network(&bus, &conditions, &nudges).await;
                                if !std::mem::replace(&mut said, true) {
                                    let why =
                                        ended.err().map_or(String::new(), |e| format!(" ({e})"));
                                    tracing::info!(
                                        "not listening to NetworkManager{why}: every network \
                                         counts as unmetered until it's there"
                                    );
                                }
                                // What it said last is no longer true to anything.
                                conditions
                                    .send_if_modified(|c| c.metered.replace(false) != Some(false));
                                tokio::time::sleep(LISTEN_AGAIN).await;
                            }
                        };
                        let power = async {
                            let mut said = false;
                            loop {
                                let ended = power(&bus, &conditions).await;
                                if !std::mem::replace(&mut said, true) {
                                    let why =
                                        ended.err().map_or(String::new(), |e| format!(" ({e})"));
                                    tracing::info!(
                                        "not listening to UPower{why}: the power counts as mains \
                                         until it's there"
                                    );
                                }
                                conditions.send_if_modified(|c| {
                                    c.on_battery.replace(false) != Some(false)
                                });
                                tokio::time::sleep(LISTEN_AGAIN).await;
                            }
                        };
                        let sleep = async {
                            if let Err(e) = sleep(&bus, &nudges).await {
                                tracing::info!("not listening to logind: {e}");
                            }
                        };
                        tokio::join!(network, power, sleep);
                    }
                    Err(e) => {
                        tracing::info!(
                            "no system bus ({e}): taking the network as unmetered and the power \
                             as mains"
                        );
                        conditions.send_if_modified(|c| settle(c, true, true));
                    }
                }
            };
            tokio::join!(listen, give_up);
            // The agent goes on without them, on what was last heard.
            let _keep = (conditions, nudges);
            pending::<()>().await
        });
        system
    }
}

/// Takes what isn't known of the network, the power or both as nothing in
/// the way. Whether that changed anything.
fn settle(conditions: &mut Conditions, network: bool, power: bool) -> bool {
    let mut changed = false;
    if network && conditions.metered.is_none() {
        conditions.metered = Some(false);
        changed = true;
    }
    if power && conditions.on_battery.is_none() {
        conditions.on_battery = Some(false);
        changed = true;
    }
    changed
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
    let nm = within(Proxy::new(
        bus,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    ))
    .await?;
    let mut owner = within(nm.receive_owner_changed()).await?;
    let mut metered = nm.receive_property_changed::<u32>("Metered").await;
    let mut state = nm.receive_property_changed::<u32>("State").await;
    let mut primary = nm
        .receive_property_changed::<OwnedObjectPath>("PrimaryConnection")
        .await;
    // Asked once as well: the streams only tell of what the service says
    // from here on when it has said nothing yet.
    let now = Some(is_metered(within(nm.get_property::<u32>("Metered")).await?));
    conditions.send_if_modified(|c| std::mem::replace(&mut c.metered, now) != now);
    let mut link = Link::default();
    loop {
        tokio::select! {
            Some(change) = metered.next() => {
                if let Ok(value) = change.get().await {
                    let now = Some(is_metered(value));
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
            // The service is gone (or another took its name): what it
            // said doesn't hold any more, and it's looked for afresh.
            Some(_) = owner.next() => return Ok(()),
            else => return Ok(()),
        }
    }
}

async fn power(bus: &Connection, conditions: &watch::Sender<Conditions>) -> zbus::Result<()> {
    let upower = within(Proxy::new(
        bus,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "org.freedesktop.UPower",
    ))
    .await?;
    let mut owner = within(upower.receive_owner_changed()).await?;
    let mut on_battery = upower.receive_property_changed::<bool>("OnBattery").await;
    let now = Some(within(upower.get_property::<bool>("OnBattery")).await?);
    conditions.send_if_modified(|c| std::mem::replace(&mut c.on_battery, now) != now);
    loop {
        tokio::select! {
            Some(change) = on_battery.next() => {
                if let Ok(now) = change.get().await {
                    let now = Some(now);
                    conditions.send_if_modified(|c| std::mem::replace(&mut c.on_battery, now) != now);
                }
            }
            Some(_) = owner.next() => return Ok(()),
            else => return Ok(()),
        }
    }
}

/// A request to a service, given up on after [`ANSWER_WAIT`]: one that
/// takes the question and never answers would otherwise keep its listener
/// from ever trying again.
async fn within<T>(asked: impl Future<Output = zbus::Result<T>>) -> zbus::Result<T> {
    tokio::time::timeout(ANSWER_WAIT, asked)
        .await
        .unwrap_or_else(|_| Err(zbus::Error::Failure("no answer in time".into())))
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
    fn a_service_that_isnt_there_settles_only_what_it_would_have_said() {
        let mut conditions = Conditions::UNKNOWN;
        // No NetworkManager: the network is settled, the power still open.
        assert!(settle(&mut conditions, true, false));
        assert_eq!(conditions.metered, Some(false));
        assert_eq!(conditions.on_battery, None);
        // UPower answered meanwhile: giving up later leaves that alone.
        conditions.on_battery = Some(true);
        assert!(!settle(&mut conditions, true, true));
        assert_eq!(conditions, Conditions::known(false, true));
        // Nothing answered in time: both settled.
        let mut conditions = Conditions::UNKNOWN;
        assert!(settle(&mut conditions, true, true));
        assert_eq!(conditions, Conditions::FINE);
    }

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
