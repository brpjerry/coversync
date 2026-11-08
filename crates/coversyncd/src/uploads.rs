//! The upload API: albums sent into the Inbox in resumable chunks.
//!
//! - `POST /api/uploads` starts one, from its folder name and each file's
//!   path and size, and answers with its id.
//! - `PUT /api/uploads/{id}/files/{index}?offset=N` writes the request body
//!   into file `index` from byte `N`. Chunks of any size, in any order,
//!   several at once.
//! - `GET /api/uploads/{id}` tells how far each file got, as the byte ranges
//!   written so far, to send the rest after a dropped connection or a
//!   restart. A chunk cut short keeps the bytes that arrived.
//! - `POST /api/uploads/{id}/finish` moves the files into the uploads folder
//!   and starts a scan, so the album shows up in the Inbox.
//!   `DELETE /api/uploads/{id}` cancels.
//!
//! Uploads take no lock that other uploads, scans or imports wait on, and
//! never the jobs lock: chunks stream from the network to their file at
//! their offset through large buffers, the disk writes overlapping the
//! network reads.

use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::body::{Body, HttpBody};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post, put};
use coversync_core::upload::{self, FileStatus, Finished, NewUpload, UploadStatus};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::api::{ApiError, ApiResult, FolderStatus, blocking, folder_status};
use crate::{Shared, spawn_scan};

/// Bytes handed to the disk at a time.
const BUFFER: usize = 1 << 20;
/// Buffers waiting for the disk, per chunk, while the next one fills.
const QUEUED: usize = 2;
/// How often progress is saved, so a restart loses at most this much.
const SAVE_EVERY: Duration = Duration::from_secs(2);
/// How often abandoned uploads are looked for, in saves.
const EXPIRE_EVERY: u32 = 30;

pub fn router() -> Router<Shared> {
    Router::new()
        .route(
            "/api/uploads",
            // Room for the paths of many files.
            get(list).post(start).layer(DefaultBodyLimit::max(8 << 20)),
        )
        .route("/api/uploads/{id}", get(status).delete(cancel))
        // The body streams to disk, bounded by the file's size, not
        // collected: no body limit applies.
        .route("/api/uploads/{id}/files/{index}", put(chunk))
        .route("/api/uploads/{id}/finish", post(finish))
}

/// The uploads folder: where uploads go, given Inbox setup.
pub fn folder(state: &crate::AppState) -> Option<PathBuf> {
    state.lib.inbox_setup().ok()?.uploads_folder()
}

#[derive(Serialize)]
struct UploadsResponse {
    /// Where uploads go, as coversyncd sees it; null without an Inbox folder.
    folder: Option<FolderStatus>,
    chunk_size: u64,
    /// The file extensions uploads take.
    extensions: Vec<&'static str>,
    max_files: usize,
    uploads: Vec<UploadStatus>,
}

async fn list(State(state): State<Shared>) -> ApiResult<Json<UploadsResponse>> {
    blocking(&state, |s| {
        Ok(UploadsResponse {
            folder: s
                .uploads
                .folder()
                .map(|f| folder_status(&f.display().to_string())),
            chunk_size: upload::CHUNK_SIZE,
            extensions: upload::extensions(),
            max_files: upload::MAX_FILES,
            uploads: s.uploads.list().iter().map(|u| u.status(false)).collect(),
        })
    })
    .await
    .map(Json)
}

async fn start(
    State(state): State<Shared>,
    Json(new): Json<NewUpload>,
) -> ApiResult<(StatusCode, Json<UploadStatus>)> {
    let upload = blocking(&state, move |s| s.uploads.start(new)).await?;
    tracing::info!(upload = upload.id(), "upload started");
    Ok((StatusCode::CREATED, Json(upload.status(true))))
}

async fn status(
    State(state): State<Shared>,
    Path(id): Path<String>,
) -> ApiResult<Json<UploadStatus>> {
    Ok(Json(state.uploads.get(&id)?.status(true)))
}

#[derive(Deserialize)]
struct ChunkQuery {
    offset: u64,
}

