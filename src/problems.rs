//! Errors and warnings in the code, per file: from the language servers
//! (`publishDiagnostics`) and from the linters. Shared across threads: the
//! servers' readers write, the window reads when `version` moves.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    /// 0-based line and UTF-16 column, as LSP counts.
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub severity: Severity,
    pub message: String,
    /// Who says so: "basedpyright", "eslint"…
    pub source: String,
}

#[derive(Default)]
pub struct Problems {
    /// File → origin (a server's program, a linter) → its problems.
    files: Mutex<HashMap<PathBuf, HashMap<String, Vec<Problem>>>>,
    version: AtomicU64,
}

impl Problems {
    /// Replaces what `origin` says about `path`.
    pub fn set(&self, path: PathBuf, origin: &str, list: Vec<Problem>) {
        let mut files = self.files.lock().unwrap();
        // Checked before `entry`: an empty entry would linger ("a.py 0").
        if files.get(&path).and_then(|f| f.get(origin)).map_or(list.is_empty(), |old| *old == list) {
            return;
        }
        let file = files.entry(path.clone()).or_default();
        if list.is_empty() {
            file.remove(origin);
        } else {
            file.insert(origin.to_string(), list);
        }
        if file.is_empty() {
            files.remove(&path);
        }
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Replaces everything `origin` says (a linter of the whole project): files it
    /// no longer mentions are clean now.
    pub fn replace_origin(&self, origin: &str, found: HashMap<PathBuf, Vec<Problem>>) {
        self.clear_origin(origin);
        for (path, list) in found {
            self.set(path, origin, list);
        }
    }

    /// Forgets everything `origin` said (its server stopped).
    pub fn clear_origin(&self, origin: &str) {
        let mut files = self.files.lock().unwrap();
        let before = files.values().filter(|f| f.contains_key(origin)).count();
        files.values_mut().for_each(|f| {
            f.remove(origin);
        });
        files.retain(|_, f| !f.is_empty());
        if before > 0 {
            self.version.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Forgets a file (deleted, renamed), whoever said something about it.
    pub fn forget(&self, path: &std::path::Path) {
        if self.files.lock().unwrap().remove(path).is_some() {
            self.version.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Moves on every change: the window looks at it often, cheaply.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    /// One file's problems, top to bottom.
    pub fn of(&self, path: &std::path::Path) -> Vec<Problem> {
        let files = self.files.lock().unwrap();
        let mut out: Vec<Problem> = files.get(path).map(|f| f.values().flatten().cloned().collect()).unwrap_or_default();
        out.sort_by_key(|p| (p.start, p.severity));
        out
    }

    /// Every file with problems, by path.
    pub fn all(&self) -> Vec<(PathBuf, Vec<Problem>)> {
        let mut paths: Vec<PathBuf> = self.files.lock().unwrap().keys().cloned().collect();
        paths.sort();
        paths.into_iter().map(|p| (p.clone(), self.of(&p))).collect()
    }

    /// (errors, warnings) in the whole project.
    pub fn counts(&self) -> (usize, usize) {
        let files = self.files.lock().unwrap();
        let all = files.values().flat_map(|f| f.values().flatten());
        all.fold((0, 0), |(e, w), p| match p.severity {
            Severity::Error => (e + 1, w),
            Severity::Warning => (e, w + 1),
            Severity::Info => (e, w),
        })
    }
}

/// `file:///a%20b/c.py` → `/a b/c.py`.
pub fn path_of_uri(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    Some(PathBuf::from(percent_encoding::percent_decode_str(path).decode_utf8().ok()?.into_owned()))
}

/// LSP `textDocument/publishDiagnostics` params → the file and its problems.
/// Hints (severity 4: "unused", "could be simpler") are left out: noise.
pub fn from_lsp(params: &Value, origin: &str) -> Option<(PathBuf, Vec<Problem>)> {
    let path = path_of_uri(params["uri"].as_str()?)?;
    let pos = |v: &Value| (v["line"].as_u64().unwrap_or(0) as u32, v["character"].as_u64().unwrap_or(0) as u32);
    let list = params["diagnostics"]
        .as_array()?
        .iter()
        .filter_map(|d| {
            let severity = match d["severity"].as_u64().unwrap_or(1) {
                1 => Severity::Error,
                2 => Severity::Warning,
                3 => Severity::Info,
                _ => return None,
            };
            Some(Problem {
                start: pos(&d["range"]["start"]),
                end: pos(&d["range"]["end"]),
                severity,
                message: d["message"].as_str().unwrap_or_default().trim().to_string(),
                source: d["source"].as_str().unwrap_or(origin).to_string(),
            })
        })
        .collect();
    Some((path, list))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn problem(line: u32, severity: Severity) -> Problem {
        Problem { start: (line, 0), end: (line, 3), severity, message: "m".into(), source: "s".into() }
    }

    #[test]
    fn lsp_diagnostics_become_problems_without_hints() {
        let params = json!({ "uri": "file:///a%20b/c.py", "diagnostics": [
            { "range": { "start": { "line": 2, "character": 4 }, "end": { "line": 2, "character": 9 } }, "severity": 1, "message": "\"foo\" is not defined ", "source": "basedpyright" },
            { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, "severity": 4, "message": "unused" },
            { "range": { "start": { "line": 5, "character": 0 }, "end": { "line": 5, "character": 1 } }, "message": "no severity: an error" }
        ]});
        let (path, list) = from_lsp(&params, "basedpyright-langserver").unwrap();
        assert_eq!(path, PathBuf::from("/a b/c.py"));
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].start, list[0].end, list[0].message.as_str(), list[0].source.as_str()), ((2, 4), (2, 9), "\"foo\" is not defined", "basedpyright"));
        assert_eq!((list[1].severity, list[1].source.as_str()), (Severity::Error, "basedpyright-langserver"));
    }

    #[test]
    fn origins_add_up_and_versions_move_only_on_change() {
        let p = Problems::default();
        let file = PathBuf::from("/x.py");
        p.set(file.clone(), "server", vec![problem(3, Severity::Error)]);
        p.set(file.clone(), "ruff", vec![problem(1, Severity::Warning)]);
        let v = p.version();
        p.set(file.clone(), "ruff", vec![problem(1, Severity::Warning)]);
        assert_eq!(p.version(), v, "same list: nothing new");
        assert_eq!(p.of(&file).iter().map(|x| x.start.0).collect::<Vec<_>>(), [1, 3], "top to bottom");
        assert_eq!(p.counts(), (1, 1));
        p.clear_origin("server");
        assert_eq!(p.counts(), (0, 1));
        p.set(file.clone(), "ruff", vec![]);
        assert!(p.all().is_empty());
        p.set(PathBuf::from("/clean.py"), "ruff", vec![]);
        assert!(p.all().is_empty(), "nothing to say: no empty entry");
    }

    #[test]
    fn forgotten_files_leave_every_origin() {
        let p = Problems::default();
        let file = PathBuf::from("/x.py");
        p.set(file.clone(), "server", vec![problem(3, Severity::Error)]);
        p.set(file.clone(), "ruff", vec![problem(1, Severity::Warning)]);
        let v = p.version();
        p.forget(&file);
        assert!(p.all().is_empty() && p.version() > v);
        p.forget(&file);
        assert_eq!(p.version(), v + 1, "nothing left: no change");
    }
}
