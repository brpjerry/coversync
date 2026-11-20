//! Appearance settings: how the album browser looks, from which tags the
//! top bar's sort menu offers and the sidebar filters by to how big the
//! album grid's covers are.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{Error, Result, rules};

/// Sort by when an album's files appeared in their folder.
pub const ADDED: &str = "added";
/// Not a tag: filters by what albums need, the tags the rules want that
/// their tracks miss, and tracks or discs their own numbering says are missing.
pub const ATTENTION: &str = "attention";

pub const DEFAULT_TABS: &[&str] = &["album_artist", "album", "date", ADDED];

/// Keys the album list fills from its own summary fields; any other sort
/// key needs a per-album value from the index (`Album::sort_values`).
pub const BUILT_IN: &[&str] = &["album_artist", "album", "date", "genre", ADDED];

const MAX_TABS: usize = 16;

/// Tags the sidebar filters by until Appearance says otherwise; the first is
/// the one its dropdown starts on.
pub const DEFAULT_FILTERS: &[&str] = &["genre", "album_artist", "date", ATTENTION];

/// Long free text, of no use for picking albums by.
const NOT_FILTERABLE: &[&str] = &["lyrics", "acoustid_fingerprint"];

/// Sixteen tags, and Needs attention and Collection (which settings saved
/// when they were sorts add to as many).
const MAX_FILTERS: usize = 18;

/// The narrowest an album grid cover may be, in CSS pixels.
pub const MIN_COVER_SIZE: u32 = 120;
pub const DEFAULT_COVER_SIZE: u32 = 176;
pub const MAX_COVER_SIZE: u32 = 320;

/// The cover size range, for the Appearance screen's slider.
#[derive(Debug, Serialize)]
pub struct CoverSizes {
    pub min: u32,
    pub max: u32,
    pub default: u32,
}

pub const COVER_SIZES: CoverSizes = CoverSizes {
    min: MIN_COVER_SIZE,
    max: MAX_COVER_SIZE,
    default: DEFAULT_COVER_SIZE,
};

