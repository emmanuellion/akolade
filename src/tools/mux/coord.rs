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
    /// Opened in the "À plat" view (else "Par projet"): each view has its own sessions.
    #[serde(default)]
    pub flat: bool,
    /// Its own look (true: as in the terminal); None: the multiplexer's.
    #[serde(default)]
    pub terminal: Option<bool>,
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

/// A message between sessions (or from you: `from` is "vous").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: u64,
    pub from: String,
    /// Who it was for, as written: a session's name, "projet" or "tous".
    pub to: String,
    pub text: String,
    pub at: u64,
    /// The sessions that have not read it yet.
    pub unread: Vec<String>,
    /// The sender's project (the "Échanges" filter).
    pub project: Option<PathBuf>,
}

/// Something the coordination did, for the "Échanges" panel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub at: u64,
    pub project: Option<PathBuf>,
    pub text: String,
}

/// A session's branch waiting to be merged into the project's `integration`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeRequest {
    pub session: String,
    pub branch: String,
    pub summary: String,
    pub at: u64,
}

/// The last merge tried: what happened and what the tests said.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeOutcome {
    pub session: String,
    /// Empty: a copy brought back into the project's folder.
    pub branch: String,
    pub merged: bool,
    pub report: String,
    pub at: u64,
    /// What it said of its work, and the files it brought: why the project changed, for a later conflict.
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectState {
    pub leases: Vec<Lease>,
    /// First in, first merged.
    pub queue: Vec<MergeRequest>,
    /// The one being merged and tested now.
    pub merging: Option<MergeRequest>,
    pub outcomes: Vec<MergeOutcome>,
}

/// Who a message goes to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scope {
    Project,
    All,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Coordinator {
    pub sessions: Vec<SessionInfo>,
    pub messages: Vec<Message>,
    pub notes: Vec<Note>,
    pub next_message: u64,
    /// Writes refused on a held file: (project key, path, session). Told when the file is free.
    pub waiting: Vec<(String, String, String)>,
    /// Per project key (`project::key`): its leases.
    #[serde(skip)]
    pub projects: BTreeMap<String, ProjectState>,
}

// ---------------------------------------------------------------- saved

impl Coordinator {
    /// `dir/multiplexeur.json`: sessions and messages; `dir/multiplexeur/<project>.json`: each project's leases and queue.
    pub fn save(&self, dir: &Path) {
        let folder = dir.join("multiplexeur");
        let _ = std::fs::create_dir_all(&folder);
        if let Ok(text) = serde_json::to_string(self) {
            let _ = crate::workspace::write_file(&dir.join("multiplexeur.json"), &text);
        }
        for (key, state) in &self.projects {
            let path = folder.join(format!("{key}.json"));
            if *state == ProjectState::default() {
                let _ = std::fs::remove_file(&path);
            } else if let Ok(text) = serde_json::to_string(state) {
                let _ = crate::workspace::write_file(&path, &text);
            }
        }
    }

    /// As saved; a merge cut short by a quit goes back to the front of its queue.
    pub fn load(dir: &Path) -> Self {
        let mut c: Coordinator = crate::settings::read_json(&dir.join("multiplexeur.json"));
        for entry in std::fs::read_dir(dir.join("multiplexeur")).into_iter().flatten().flatten() {
            let path = entry.path();
            let Some(key) = path.file_stem().map(|k| k.to_string_lossy().into_owned()).filter(|_| path.extension().is_some_and(|e| e == "json")) else { continue };
            let mut state: ProjectState = crate::settings::read_json(&path);
            if let Some(cut) = state.merging.take() {
                state.queue.insert(0, cut);
            }
            c.projects.insert(key, state);
        }
        c
    }
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

