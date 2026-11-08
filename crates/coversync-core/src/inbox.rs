//! Inbox settings: how files found on disk are brought into the Library.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What happens to an Inbox file when it is ingested into the Library.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IngestMode {
    /// The Library gets a copy; the original stays on disk, hidden from the Inbox.
    #[default]
    Copy,
    /// The file itself moves into the Library directory.
    Move,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxSetup {
    #[serde(default)]
    pub ingest: IngestMode,
    /// Absolute path of the folder scanned for unmanaged music.
    #[serde(default)]
    pub folder: Option<String>,
    /// Where uploaded albums go, if not `Uploads` in the Inbox folder: an
    /// absolute path, inside the Inbox folder or apart from it. One apart is
    /// scanned as part of the Inbox.
    #[serde(default)]
    pub uploads: Option<String>,
    /// Empty tags filled with safe assumptions on import.
    #[serde(default)]
    pub autofill: crate::autofill::Autofill,
    #[serde(default)]
    pub suggestions: Suggestions,
}

/// Smart suggestions: identify albums on MusicBrainz when importing and
/// offer its tags. The AcoustID API key is stored apart from these settings
/// (see `Library::acoustid_key`) and never sent back to the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestions {
    #[serde(default)]
    pub enabled: bool,
    /// For releases whose titles and names don't sort A–Z (Japanese, Korean,
    /// Cyrillic…), suggest MusicBrainz's romanized or English ones.
    #[serde(default)]
    pub prefer_english: bool,
    /// With `prefer_english`: people's names romanized from Chinese, Japanese
    /// or Korean put the family name first ("Nishikino Maki").
    #[serde(default)]
    pub family_name_first: bool,
    /// Keep AcoustID's and MusicBrainz's answers in the
    /// [`LookupCache`](crate::LookupCache), so an album is looked up once.
    /// Off, every lookup asks both services and nothing is read or saved.
    #[serde(default = "yes")]
    pub save_lookups: bool,
}

impl Default for Suggestions {
    fn default() -> Self {
        Self {
            enabled: false,
            prefer_english: false,
            family_name_first: false,
            save_lookups: true,
        }
    }
}

fn yes() -> bool {
    true
}

/// Check an AcoustID application API key as typed; None clears it.
pub fn acoustid_key(key: &str) -> crate::Result<Option<String>> {
    let key = key.trim();
    if key.is_empty() {
        return Ok(None);
    }
    if key.len() > 64
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(crate::Error::Invalid(
            "An AcoustID API key is a short code of letters and digits, like the one on acoustid.org/my-applications.".into(),
        ));
    }
    Ok(Some(key.to_string()))
}

impl InboxSetup {
    pub fn normalize(mut self) -> crate::Result<Self> {
        self.folder = crate::rules::folder_setting(self.folder, "Inbox folder")?;
        self.uploads = crate::rules::folder_setting(self.uploads, "uploads folder")?;
        Ok(self)
    }

    /// Where uploads go: the uploads folder, or `Uploads` in the Inbox
    /// folder. None without either.
    pub fn uploads_folder(&self) -> Option<PathBuf> {
        self.uploads.as_deref().map(PathBuf::from).or_else(|| {
            self.folder
                .as_deref()
                .map(|f| Path::new(f).join(crate::upload::DEFAULT_FOLDER))
        })
    }
}

/// A folder as the scan sees it: the part that exists with its links
/// resolved (as `canonicalize` does), and the rest, which may be created
/// later, as written, without empty or `.` names. Two spellings of one
/// folder, through a link or with `//`, come out the same.
pub fn resolved(path: &Path) -> PathBuf {
    for existing in path.ancestors() {
        if let Ok(real) = existing.canonicalize() {
            let rest = path.strip_prefix(existing).unwrap_or(Path::new(""));
            return real.join(rest.components().collect::<PathBuf>());
        }
    }
    path.components().collect()
}

/// `inner` is `outer` or inside it, as resolved paths.
fn within(inner: &Path, outer: &Path) -> bool {
    inner.starts_with(outer)
}

