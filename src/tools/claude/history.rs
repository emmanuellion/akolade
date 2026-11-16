//! Claude Code's usage per day, kept across sessions in Akolade's folder.

use std::{collections::BTreeMap, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use super::Usage;
use crate::settings::{app_dir, read_json};

/// Usage per day ("2026-11-15"), kept across sessions in Akolade's folder.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub days: BTreeMap<String, Day>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Day {
    pub usage: Usage,
    pub turns: u32,
    pub sessions: u32,
}

impl History {
    pub(super) fn path() -> PathBuf {
        app_dir().join("claude-usage.json")
    }

    pub(super) fn load() -> Self {
        read_json(&Self::path())
    }

    pub(super) fn save(&self) {
        let _ = fs::create_dir_all(app_dir());
        // Through a temporary file: a crash mid-write must not wipe the history.
        if let Ok(text) = serde_json::to_string(self) {
            let _ = crate::workspace::write_file(&Self::path(), &text);
        }
    }

    pub fn record(&mut self, day: &str, usage: &Usage, turns: u32, sessions: u32) {
        let d = self.days.entry(day.to_string()).or_default();
        d.usage.add(usage);
        d.turns += turns;
        d.sessions += sessions;
    }

    /// Everything from `first` (a day) on.
    pub fn since(&self, first: &str) -> Day {
        let mut total = Day::default();
        for d in self.days.range(first.to_string()..).map(|(_, d)| d) {
            total.usage.add(&d.usage);
            (total.turns, total.sessions) = (total.turns + d.turns, total.sessions + d.sessions);
        }
        total
    }
}

pub(super) fn day(days_ago: i64) -> String {
    (chrono::Local::now() - chrono::Duration::days(days_ago)).format("%Y-%m-%d").to_string()
}
