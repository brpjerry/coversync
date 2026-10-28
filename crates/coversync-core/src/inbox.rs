//! Inbox settings: how files found on disk are brought into the Library.

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
    /// Empty tags filled with safe assumptions on import.
    #[serde(default)]
    pub autofill: crate::autofill::Autofill,
    #[serde(default)]
    pub suggestions: Suggestions,
}

/// Smart suggestions: identify albums on MusicBrainz when importing and
/// offer its tags. The AcoustID API key is stored apart from these settings
/// (see `Library::acoustid_key`) and never sent back to the browser.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
        Ok(self)
    }
}

/// The Inbox and Library folders must not contain one another, or each
/// would show up in the other.
pub fn check_folders(inbox: Option<&str>, library: Option<&str>) -> crate::Result<()> {
    let (Some(a), Some(b)) = (inbox, library) else {
        return Ok(());
    };
    let within =
        |inner: &str, outer: &str| inner == outer || inner.starts_with(&format!("{outer}/"));
    if within(a, b) || within(b, a) {
        return Err(crate::Error::Invalid(
            "The Inbox and Library folders can't be the same or inside one another.".into(),
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
}
