//! Facts: what the device did, kept in `.coversync-replica/facts` until the
//! server confirms them, and sent in batches. Appends aren't flushed one by
//! one: `Storage::end_changes` makes the journal and the folder durable
//! together at the end of a sync. A sync that never gets there leaves its
//! dirty note, and so does one whose journal couldn't be written: the next
//! sync checks the folder rather than trust facts that may be lost or stale
//! (repeating a move isn't always safe).

use std::sync::{Arc, Mutex};

use coversync_protocol::{Fact, Failure};

use crate::storage::Storage;

/// Most facts in one batch.
const BATCH: usize = 5000;

pub(crate) struct Facts {
    storage: Arc<dyn Storage>,
    /// The journal couldn't be read whole when loaded.
    damaged: bool,
    inner: Mutex<Inner>,
    lost: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Inner {
    /// Facts not yet confirmed, in the journal's order.
    unconfirmed: Vec<Fact>,
    failed: Vec<Failure>,
}

/// A batch being sent: confirmed with [`Facts::confirmed`].
pub(crate) struct Batch {
    pub done: Vec<Fact>,
    pub failed: Vec<Failure>,
}

impl Facts {
    /// The facts an earlier sync left unconfirmed.
    pub(crate) fn load(storage: Arc<dyn Storage>) -> Self {
        let mut damaged = false;
        let unconfirmed = match storage.read_journal() {
            Ok(bytes) => bytes
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
                .filter_map(|l| {
                    let fact = serde_json::from_slice(l).ok();
                    damaged |= fact.is_none();
                    fact
                })
                .collect(),
            Err(e) => {
                tracing::warn!("couldn't read the facts journal: {e}");
                damaged = true;
                Vec::new()
            }
        };
        if damaged {
            tracing::warn!("the facts journal is damaged: the folder will be checked");
        }
        Self {
            damaged,
            storage,
            inner: Mutex::new(Inner {
                unconfirmed,
                failed: Vec::new(),
            }),
            lost: Default::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Facts done and not yet confirmed.
    pub(crate) fn unconfirmed(&self) -> usize {
        self.lock().unconfirmed.len()
    }

    /// Facts waiting, from this sync or an earlier one.
    pub(crate) fn waiting(&self) -> usize {
        let i = self.lock();
        i.unconfirmed.len() + i.failed.len()
    }

    /// Forget every fact: a reconcile rebuilds what the server knows from
    /// the folder itself.
    pub(crate) fn forget(&self) {
        let mut i = self.lock();
        i.unconfirmed.clear();
        if let Err(e) = self.storage.replace_journal(b"") {
            // Stale facts left there would be replayed next time.
            tracing::warn!("couldn't empty the facts journal: {e}");
            self.lost.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Something done: journaled at once, sent with the next batch.
    pub(crate) fn done(&self, fact: Fact) {
        let mut line = serde_json::to_vec(&fact).expect("facts serialize");
        line.push(b'\n');
        let mut i = self.lock();
        if let Err(e) = self.storage.append_journal(&line) {
            tracing::warn!("couldn't journal a fact: {e}");
            self.lost.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        i.unconfirmed.push(fact);
    }

    /// The journal couldn't be read whole: facts of an earlier sync may be
    /// gone.
    pub(crate) fn damaged(&self) -> bool {
        self.damaged
    }

    /// The journal couldn't be written as it should (a fact appended, a
    /// trim, a clear): what it holds can't be trusted next time.
    pub(crate) fn lost(&self) -> bool {
        self.lost.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn failed(&self, failure: Failure) {
        self.lock().failed.push(failure);
    }

    /// The next batch: the oldest unconfirmed facts, and the failures.
    pub(crate) fn batch(&self) -> Batch {
        let mut i = self.lock();
        let n = i.unconfirmed.len().min(BATCH);
        Batch {
            done: i.unconfirmed[..n].to_vec(),
            failed: std::mem::take(&mut i.failed),
        }
    }

    /// The server took the batch's `n` facts: they leave the journal.
    pub(crate) fn confirmed(&self, n: usize) {
        if n == 0 {
            return;
        }
        let mut i = self.lock();
        let n = n.min(i.unconfirmed.len());
        // Facts are only added at the end, so the batch is the start.
        i.unconfirmed.drain(..n);
        let mut rest = Vec::new();
        for f in &i.unconfirmed {
            serde_json::to_writer(&mut rest, f).expect("facts serialize");
            rest.push(b'\n');
        }
        if let Err(e) = self.storage.replace_journal(&rest) {
            // Emptied first, it may now hold none of the facts still
            // unconfirmed.
            tracing::warn!("couldn't trim the facts journal: {e}");
            self.lost.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The batch didn't reach the server: its failures go with the next.
    pub(crate) fn unsent(&self, mut failed: Vec<Failure>) {
        let mut i = self.lock();
        failed.append(&mut i.failed);
        i.failed = failed;
    }
}
