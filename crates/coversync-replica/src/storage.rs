//! Where a device keeps its files. The engine reaches the sync folder only
//! through [`Storage`], so each kind of device brings its own: a plain
//! folder ([`crate::FsStorage`]), Android's shared storage (AND-03), or a
//! player's card.
//!
//! Paths are relative to the sync folder, with `/` between names, and the
//! engine only passes ones [`coversync_protocol::valid_path`] allows. A
//! storage still never follows a symbolic link inside the folder: the plan's
//! paths are the server's, but links are the device's own and could lead
//! anywhere.
//!
//! Every call blocks. The engine makes them on blocking threads, never on
//! the async runtime's.

use std::io::{self, Read, Seek, Write};

use coversync_protocol::{Capabilities, Listed};

/// What's at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Entry {
    File {
        size: u64,
    },
    Dir,
    /// A link, a device node, anything else: never touched.
    Other,
}

/// A file being downloaded, in the client's folder.
pub trait Partial: Write + Send {
    /// Make what's written durable, before the file is put in place.
    fn sync(&mut self) -> io::Result<()>;
}

/// A file opened for reading (a reconcile's samples and hashes).
pub trait Source: Read + Seek + Send {}
impl<T: Read + Seek + Send> Source for T {}

pub trait Storage: Send + Sync + 'static {
    /// Keep other syncs out of this folder until the guard is dropped: two
    /// at once would trip over each other's partial files. Fails when
    /// another holds it. Storage only one process can reach needs none.
    fn lock(&self) -> io::Result<Option<Box<dyn Send>>> {
        Ok(None)
    }

    /// What the storage can hold, which shapes the paths the server plans.
    fn capabilities(&self) -> Capabilities;

    /// Free space for the sync folder, if the storage can tell.
    fn free_bytes(&self) -> io::Result<Option<u64>>;

    /// Every file in the sync folder, with its size, except the client's own
    /// folder ([`coversync_protocol::CLIENT_DIR`]). Links aren't followed or
    /// listed. A file or folder whose path `skip` takes is left out, and a
    /// folder isn't even entered (a player's `.rockbox`, or everything once
    /// the sync is cancelled).
    fn list(&self, skip: &dyn Fn(&str) -> bool, each: &mut dyn FnMut(Listed)) -> io::Result<()>;

    /// What's at `path`, without following a link. None if nothing is.
    fn stat(&self, path: &str) -> io::Result<Option<Entry>>;

    /// Remove the file at `path`.
    fn remove_file(&self, path: &str) -> io::Result<()>;

    /// Create the folder `path` and any parents it lacks.
    fn create_dirs(&self, path: &str) -> io::Result<()>;

    /// Remove the folder `path` if it's empty. A folder that isn't empty, or
    /// isn't there, is no error.
    fn remove_dir(&self, path: &str) -> io::Result<()>;

    /// Rename the file `from` to `to`, replacing a file at `to`.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;

    /// Open the file at `path` for reading.
    fn open(&self, path: &str) -> io::Result<Box<dyn Source>>;

    // Partial downloads, named by the engine, in the client's folder.

    /// The size of the partial file `name`, if there is one.
    fn partial_len(&self, name: &str) -> io::Result<Option<u64>>;

    /// Open the partial file `name` for writing: emptied, or (`resume`)
    /// appended to.
    fn write_partial(&self, name: &str, resume: bool) -> io::Result<Box<dyn Partial>>;

    /// Read the partial file `name` (to hash what a resumed download
    /// already has).
    fn read_partial(&self, name: &str) -> io::Result<Box<dyn Read + Send>>;

    /// Remove the partial file `name`, if it's there.
    fn remove_partial(&self, name: &str) -> io::Result<()>;

    /// Put the finished partial file `name` at `path`. `replace`: the server
    /// believes a file is there, which this replaces (storage that can't
    /// rename over a file deletes it first). Whatever is there goes either
    /// way: the folder belongs to coversync.
    fn publish(&self, name: &str, path: &str, replace: bool) -> io::Result<()>;

    /// Every partial file, with its size: room the client can take back.
    fn partials(&self) -> io::Result<Vec<(String, u64)>>;

    /// Remove every partial file (after a sync that finished).
    fn clear_partials(&self) -> io::Result<()>;

    /// A sync is about to change the folder: note it durably. Returns
    /// whether an earlier sync noted it and never finished (a crash, or the
    /// power cut), after which the folder may not be what the server was
    /// told, so it gets checked.
    fn begin_changes(&self) -> io::Result<bool>;

    /// The sync is done changing the folder: make everything it did durable
    /// (renames, removals and the facts journal alike), then clear the note.
    fn end_changes(&self) -> io::Result<()>;

    // The client's own small files.

    /// The marker (`device.json`), if there is one.
    fn read_marker(&self) -> io::Result<Option<Vec<u8>>>;

    /// Replace the marker, all at once.
    fn write_marker(&self, bytes: &[u8]) -> io::Result<()>;

    /// The facts journal: facts not yet confirmed by the server.
    fn read_journal(&self) -> io::Result<Vec<u8>>;

    /// Add to the journal's end. Called once per finished operation, so it
    /// should be cheap: no flush is needed, as `end_changes` flushes the
    /// journal with the folder, and a sync that never gets there has the
    /// next one check the folder.
    fn append_journal(&self, bytes: &[u8]) -> io::Result<()>;

    /// Replace the journal's contents.
    fn replace_journal(&self, bytes: &[u8]) -> io::Result<()>;
}