/// The Inbox and Library folders must not contain one another, or each
/// would show up in the other.
pub fn check_folders(inbox: Option<&str>, library: Option<&str>) -> crate::Result<()> {
    let (Some(a), Some(b)) = (inbox, library) else {
        return Ok(());
    };
    let (a, b) = (resolved(Path::new(a)), resolved(Path::new(b)));
    if within(&a, &b) || within(&b, &a) {
        return Err(crate::Error::Invalid(
            "The Inbox and Library folders can't be the same or inside one another.".into(),
        ));
    }
    Ok(())
}

/// The uploads folder (the one set in Inbox setup, or `Uploads` in the
/// Inbox folder) is part of the Inbox: inside the Inbox folder, or apart
/// from it and from the Library folder. One holding the Inbox folder would
/// scan it twice. Links are followed, as the scan follows them.
pub fn check_uploads(
    uploads: Option<&str>,
    inbox: Option<&str>,
    library: Option<&str>,
) -> crate::Result<()> {
    let uploads = uploads
        .map(PathBuf::from)
        .or_else(|| inbox.map(|i| Path::new(i).join(crate::upload::DEFAULT_FOLDER)));
    let Some(u) = uploads.as_deref().map(resolved) else {
        return Ok(());
    };
    if let Some(i) = inbox.map(|i| resolved(Path::new(i)))
        && within(&i, &u)
        && i != u
    {
        return Err(crate::Error::Invalid(
            "The uploads folder can't hold the Inbox folder. Choose one inside it, or apart from it."
                .into(),
        ));
    }
    if let Some(l) = library.map(|l| resolved(Path::new(l)))
        && (within(&u, &l) || within(&l, &u))
    {
        return Err(crate::Error::Invalid(
            "The uploads folder and the Library folder can't be the same or inside one another."
                .into(),
        ));
    }
    Ok(())
}

