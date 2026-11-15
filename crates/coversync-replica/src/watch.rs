//! Knowing when to sync: the server's `events` stream and its `status`
//! check. [`Watch`] puts them together for a client that syncs by itself:
//! it waits for the device's quiet period after the last change, answers
//! Sync now at once, and checks the status now and then in case the stream
//! dropped without a word. Whether conditions allow a sync (unmetered
//! network, charging) is the client's to judge, from [`Watch::auto`].

use std::sync::Arc;
use std::time::Duration;

use coversync_protocol::{
    AutoSync, HEARTBEAT_RANGE, HEARTBEAT_SECONDS, PREFIX, Reason, Status, TargetEvent,
};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Method, StatusCode};
use tokio::time::Instant;

use crate::Error;
use crate::http::{Api, IDLE_TIMEOUT, next_chunk, parse, read_all, refusal};

/// The answer to a status check.
#[derive(Debug, Clone)]
pub enum StatusCheck {
    /// Still what the ETag named.
    Unchanged,
    Changed {
        status: Status,
        etag: Option<String>,
    },
}

pub(crate) async fn status(api: &Api, etag: Option<&str>) -> Result<StatusCheck, Error> {
    let mut headers = Vec::new();
    if let Some(v) = etag.and_then(|e| HeaderValue::from_str(e).ok()) {
        headers.push((header::IF_NONE_MATCH, v));
    }
    let res = api
        .send(Method::GET, &format!("{PREFIX}/status"), &headers, None)
        .await?;
    if res.status() == StatusCode::NOT_MODIFIED {
        return Ok(StatusCheck::Unchanged);
    }
    let status = res.status();
    let etag = res
        .headers()
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = read_all(res.into_body(), IDLE_TIMEOUT).await?;
    if !status.is_success() {
        return Err(refusal(status, &body));
    }
    Ok(StatusCheck::Changed {
        status: parse(&body)?,
        etag,
    })
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Keep the event stream open. Without it, only status checks: a
    /// portable device in the background.
    pub listen: bool,
    /// The stream's heartbeat, in seconds (15 to 600): how soon a dead
    /// connection is noticed, against how often the radio wakes.
    pub heartbeat: u32,
    /// Check the status this often too.
    pub status_every: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            listen: true,
            heartbeat: HEARTBEAT_SECONDS,
            status_every: Duration::from_secs(3600),
        }
    }
}

/// Time to sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    pub reason: Reason,
}

pub struct Watch {
    api: Arc<Api>,
    options: WatchOptions,
    auto: Option<AutoSync>,
    /// The target last synced to, or found to need nothing.
    synced: Option<String>,
    /// When the quiet period after the last change ends.
    due: Option<Instant>,
    /// The target that last change was to.
    due_for: Option<String>,
    /// Someone chose Sync now.
    requested: bool,
    stream: Option<Events>,
    reconnect_at: Instant,
    failures: u32,
    next_status: Instant,
    etag: Option<String>,
    /// The first status check is the device waking.
    woke: bool,
    /// The next status check is the one the event stream's opening asked
    /// for: it refreshes the settings and catches Sync now, but leaves
    /// pending work to the stream's first event, which tells of the current
    /// target (and is dropped if a sync under way reaches it).
    refresh: bool,
}

impl Watch {
    pub(crate) fn new(api: Arc<Api>, mut options: WatchOptions) -> Self {
        options.heartbeat = options
            .heartbeat
            .clamp(*HEARTBEAT_RANGE.start(), *HEARTBEAT_RANGE.end());
        let now = Instant::now();
        Self {
            api,
            options,
            auto: None,
            synced: None,
            due: None,
            due_for: None,
            requested: false,
            stream: None,
            reconnect_at: now,
            failures: 0,
            next_status: now,
            etag: None,
            woke: true,
            refresh: false,
        }
    }

    /// The device's auto-sync settings, once heard.
    pub fn auto(&self) -> Option<AutoSync> {
        self.auto
    }

