//! The coordinator: who works where (the registry), which files each session holds
//! (leases, per project), the messages between sessions, and each project's
//! integration queue. Plain data and rules, no UI: the multiplexer and the MCP
//! server share one, and it is saved as JSON in Akolade's folder.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::project::WorkMode;

/// How long a lease lasts without a write by its owner (renewed at each one).
pub const LEASE_SECS: u64 = 15 * 60;

/// A session as the coordinator knows it (kept across restarts).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub role: String,
    /// Its project (a repository's root, or a folder without git).
    pub project: PathBuf,
    /// Where it works: the project's folder, or its worktree.
    pub cwd: PathBuf,
    /// The root its paths are relative to: the project, or the worktree's root.
    pub checkout: PathBuf,
    pub mode: WorkMode,
    pub branch: Option<String>,
    /// Claude Code's conversation, to resume it.
    pub conversation: Option<String>,
    /// Its form's choices (indexes of MODELS, MODES, EFFORTS) and budget, to start it again as it was.
    #[serde(default)]
    pub model: usize,
    #[serde(default)]
    pub perm: usize,
    #[serde(default)]
    pub effort: usize,
    #[serde(default)]
    pub budget: Option<f64>,
}

/// A file a session holds: no other session of its project may write it until released or expired.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    /// Relative to the repository's root, `/`-separated.
    pub path: String,
    pub owner: String,
    pub intent: String,
    /// Unix seconds.
    pub until: u64,
}

