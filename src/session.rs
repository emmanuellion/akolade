//! Each project's open tabs, reopened when Akolade starts again on it.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct Session {
    /// The files of the tabs, in their order.
    pub files: Vec<PathBuf>,
    pub active: Option<PathBuf>,
}

/// Tabs kept per project, at most.
const MAX_FILES: usize = 40;

fn file(dir: &Path) -> PathBuf {
    dir.join("session.json")
}

fn read_all(dir: &Path) -> HashMap<PathBuf, Session> {
    crate::settings::read_json(&file(dir))
}

/// The project's last tabs, those whose file still exists.
pub fn load(dir: &Path, root: &Path) -> Session {
    let mut s = read_all(dir).remove(root).unwrap_or_default();
    s.files.retain(|f| f.is_file());
    s.active = s.active.filter(|a| s.files.contains(a));
    s
}

/// Remembers the project's tabs (written only when they changed).
pub fn save(dir: &Path, root: &Path, mut session: Session) {
    session.files.truncate(MAX_FILES);
    let mut all = read_all(dir);
    if all.get(root) == Some(&session) {
        return;
    }
    // Folders deleted since (temporary ones…) would only make the file grow.
    all.retain(|r, _| r.is_dir());
    all.insert(root.to_path_buf(), session);
    let _ = fs::create_dir_all(dir);
    if let Ok(text) = serde_json::to_string_pretty(&all) {
        let _ = crate::workspace::write_file(&file(dir), &text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_come_back_per_project_without_deleted_files() {
        let dir = std::env::temp_dir().join(format!("akolade-session-{}", std::process::id()));
        let (a, b) = (dir.join("projet-a"), dir.join("projet-b"));
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("main.rs"), "").unwrap();
        fs::write(a.join("lib.rs"), "").unwrap();
        let session = Session { files: vec![a.join("main.rs"), a.join("lib.rs"), a.join("gone.rs")], active: Some(a.join("gone.rs")) };
        save(&dir, &a, session);
        save(&dir, &b, Session { files: vec![], active: None });
        let back = load(&dir, &a);
        assert_eq!(back.files, vec![a.join("main.rs"), a.join("lib.rs")], "a deleted file is not reopened");
        assert_eq!(back.active, None, "the active tab's file is gone");
        assert_eq!(load(&dir, &dir.join("unknown")), Session::default());
        // Hand-broken file: kept aside, not overwritten by the next save.
        fs::write(file(&dir), "{ oops").unwrap();
        save(&dir, &b, Session::default());
        assert_eq!(fs::read_to_string(dir.join("session.json.bak")).unwrap(), "{ oops");
        fs::remove_dir_all(dir).unwrap();
    }
}
