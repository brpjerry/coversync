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
                let reason = if std::mem::take(&mut self.woke) {
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
                        // Anything missed while away shows in the status.
                        if self.failures > 0 {
                            self.next_status = Instant::now();
                        }
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
        match status(&self.api, self.etag.as_deref()).await {
            Ok(StatusCheck::Unchanged) => {
                self.next_status = Instant::now() + self.options.status_every;
                Ok(None)
            }
            Ok(StatusCheck::Changed { status, etag }) => {
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
                if (status.pending.files > 0 || reason == Reason::Wake) && status.auto.enabled {
                    // A quiet period under way runs out first: a run of
                    // edits still becomes one sync.
                    if self.due.is_none() {
                        return Ok(Some(Trigger { reason }));
                    }
                } else if status.pending.files == 0 {
                    // Nothing to do for this target.
                    self.synced = Some(status.target);
                }
                Ok(None)
            }
            Err(Error::Unauthorized) => Err(Error::Unauthorized),
            Err(e) => {
                tracing::debug!("status check: {e}");
                self.woke = self.woke || reason == Reason::Wake;
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
            match next_chunk(&mut self.body, idle).await? {
                Some(chunk) => self.buf.extend_from_slice(&chunk),
                None => return Ok(None),
            }
        }
    }
}
