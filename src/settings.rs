//! User settings, stored as JSON in `%LOCALAPPDATA%\FindForge\settings.json`.

use crate::query::FilterDef;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    Dark,
    Light,
    System,
}

/// A saved search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    pub name: String,
    pub query: String,
    /// Name of the filter that was active, if any.
    #[serde(default)]
    pub filter: Option<String>,
}

/// A column of the results table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnSetting {
    /// `name`, `path`, `size`, `modified`, `created`, `type`.
    pub id: String,
    pub visible: bool,
    pub width: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,

    // ---- Search options -------------------------------------------------------------
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub regex: bool,
    /// When false (the default) `resume` also finds `résumé`.
    pub match_diacritics: bool,

    // ---- What is indexed ------------------------------------------------------------
    /// Index every fixed drive automatically.
    pub index_fixed_drives: bool,
    /// Also index removable drives (USB sticks, card readers).
    pub index_removable: bool,
    /// Drives left out even though the options above would include them (`D:`).
    pub skip_volumes: Vec<String>,
    /// Extra folders to index (network shares, folders on skipped drives).
    pub folders: Vec<String>,
    /// Show hidden / system files and folders in results.
    pub include_hidden: bool,
    pub include_system: bool,
    /// Folders left out of the index (full paths).
    pub exclude_folders: Vec<String>,
    /// File or folder names left out of the index (wildcards allowed).
    pub exclude_names: Vec<String>,
    /// Minutes between rescans of volumes that cannot be watched live.
    pub rescan_minutes: u32,

    // ---- Results --------------------------------------------------------------------
    pub columns: Vec<ColumnSetting>,
    /// Column id the results are sorted by.
    pub sort_key: String,
    pub sort_ascending: bool,
    pub preview: bool,
    pub confirm_delete: bool,

    // ---- History, bookmarks, filters ---------------------------------------------------
    pub keep_history: bool,
    pub history: Vec<String>,
    pub bookmarks: Vec<Bookmark>,
    pub user_filters: Vec<FilterDef>,

    // ---- Desktop integration -----------------------------------------------------------
    /// Global hotkey that brings the window to the front, e.g. `Ctrl+Alt+F`.
    pub hotkey: String,
    pub hotkey_enabled: bool,
    pub tray_icon: bool,
    /// Closing the window keeps FindForge running in the tray.
    pub close_to_tray: bool,
    /// Ask for administrator rights at every start (fast MFT indexing).
    pub always_admin: bool,
}

pub fn default_columns() -> Vec<ColumnSetting> {
    [
        ("name", true, 340.0),
        ("path", true, 380.0),
        ("size", true, 90.0),
        ("modified", true, 140.0),
        ("created", false, 140.0),
        ("type", true, 110.0),
    ]
    .into_iter()
    .map(|(id, visible, width)| ColumnSetting {
        id: id.to_string(),
        visible,
        width,
    })
    .collect()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            theme: ThemeChoice::Dark,
            match_case: false,
            whole_word: false,
            match_path: false,
            regex: false,
            match_diacritics: false,
            index_fixed_drives: true,
            index_removable: false,
            skip_volumes: Vec::new(),
            folders: Vec::new(),
            include_hidden: true,
            include_system: false,
            exclude_folders: Vec::new(),
            exclude_names: Vec::new(),
            rescan_minutes: 30,
            columns: default_columns(),
            sort_key: "name".into(),
            sort_ascending: true,
            preview: false,
            confirm_delete: true,
            keep_history: true,
            history: Vec::new(),
            bookmarks: Vec::new(),
            user_filters: Vec::new(),
            hotkey: "Ctrl+Alt+F".into(),
            hotkey_enabled: false,
            tray_icon: false,
            close_to_tray: false,
            always_admin: false,
        }
    }
}

pub const HISTORY_LIMIT: usize = 50;

impl Settings {
    pub fn load(dir: &Path) -> Settings {
        let mut s: Settings = std::fs::read_to_string(dir.join("settings.json"))
            .ok()
            // Tolerate a byte order mark (some editors add one).
            .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default();
        s.repair();
        s
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("settings.json"), json)
            .map_err(|e| format!("Could not save settings: {e}"))
    }

    /// Makes loaded settings usable: every known column present exactly once.
    pub fn repair(&mut self) {
        let defaults = default_columns();
        self.columns
            .retain(|c| defaults.iter().any(|d| d.id == c.id));
        let mut seen: Vec<String> = Vec::new();
        self.columns.retain(|c| {
            let fresh = !seen.contains(&c.id);
            seen.push(c.id.clone());
            fresh
        });
        for d in defaults {
            if !self.columns.iter().any(|c| c.id == d.id) {
                self.columns.push(d);
            }
        }
        for c in &mut self.columns {
            c.width = c.width.clamp(50.0, 1200.0);
            if c.id == "name" {
                c.visible = true;
            }
        }
        self.rescan_minutes = self.rescan_minutes.clamp(1, 24 * 60);
        self.history.truncate(HISTORY_LIMIT);
    }

    /// Remembers a search (most recent first, no duplicates).
    pub fn push_history(&mut self, query: &str) {
        let q = query.trim();
        if q.is_empty() || !self.keep_history {
            return;
        }
        self.history.retain(|h| h != q);
        self.history.insert(0, q.to_string());
        self.history.truncate(HISTORY_LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tolerant_loading() {
        let dir =
            std::env::temp_dir().join(format!("findforge-settings-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(Settings::load(&dir), Settings::default());
        let mut s = Settings {
            theme: ThemeChoice::Light,
            regex: true,
            exclude_names: vec!["*.tmp".into()],
            ..Default::default()
        };
        s.bookmarks.push(Bookmark {
            name: "Big videos".into(),
            query: "size:>1gb".into(),
            filter: Some("Video".into()),
        });
        s.save(&dir).unwrap();
        assert_eq!(Settings::load(&dir), s);
        // Partial files keep defaults for the rest; broken files fall back entirely.
        std::fs::write(
            dir.join("settings.json"),
            r#"{"theme":"System","future_option":true,"columns":[{"id":"size","visible":true,"width":5.0},{"id":"bogus","visible":true,"width":80.0}]}"#,
        )
        .unwrap();
        let loaded = Settings::load(&dir);
        assert_eq!(loaded.theme, ThemeChoice::System);
        assert!(loaded.index_fixed_drives);
        assert_eq!(loaded.columns.len(), default_columns().len());
        assert_eq!(loaded.columns[0].id, "size");
        assert_eq!(loaded.columns[0].width, 50.0);
        assert!(loaded.columns.iter().all(|c| c.id != "bogus"));
        std::fs::write(dir.join("settings.json"), "\u{feff}{\"theme\":\"Light\"}").unwrap();
        assert_eq!(Settings::load(&dir).theme, ThemeChoice::Light);
        std::fs::write(dir.join("settings.json"), "{{{").unwrap();
        assert_eq!(Settings::load(&dir), Settings::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_is_deduplicated_and_bounded() {
        let mut s = Settings::default();
        for i in 0..80 {
            s.push_history(&format!("q{i}"));
        }
        s.push_history("q70");
        s.push_history("   ");
        assert_eq!(s.history.len(), HISTORY_LIMIT);
        assert_eq!(s.history[0], "q70");
        assert_eq!(s.history.iter().filter(|h| *h == "q70").count(), 1);
        s.keep_history = false;
        s.push_history("secret");
        assert!(!s.history.contains(&"secret".to_string()));
    }
}
