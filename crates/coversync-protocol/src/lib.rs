//! The sync API's requests and replies (`/api/sync/v1/`), shared by
//! coversyncd and every client: the sync engine, Android (through UniFFI)
//! and the player. Serde and nothing heavy, so it builds anywhere.
//!
//! The server plans every sync. A device starts a session, sends its folder's
//! listing when the server asks for one (a reconcile), then reads the plan
//! page by page and carries it out: deletes, mkdirs, moves and rmdirs in
//! order, then puts, which download files by content hash. It reports what
//! it did as [`Fact`]s, and ends with an [`EndReport`]. The server keeps what
//! each device holds (its inventory), so the device keeps no database.
//!
//! Content hashes are BLAKE3 of a file's exact bytes, as 64 lowercase hex
//! characters. Times are Unix seconds.

use std::io::{self, Read, Seek, SeekFrom};

use serde::{Deserialize, Serialize};

/// The API's version, in its paths.
pub const VERSION: u32 = 1;

/// Where the API's routes start.
pub const PREFIX: &str = "/api/sync/v1";

/// Most operations on one plan page.
pub const PAGE_OPS: usize = 1000;

/// Most bytes on one plan page, about: a page ends after the operation
/// that takes it past this.
pub const PAGE_BYTES: usize = 256 * 1024;

/// Room the server leaves free on a device that reports its free space.
pub const RESERVE_BYTES: u64 = 64 << 20;

/// The heartbeat a device's event stream gets unless it asks for another
/// (`?heartbeat=` seconds), and the range it may ask for.
pub const HEARTBEAT_SECONDS: u32 = 30;
pub const HEARTBEAT_RANGE: std::ops::RangeInclusive<u32> = 15..=600;

/// The folder in a device's sync folder that belongs to the client: the
/// marker, partial downloads and unsent facts. A reconcile's listing leaves
/// it out, and no plan touches it.
pub const CLIENT_DIR: &str = ".coversync-replica";

/// Operations every client understands. A client lists the ones it does in
/// [`Capabilities::ops`]; later kinds are only planned for clients that
/// list them.
pub const BASIC_OPS: &[&str] = &["delete", "move", "rmdir", "mkdir", "put"];

/// The client, as it says in each session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Client {
    /// "android", "linux", "rockbox", …
    pub platform: String,
    pub version: String,
}

/// What a device's storage can hold, which shapes the paths it gets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    /// The longest path, in UTF-8 bytes, relative to the sync folder. None:
    /// no limit beyond the names'.
    pub max_path_bytes: Option<u32>,
    /// The longest name of a file or folder, in UTF-8 bytes.
    pub max_name_bytes: Option<u32>,
    /// The storage ignores case in names (FAT, exFAT, Android's shared
    /// storage). The server keeps such names apart for every device anyway.
    pub case_insensitive: bool,
    /// The operations the client can carry out (see [`BASIC_OPS`]).
    pub ops: Vec<String>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            max_path_bytes: None,
            max_name_bytes: None,
            case_insensitive: false,
            ops: BASIC_OPS.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// The marker a device keeps in `<folder>/.coversync-replica/device.json`.
/// It tells the server whether its inventory for the device still
/// describes this folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub device: i64,
    /// From the server's last reconcile of this folder.
    pub epoch: String,
}

/// What started a sync, as shown in Sync setup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// Someone asked for it, on the device or in Sync setup.
    #[default]
    User,
    /// A `target` event, after the quiet period.
    Event,
    /// A periodic check found something to sync.
    Timer,
    /// The device woke or powered on.
    Wake,
    /// A reason this build doesn't know.
    #[serde(other)]
    Other,
}

/// `POST sessions`: start a sync.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionStart {
    pub client: Client,
    pub capabilities: Capabilities,
    /// From the folder, or None when it has none.
    pub marker: Option<Marker>,
    /// Ask for a reconcile anyway, say after a sync that didn't end cleanly.
    pub check: bool,
    /// Free space in the sync folder's storage. Without it, the plan isn't
    /// fitted to the space.
    pub free_bytes: Option<u64>,
    pub reason: Reason,
}

/// The reply to [`SessionStart`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStarted {
    pub session: String,
    /// Send the folder's listing (`POST sessions/{s}/reconcile`) before
    /// asking for the plan.
    pub reconcile: bool,
    pub target: String,
}

/// A plan's size, in its header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Totals {
    pub delete: u64,
    #[serde(rename = "move")]
    pub moves: u64,
    pub rmdir: u64,
    pub mkdir: u64,
    pub put: u64,
    pub put_bytes: u64,
    /// Files the server is still making or hashing, left out of this plan.
    pub preparing: u64,
    /// Room the whole target would still need on the device, beyond its free
    /// space and the reserve. Puts that didn't fit are left out.
    pub short_bytes: u64,
}