/// Inbox files hidden because they were already copied into the Library.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct HiddenStats {
    pub files: u32,
    /// Albums with at least one hidden file still on disk.
    pub albums: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_copy_and_uses_lowercase_names() {
        assert_eq!(InboxSetup::default().ingest, IngestMode::Copy);
        let parsed: InboxSetup = serde_json::from_str(r#"{"ingest":"move"}"#).unwrap();
        assert_eq!(parsed.ingest, IngestMode::Move);
        let empty: InboxSetup = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.ingest, IngestMode::Copy);
        assert!(serde_json::from_str::<InboxSetup>(r#"{"ingest":"link"}"#).is_err());
    }

    #[test]
    fn folders_must_be_absolute_and_apart() {
        let s = InboxSetup {
            folder: Some(" /data/Music/ ".into()),
            ..Default::default()
        };
        assert_eq!(
            s.normalize().unwrap().folder.as_deref(),
            Some("/data/Music")
        );
        let rel = InboxSetup {
            folder: Some("Music".into()),
            ..Default::default()
        };
        assert!(rel.normalize().is_err());
        let blank = InboxSetup {
            folder: Some("  ".into()),
            ..Default::default()
        };
        assert_eq!(blank.normalize().unwrap().folder, None);
        assert!(check_folders(Some("/m"), Some("/m/lib")).is_err());
        assert!(check_folders(Some("/m/in"), Some("/m")).is_err());
        assert!(check_folders(Some("/m"), Some("/m")).is_err());
        assert!(check_folders(Some("/m"), Some("/music")).is_ok());
        assert!(check_folders(None, Some("/m")).is_ok());
    }

    #[test]
    fn uploads_go_in_the_inbox_unless_set_elsewhere() {
        let mut s = InboxSetup {
            folder: Some("/data2/Music".into()),
            uploads: Some(" ".into()),
            ..Default::default()
        }
        .normalize()
        .unwrap();
        assert_eq!(s.uploads, None);
        assert_eq!(
            s.uploads_folder().as_deref(),
            Some(Path::new("/data2/Music/Uploads"))
        );
        s.uploads = Some("/data2/Uploads/".into());
        let s = s.normalize().unwrap();
        assert_eq!(
            s.uploads_folder().as_deref(),
            Some(Path::new("/data2/Uploads"))
        );
        assert_eq!(InboxSetup::default().uploads_folder(), None);
        let rel = InboxSetup {
            uploads: Some("Uploads".into()),
            ..Default::default()
        };
        assert!(rel.normalize().is_err());
        // Saved before uploads existed.
        let before: InboxSetup = serde_json::from_str(r#"{"folder":"/m"}"#).unwrap();
        assert_eq!(before.uploads, None);
    }

    #[test]
    fn an_uploads_folder_is_inside_the_inbox_or_apart() {
        let (inbox, lib) = (Some("/m/in"), Some("/m/lib"));
        assert!(check_uploads(None, inbox, lib).is_ok());
        assert!(check_uploads(Some("/m/in/Up"), inbox, lib).is_ok());
        assert!(check_uploads(Some("/m/in"), inbox, lib).is_ok());
        assert!(check_uploads(Some("/m/up"), inbox, lib).is_ok());
        assert!(check_uploads(Some("/m/inbox"), inbox, lib).is_ok());
        assert!(check_uploads(Some("/m"), inbox, None).is_err());
        assert!(check_uploads(Some("/m/lib"), inbox, lib).is_err());
        assert!(check_uploads(Some("/m/lib/Up"), inbox, lib).is_err());
        assert!(check_uploads(Some("/m/up"), None, Some("/m/up/lib")).is_err());
    }

    #[test]
    fn folders_are_compared_as_the_scan_sees_them() {
        let dir = std::env::temp_dir().join(format!("coversync-aliases-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["Inbox", "Library"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let dir = dir.canonicalize().unwrap();
        let s = |p: &Path| p.display().to_string();
        let (inbox, library) = (s(&dir.join("Inbox")), s(&dir.join("Library")));
        let link = dir.join("Elsewhere");
        std::os::unix::fs::symlink(dir.join("Library"), &link).unwrap();

        // Redundant separators, and the part not made yet.
        assert_eq!(
            resolved(Path::new(&format!("{}//Inbox/./Up//x", dir.display()))),
            dir.join("Inbox/Up/x")
        );
        // A link to the Library, or a folder in it through one, is the Library.
        assert_eq!(resolved(&link.join("Up")), dir.join("Library/Up"));
        let lib = Some(library.as_str());
        assert!(check_uploads(Some(&s(&link)), Some(&inbox), lib).is_err());
        assert!(check_uploads(Some(&s(&link.join("Up"))), Some(&inbox), lib).is_err());
        assert!(check_folders(Some(&s(&link)), lib).is_err());
        assert!(check_uploads(Some(&s(&dir.join("Uploads"))), Some(&inbox), lib).is_ok());
        // The default, Uploads in the Inbox folder, is checked too.
        std::os::unix::fs::symlink(dir.join("Library"), dir.join("Inbox/Uploads")).unwrap();
        assert!(check_uploads(None, Some(&inbox), lib).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn acoustid_keys() {
        assert_eq!(
            acoustid_key(" abcDEF123 ").unwrap().as_deref(),
            Some("abcDEF123")
        );
        assert_eq!(acoustid_key("  ").unwrap(), None);
        assert!(acoustid_key("abc def").is_err());
        assert!(acoustid_key("key&client=x").is_err());
        let off: InboxSetup = serde_json::from_str("{}").unwrap();
        assert!(!off.suggestions.enabled);
    }

    #[test]
    fn lookups_are_saved_unless_turned_off() {
        assert!(InboxSetup::default().suggestions.save_lookups);
        // Saved before the switch existed.
        let before: InboxSetup =
            serde_json::from_str(r#"{"suggestions":{"enabled":true,"prefer_english":true}}"#)
                .unwrap();
        assert!(before.suggestions.save_lookups);
        let off: InboxSetup =
            serde_json::from_str(r#"{"suggestions":{"enabled":true,"save_lookups":false}}"#)
                .unwrap();
        assert!(!off.suggestions.save_lookups);
    }
}
