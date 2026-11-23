//! The last things played (files, addresses, radio stations, YouTube and Spotify links),
//! newest first, kept across runs in `history.json` in playr's data folder.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub const LIMIT: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// What was opened, exactly as [`crate::player::Player::open`] got it: replaying it opens
    /// the same thing again.
    pub target: String,
    pub title: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    pub items: Vec<Item>,
}

impl History {
    /// Puts `target` on top. Playing something again moves it up rather than listing it twice,
    /// and only the newest [`LIMIT`] are kept.
    pub fn record(&mut self, target: &str, title: &str, at: u64) {
        let target = target.trim();
        if target.is_empty() {
            return;
        }
        self.items.retain(|i| i.target != target);
        let title = if title.trim().is_empty() { target } else { title.trim() };
        self.items.insert(0, Item { target: target.to_string(), title: title.to_string(), at });
        self.items.truncate(LIMIT);
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn to_json(&self) -> String {
        let items: Vec<Value> =
            self.items.iter().map(|i| json!({ "target": i.target, "title": i.title, "at": i.at })).collect();
        serde_json::to_string_pretty(&json!({ "items": items })).unwrap_or_default()
    }

    /// Reads [`to_json`](Self::to_json)'s output. Entries it cannot read are skipped, so a
    /// damaged file costs those entries, never the player's start.
    pub fn parse(text: &str) -> History {
        let mut history = History::default();
        let Ok(data) = serde_json::from_str::<Value>(text) else { return history };
        let Some(list) = data.get("items").and_then(Value::as_array) else { return history };
        for v in list {
            let Some(target) = v.get("target").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) else {
                continue;
            };
            if history.items.iter().any(|i| i.target == target) || history.items.len() == LIMIT {
                continue;
            }
            let title = v.get("title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).unwrap_or(target);
            let at = v.get("at").and_then(Value::as_u64).unwrap_or(0);
            history.items.push(Item { target: target.to_string(), title: title.to_string(), at });
        }
        history
    }

    pub fn load(path: &Path) -> History {
        std::fs::read_to_string(path).map(|t| History::parse(&t)).unwrap_or_default()
    }

    /// Writes through a temporary file, so a crash mid-write never leaves half a history.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let partial = path.with_extension("json.part");
        std::fs::write(&partial, self.to_json()).map_err(|e| format!("cannot save the history: {e}"))?;
        std::fs::rename(&partial, path).map_err(|e| format!("cannot save the history: {e}"))
    }
}

pub fn default_path() -> Option<PathBuf> {
    crate::youtube::data_dir().map(|d| d.join("history.json"))
}