/// A plan page's first line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanHeader {
    pub target: String,
    pub totals: Totals,
}

/// One step of a plan. Paths are relative to the sync folder, with `/`
/// between names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    /// Remove the file at `path`, if its size is `size`. A file of another
    /// size is left alone and reported as `changed`.
    Delete { path: String, size: u64 },
    /// Rename `from` to `path`, which is free by then. Done if `from` has
    /// gone and `path` has `size` bytes.
    Move {
        from: String,
        path: String,
        size: u64,
        hash: String,
    },
    /// Remove the folder `path` if it's empty.
    Rmdir { path: String },
    /// Create the folder `path` (its parents come first in the plan).
    Mkdir { path: String },
    /// Download content `hash` (`GET content/{hash}`) and put it at `path`.
    Put {
        path: String,
        hash: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        track: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        album: Option<String>,
        /// The server believes `path` holds a file now, which this
        /// replaces: storage that can't rename over a file deletes it first.
        #[serde(default, skip_serializing_if = "is_false")]
        replace: bool,
    },
}

fn is_false(b: &bool) -> bool {
    !b
}

impl Op {
    /// The path the operation leaves something at, or removes.
    pub fn path(&self) -> &str {
        match self {
            Op::Delete { path, .. }
            | Op::Move { path, .. }
            | Op::Rmdir { path }
            | Op::Mkdir { path }
            | Op::Put { path, .. } => path,
        }
    }
}

/// A plan page's last line: the cursor for the next page, or None at the end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEnd {
    pub next: Option<String>,
}

/// A line of a plan page (NDJSON): the header, an operation, or the end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanLine {
    Header(PlanHeader),
    Op(Op),
    End(PlanEnd),
}

impl PlanLine {
    /// Read one line. The server writes each kind's own key first
    /// (`{"op"`, `{"next"`, `{"target"`), so the kind is told from the
    /// start of the line without trying each.
    pub fn parse(line: &[u8]) -> serde_json::Result<Self> {
        let line = line.trim_ascii();
        if line.starts_with(br#"{"op""#) {
            serde_json::from_slice(line).map(PlanLine::Op)
        } else if line.starts_with(br#"{"next""#) {
            serde_json::from_slice(line).map(PlanLine::End)
        } else {
            serde_json::from_slice(line).map(PlanLine::Header)
        }
    }

    /// The line as the server writes it, without its newline.
    pub fn to_line(&self) -> String {
        match self {
            PlanLine::Header(h) => serde_json::to_string(h),
            PlanLine::Op(op) => serde_json::to_string(op),
            PlanLine::End(e) => serde_json::to_string(e),
        }
        .expect("plan lines serialize")
    }
}

/// Something a device did, as it reports it. Facts are idempotent: the
/// same one twice changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Fact {
    /// `path` now holds content `hash`.
    Put { path: String, hash: String },
    /// What was at `from` is now at `path`.
    Move { from: String, path: String },
    /// Nothing is at `path` now.
    Delete { path: String },
}

/// Why an operation failed, as [`Failure::error`] gives it.
pub mod errors {
    /// The storage is full.
    pub const NO_SPACE: &str = "no_space";
    /// The storage refused the path for its length.
    pub const PATH_TOO_LONG: &str = "path_too_long";
    /// The client protects that name (say, `.rockbox`).
    pub const PROTECTED: &str = "protected";
    /// The file wasn't the size the operation said.
    pub const CHANGED: &str = "changed";
    /// The content left the target meanwhile (`410 Gone`).
    pub const GONE: &str = "gone";
    /// Anything else the storage or network refused.
    pub const IO: &str = "io";
}

/// An operation the device couldn't carry out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    /// The operation's kind ("put", "move", …).
    pub op: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// One of [`errors`], or another code.
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// How far a sync has got, for Sync setup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Progress {
    pub files: u64,
    pub bytes: u64,
    /// Bytes a second, lately.
    pub rate: Option<u64>,
}

/// `POST sessions/{s}/facts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FactsBatch {
    pub done: Vec<Fact>,
    pub failed: Vec<Failure>,
    pub progress: Option<Progress>,
    pub free_bytes: Option<u64>,
}

/// The reply to [`FactsBatch`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactsReply {
    pub target: String,
    /// The target moved on during the session: finish the downloads under
    /// way, then ask for the plan again from its start.
    pub changed: bool,
}

/// A file in the sync folder, as a reconcile's listing gives it (one NDJSON
/// line each).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    pub path: String,
    pub size: u64,
}

/// A file the server wants sampled: the byte ranges to read, each
/// `[start, end)`, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleRequest {
    pub path: String,
    pub ranges: Vec<[u64; 2]>,
}

/// A sampled file: its [`sample`], or its full content hash instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sampled {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// Files and their bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
    pub files: u64,
    pub bytes: u64,
}

