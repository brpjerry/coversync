//! Change notices, pushed to clients as server-sent events from
//! `GET /api/events`. Each client hears about scans, imports, tag edits,
//! saved setups and finished lookups as they happen, whoever made them,
//! instead of polling for them.
//!
//! Every event is named for what happened and carries a JSON object. A
//! client that falls too far behind is sent `resync` (reload everything)
//! in place of the notices it missed. A quiet stream sends `ping` every
//! [`PING`], which keeps proxies from closing it and tells clients it's
//! still alive.

use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderName};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use coversync_core::{Collection, ScanProgress};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, watch};

use crate::Shared;

/// How many notices a client can fall behind before it's told to resync.
const BACKLOG: usize = 256;
/// How long a stream stays quiet before it sends a ping.
const PING: Duration = Duration::from_secs(20);
/// How often a running scan reports its progress.
const SCAN_PROGRESS: Duration = Duration::from_secs(1);

/// Which setup was saved.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Setup {
    Library,
    /// Also sent when the Inbox's hidden files are shown again.
    Inbox,
    Appearance,
    Sync,
}

/// Something that changed. Each is sent as an event named by [`Notice::name`],
/// with its fields as JSON.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum Notice {
    /// A scan's progress, as `/api/status` reports it, while the scan runs.
    Scan(ScanProgress),
    /// A scan ended, and the Inbox and Library may have changed.
    ScanFinished(ScanProgress),
    /// An Inbox album was imported into the Library.
    Imported {
        /// The album's id in the Library.
        album_id: String,
        /// The Inbox album it came from.
        inbox_id: String,
    },
    /// An album's tags were edited.
    AlbumChanged {
        /// The album's id after the edit.
        id: String,
        /// Its id before, when the edit changed it (a new album title does).
        previous_id: Option<String>,
        collection: Collection,
    },
    /// A setup screen's settings were saved.
    SetupSaved { setup: Setup },
    /// A Smart suggestions lookup ended.
    LookupFinished {
        album_id: String,
        /// "done" or "failed", as `/api/identify/{album_id}` reports it.
        state: &'static str,
    },
}

impl Notice {
    /// The event's name, which clients listen for.
    fn name(&self) -> &'static str {
        match self {
            Notice::Scan(_) => "scan",
            Notice::ScanFinished(_) => "scan_finished",
            Notice::Imported { .. } => "imported",
            Notice::AlbumChanged { .. } => "album_changed",
            Notice::SetupSaved { .. } => "setup_saved",
            Notice::LookupFinished { .. } => "lookup_finished",
        }
    }

    fn event(&self) -> Event {
        let data = serde_json::to_string(self).expect("notices serialize");
        Event::default().event(self.name()).data(data)
    }
}

/// Where notices are published, and the clients listening for them.
pub struct Events {
    notices: broadcast::Sender<Notice>,
    /// Set when the daemon is shutting down, which ends every stream.
    closed: watch::Sender<bool>,
}

impl Default for Events {
    fn default() -> Self {
        Events {
            notices: broadcast::channel(BACKLOG).0,
            closed: watch::Sender::new(false),
        }
    }
}

impl Events {
    /// Tell every client listening. Nobody listening is fine.
    pub fn publish(&self, notice: Notice) {
        let _ = self.notices.send(notice);
    }

    /// End every stream, and any opened from now on, so a graceful shutdown
    /// doesn't wait on clients that never hang up.
    pub fn close(&self) {
        self.closed.send_replace(true);
    }

    /// Notices from now on, as published.
    #[cfg(test)]
    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.notices.subscribe()
    }

    /// Notices from now on, ending when [`Events::close`] is called.
    fn listen(&self) -> impl Stream<Item = Event> + use<> {
        let state = (self.notices.subscribe(), self.closed.subscribe());
        stream::unfold(state, |(mut notices, mut closed)| async move {
            let event = tokio::select! {
                received = notices.recv() => match received {
                    Ok(notice) => notice.event(),
                    // The notices missed are gone: the client reloads instead.
                    Err(RecvError::Lagged(missed)) => {
                        tracing::debug!(missed, "event stream fell behind");
                        Event::default().event("resync").data("{}")
                    }
                    Err(RecvError::Closed) => return None,
                },
                _ = closed.wait_for(|closed| *closed) => return None,
            };
            Some((event, (notices, closed)))
        })
    }
}

/// `GET /api/events`: notices as server-sent events, for as long as the
/// client stays and its login lasts.
pub async fn stream(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let events = &state.events;
    respond(events, signed_out(state.clone(), headers))
}

/// Resolves once the login a stream was opened with has ended, as checked
/// every [`PING`]: the stream then ends too, rather than going on telling
/// a browser that has signed out.
async fn signed_out(state: Shared, headers: HeaderMap) {
    loop {
        tokio::time::sleep(PING).await;
        if !crate::auth::still_valid(&state, &headers).await {
            return;
        }
    }
}

