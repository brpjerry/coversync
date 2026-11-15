//! Facts: what the device did, kept in `.coversync-replica/facts` until the
//! server confirms them, and sent in batches. A fact the server never heard
//! of only repeats its operation in the next plan (a put downloads again),
//! so the journal is never flushed to the disk: it's there so that a sync
//! cut off (the network, the app closed) loses nothing it did.

use std::sync::{Arc, Mutex};

use coversync_protocol::{Fact, Failure};

use crate::storage::Storage;

/// Most facts in one batch.
const BATCH: usize = 5000;

pub(crate) struct Facts {
    storage: Arc<dyn Storage>,
    inner: Mutex<Inner>,
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
        let unconfirmed = match storage.read_journal() {
            Ok(bytes) => bytes
                .split(|&b| b == b'\n')
                .filter(|l| !l.is_empty())
                // A line cut short by a crash is dropped: its operation
                // only repeats.
                .filter_map(|l| serde_json::from_slice(l).ok())
                .collect(),
            Err(e) => {
                tracing::warn!("couldn't read the facts journal: {e}");
                Vec::new()
            }
        };
        Self {
            storage,
            inner: Mutex::new(Inner {
                unconfirmed,
                failed: Vec::new(),
            }),
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
            tracing::warn!("couldn't empty the facts journal: {e}");
        }
    }

    /// Something done: journaled at once, sent with the next batch.
    pub(crate) fn done(&self, fact: Fact) {
        let mut line = serde_json::to_vec(&fact).expect("facts serialize");
        line.push(b'\n');
        let mut i = self.lock();
        if let Err(e) = self.storage.append_journal(&line) {
            tracing::debug!("couldn't journal a fact: {e}");
        }
        i.unconfirmed.push(fact);
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
            tracing::debug!("couldn't trim the facts journal: {e}");
        }
    }

    /// The batch didn't reach the server: its failures go with the next.
    pub(crate) fn unsent(&self, mut failed: Vec<Failure>) {
        let mut i = self.lock();
        failed.append(&mut i.failed);
        i.failed = failed;
    }
}