    pub fn name_of(&self, id: &str) -> String {
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
            let gone: Vec<String> = p.leases.iter().filter(|l| l.owner == id).map(|l| l.path.clone()).collect();
            p.leases.retain(|l| l.owner != id);
            p.queue.retain(|r| r.session != id);
            self.waiting.retain(|(_, _, s)| s != id);
            self.freed(&key, &gone, now());
        }
        self.sessions.retain(|s| s.id != id);
    }

    /// The project's leases still running at `now`.
    pub fn leases(&mut self, project: &Path, now: u64) -> Vec<Lease> {
        let key = super::project::key(project);
        self.expire(&key, now);
        self.projects.entry(key).or_default().leases.clone()
    }

    /// Leases past their time go (their files freed).
    fn expire(&mut self, key: &str, now: u64) {
        let p = self.projects.entry(key.to_string()).or_default();
        let gone: Vec<String> = p.leases.iter().filter(|l| l.until <= now).map(|l| l.path.clone()).collect();
        p.leases.retain(|l| l.until > now);
        self.freed(key, &gone, now);
    }

    /// Files free again: the sessions refused on them are told (an Akolade message), to try again.
    fn freed(&mut self, key: &str, paths: &[String], now: u64) {
        let (told, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiting).into_iter().partition(|(k, p, _)| k == key && paths.contains(p));
        self.waiting = kept;
        for (_, path, session) in told {
            if self.session(&session).is_some() {
                let name = self.name_of(&session);
                self.post("akolade", name, vec![session], &format!("{path} est libre : tu peux reprendre la modification qui avait été refusée."), now);
            }
        }
    }

    /// A write by `id` to `path` (absolute, or relative to its checkout): granted, and
    /// the lease taken or renewed, unless another session of its project holds the file.
    pub fn check_write(&mut self, id: &str, path: &str, now: u64) -> WriteCheck {
        let Some(me) = self.session(id).cloned() else { return WriteCheck::Outside };
        let Some(rel) = normalize(&me.checkout.to_string_lossy(), path) else { return WriteCheck::Outside };
        let key = super::project::key(&me.project);
        self.expire(&key, now);
        let held = self.projects.entry(key.clone()).or_default().leases.iter().find(|l| l.path == rel && l.owner != id).cloned();
        if let Some(l) = held {
            let name = self.name_of(&l.owner);
            if !self.waiting.iter().any(|(k, p, s)| *k == key && *p == rel && s == id) {
                self.waiting.push((key.clone(), rel.clone(), id.to_string()));
                self.note(Some(&me.project), format!("{} refusée sur {rel}, réservé par {name}", me.name), now);
            }
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
    pub fn release(&mut self, id: &str, paths: &[String], now: u64) -> usize {
        let Some(me) = self.session(id).cloned() else { return 0 };
        let rels: Vec<String> = paths.iter().filter_map(|p| normalize(&me.checkout.to_string_lossy(), p)).collect();
        let key = super::project::key(&me.project);
        let Some(p) = self.projects.get_mut(&key) else { return 0 };
        let gone: Vec<String> = p.leases.iter().filter(|l| l.owner == id && (rels.is_empty() || rels.contains(&l.path))).map(|l| l.path.clone()).collect();
        p.leases.retain(|l| !gone.contains(&l.path));
        self.freed(&key, &gone, now);
        gone.len()
    }

    /// A line in the "Échanges" panel.
    pub fn note(&mut self, project: Option<&Path>, text: String, now: u64) {
        self.notes.push(Note { at: now, project: project.map(Path::to_path_buf), text });
        // shortcut: the 500 last only, the panel's reach; keep a file per day if a history is wanted.
        if self.notes.len() > 500 {
            self.notes.remove(0);
        }
    }

    /// A session by its id or its name (names first in `from`'s project: two projects may both have an "api").
    fn resolve(&self, from: Option<&str>, to: &str) -> Result<String, String> {
        if self.session(to).is_some() {
            return Ok(to.to_string());
        }
        let mine = from.and_then(|f| self.session(f)).map(|s| s.project.clone());
        let named: Vec<&SessionInfo> = self.sessions.iter().filter(|s| s.name.eq_ignore_ascii_case(to)).collect();
        let local: Vec<&SessionInfo> = named.iter().copied().filter(|s| Some(&s.project) == mine.as_ref()).collect();
        match (local.as_slice(), named.as_slice()) {
            ([one], _) | ([], [one]) => Ok(one.id.clone()),
            ([], []) => Err(format!("Aucune session « {to} ». Celles qui existent : {}.", self.sessions.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "))),
            _ => Err(format!("Plusieurs sessions s'appellent « {to} » : donnez son identifiant ({}).", named.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", "))),
        }
    }

    fn post(&mut self, from: &str, to: String, recipients: Vec<String>, text: &str, now: u64) -> u64 {
        self.next_message += 1;
        let project = self.session(from).map(|s| s.project.clone());
        self.messages.push(Message { id: self.next_message, from: from.to_string(), to, text: text.to_string(), at: now, unread: recipients, project });
        // shortcut: the 1000 last only; enough for a working day of a few sessions.
        if self.messages.len() > 1000 {
            self.messages.remove(0);
        }
        self.next_message
    }

    /// `send_message`: to one session (by id or name), of any project. `from` may be "vous".
    pub fn send(&mut self, from: &str, to: &str, text: &str, now: u64) -> Result<String, String> {
        let id = self.resolve(Some(from), to)?;
        if id == from {
            return Err("C'est vous-même : écrivez à une autre session.".into());
        }
        let name = self.name_of(&id);
        self.post(from, name.clone(), vec![id], text, now);
        Ok(name)
    }

    /// `broadcast`: to the other sessions of `from`'s project, or to all.
    pub fn broadcast(&mut self, from: &str, scope: Scope, text: &str, now: u64) -> Vec<String> {
        let recipients: Vec<String> = match scope {
            Scope::Project => self.peers(from).iter().map(|s| s.id.clone()).collect(),
            Scope::All => self.sessions.iter().filter(|s| s.id != from).map(|s| s.id.clone()).collect(),
        };
        let names = recipients.iter().map(|id| self.name_of(id)).collect();
        self.post(from, if scope == Scope::All { "tous".into() } else { "projet".into() }, recipients, text, now);
        names
    }

    /// From you (the multiplexer's box): to a session, the sessions of a project, or all.
    pub fn send_from_user(&mut self, recipients: Vec<String>, to: String, text: &str, now: u64) {
        self.post("vous", to, recipients, text, now);
    }

    /// `read_inbox`: the messages `id` has not read, now read.
    pub fn inbox(&mut self, id: &str) -> Vec<Message> {
        let mut out = vec![];
        for m in self.messages.iter_mut().filter(|m| m.unread.iter().any(|u| u == id)) {
            m.unread.retain(|u| u != id);
            out.push(m.clone());
        }
        out
    }

    /// A message as a session reads it: "[Message de front · projet api] …".
    pub fn render(&self, m: &Message) -> String {
        let from = match self.session(&m.from) {
            Some(s) => format!("{} · projet {}", s.name, s.project.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            None if m.from == "vous" => "l'utilisateur, depuis le multiplexeur".into(),
            None if m.from == "akolade" => "Akolade".into(),
            None => m.from.clone(),
        };
        format!("[Message de {from}] {}", m.text)
    }

    /// `request_merge`: the session's branch joins its project's queue (once).
    pub fn request_merge(&mut self, id: &str, summary: &str, now: u64) -> Result<usize, String> {
        let me = self.session(id).cloned().ok_or("Session inconnue.")?;
        // A copy: no branch, brought back into the folder (an empty branch says so).
        let branch = me.branch.clone().or((me.mode == WorkMode::Copy).then(String::new)).ok_or("Cette session travaille directement dans le projet (mode Directe) : il n'y a rien à fusionner, ses modifications y sont déjà.")?;
        let p = self.projects.entry(super::project::key(&me.project)).or_default();
        p.queue.retain(|r| r.session != id);
        p.queue.push(MergeRequest { session: id.to_string(), branch, summary: summary.to_string(), at: now });
        let pos = p.queue.len() + p.merging.is_some() as usize;
        let what = if me.mode == WorkMode::Copy { "la réintégration de sa copie" } else { "la fusion de sa branche" };
        self.note(Some(&me.project), format!("{} demande {what} : {summary}", me.name), now);
        Ok(pos)
    }

    /// The next branch to merge, of a project whose queue is idle: (project, request, session's name).
    pub fn next_merge(&mut self) -> Option<(PathBuf, MergeRequest, String)> {
        let projects: Vec<PathBuf> = self.sessions.iter().map(|s| s.project.clone()).collect();
        for project in projects {
            let p = self.projects.entry(super::project::key(&project)).or_default();
            if p.merging.is_none() && !p.queue.is_empty() {
                let req = p.queue.remove(0);
                p.merging = Some(req.clone());
                let name = self.name_of(&req.session);
                return Some((project, req, name));
            }
        }
        None
    }

    /// The merge being done ended: kept in the project's history, told to the session (an
    /// Akolade message: it reads the error and fixes its branch), noted in the panel.
    pub fn finish_merge(&mut self, project: &Path, merged: bool, report: String, files: Vec<String>, now: u64) {
        let Some(req) = self.projects.get_mut(&super::project::key(project)).and_then(|p| p.merging.take()) else { return };
        let name = self.name_of(&req.session);
        let p = self.projects.entry(super::project::key(project)).or_default();
        p.outcomes.push(MergeOutcome { session: req.session.clone(), branch: req.branch.clone(), merged, report: report.clone(), at: now, summary: req.summary.clone(), files });
        if p.outcomes.len() > 20 {
            p.outcomes.remove(0);
        }
        let copy = req.branch.is_empty();
        let text = match (copy, merged) {
            (true, true) => format!("Copie de {name} réintégrée dans le dossier du projet"),
            (true, false) => format!("Copie de {name} non réintégrée"),
            (false, true) => format!("Branche {} de {name} fusionnée dans integration", req.branch),
            (false, false) => format!("Branche {} de {name} renvoyée (build, tests ou conflit)", req.branch),
        };
        self.note(Some(project), text, now);
        if self.session(&req.session).is_some() {
            self.post("akolade", name, vec![req.session], &format!("{} — {report}", if copy { "Réintégration" } else { "File d'intégration" }), now);
        }
    }

    /// From the multiplexer: a lease ends whoever holds it.
    pub fn force_release(&mut self, project: &Path, path: &str, now: u64) {
        let key = super::project::key(project);
        if let Some(p) = self.projects.get_mut(&key) {
            p.leases.retain(|l| l.path != path);
        }
        self.freed(&key, &[path.to_string()], now);
    }
}

// ---------------------------------------------------------------- the shared one

static HUB: std::sync::LazyLock<std::sync::Mutex<Coordinator>> = std::sync::LazyLock::new(Default::default);
static WATCHERS: std::sync::Mutex<Vec<futures::channel::mpsc::UnboundedSender<()>>> = std::sync::Mutex::new(Vec::new());

/// The coordinator of this Akolade (the multiplexer's and the MCP server's).
pub fn with<R>(f: impl FnOnce(&mut Coordinator) -> R) -> R {
    f(&mut HUB.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Something changed: saved, and the multiplexer redraws (from any thread).
pub fn changed() {
    #[cfg(not(test))]
    with(|c| c.save(&crate::settings::app_dir()));
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
            flat: false,
            terminal: None,
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
        // Released: b, refused on it, is told it may try again (once).
        c.release("a", &["src/lib.rs".into()], 111);
        assert_eq!(c.inbox("b").len(), 1);
        assert_eq!(c.check_write("a", "src/lib.rs", 100), WriteCheck::Granted);
        assert!(c.notes.iter().any(|n| n.text == "b-nom refusée sur src/lib.rs, réservé par a-nom"));
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
        assert_eq!(c.release("a", &["src/api.rs".into()], 2), 1);
        assert_eq!(c.check_write("b", "src/api.rs", 2), WriteCheck::Granted, "released: free");
        c.force_release(Path::new("/p"), "src/api.rs", 2);
        assert_eq!(c.check_write("a", "src/api.rs", 3), WriteCheck::Granted, "forced free");
        // A session that leaves frees all it held, and the ones refused on its files are told.
        assert!(matches!(c.check_write("b", "src/db.rs", 4), WriteCheck::Held { .. }));
        c.inbox("b");
        c.remove("a");
        assert!(c.inbox("b")[0].text.contains("src/db.rs est libre"), "told it may try again");
        assert_eq!(c.check_write("b", "src/db.rs", 4), WriteCheck::Granted);
    }

    #[test]
    fn messages_go_to_a_session_a_project_or_all_and_are_read_once() {
        let mut c = Coordinator::default();
        c.add(info("a", "/x", "/wt/x/a"));
        c.add(info("b", "/x", "/wt/x/b"));
        c.add(info("y", "/y", "/y"));
        // By name, across projects.
        assert_eq!(c.send("a", "y-nom", "L'API change : /users renvoie un tableau.", 1), Ok("y-nom".into()));
        assert!(c.send("a", "a", "moi", 1).is_err());
        assert!(c.send("a", "personne", "?", 1).unwrap_err().contains("Aucune session"));
        // Broadcast: the project by default, everyone on demand.
        assert_eq!(c.broadcast("a", Scope::Project, "Je prends src/api.rs", 2), ["b-nom"]);
        assert_eq!(c.broadcast("b", Scope::All, "Pause café", 3).len(), 2);
        let y = c.inbox("y");
        assert_eq!(y.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["L'API change : /users renvoie un tableau.", "Pause café"]);
        assert!(c.render(&y[0]).starts_with("[Message de a-nom · projet x]"));
        assert!(c.inbox("y").is_empty(), "read once");
        assert_eq!(c.inbox("b").len(), 1);
        assert_eq!(c.inbox("a").len(), 1, "b's broadcast to all");
        // Same name in two projects: the sender's project wins.
        let mut c2 = c.clone();
        let mut twin = info("y2", "/x", "/wt/x/y2");
        twin.name = "y-nom".into();
        c2.add(twin);
        assert_eq!(c2.send("a", "y-nom", "?", 4), Ok("y-nom".into()));
        assert_eq!(c2.inbox("y2").len(), 1);
    }

    #[test]
    fn merge_requests_queue_in_order_per_project() {
        let mut c = Coordinator::default();
        let mut a = info("a", "/x", "/wt/x/a");
        a.branch = Some("session/a".into());
        let mut b = info("b", "/x", "/wt/x/b");
        b.branch = Some("session/b".into());
        let mut d = info("d", "/x", "/x");
        d.mode = WorkMode::Direct;
        let mut y = info("y", "/y", "/wt/y/y");
        y.branch = Some("session/y".into());
        for s in [a, b, d, y] {
            c.add(s);
        }
        assert_eq!(c.request_merge("a", "route /users", 1), Ok(1));
        assert_eq!(c.request_merge("b", "écran", 2), Ok(2));
        assert!(c.request_merge("d", "?", 3).unwrap_err().contains("Directe"));
        assert_eq!(c.request_merge("y", "autre projet", 4), Ok(1), "its own project's queue");
        assert_eq!(c.request_merge("a", "route /users v2", 5), Ok(2), "asked again: back of the line");
        let q: Vec<&str> = c.projects[&super::super::project::key(Path::new("/x"))].queue.iter().map(|r| r.session.as_str()).collect();
        assert_eq!(q, ["b", "a"]);
        // One at a time per project, first asked first; projects do not wait for each other.
        let (p1, r1, _) = c.next_merge().unwrap();
        let (p2, r2, _) = c.next_merge().unwrap();
        assert_eq!((p1.as_path(), r1.session.as_str(), p2.as_path(), r2.session.as_str()), (Path::new("/x"), "b", Path::new("/y"), "y"));
        assert!(c.next_merge().is_none(), "/x is busy with b");
        c.finish_merge(Path::new("/x"), false, "tests en échec".into(), vec![], 6);
        assert!(c.inbox("b")[0].text.contains("tests en échec"), "the session is told");
        assert_eq!(c.next_merge().unwrap().1.session, "a");
    }

    #[test]
    fn everything_comes_back_after_a_restart() {
        let dir = std::env::temp_dir().join(format!("akolade-mux-saved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = Coordinator::default();
        let mut a = info("a", "/x", "/wt/x/a");
        a.branch = Some("session/a".into());
        a.conversation = Some("conv-a".into());
        c.add(a);
        c.add(info("b", "/x", "/wt/x/b"));
        c.add(info("y", "/y", "/y"));
        c.claim("a", &["src/api.rs".into()], "route", 100);
        c.send("b", "y-nom", "hello", 100).unwrap();
        c.broadcast("a", Scope::Project, "je réserve api.rs", 100);
        c.request_merge("a", "route", 100).unwrap();
        c.next_merge();
        c.save(&dir);
        let mut back = Coordinator::load(&dir);
        assert_eq!(back.sessions, c.sessions);
        assert_eq!(back.messages, c.messages);
        assert_eq!(back.session("a").unwrap().conversation.as_deref(), Some("conv-a"));
        assert!(matches!(back.check_write("b", "src/api.rs", 200), WriteCheck::Held { .. }), "the lease is back");
        let x = &back.projects[&super::super::project::key(Path::new("/x"))];
        assert!(x.merging.is_none() && x.queue[0].session == "a", "a merge cut short waits again, first");
        assert_eq!(back.inbox("y").len(), 1);
        // Nothing left for a project: its file goes.
        c.force_release(Path::new("/x"), "src/api.rs", 0);
        c.projects.get_mut(&super::super::project::key(Path::new("/x"))).unwrap().merging = None;
        c.projects.get_mut(&super::super::project::key(Path::new("/x"))).unwrap().queue.clear();
        c.projects.get_mut(&super::super::project::key(Path::new("/x"))).unwrap().outcomes.clear();
        c.save(&dir);
        assert!(!dir.join("multiplexeur").join(format!("{}.json", super::super::project::key(Path::new("/x")))).exists());
        let _ = std::fs::remove_dir_all(&dir);
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