/// The event stream for a client listening from now on, until `ended`.
fn respond(events: &Events, ended: impl Future<Output = ()> + Send + 'static) -> Response {
    // A comment goes out first: Firefox only reports a stream open once
    // some of its body has arrived, and the first notice or ping could be
    // 20 s away.
    let hello = stream::iter([Event::default().comment("coversyncd")]);
    let events = hello
        .chain(events.listen())
        .take_until(ended)
        .map(Ok::<_, Infallible>);
    let ping = Event::default().event("ping").data("{}");
    (
        // Caddy passes event streams on as they come; nginx needs telling.
        [(HeaderName::from_static("x-accel-buffering"), "no")],
        Sse::new(events).keep_alive(KeepAlive::new().interval(PING).event(ping)),
    )
        .into_response()
}

/// Publish a running scan's progress whenever it has moved on, about once a
/// second, until the task is aborted.
pub async fn report_scan(state: Shared) {
    let mut tick = tokio::time::interval(SCAN_PROGRESS);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last: Option<ScanProgress> = None;
    loop {
        tick.tick().await;
        let progress = state
            .progress
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // Until the scan has started, the progress is the last scan's.
        if progress.running && last.as_ref() != Some(&progress) {
            state.events.publish(Notice::Scan(progress.clone()));
            last = Some(progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::BodyDataStream;

    fn setup_saved(setup: Setup) -> Notice {
        Notice::SetupSaved { setup }
    }

    /// What a client listening from now on is sent, as it goes over the
    /// wire, after the comment every stream starts with.
    async fn listen(events: &Events) -> BodyDataStream {
        let mut sent = respond(events, std::future::pending())
            .into_body()
            .into_data_stream();
        assert_eq!(next(&mut sent).await.unwrap(), ": coversyncd\n\n");
        sent
    }

    async fn next(sent: &mut BodyDataStream) -> Option<String> {
        let event = tokio::time::timeout(Duration::from_secs(5), sent.next())
            .await
            .expect("an event or the end")?;
        Some(String::from_utf8(event.unwrap().to_vec()).unwrap())
    }

    #[test]
    fn notices_are_named_events_with_json_fields() {
        let changed = Notice::AlbumChanged {
            id: "b".into(),
            previous_id: Some("a".into()),
            collection: Collection::Library,
        };
        assert_eq!(changed.name(), "album_changed");
        assert_eq!(
            serde_json::to_value(&changed).unwrap(),
            serde_json::json!({ "id": "b", "previous_id": "a", "collection": "library" })
        );
        assert_eq!(
            serde_json::to_value(setup_saved(Setup::Inbox)).unwrap(),
            serde_json::json!({ "setup": "inbox" })
        );
        let scan = serde_json::to_value(Notice::ScanFinished(ScanProgress::default())).unwrap();
        assert_eq!(scan["running"], false);
    }

    #[tokio::test]
    async fn listeners_hear_notices_published_after_they_start() {
        let events = Events::default();
        events.publish(setup_saved(Setup::Library));
        let mut sent = listen(&events).await;
        events.publish(setup_saved(Setup::Sync));
        assert_eq!(
            next(&mut sent).await.unwrap(),
            "event: setup_saved\ndata: {\"setup\":\"sync\"}\n\n"
        );
    }

    #[tokio::test]
    async fn a_listener_that_falls_behind_is_told_to_resync() {
        let events = Events::default();
        let mut sent = listen(&events).await;
        for _ in 0..BACKLOG + 10 {
            events.publish(setup_saved(Setup::Appearance));
        }
        let first = next(&mut sent).await.unwrap();
        assert_eq!(first, "event: resync\ndata: {}\n\n");
        // Then it carries on with the notices still kept.
        let second = next(&mut sent).await.unwrap();
        assert!(second.starts_with("event: setup_saved\n"), "{second}");
    }

    #[tokio::test]
    async fn closing_ends_every_stream() {
        let events = Events::default();
        let mut open = listen(&events).await;
        events.close();
        assert_eq!(next(&mut open).await, None);
        let mut opened_after = listen(&events).await;
        assert_eq!(next(&mut opened_after).await, None);
    }

    #[tokio::test]
    async fn a_stream_ends_when_its_login_does() {
        let events = Events::default();
        let (signed_out, ended) = tokio::sync::oneshot::channel::<()>();
        let mut sent = respond(&events, async {
            let _ = ended.await;
        })
        .into_body()
        .into_data_stream();
        assert_eq!(next(&mut sent).await.unwrap(), ": coversyncd\n\n");
        events.publish(setup_saved(Setup::Sync));
        assert!(
            next(&mut sent)
                .await
                .unwrap()
                .starts_with("event: setup_saved\n")
        );
        signed_out.send(()).unwrap();
        assert_eq!(next(&mut sent).await, None);
    }
}