/// The end of a reconcile: write `epoch` into the marker, then ask for the
/// plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileDone {
    pub epoch: String,
    /// Files kept where they are.
    pub adopted: u64,
    /// Files kept, which the plan moves.
    pub moves: u64,
    /// Files coversync didn't put there, left alone.
    pub other: Count,
}

/// How a sync ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Complete,
    /// Stopped for space, time or a lost network, with work left.
    Partial,
    Cancelled,
    Failed,
}

/// `POST sessions/{s}/end`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndReport {
    pub result: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub files: u64,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub seconds: u64,
    #[serde(default)]
    pub free_bytes: Option<u64>,
}

/// When a device syncs by itself. Set per device in Sync setup; each kind
/// of device applies what fits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoSync {
    pub enabled: bool,
    /// How long after the last change to wait, so a run of edits becomes
    /// one sync.
    pub quiet_seconds: u32,
    pub wifi_only: bool,
    pub charging_only: bool,
}

impl AutoSync {
    /// The quiet periods Sync setup offers: none to an hour.
    pub const QUIET_RANGE: std::ops::RangeInclusive<u32> = 0..=3600;
}

impl Default for AutoSync {
    fn default() -> Self {
        Self {
            enabled: true,
            quiet_seconds: 60,
            wifi_only: true,
            charging_only: false,
        }
    }
}

/// A device's last sync, as it ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSync {
    pub at: i64,
    pub result: Outcome,
}

/// `GET status`: whether there's anything to sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub target: String,
    /// When the target last changed, as far as the server has seen.
    pub changed_at: Option<i64>,
    pub pending: Count,
    pub preparing: u64,
    /// Someone chose Sync now for this device in Sync setup.
    pub requested: bool,
    pub auto: AutoSync,
    pub last: Option<LastSync>,
}

/// The `target` event's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetEvent {
    pub target: String,
    pub changed_at: Option<i64>,
}

/// A file the server believes the device holds (`GET inventory`, one NDJSON
/// line each; the last line is a [`PlanEnd`] with the next cursor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub path: String,
    pub hash: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
}

/// A file's sample: BLAKE3 of its size (u64, little-endian), then the bytes
/// of each range in order, as 64 hex characters. Ranges past the end, or out
/// of order, are refused.
pub fn sample(file: &mut (impl Read + Seek), size: u64, ranges: &[[u64; 2]]) -> io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    let mut buf = vec![0; 64 * 1024];
    let mut previous = 0;
    for &[start, end] in ranges {
        if start < previous || end < start || end > size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sample ranges must be in order and within the file",
            ));
        }
        previous = end;
        file.seek(SeekFrom::Start(start))?;
        let mut left = end - start;
        while left > 0 {
            let n = buf.len().min(left as usize);
            file.read_exact(&mut buf[..n])?;
            hasher.update(&buf[..n]);
            left -= n as u64;
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn plan_lines_are_told_apart_by_their_first_key() {
        let lines = [
            PlanLine::Header(PlanHeader {
                target: "t".into(),
                totals: Totals {
                    put: 1,
                    ..Default::default()
                },
            }),
            PlanLine::Op(Op::Put {
                path: "A/1.flac".into(),
                hash: "h".into(),
                size: 3,
                track: Some(-5),
                album: Some("al".into()),
                replace: false,
            }),
            PlanLine::Op(Op::Move {
                from: "a".into(),
                path: "b".into(),
                size: 1,
                hash: "h".into(),
            }),
            PlanLine::End(PlanEnd { next: None }),
        ];
        for line in lines {
            let text = line.to_line();
            assert_eq!(PlanLine::parse(text.as_bytes()).unwrap(), line, "{text}");
        }
        // Defaults are left out on the wire.
        let put = PlanLine::Op(Op::Put {
            path: "p".into(),
            hash: "h".into(),
            size: 1,
            track: None,
            album: None,
            replace: false,
        });
        assert_eq!(
            put.to_line(),
            r#"{"op":"put","path":"p","hash":"h","size":1}"#
        );
    }

    #[test]
    fn samples_cover_the_size_and_the_ranges() {
        let data: Vec<u8> = (0..200u8).collect();
        let a = sample(&mut Cursor::new(&data), 200, &[[0, 10], [190, 200]]).unwrap();
        let mut changed = data.clone();
        changed[100] = 0;
        // A byte outside the ranges doesn't count; one inside does.
        assert_eq!(
            sample(&mut Cursor::new(&changed), 200, &[[0, 10], [190, 200]]).unwrap(),
            a
        );
        changed[5] = 0;
        assert_ne!(
            sample(&mut Cursor::new(&changed), 200, &[[0, 10], [190, 200]]).unwrap(),
            a
        );
        assert!(sample(&mut Cursor::new(&data), 200, &[[10, 20], [0, 5]]).is_err());
        assert!(sample(&mut Cursor::new(&data), 200, &[[0, 201]]).is_err());
    }
}