    /// A sync reached `target` (its [`crate::Report::target`]): news of it
    /// is nothing new.
    pub fn synced(&mut self, target: Option<&str>) {
        if let Some(t) = target {
            // A sync reached it, its session having checked the folder.
            self.woke = false;
            // A change heard while that sync ran, which it reached: nothing
            // left to wait for (the stream's first event, say).
            if self.due_for.as_deref() == Some(t) {
                self.due = None;
                self.due_for = None;
            }
            self.synced = Some(t.to_string());
        }
    }

    /// Ask for a status check soon, say when the device joined a network.
    pub fn check_soon(&mut self) {
        // Pending work is looked at afresh, not answered "unchanged".
        self.etag = None;
        self.refresh = false;
        self.next_status = Instant::now();
    }

    fn enabled(&self) -> bool {
        self.auto.is_none_or(|a| a.enabled)
    }

    fn quiet(&self) -> Duration {
        Duration::from_secs(self.auto.unwrap_or_default().quiet_seconds.into())
    }

    /// Wait until it's time to sync. Fails only when the token is refused;
    /// a network that's away is waited out.
    pub async fn next(&mut self) -> Result<Trigger, Error> {
        loop {
            let now = Instant::now();
            if std::mem::take(&mut self.requested) {
                self.due = None;
                return Ok(Trigger {
                    reason: Reason::User,
                });
            }
            if self.due.is_some_and(|due| due <= now) {
                self.due = None;
                if self.enabled() {
                    return Ok(Trigger {
                        reason: Reason::Event,
                    });
                }
            }
            if self.next_status <= now {
                // Still waking until a status has been read: a check cut
                // short (the caller dropped this future) doesn't count.
                let reason = if self.woke {
                    Reason::Wake
                } else {
                    Reason::Timer
                };
                if let Some(trigger) = self.check(reason).await? {
                    return Ok(trigger);
                }
                continue;
            }
            if self.options.listen && self.stream.is_none() && self.reconnect_at <= now {
                match Events::connect(&self.api, self.options.heartbeat).await {
                    Ok(events) => {
                        tracing::debug!("listening for the server's events");
                        self.stream = Some(events);
                        // The stream tells of changes from now on, its first
                        // event of the target now: the status, read again,
                        // has the settings as they are (they may have
                        // changed during a first sync) and a Sync now asked
                        // for meanwhile.
                        self.check_soon();
                        self.refresh = true;
                        self.failures = 0;
                    }
                    Err(Error::Unauthorized) => return Err(Error::Unauthorized),
                    Err(e) => self.dropped(&e),
                }
                continue;
            }
            let mut wake = self.next_status;
            if let Some(due) = self.due {
                wake = wake.min(due);
            }
            if self.options.listen && self.stream.is_none() {
                wake = wake.min(self.reconnect_at);
            }
            let idle = Duration::from_secs(u64::from(self.options.heartbeat) * 2 + 10);
            let event = match self.stream.as_mut() {
                Some(stream) => tokio::select! {
                    e = stream.next(idle) => Some(e),
                    _ = tokio::time::sleep_until(wake) => None,
                },
                None => {
                    tokio::time::sleep_until(wake).await;
                    None
                }
            };
            match event {
                None => {}
                Some(Ok(Some((name, data)))) => self.heard(&name, &data),
                Some(Ok(None)) => {
                    self.stream = None;
                    self.dropped(&Error::Network("the event stream ended".into()));
                }
                Some(Err(e)) => {
                    self.stream = None;
                    if matches!(e, Error::Unauthorized) {
                        return Err(e);
                    }
                    self.dropped(&e);
                }
            }
        }
    }

    fn dropped(&mut self, e: &Error) {
        self.failures += 1;
        let wait = Duration::from_secs((1u64 << self.failures.min(6)).min(60));
        tracing::debug!("event stream: {e}; again in {wait:?}");
        self.reconnect_at = Instant::now() + wait;
    }

