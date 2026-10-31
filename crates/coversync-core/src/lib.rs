//! Library indexing shared by the coversync daemon and a future desktop client.

pub mod appearance;
pub mod art;
pub mod autofill;
pub mod cover;
pub mod db;
pub mod identify;
pub mod import;
pub mod inbox;
pub mod model;
pub mod musicbrainz;
pub mod romanize;
pub mod rules;
pub mod scan;
pub mod sync;
pub mod tags;
pub mod template;

pub use db::Library;
pub use inbox::{HiddenStats, InboxSetup, IngestMode};
pub use model::{Album, AlbumDetail, Collection, ScanProgress, TagCoverage, TagEdit, Track};
pub use rules::LibrarySetup;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("tags: {0}")]
    Tags(String),
    #[error("image: {0}")]
    Image(#[from] image::ImageError),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Invalid(String),
    /// A web service (AcoustID, MusicBrainz) failed or couldn't be reached.
    #[error("{0}")]
    Remote(String),
}

pub type Result<T> = std::result::Result<T, Error>;