/// Write one chunk: the body, from `offset`.
async fn chunk(
    State(state): State<Shared>,
    Path((id, index)): Path<(String, usize)>,
    Query(q): Query<ChunkQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<FileStatus>> {
    let upload = state.uploads.get(&id)?;
    let size = upload.file_size(index)?;
    let room = size.checked_sub(q.offset);
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    let Some(room) = room.filter(|r| length.is_none_or(|n| n <= *r)) else {
        return Err(past_the_end(size));
    };
    // The disk side: a blocking thread writing buffers as they fill, and
    // handing them back to be filled again.
    let (full_tx, mut full_rx) = mpsc::channel::<Vec<u8>>(QUEUED);
    let (empty_tx, mut empty_rx) = mpsc::channel::<Vec<u8>>(QUEUED + 2);
    let offset = q.offset;
    let writer = tokio::task::spawn_blocking(move || {
        let mut w = upload.writer(index, offset)?;
        while let Some(mut buf) = full_rx.blocking_recv() {
            w.write(&buf)?;
            buf.clear();
            let _ = empty_tx.try_send(buf);
        }
        w.close()
    });
    let pumped = pump(body, room, &full_tx, &mut empty_rx).await;
    drop(full_tx);
    let written = writer.await?;
    match pumped {
        // The writer stopped first; its error says why.
        Ok(()) | Err(Pumped::WriterGone) => Ok(Json(written?)),
        Err(Pumped::TooLong) => Err(past_the_end(size)),
        Err(Pumped::Cut(e)) => {
            // Nobody is left to read this; what arrived is kept.
            tracing::debug!(upload = id, index, "chunk cut short: {e}");
            written?;
            Err(ApiError(
                StatusCode::BAD_REQUEST,
                "The chunk was cut short; ask how far the file got, and send the rest.".into(),
            ))
        }
    }
}

enum Pumped {
    /// More bytes than fit in the file.
    TooLong,
    /// The connection broke.
    Cut(axum::Error),
    WriterGone,
}

/// Read the body into buffers of [`BUFFER`] bytes and send each to the
/// writer, at most `room` bytes. What arrived before an error is sent too.
async fn pump(
    mut body: Body,
    room: u64,
    full: &mpsc::Sender<Vec<u8>>,
    empty: &mut mpsc::Receiver<Vec<u8>>,
) -> Result<(), Pumped> {
    let mut buf = Vec::with_capacity(BUFFER);
    let mut taken = 0u64;
    let outcome = loop {
        let Some(frame) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await
        else {
            break Ok(());
        };
        let data = match frame.map(|f| f.into_data()) {
            Ok(Ok(data)) => data,
            // Trailers.
            Ok(Err(_)) => continue,
            Err(e) => break Err(Pumped::Cut(e)),
        };
        taken += data.len() as u64;
        if taken > room {
            break Err(Pumped::TooLong);
        }
        let mut data = &data[..];
        while !data.is_empty() {
            let n = (BUFFER - buf.len()).min(data.len());
            buf.extend_from_slice(&data[..n]);
            data = &data[n..];
            if buf.len() == BUFFER {
                let next = empty
                    .try_recv()
                    .unwrap_or_else(|_| Vec::with_capacity(BUFFER));
                if full.send(std::mem::replace(&mut buf, next)).await.is_err() {
                    return Err(Pumped::WriterGone);
                }
            }
        }
    };
    if !buf.is_empty() && full.send(buf).await.is_err() {
        return Err(Pumped::WriterGone);
    }
    outcome
}

fn past_the_end(size: u64) -> ApiError {
    ApiError(
        StatusCode::UNPROCESSABLE_ENTITY,
        format!("That chunk goes past the end of the file, which is {size} bytes."),
    )
}

/// Finish an upload, then scan. That goes on even if the client hangs up,
/// so the album still appears; asking again gives the same answer.
async fn finish(State(state): State<Shared>, Path(id): Path<String>) -> ApiResult<Json<Finished>> {
    let task = tokio::spawn(async move {
        let done = blocking(&state, move |s| s.uploads.finish(&id)).await?;
        tracing::info!(path = done.path, "upload finished");
        spawn_scan(state);
        Ok(done)
    });
    task.await?.map(Json)
}

async fn cancel(State(state): State<Shared>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    blocking(&state, move |s| s.uploads.cancel(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Save upload progress every few seconds, and remove abandoned uploads.
pub async fn housekeeping(state: Shared) {
    let mut tick = tokio::time::interval(SAVE_EVERY);
    let mut n = 0u32;
    loop {
        tick.tick().await;
        let s = state.clone();
        let expire = n.is_multiple_of(EXPIRE_EVERY);
        n = n.wrapping_add(1);
        let _ = tokio::task::spawn_blocking(move || {
            s.uploads.save();
            if expire {
                s.uploads.expire(now());
            }
        })
        .await;
    }
}

/// Follow Inbox setup to the uploads folder. Uploads into the one it was
/// are cancelled.
pub fn follow_folder(state: &crate::AppState) {
    let folder = folder(state);
    let cancelled = state.uploads.set_folder(folder.clone());
    if cancelled > 0 {
        tracing::info!(
            cancelled,
            "the uploads folder changed; uploads under way were cancelled"
        );
    }
    tracing::debug!(folder = ?folder, "uploads folder");
}

#[cfg(test)]
mod tests;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