    fn heard(&mut self, name: &str, data: &str) {
        match name {
            "target" => {
                let Ok(event) = serde_json::from_str::<TargetEvent>(data) else {
                    return;
                };
                if self.synced.as_deref() == Some(event.target.as_str()) {
                    // Reached already: an older change's wait goes too.
                    self.due = None;
                    self.due_for = None;
                    return;
                }
                // Each change starts the quiet period again: a run of edits
                // becomes one sync.
                self.due = Some(Instant::now() + self.quiet());
                self.due_for = Some(event.target);
            }
            "sync" => self.requested = true,
            "settings" => {
                if let Ok(auto) = serde_json::from_str::<AutoSync>(data) {
                    let was_off = self.auto.is_some_and(|a| !a.enabled);
                    self.auto = Some(auto);
                    if !auto.enabled {
                        self.due = None;
                    } else if was_off {
                        // Turned back on: whatever waited meanwhile is
                        // looked at now, not at the next periodic check.
                        self.check_soon();
                    }
                }
            }
            _ => {}
        }
    }

    async fn check(&mut self, reason: Reason) -> Result<Option<Trigger>, Error> {
        let refresh = self.refresh;
        match status(&self.api, self.etag.as_deref()).await {
            Ok(StatusCheck::Unchanged) => {
                self.woke = false;
                self.refresh = false;
                self.next_status = Instant::now() + self.options.status_every;
                Ok(None)
            }
            Ok(StatusCheck::Changed { status, etag }) => {
                // With auto sync off, the wake's sync is still owed: only a
                // session checks the folder's marker, so it comes when auto
                // sync is turned on (or a sync is reported reached).
                let owed = reason == Reason::Wake && !status.auto.enabled;
                self.woke = owed;
                self.refresh = false;
                self.next_status = Instant::now() + self.options.status_every;
                // Only a status that asked for nothing may come back as
                // "unchanged": one with work pending must be looked at
                // again, say once the client's conditions allow a sync.
                let idle = status.pending.files == 0 && !status.requested && reason != Reason::Wake;
                self.etag = if idle { etag } else { None };
                self.auto = Some(status.auto);
                if status.requested {
                    self.due = None;
                    return Ok(Some(Trigger {
                        reason: Reason::User,
                    }));
                }
                // On waking, a session is worth starting even with nothing
                // pending: only it checks the folder's marker (a new or
                // emptied folder, which the server can't know of).
                let pending = status.pending.files > 0 && !refresh;
                if (pending || reason == Reason::Wake) && status.auto.enabled {
                    // A quiet period under way runs out first: a run of
                    // edits still becomes one sync.
                    if self.due.is_none() {
                        return Ok(Some(Trigger { reason }));
                    }
                } else if status.pending.files == 0 && !owed {
                    // Nothing to do for this target.
                    self.synced(Some(&status.target));
                }
                Ok(None)
            }
            Err(Error::Unauthorized) => Err(Error::Unauthorized),
            Err(e) => {
                tracing::debug!("status check: {e}");
                self.next_status = Instant::now() + Duration::from_secs(60);
                Ok(None)
            }
        }
    }
}

/// The server's event stream, read line by line.
struct Events {
    body: Incoming,
    buf: Vec<u8>,
    event: String,
    data: String,
    /// When bytes last came: the idle limit runs from then, however often
    /// a wait for them is cut short (a status check, the caller's select!).
    heard_at: Instant,
}

impl Events {
    async fn connect(api: &Api, heartbeat: u32) -> Result<Self, Error> {
        let res = api
            .send(
                Method::GET,
                &format!("{PREFIX}/events?heartbeat={heartbeat}"),
                &[(
                    header::ACCEPT,
                    HeaderValue::from_static("text/event-stream"),
                )],
                None,
            )
            .await?;
        let status = res.status();
        if !status.is_success() {
            let body = read_all(res.into_body(), IDLE_TIMEOUT).await?;
            return Err(refusal(status, &body));
        }
        Ok(Self {
            body: res.into_body(),
            buf: Vec::new(),
            event: String::new(),
            data: String::new(),
            heard_at: Instant::now(),
        })
    }