/// A tag the album browser can sort or filter by.
#[derive(Debug, Serialize)]
pub struct Sortable {
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SortGroup {
    pub id: &'static str,
    pub label: &'static str,
}

/// The group of what only coversync knows about an album.
const LIBRARY: &str = "library";

/// The text tag kinds, in their groups, but for Collection (a filter of the
/// Library's own).
fn tags() -> impl Iterator<Item = Sortable> {
    rules::CATALOG
        .iter()
        .filter(|k| !rules::NOT_EDITABLE.contains(&k.id) && k.id != rules::COLLECTION)
        .map(|k| Sortable {
            id: k.id,
            label: k.label,
            group: k.group,
        })
}

/// Everything the sort menu can offer: "Recently added" and the text tag kinds.
pub fn sortable() -> Vec<Sortable> {
    let added = Sortable {
        id: ADDED,
        label: "Recently added",
        group: LIBRARY,
    };
    std::iter::once(added).chain(tags()).collect()
}

/// Everything the sidebar can filter by: what albums need and their
/// collection, then the text tag kinds, less long free text.
pub fn filterable() -> Vec<Sortable> {
    let own = [
        Sortable {
            id: ATTENTION,
            label: "Needs attention",
            group: LIBRARY,
        },
        Sortable {
            id: rules::COLLECTION,
            label: "Collection",
            group: LIBRARY,
        },
    ];
    let tags = tags().filter(|s| !NOT_FILTERABLE.contains(&s.id));
    own.into_iter().chain(tags).collect()
}

pub fn groups() -> Vec<SortGroup> {
    let mut out = vec![SortGroup {
        id: LIBRARY,
        label: "Library",
    }];
    out.extend(rules::GROUPS.iter().map(|g| SortGroup {
        id: g.id,
        label: g.label,
    }));
    out
}

fn default_tabs() -> Vec<String> {
    DEFAULT_TABS.iter().map(|s| s.to_string()).collect()
}

fn default_filters() -> Vec<String> {
    DEFAULT_FILTERS.iter().map(|s| s.to_string()).collect()
}

fn default_cover_size() -> u32 {
    DEFAULT_COVER_SIZE
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppearanceSetup {
    /// The top bar's sort menu, in order.
    #[serde(default = "default_tabs")]
    pub sort_tabs: Vec<String>,
    /// Tag kinds the sidebar filters albums by, in its dropdown's order.
    #[serde(default = "default_filters")]
    pub filter_tags: Vec<String>,
    /// How wide album grid covers are at least, in CSS pixels; each row's
    /// covers stretch to fill it. Phones show them a fifth smaller.
    #[serde(default = "default_cover_size")]
    pub cover_size: u32,
}

impl Default for AppearanceSetup {
    fn default() -> Self {
        Self {
            sort_tabs: default_tabs(),
            filter_tags: default_filters(),
            cover_size: DEFAULT_COVER_SIZE,
        }
    }
}

impl AppearanceSetup {
    /// Keep known keys once, in the order given, and the cover size in range.
    pub fn normalize(mut self) -> Result<Self> {
        self.cover_size = self.cover_size.clamp(MIN_COVER_SIZE, MAX_COVER_SIZE);
        if self.sort_tabs.is_empty() {
            return Err(Error::Invalid("Keep at least one sort.".into()));
        }
        // These were sorts before they were filters; moved in their order.
        self.sort_tabs.retain(|t| match t.as_str() {
            ATTENTION | rules::COLLECTION => {
                self.filter_tags.push(t.clone());
                false
            }
            _ => true,
        });
        // Sorts that were only those leave the default ones.
        if self.sort_tabs.is_empty() {
            self.sort_tabs = default_tabs();
        }
        let known = sortable();
        if let Some(bad) = self
            .sort_tabs
            .iter()
            .find(|t| !known.iter().any(|s| s.id == *t))
        {
            return Err(Error::Invalid(format!("Can't sort by '{bad}'.")));
        }
        let mut seen = HashSet::new();
        self.sort_tabs.retain(|t| seen.insert(t.clone()));
        if self.sort_tabs.len() > MAX_TABS {
            return Err(Error::Invalid(format!(
                "Up to {MAX_TABS} sorts fit in the sort menu."
            )));
        }
        let filterable = filterable();
        if let Some(bad) = self
            .filter_tags
            .iter()
            .find(|t| !filterable.iter().any(|s| s.id == *t))
        {
            return Err(Error::Invalid(format!("'{bad}' can't be a tag filter")));
        }
        let mut seen = HashSet::new();
        self.filter_tags.retain(|t| seen.insert(t.clone()));
        if self.filter_tags.len() > MAX_FILTERS {
            return Err(Error::Invalid(format!(
                "Up to {MAX_FILTERS} filters fit in the sidebar."
            )));
        }
        Ok(self)
    }

    /// Sort keys whose values the album list must carry.
    pub fn extra_sort_keys(&self) -> Vec<&str> {
        self.sort_tabs
            .iter()
            .map(String::as_str)
            .filter(|t| !BUILT_IN.contains(t))
            .collect()
    }

    /// Tag kinds whose values the album list must carry for the sidebar's filter.
    pub fn filter_keys(&self) -> Vec<&str> {
        self.filter_tags.iter().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_validation() {
        let d = AppearanceSetup::default();
        assert_eq!(d.sort_tabs, DEFAULT_TABS);
        assert!(d.extra_sort_keys().is_empty());
        let tabs = |t: &[&str]| AppearanceSetup {
            sort_tabs: t.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let s = tabs(&["label", "date", "label"]).normalize().unwrap();
        assert_eq!(s.sort_tabs, ["label", "date"]);
        assert_eq!(s.extra_sort_keys(), ["label"]);
        assert!(tabs(&[]).normalize().is_err());
        assert!(tabs(&["rating"]).normalize().is_err());
        assert!(tabs(&["nope"]).normalize().is_err());
        // Needs attention and Collection, sorts once, move to the filters
        // (kept once, after those there).
        let moved = AppearanceSetup {
            filter_tags: vec!["genre".into(), "collection".into()],
            ..tabs(&["attention", "label", "collection"])
        }
        .normalize()
        .unwrap();
        assert_eq!(moved.sort_tabs, ["label"]);
        assert_eq!(moved.filter_tags, ["genre", "collection", "attention"]);
        // In their order, sorts that were only those leave the default ones,
        // a full list of filters takes them, and the rest is kept.
        let full: Vec<String> = filterable()
            .iter()
            .filter(|f| f.group != LIBRARY)
            .take(MAX_FILTERS - 2)
            .map(|f| f.id.to_string())
            .collect();
        let only = AppearanceSetup {
            filter_tags: full.clone(),
            cover_size: 240,
            ..tabs(&["collection", "attention"])
        }
        .normalize()
        .unwrap();
        assert_eq!(only.sort_tabs, DEFAULT_TABS);
        assert_eq!(only.filter_tags[..full.len()], full);
        assert_eq!(only.filter_tags[full.len()..], ["collection", "attention"]);
        assert_eq!(only.cover_size, 240);
        assert_eq!(only.clone().normalize().unwrap(), only);
        let parsed: AppearanceSetup = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed, d);
    }

    #[test]
    fn filter_tags_are_validated() {
        let d = AppearanceSetup::default();
        assert_eq!(d.filter_tags, DEFAULT_FILTERS);
        assert_eq!(d.filter_keys(), DEFAULT_FILTERS);
        let filters = |t: &[&str]| AppearanceSetup {
            filter_tags: t.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let s = filters(&["label", "genre", "label"]).normalize().unwrap();
        assert_eq!(s.filter_tags, ["label", "genre"]);
        // The sidebar can go without filters, unlike the sort menu without sorts.
        assert!(filters(&[]).normalize().unwrap().filter_tags.is_empty());
        for bad in ["added", "lyrics", "rating", "nope"] {
            assert!(filters(&[bad]).normalize().is_err(), "{bad}");
        }
        // Needs attention and Collection filter, with the Library's own, and don't sort.
        let own: Vec<_> = filterable()
            .into_iter()
            .filter(|f| f.group == LIBRARY)
            .map(|f| f.id)
            .collect();
        assert_eq!(own, [ATTENTION, "collection"]);
        assert!(!sortable().iter().any(|s| own.contains(&s.id)));
        assert!(
            filterable()
                .iter()
                .all(|f| own.contains(&f.id) || sortable().iter().any(|s| s.id == f.id))
        );
        // Settings saved before tag filters existed get the defaults.
        let old: AppearanceSetup = serde_json::from_str(r#"{"sort_tabs":["genre"]}"#).unwrap();
        assert_eq!(old.filter_tags, DEFAULT_FILTERS);
    }

    #[test]
    fn cover_sizes_stay_in_range() {
        let size = |cover_size| {
            AppearanceSetup {
                cover_size,
                ..Default::default()
            }
            .normalize()
            .unwrap()
            .cover_size
        };
        assert_eq!(AppearanceSetup::default().cover_size, DEFAULT_COVER_SIZE);
        assert_eq!(size(240), 240);
        assert_eq!(size(10), MIN_COVER_SIZE);
        assert_eq!(size(4000), MAX_COVER_SIZE);
        // Settings saved before cover sizes existed keep their tabs.
        let old: AppearanceSetup = serde_json::from_str(r#"{"sort_tabs":["genre"]}"#).unwrap();
        assert_eq!(
            (old.sort_tabs[0].as_str(), old.cover_size),
            ("genre", DEFAULT_COVER_SIZE)
        );
    }
}