/// What a session's write meets.
#[derive(Debug, PartialEq)]
pub enum WriteCheck {
    /// Free (or its own): the lease is (re)taken.
    Granted,
    /// Another session of the project holds it.
    Held { owner: String, name: String, intent: String },
    /// Not under the session's checkout: no lease applies.
    Outside,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectState {
    pub leases: Vec<Lease>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Coordinator {
    pub sessions: Vec<SessionInfo>,
    /// Per project key (`project::key`): its leases.
    #[serde(skip)]
    pub projects: BTreeMap<String, ProjectState>,
}

/// `path` relative to `root`, `/`-separated, without `.` or `..` (None: outside `root`).
/// Windows paths too: `\` and `/` alike, drive letters and case not told apart.
pub fn normalize(root: &str, path: &str) -> Option<String> {
    let unify = |s: &str| s.replace('\\', "/");
    let drive = |s: &str| s.len() >= 2 && s.as_bytes()[1] == b':' && s.as_bytes()[0].is_ascii_alphabetic();
    let (root, path) = (unify(root), unify(path));
    let windows = drive(&root);
    let full = if path.starts_with('/') || drive(&path) { path } else { format!("{root}/{path}") };
    let mut parts: Vec<&str> = vec![];
    for seg in full.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    let base: Vec<&str> = root.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let same = |a: &str, b: &str| if windows { a.eq_ignore_ascii_case(b) } else { a == b };
    if parts.len() <= base.len() || !base.iter().zip(&parts).all(|(a, b)| same(a, b)) {
        return None;
    }
    let rel = parts[base.len()..].join("/");
    Some(if windows { rel.to_lowercase() } else { rel })
}

impl Coordinator {
    pub fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn name_of(&self, id: &str) -> String {
        self.session(id).map_or_else(|| id.to_string(), |s| s.name.clone())
    }

    fn key_of(&self, id: &str) -> Option<String> {
        self.session(id).map(|s| super::project::key(&s.project))
    }

    /// The other sessions of `id`'s project.
    pub fn peers(&self, id: &str) -> Vec<&SessionInfo> {
        let Some(me) = self.session(id) else { return vec![] };
        self.sessions.iter().filter(|s| s.id != id && s.project == me.project).collect()
    }

    pub fn add(&mut self, info: SessionInfo) {
        self.sessions.retain(|s| s.id != info.id);
        self.sessions.push(info);
    }

    /// The session leaves: its leases go with it.
    pub fn remove(&mut self, id: &str) {
        if let Some(key) = self.key_of(id)
            && let Some(p) = self.projects.get_mut(&key)
        {
            p.leases.retain(|l| l.owner != id);
        }
        self.sessions.retain(|s| s.id != id);
    }

    /// The project's leases still running at `now`.
    pub fn leases(&mut self, project: &Path, now: u64) -> Vec<Lease> {
        let p = self.projects.entry(super::project::key(project)).or_default();
        p.leases.retain(|l| l.until > now);
        p.leases.clone()
    }

    /// A write by `id` to `path` (absolute, or relative to its checkout): granted, and
    /// the lease taken or renewed, unless another session of its project holds the file.
    pub fn check_write(&mut self, id: &str, path: &str, now: u64) -> WriteCheck {
        let Some(me) = self.session(id).cloned() else { return WriteCheck::Outside };
        let Some(rel) = normalize(&me.checkout.to_string_lossy(), path) else { return WriteCheck::Outside };
        let key = super::project::key(&me.project);
        let held = {
            let p = self.projects.entry(key.clone()).or_default();
            p.leases.retain(|l| l.until > now);
            p.leases.iter().find(|l| l.path == rel && l.owner != id).cloned()
        };
        if let Some(l) = held {
            let name = self.name_of(&l.owner);
            return WriteCheck::Held { owner: l.owner, name, intent: l.intent };
        }
        let p = self.projects.entry(key).or_default();
        match p.leases.iter_mut().find(|l| l.path == rel) {
            Some(l) => l.until = now + LEASE_SECS,
            None => p.leases.push(Lease { path: rel, owner: id.to_string(), intent: "modification".into(), until: now + LEASE_SECS }),
        }
        WriteCheck::Granted
    }

    /// `claim_files`: the free files (or already its own) are taken with `intent`; the others are reported.
    pub fn claim(&mut self, id: &str, paths: &[String], intent: &str, now: u64) -> (Vec<String>, Vec<(String, String, String)>) {
        let Some(me) = self.session(id).cloned() else { return (vec![], vec![]) };
        let (mut taken, mut refused) = (vec![], vec![]);
        for path in paths {
            let Some(rel) = normalize(&me.checkout.to_string_lossy(), path) else {
                refused.push((path.clone(), String::new(), "hors du projet".into()));
                continue;
            };
            match self.check_write(id, &rel, now) {
                WriteCheck::Held { name, intent, .. } => refused.push((rel, name, intent)),
                _ => {
                    if let Some(l) = self.projects.get_mut(&super::project::key(&me.project)).and_then(|p| p.leases.iter_mut().find(|l| l.path == rel)) {
                        l.intent = intent.to_string();
                    }
                    taken.push(rel);
                }
            }
        }
        (taken, refused)
    }

    /// `release_files`: its own leases on `paths` (all of them if empty) go.
    pub fn release(&mut self, id: &str, paths: &[String]) -> usize {
        let Some(me) = self.session(id).cloned() else { return 0 };
        let rels: Vec<String> = paths.iter().filter_map(|p| normalize(&me.checkout.to_string_lossy(), p)).collect();
        let Some(p) = self.projects.get_mut(&super::project::key(&me.project)) else { return 0 };
        let before = p.leases.len();
        p.leases.retain(|l| !(l.owner == id && (rels.is_empty() || rels.contains(&l.path))));
        before - p.leases.len()
    }

    /// From the multiplexer: a lease ends whoever holds it.
    pub fn force_release(&mut self, project: &Path, path: &str) {
        if let Some(p) = self.projects.get_mut(&super::project::key(project)) {
            p.leases.retain(|l| l.path != path);
        }
    }
}

// ---------------------------------------------------------------- the shared one

static HUB: std::sync::LazyLock<std::sync::Mutex<Coordinator>> = std::sync::LazyLock::new(Default::default);
static WATCHERS: std::sync::Mutex<Vec<futures::channel::mpsc::UnboundedSender<()>>> = std::sync::Mutex::new(Vec::new());

/// The coordinator of this Akolade (the multiplexer's and the MCP server's).
pub fn with<R>(f: impl FnOnce(&mut Coordinator) -> R) -> R {
    f(&mut HUB.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Something changed: the multiplexer redraws (from any thread).
pub fn changed() {
    WATCHERS.lock().unwrap_or_else(|e| e.into_inner()).retain(|tx| tx.unbounded_send(()).is_ok());
}

/// Told of each change (`changed`).
pub fn watch() -> futures::channel::mpsc::UnboundedReceiver<()> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    WATCHERS.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
    rx
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, project: &str, checkout: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: format!("{id}-nom"),
            role: String::new(),
            project: project.into(),
            cwd: checkout.into(),
            checkout: checkout.into(),
            mode: WorkMode::Isolated,
            branch: None,
            conversation: None,
            model: 0,
            perm: 0,
            effort: 0,
            budget: None,
        }
    }

    #[test]
    fn paths_are_normalized_on_every_system() {
        assert_eq!(normalize("/w/a", "/w/a/src/./lib.rs").as_deref(), Some("src/lib.rs"));
        assert_eq!(normalize("/w/a", "src/../README.md").as_deref(), Some("README.md"));
        assert_eq!(normalize("/w/a", "/w/b/x.rs"), None, "another checkout");
        assert_eq!(normalize("/w/a", "/w/a/../b/x.rs"), None, "`..` leaves the root");
        assert_eq!(normalize("/w/a", "/w/a"), None, "the root itself is no file");
        assert_eq!(normalize("C:\\Work\\Api", "c:/work/api/Src\\Main.rs").as_deref(), Some("src/main.rs"));
        assert_eq!(normalize("C:\\Work\\Api", "D:\\Work\\Api\\x.rs"), None);
    }

    #[test]
    fn leases_are_taken_refused_renewed_and_expire() {
        let mut c = Coordinator::default();
        c.add(info("a", "/p", "/wt/p/a"));
        c.add(info("b", "/p", "/wt/p/b"));
        // Each in its worktree, the same file of the project.
        assert_eq!(c.check_write("a", "/wt/p/a/src/lib.rs", 100), WriteCheck::Granted);
        assert_eq!(c.check_write("b", "/wt/p/b/src/lib.rs", 110), WriteCheck::Held { owner: "a".into(), name: "a-nom".into(), intent: "modification".into() });
        assert_eq!(c.check_write("b", "/wt/p/b/src/other.rs", 110), WriteCheck::Granted);
        assert_eq!(c.check_write("a", "/elsewhere/x", 110), WriteCheck::Outside);
        // Renewed by its owner's writes, then expired.
        assert_eq!(c.check_write("a", "src/lib.rs", 100 + LEASE_SECS - 1), WriteCheck::Granted);
        assert!(matches!(c.check_write("b", "src/lib.rs", 100 + LEASE_SECS + 10), WriteCheck::Held { .. }), "renewed");
        assert_eq!(c.check_write("b", "src/lib.rs", 100 + 2 * LEASE_SECS), WriteCheck::Granted, "expired: free");
        assert_eq!(c.leases(Path::new("/p"), 100 + 2 * LEASE_SECS).iter().filter(|l| l.path == "src/lib.rs").map(|l| l.owner.as_str()).collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn claims_releases_and_forced_releases() {
        let mut c = Coordinator::default();
        c.add(info("a", "/p", "/wt/p/a"));
        c.add(info("b", "/p", "/wt/p/b"));
        let (taken, refused) = c.claim("a", &["src/api.rs".into(), "/wt/p/a/src/db.rs".into()], "nouvelle route", 0);
        assert_eq!(taken, ["src/api.rs", "src/db.rs"]);
        assert!(refused.is_empty());
        let (taken, refused) = c.claim("b", &["src/api.rs".into(), "src/ui.rs".into(), "/etc/passwd".into()], "écran", 1);
        assert_eq!(taken, ["src/ui.rs"]);
        assert_eq!(refused[0], ("src/api.rs".into(), "a-nom".into(), "nouvelle route".into()));
        assert_eq!(refused[1].2, "hors du projet");
        assert_eq!(c.release("a", &["src/api.rs".into()]), 1);
        assert_eq!(c.check_write("b", "src/api.rs", 2), WriteCheck::Granted, "released: free");
        c.force_release(Path::new("/p"), "src/api.rs");
        assert_eq!(c.check_write("a", "src/api.rs", 3), WriteCheck::Granted, "forced free");
        // A session that leaves frees all it held.
        c.remove("a");
        assert_eq!(c.check_write("b", "src/db.rs", 4), WriteCheck::Granted);
    }

    #[test]
    fn projects_never_share_leases() {
        let mut c = Coordinator::default();
        c.add(info("x1", "/x", "/x"));
        c.add(info("y1", "/y", "/y"));
        assert_eq!(c.check_write("x1", "/x/main.rs", 0), WriteCheck::Granted);
        assert_eq!(c.check_write("y1", "/y/main.rs", 0), WriteCheck::Granted, "same name, another project");
        assert_eq!(c.leases(Path::new("/y"), 0).len(), 1);
        assert!(c.peers("x1").is_empty());
    }
}
