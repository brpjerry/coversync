//! Sync setup's view of each device's sync, and what it can ask for:
//! a sync now, a check of the device's folder, retrying failed files, and
//! removing (or keeping) the files coversync didn't put there.
//!
//! A password login reads any device's and asks for anything; an app may
//! read only its own device's.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use coversync_protocol::Count;
use serde::{Deserialize, Serialize};

use super::api::{Problem, Running, work_out};
use super::store::{FailedPut, Flag, LastSession};
use crate::Shared;
use crate::auth::Access;
use crate::events::Notice;

/// Failed files Sync setup lists, at most.
const FAILURES_SHOWN: usize = 50;

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/devices/{id}/sync/state", get(state))
        .route("/api/devices/{id}/sync/action", post(action))
}

#[derive(Serialize)]
struct DeviceSyncState {
    /// A session under way.
    running: Option<Running>,
    last: Option<LastSession>,
    /// The device's folder has been reconciled once at least.
    reconciled: bool,
    /// What the server believes the device holds.
    held: Count,
    /// What it still needs: files to put, move or delete, and bytes to
    /// download.
    pending: Count,
    preparing: u64,
    /// Files whose path is too long for the device.
    too_long: u64,
    /// Room the device would still need, from the free space it last
    /// reported.
    short_bytes: u64,
    failed: Failed,
    other: Count,
    remove_other: bool,
    check_requested: bool,
    sync_requested: bool,
    /// The Library folder is unavailable: nothing to sync to.
    no_library: bool,
}

#[derive(Serialize)]
struct Failed {
    count: u64,
    files: Vec<FailedPut>,
}

fn may_read(access: &Access, id: i64) -> Result<(), Problem> {
    if access.password_login() || access.device == Some(id) {
        return Ok(());
    }
    Err(Problem::new(
        StatusCode::FORBIDDEN,
        "other_device",
        "An app can only read its own device's sync.",
    ))
}

async fn state(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<i64>,
) -> Result<Json<DeviceSyncState>, Problem> {
    may_read(&access, id)?;
    if state.auth.device_sync(id)?.is_none() {
        return Err(Problem::new(
            StatusCode::NOT_FOUND,
            "no_device",
            "There's no such device.",
        ));
    }
    let running = state.sync.sessions.running(id);
    let free = state.sync.sessions.free_bytes(id);
    let s = state.clone();
    tokio::task::spawn_blocking(move || {
        let st = s.sync.store.device(id)?;
        let free = free.or(st.last.as_ref().and_then(|l| l.report.free_bytes));
        let files = s.sync.store.files(id)?;
        let mut failed = s.sync.store.failures(id)?;
        let worked = match work_out(&s, id, free) {
            Ok(worked) => Some(worked),
            // Without a Library there's nothing to sync to, which Sync setup
            // says; anything else is an error, not an empty state.
            Err(Problem(StatusCode::SERVICE_UNAVAILABLE, ..)) => None,
            Err(e) => return Err(e),
        };
        let no_library = worked.is_none();
        let (pending, preparing, too_long, short_bytes) = match &worked {
            Some(w) => {
                let t = &w.header.totals;
                // A failure counts while its content is still what the
                // device should hold there, and doesn't yet.
                failed.retain(|f| {
                    w.target.at(&f.path).is_some_and(|e| e.hash == f.hash)
                        && files
                            .inventory
                            .get(&f.path)
                            .is_none_or(|h| h.hash != f.hash)
                });
                (
                    Count {
                        files: t.put + t.moves + t.delete,
                        bytes: t.put_bytes,
                    },
                    t.preparing,
                    w.target.too_long,
                    t.short_bytes,
                )
            }
            None => (Count::default(), 0, 0, 0),
        };
        let count = failed.len() as u64;
        failed.truncate(FAILURES_SHOWN);
        let list = failed;
        Ok(Json(DeviceSyncState {
            running,
            last: st.last,
            reconciled: st.epoch.is_some(),
            held: Count {
                files: files.inventory.len() as u64,
                bytes: files.inventory.values().map(|h| h.size).sum(),
            },
            pending,
            preparing,
            too_long,
            short_bytes,
            failed: Failed { count, files: list },
            other: Count {
                files: files.other.len() as u64,
                bytes: files.other.values().sum(),
            },
            remove_other: st.remove_other,
            check_requested: st.check_requested,
            sync_requested: st.sync_requested,
            no_library,
        }))
    })
    .await
    .map_err(|e| Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "failed", e.to_string()))?
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    /// Tell the device to sync now (through its event stream, or at its
    /// next status check).
    Sync,
    /// Reconcile the device's folder at its next sync.
    Check,
    /// Try the failed files again.
    Retry,
    /// Remove the files coversync didn't put in the device's folder.
    RemoveOther,
    /// Keep them after all.
    KeepOther,
}

#[derive(Deserialize)]
struct Asked {
    action: Action,
}

async fn action(
    State(state): State<Shared>,
    Extension(access): Extension<Access>,
    Path(id): Path<i64>,
    Json(asked): Json<Asked>,
) -> Result<StatusCode, Problem> {
    if !access.password_login() {
        return Err(Problem::new(
            StatusCode::FORBIDDEN,
            "password_only",
            "Only a browser signed in with the password can change a device's sync.",
        ));
    }
    if state.auth.device_sync(id)?.is_none() {
        return Err(Problem::new(
            StatusCode::NOT_FOUND,
            "no_device",
            "There's no such device.",
        ));
    }
    let s = state.clone();
    let action = asked.action;
    tokio::task::spawn_blocking(move || {
        match action {
            Action::Sync | Action::KeepOther => {}
            Action::Check => s.sync.store.set_flag(id, Flag::Check, true)?,
            Action::Retry => s.sync.store.clear_failures(id)?,
            Action::RemoveOther => s.sync.store.set_flag(id, Flag::RemoveOther, true)?,
        }
        if let Action::KeepOther = action {
            s.sync.store.set_flag(id, Flag::RemoveOther, false)
        } else {
            // Each of these is done at the device's next sync: ask for one,
            // so a device checking its status, rather than listening, starts
            // it too.
            s.sync.store.set_flag(id, Flag::Sync, true)
        }
    })
    .await
    .map_err(|e| Problem::new(StatusCode::INTERNAL_SERVER_ERROR, "failed", e.to_string()))??;
    // A device listening hears of it at once; the others at their next
    // status check.
    if !matches!(action, Action::KeepOther) {
        let _ = state.sync.requests.send(id);
    }
    state.sync.changed();
    state.events.publish(Notice::DeviceSync { device: id });
    Ok(StatusCode::NO_CONTENT)
}