    /// The next event's name and data; None when the stream ends. Each
    /// wait for bytes is cut off after `idle` (the heartbeat keeps a live
    /// stream talking).
    async fn next(&mut self, idle: Duration) -> Result<Option<(String, String)>, Error> {
        loop {
            while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    if self.event.is_empty() && self.data.is_empty() {
                        continue;
                    }
                    let name = std::mem::take(&mut self.event);
                    let data = std::mem::take(&mut self.data);
                    return Ok(Some((
                        if name.is_empty() {
                            "message".into()
                        } else {
                            name
                        },
                        data,
                    )));
                }
                if line.starts_with(':') {
                    continue;
                }
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "event" => self.event = value.to_string(),
                    "data" => {
                        if !self.data.is_empty() {
                            self.data.push('\n');
                        }
                        self.data.push_str(value);
                    }
                    _ => {}
                }
            }
            if self.buf.len() > 1 << 20 {
                return Err(Error::Protocol("an event too long".into()));
            }
            let left = (self.heard_at + idle).saturating_duration_since(Instant::now());
            match next_chunk(&mut self.body, left).await? {
                Some(chunk) => {
                    self.heard_at = Instant::now();
                    self.buf.extend_from_slice(&chunk);
                }
                None => return Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Server, Tls, Token};

    fn watch() -> Watch {
        let server = Server {
            url: "http://127.0.0.1:9".into(),
            token: Token::new("cst_test").unwrap(),
            tls: Tls::System,
        };
        Watch::new(
            Arc::new(Api::new(&server, 1).unwrap()),
            WatchOptions::default(),
        )
    }

    fn target(w: &mut Watch, t: &str) {
        w.heard(
            "target",
            &format!(r#"{{"target":"{t}","changed_at":null}}"#),
        );
    }

    #[test]
    fn a_sync_clears_the_wait_for_what_it_reached_and_no_other() {
        // Reaching an older target leaves the wait for a newer one.
        let mut w = watch();
        target(&mut w, "t_a");
        target(&mut w, "t_b");
        w.synced(Some("t_a"));
        assert!(w.due.is_some());
        w.synced(Some("t_b"));
        assert!(w.due.is_none());

        // Events queued during a sync, read after it reached the newest:
        // the older one's wait goes when the newest is read.
        let mut w = watch();
        w.synced(Some("t_2"));
        target(&mut w, "t_1");
        assert!(w.due.is_some());
        target(&mut w, "t_2");
        assert!(w.due.is_none());
    }

    #[tokio::test]
    async fn a_check_cut_short_is_still_the_wake() {
        // A server that takes the request and never answers.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Server {
            url: format!("http://{}", listener.local_addr().unwrap()),
            token: Token::new("cst_test").unwrap(),
            tls: Tls::System,
        };
        let mut w = Watch::new(
            Arc::new(Api::new(&server, 1).unwrap()),
            WatchOptions::default(),
        );
        let cut = tokio::time::timeout(Duration::from_millis(200), w.next()).await;
        assert!(cut.is_err());
        // The caller dropped it mid-request: the next check still wakes.
        assert!(w.woke);
        drop(listener);
    }

    #[test]
    fn auto_sync_turned_back_on_checks_at_once() {
        let mut w = watch();
        let later = Instant::now() + Duration::from_secs(600);
        w.next_status = later;
        w.heard(
            "settings",
            r#"{"enabled":false,"quiet_seconds":60,"wifi_only":true,"charging_only":false}"#,
        );
        assert_eq!(w.next_status, later);
        w.heard(
            "settings",
            r#"{"enabled":true,"quiet_seconds":60,"wifi_only":true,"charging_only":false}"#,
        );
        assert!(w.next_status <= Instant::now());
    }
}
