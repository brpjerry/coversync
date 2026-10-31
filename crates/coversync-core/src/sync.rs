//! Sync settings. Syncing the Library to devices is planned; for now the
//! setting only shows or hides the sidebar's Sync section, where those
//! devices will be listed with their own settings.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncSetup {
    /// Sync is on: the sidebar shows its Sync section.
    pub enabled: bool,
}

impl Default for SyncSetup {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_unless_turned_off() {
        assert!(SyncSetup::default().enabled);
        let empty: SyncSetup = serde_json::from_str("{}").unwrap();
        assert!(empty.enabled);
        let off: SyncSetup = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
        assert!(!off.enabled);
    }
}
