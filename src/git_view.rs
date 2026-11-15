//! The "Git" tab: changes, diff, staging, commit, branches, history, push/pull.

use std::{
    collections::HashSet,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gpui::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div, hsla, prelude::FluentBuilder, uniform_list,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    popover::Popover,
    v_flex,
};

use crate::{
    git::{self, Change, Commit, DiffLine, LineKind, Status},
    icons,
    settings::AppSettings,
    ui::{empty_state, loading, section_title},
    workspace::{Workspace, button, r},
};

/// shortcut: rows past this are summed up, not listed; switch the lists to uniform_list if real projects need more.
const MAX_ROWS: usize = 500;
const HISTORY: usize = 200;

#[derive(Clone)]
struct Snapshot {
    status: Status,
    /// Has a remote to push to and pull from.
    remote: bool,
    branches: Vec<String>,
    log: Vec<Commit>,
    stashes: Vec<git::Stash>,
    /// A merge waits to be finished (commit) or aborted.
    merging: bool,
}

enum State {
    Loading,
    NoFolder,
    NotARepo,
    Ready(Snapshot),
}

#[derive(Clone, PartialEq)]
enum Selection {
    File { path: String, staged: bool },
    Commit(String),
    /// A file in conflict: its blocks, to resolve.
    Conflict(String),
}

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Changes,
    History,
    Stashes,
}

pub struct GitView {
    workspace: WeakEntity<Workspace>,
    state: State,
    selected: Option<Selection>,
    diff: Rc<Vec<DiffLine>>,
    pane: Pane,
    show_branches: bool,
    message: Entity<InputState>,
    new_branch: Entity<InputState>,
    /// What runs now (button spinners): "commit", "push"…
    busy: Option<&'static str>,
    loading: bool,
    /// The repository's top folder: git paths start there, even when the
    /// project is one of its sub-folders.
    top: Option<PathBuf>,
    /// The selected conflicted file, cut into segments (read with the snapshot).
    conflict: Rc<Vec<git::Segment>>,
    /// The selected conflicted file is not UTF-8 text (binary): no blocks to show.
    conflict_unreadable: bool,
    /// A file to show once the repository is read (from the editor's "Changements").
    pending: Option<PathBuf>,
    /// Drawn since the last poll tick: a hidden tab does not run git every 3 s.
    rendered: bool,
    /// A poll was skipped while hidden: read again when shown.
    stale: bool,
    /// The read in flight: a newer one drops it, so a slow old diff never lands on a newer selection.
    _load: Task<()>,
    _poll: Task<()>,
    _subs: Vec<Subscription>,
}

impl GitView {
    /// `root`: the workspace's folder (created from inside a workspace update,
    /// the view cannot read the workspace yet).
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let message = cx.new(|cx| InputState::new(window, cx).multi_line(true).auto_grow(2, 8).placeholder("Message du commit (⌘⏎ pour valider)"));
        let new_branch = cx.new(|cx| InputState::new(window, cx).placeholder("nouvelle-branche"));
        let _subs = vec![
            cx.subscribe_in(&message, window, |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { secondary: true } => this.commit(window, cx),
                _ => cx.notify(),
            }),
            cx.subscribe_in(&new_branch, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.create_branch(window, cx);
                }
            }),
        ];
        // Files change outside (editor saves, terminal, other tools): look again now and then.
        let _poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let tick = this.update(cx, |this, cx| {
                    if std::mem::take(&mut this.rendered) {
                        this.refresh(cx);
                    } else {
                        this.stale = true;
                    }
                });
                if tick.is_err() {
                    break;
                }
            }
        });
        let mut view = Self {
            workspace,
            state: State::Loading,
            selected: None,
            diff: Rc::default(),
            pane: Pane::Changes,
            show_branches: false,
            message,
            new_branch,
            busy: None,
            loading: false,
            top: None,
            conflict: Rc::default(),
            conflict_unreadable: false,
            pending: None,
            rendered: false,
            stale: false,
            _load: Task::ready(()),
            _poll,
            _subs,
        };
        view.load(root, cx);
        view
    }

    fn root(&self, cx: &App) -> Option<PathBuf> {
        self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf))
    }

    fn snapshot(&self) -> Option<&Snapshot> {
        match &self.state {
            State::Ready(s) => Some(s),
            _ => None,
        }
    }

    /// Reads status, branches, history and the selected diff again, in the background.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let root = self.root(cx);
        self.load(root, cx);
    }

    fn load(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let Some(root) = root else {
            self.state = State::NoFolder;
            return cx.notify();
        };
        self.loading = true;
        // A commit cannot change: its diff, once read, is not read again.
        let selected = self.selected.clone().filter(|s| !matches!(s, Selection::Commit(_)) || self.diff.is_empty());
        let task = cx.background_executor().spawn(async move {
            let Some(root) = git::toplevel(&root) else {
                return (State::NotARepo, None, None, None);
            };
            let status = git::status(&root).unwrap_or_default();
            let diff = selected.as_ref().map(|s| diff_of(&root, &status, s));
            // None: not UTF-8 text (binary), or unreadable.
            let conflict = match &selected {
                Some(Selection::Conflict(path)) => std::fs::read_to_string(root.join(path)).ok().filter(|t| !t.contains('\0')).map(|t| git::parse_conflicts(&t)),
                _ => Some(vec![]),
            };
            let remote = git::git(&root, &["remote"]).is_ok_and(|o| !o.trim().is_empty());
            let snapshot = Snapshot { branches: git::branches(&root), log: git::log(&root, HISTORY), stashes: git::stashes(&root), merging: git::merging(&root), remote, status };
            (State::Ready(snapshot), diff, Some(root), conflict)
        });
        self._load = cx.spawn(async move |this, cx| {
            let (state, diff, top, conflict) = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                (this.state, this.top, this.conflict_unreadable) = (state, top, conflict.is_none());
                this.conflict = Rc::new(conflict.unwrap_or_default());
                if let Some(path) = this.pending.take() {
                    this.select_path(path, cx);
                }
                match diff {
                    Some(Some(diff)) => this.diff = Rc::new(diff),
                    // The selected file has no change any more.
                    Some(None) => (this.selected, this.diff) = (None, Rc::default()),
                    None => {}
                }
                cx.notify();
            });
        });
    }

    /// Shows a file's changes (its conflict if it has one), once the repository is read again.
    pub fn show_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // A hidden tab skips polls: its snapshot may be old.
        self.pending = Some(path);
        self.loading = false;
        self.refresh(cx);
    }

    fn select_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let (Some(top), Some(s)) = (self.top.clone(), self.snapshot()) else { return };
        let Some(rel) = repo_path(&top, &path) else { return };
        let Some(change) = s.status.changes.iter().find(|c| c.path == rel) else { return };
        let selection = if change.conflicted() { Selection::Conflict(rel) } else { Selection::File { staged: !change.unstaged(), path: rel } };
        self.pane = Pane::Changes;
        self.select(selection, cx);
    }

    /// Keeps one side of a conflict block, writes the file, reads the repository again.
    fn keep(&mut self, path: String, index: usize, keep: git::Keep, window: &mut Window, cx: &mut Context<Self>) {
        let Some(top) = self.top.clone() else { return };
        // Read again: a save since the last read (up to 3 s ago) must not be overwritten.
        let now = git::parse_conflicts(&std::fs::read_to_string(top.join(&path)).unwrap_or_default());
        if now != *self.conflict {
            window.push_notification(Notification::warning(format!("{path} a changé entre-temps : vérifiez à nouveau ses conflits.")), cx);
            self.loading = false;
            return self.refresh(cx);
        }
        let text = git::resolve(&self.conflict, index, keep);
        if let Err(e) = crate::workspace::write_file(&top.join(&path), &text) {
            return window.push_notification(Notification::error(format!("Impossible d'écrire {path} : {e}")), cx);
        }
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| ws.reload_from_disk(window, cx));
        }
        self.conflict = Rc::new(git::parse_conflicts(&text));
        self.conflict_unreadable = false;
        self.loading = false;
        self.refresh(cx);
    }

    /// "Remiser": puts the changes aside (new files too), with the commit message as its name.
    fn stash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.message.read(cx).value().trim().to_string();
        let message = if typed.is_empty() { format!("Remise du {}", chrono::Local::now().format("%d/%m à %H:%M")) } else { typed };
        self.run("stash", Some("Modifications remisées : retrouvez-les dans « Remises »."), move |root| git::git(root, &["stash", "push", "--include-untracked", "-m", &message]), window, cx);
    }

    /// `hash`: the stash's commit (its "stash@{n}" is looked up right before acting).
    fn unstash(&mut self, hash: String, pop: bool, window: &mut Window, cx: &mut Context<Self>) {
        let done = if pop { "Remise réappliquée et retirée." } else { "Remise réappliquée (elle reste dans la liste)." };
        self.run("stash", Some(done), move |root| git::git(root, &["stash", if pop { "pop" } else { "apply" }, &git::stash_ref(root, &hash)?]), window, cx);
    }

    fn drop_stash(&mut self, hash: String, message: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(PromptLevel::Warning, &format!("Supprimer la remise « {message} » ?"), Some("Les modifications qu'elle garde seront perdues."), &["Supprimer", "Garder"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| this.run("stash", None, move |root| git::git(root, &["stash", "drop", &git::stash_ref(root, &hash)?]), window, cx));
            }
        })
        .detach();
    }

    fn abort_merge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(PromptLevel::Warning, "Annuler la fusion ?", Some("Les fichiers reviennent à leur état d'avant la fusion ; les conflits résolus sont perdus."), &["Annuler la fusion", "Continuer"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| this.run("merge", Some("Fusion annulée."), |root| git::git(root, &["merge", "--abort"]), window, cx));
            }
        })
        .detach();
    }

    fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        self.selected = Some(selection);
        self.diff = Rc::default();
        self.loading = false; // a refresh in flight would bring the old selection's diff
        self.refresh(cx);
    }

    /// Runs a git command in the background, then reports errors and refreshes.
    fn run(&mut self, label: &'static str, done: Option<&'static str>, op: impl FnOnce(&Path) -> Result<String, String> + Send + 'static, window: &mut Window, cx: &mut Context<Self>) {
        // From the repository's top (its paths start there); `git init` from the project.
        let Some(root) = self.top.clone().or_else(|| self.root(cx)) else { return };
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(label);
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            let result = op(&root);
            if result.is_ok() {
                git::refresh_index(&root);
            }
            result
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(_) => {
                        // The message goes only once committed: a failed commit (hook, no user.email) keeps it.
                        if label == "commit" {
                            this.message.update(cx, |m, cx| m.set_value("", window, cx));
                        }
                        if let Some(done) = done {
                            window.push_notification(Notification::success(done), cx);
                        }
                    }
                    Err(e) => window.push_notification(Notification::error(format!("Git : {e}")), cx),
                }
                // Checkouts, pulls and discards rewrite files: open tabs follow.
                if let Some(ws) = this.workspace.upgrade() {
                    ws.update(cx, |ws, cx| ws.reload_from_disk(window, cx));
                }
                this.loading = false;
                this.refresh(cx);
            });
        })
        .detach();
    }

    fn stage(&mut self, paths: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.run("stage", None, move |root| git::stage(root, &paths.iter().map(String::as_str).collect::<Vec<_>>()), window, cx);
    }

    fn unstage(&mut self, paths: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let unborn = self.snapshot().is_some_and(|s| s.status.unborn);
        let paths = with_origins(self.snapshot().map_or(&[], |s| &s.status.changes), paths);
        self.run("stage", None, move |root| git::unstage(root, &paths.iter().map(String::as_str).collect::<Vec<_>>(), unborn), window, cx);
    }

    fn discard(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Annuler les modifications de {path} ?"),
            Some("Le fichier revient à son dernier état enregistré dans Git. Ces modifications seront perdues."),
            &["Annuler les modifications", "Garder"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| this.run("stage", None, move |root| git::discard(root, &[&path]), window, cx));
            }
        })
        .detach();
    }

    /// Ends a modify/delete conflict by deleting the file.
    fn delete_conflicted(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(PromptLevel::Warning, &format!("Supprimer {path} ?"), Some("Le fichier est supprimé du disque et du dépôt. Sa version actuelle sera perdue."), &["Supprimer", "Garder"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| this.run("stage", Some("Conflit résolu."), move |root| git::git(root, &["rm", "-q", "--", &path]), window, cx));
            }
        })
        .detach();
    }

    /// Ends a conflict with no readable blocks by keeping one version whole.
    fn take_side(&mut self, path: String, ours: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (title, detail, label) = if ours {
            (format!("Garder votre version de {path} ?"), "L'autre version de ce fichier sera perdue.", "Garder la mienne")
        } else {
            (format!("Garder l'autre version de {path} ?"), "Votre version de ce fichier sera perdue.", "Garder l'autre")
        };
        let answer = window.prompt(PromptLevel::Warning, &title, Some(detail), &[label, "Annuler"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| this.run("stage", Some("Conflit résolu."), move |root| git::take_side(root, &path, ours), window, cx));
            }
        })
        .detach();
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // ⌘⏎ while another action runs: `run` would do nothing.
        if self.busy.is_some() {
            return;
        }
        let message = self.message.read(cx).value().trim().to_string();
        let Some(snapshot) = self.snapshot() else { return };
        // Unresolved conflicts would be committed with their <<<<<<< markers.
        // A merge whose conflicts all kept "ours" changes nothing, yet still needs its commit.
        if message.is_empty() || (snapshot.status.changes.is_empty() && !snapshot.merging) || snapshot.status.changes.iter().any(Change::conflicted) {
            return;
        }
        // Nothing staged: commit everything, like `git commit -a` (new files included).
        let stage_all = !snapshot.status.changes.iter().any(Change::staged);
        let done = if stage_all { "Toutes les modifications ont été validées." } else { "Commit créé." };
        self.run(
            "commit",
            Some(done),
            move |root| {
                if stage_all {
                    // The snapshot may be 3 s old: never stage a conflict that came since.
                    if !git::git(root, &["ls-files", "-u"])?.trim().is_empty() {
                        return Err("résolvez d'abord les conflits.".into());
                    }
                    git::git(root, &["add", "-A"])?;
                }
                git::commit(root, &message)
            },
            window,
            cx,
        );
    }

    fn push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.snapshot() else { return };
        // First push of a branch: publish it on origin.
        let (args, done): (Vec<String>, _) = match &s.status.upstream {
            Some(_) => (vec!["push".into()], "Commits envoyés."),
            None => (vec!["push".into(), "-u".into(), "origin".into(), s.status.branch.clone()], "Branche publiée sur origin."),
        };
        self.run("push", Some(done), move |root| git::git(root, &args.iter().map(String::as_str).collect::<Vec<_>>()), window, cx);
    }

    fn pull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run("pull", Some("Branche à jour."), |root| git::git(root, &["pull", "--no-edit"]), window, cx);
    }

    fn fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run("fetch", None, |root| git::git(root, &["fetch", "--all", "--prune"]), window, cx);
    }

    fn switch_branch(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.show_branches = false;
        self.run("branch", None, move |root| git::git(root, &["switch", &name]), window, cx);
    }

    fn create_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.new_branch.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        self.new_branch.update(cx, |i, cx| i.set_value("", window, cx));
        self.show_branches = false;
        self.run("branch", None, move |root| git::git(root, &["switch", "-c", &name]), window, cx);
    }

    fn open_file(&self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(top), Some(ws)) = (self.top.clone(), self.workspace.upgrade()) else { return };
        let file = top.join(path);
        if file.is_file() {
            ws.update(cx, |ws, cx| ws.open_path(file, window, cx));
        }
    }
}

/// The diff to show for a selection; None when that file no longer changes.
fn diff_of(root: &Path, status: &Status, selection: &Selection) -> Option<Vec<DiffLine>> {
    match selection {
        // Resolved: the selection goes, the file is listed with the others.
        Selection::Conflict(path) => status.changes.iter().any(|c| &c.path == path && c.conflicted()).then(Vec::new),
        Selection::Commit(hash) => Some(git::parse_diff(&git::show(root, hash))),
        Selection::File { path, staged } => {
            let change = status.changes.iter().find(|c| &c.path == path && if *staged { c.staged() } else { c.unstaged() })?;
            Some(git::parse_diff(&git::diff(root, change, *staged)))
        }
    }
}

/// The repository path of a file, also when the project was opened through a
/// symlink (git's top folder is the real path).
fn repo_path(top: &Path, file: &Path) -> Option<String> {
    let real = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    real.strip_prefix(top).or_else(|_| file.strip_prefix(top)).ok().map(|p| p.to_string_lossy().into_owned())
}

/// `paths` plus the original path of each rename among them: unstaging only the
/// new path would leave the old one's deletion staged.
fn with_origins(changes: &[Change], mut paths: Vec<String>) -> Vec<String> {
    let listed: HashSet<&str> = paths.iter().map(String::as_str).collect();
    let origins: Vec<String> = changes.iter().filter(|c| listed.contains(c.path.as_str())).filter_map(|c| c.orig.clone()).collect();
    paths.extend(origins);
    paths
}

pub fn letter_color(letter: char, cx: &App) -> Hsla {
    let theme = cx.theme();
    let color = match letter {
        'A' | 'U' => theme.green,
        // '!': a conflict.
        'D' | '!' => theme.red,
        'R' | 'C' => theme.blue,
        _ => theme.yellow,
    };
    // Light themes: darker, to read as text on a light background.
    if theme.is_dark() { color } else { Hsla { l: color.l * 0.78, ..color } }
}

/// "src/ui/button.rs" → ("button.rs", "src/ui").
fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').map_or((path, ""), |(dir, name)| (name, dir))
}

/// The change's letter on a tinted square: M, A, D, U, R.
fn badge(letter: char, cx: &App) -> impl IntoElement {
    let color = letter_color(letter, cx);
    div()
        .size(r(19.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(r(5.))
        .bg(color.opacity(0.16))
        .text_color(color)
        .text_size(r(10.5))
        .font_weight(FontWeight::BOLD)
        .child(letter.to_string())
}

fn count(n: usize, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div().h(r(18.)).px(r(6.)).flex().items_center().rounded_full().bg(theme.foreground.opacity(0.08)).text_size(r(11.)).text_color(theme.muted_foreground).child(n.to_string())
}

/// The author's initials on a circle whose color comes from the name.
fn avatar(name: &str, cx: &App) -> impl IntoElement {
    let initials: String = name.split_whitespace().filter_map(|w| w.chars().next()).take(2).flat_map(char::to_uppercase).collect();
    let hue = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 360;
    let (bg, fg) = if cx.theme().is_dark() { (0.30, 0.82) } else { (0.86, 0.32) };
    div()
        .size(r(30.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(hsla(hue as f32 / 360., 0.5, bg, 1.))
        .text_color(hsla(hue as f32 / 360., 0.6, fg, 1.))
        .text_size(r(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .child(initials)
}

fn branch_icon() -> Icon {
    Icon::empty().path("icons/git-branch.svg")
}

impl GitView {
    fn render_header(&self, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let st = &s.status;
        let muted = theme.muted_foreground;
        let (dot, upstream) = match &st.upstream {
            Some(up) if st.ahead == 0 && st.behind == 0 => (theme.green, format!("À jour avec {up}")),
            Some(up) => (theme.yellow, format!("{up} · {} à envoyer · {} à récupérer", st.ahead, st.behind)),
            None if st.unborn => (muted, "Aucun commit pour l'instant".into()),
            None if !s.remote => (muted, "Dépôt local, sans dépôt distant".into()),
            None => (theme.yellow, "Branche pas encore publiée".into()),
        };
        let trigger = button("branch")
            .outline()
            .h(r(40.))
            .px(r(10.))
            .loading(self.busy == Some("branch"))
            .tooltip("Changer de branche ou en créer une")
            .child(
                h_flex()
                    .gap(r(10.))
                    .child(div().size(r(26.)).flex().items_center().justify_center().rounded(r(7.)).bg(theme.primary.opacity(0.14)).child(branch_icon().size(r(15.)).text_color(theme.primary)))
                    .child(
                        v_flex()
                            .items_start()
                            .min_w_0()
                            .max_w(r(220.))
                            .child(div().text_size(r(10.)).font_weight(FontWeight::MEDIUM).text_color(muted).child("BRANCHE"))
                            .child(div().max_w_full().truncate().text_size(r(13.5)).font_weight(FontWeight::SEMIBOLD).child(if st.branch.is_empty() { "HEAD".to_string() } else { st.branch.clone() })),
                    )
                    .child(Icon::new(IconName::ChevronDown).small().text_color(muted)),
            );
        let this = cx.entity().downgrade();
        let picker = Popover::new("branch-picker")
            .open(self.show_branches)
            .on_open_change(move |open, _, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.show_branches = *open;
                    cx.notify();
                });
            })
            .trigger(trigger)
            .child(self.render_branches(s, cx));
        let theme = cx.theme();
        h_flex()
            .h(r(68.))
            .flex_shrink_0()
            .px(r(20.))
            .gap(r(14.))
            .border_b_1()
            .border_color(theme.border)
            .child(picker)
            .child(h_flex().min_w_0().gap(r(8.)).text_size(r(12.5)).text_color(muted).child(div().size(r(8.)).flex_shrink_0().rounded_full().bg(dot)).child(div().truncate().child(upstream)))
            .child(div().flex_1())
            .when(!s.status.changes.is_empty(), |this| {
                this.child(
                    button("stash")
                        .outline()
                        .small()
                        .icon(Icon::empty().path("icons/archive.svg"))
                        .label("Remiser")
                        .loading(self.busy == Some("stash"))
                        .disabled(self.busy.is_some())
                        .tooltip("Mettre les modifications de côté pour plus tard (git stash)")
                        .on_click(cx.listener(|this, _, window, cx| this.stash(window, cx))),
                )
            })
            .when(s.remote, |this| this.child(self.render_sync(s, cx)))
    }

    /// Fetch, pull and push, with their counts: only with a remote.
    fn render_sync(&self, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let st = &s.status;
        let busy = self.busy;
        let with_count = |label: &str, n: u32| if n > 0 { format!("{label} · {n}") } else { label.to_string() };
        h_flex()
            .gap(r(8.))
            .child(
                button("fetch")
                    .outline()
                    .small()
                    .icon(Icon::empty().path("icons/refresh.svg"))
                    .label("Récupérer")
                    .loading(busy == Some("fetch"))
                    .tooltip("Regarder s'il y a du nouveau sur les dépôts distants (fetch)")
                    .on_click(cx.listener(|this, _, window, cx| this.fetch(window, cx))),
            )
            .child(
                button("pull")
                    .outline()
                    .small()
                    .icon(IconName::ArrowDown)
                    .label(with_count("Tirer", st.behind))
                    .loading(busy == Some("pull"))
                    .disabled(st.upstream.is_none())
                    .tooltip("Récupérer et intégrer les commits distants (pull)")
                    .on_click(cx.listener(|this, _, window, cx| this.pull(window, cx))),
            )
            .child(
                button("push")
                    .small()
                    .map(|b| if st.ahead > 0 || st.upstream.is_none() { b.primary() } else { b.outline() })
                    .icon(IconName::ArrowUp)
                    .label(if st.upstream.is_some() { with_count("Pousser", st.ahead) } else { "Publier".into() })
                    .loading(busy == Some("push"))
                    .disabled(st.unborn)
                    .tooltip(if st.upstream.is_some() { "Envoyer les commits (push)" } else { "Publier la branche sur origin" })
                    .on_click(cx.listener(|this, _, window, cx| this.push(window, cx))),
            )
    }

    fn render_branches(&self, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .w(r(320.))
            .gap(r(2.))
            .child(div().px(r(8.)).pb(r(6.)).text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.muted_foreground).child(format!("BRANCHES · {}", s.branches.len())))
            .child(
                v_flex().id("branch-list").max_h(r(280.)).overflow_y_scroll().gap(r(2.)).children(s.branches.iter().map(|name| {
                    let current = *name == s.status.branch;
                    // Switching while another git action runs would be ignored.
                    let clickable = !current && self.busy.is_none();
                    let target = name.clone();
                    h_flex()
                        .id(SharedString::from(format!("br-{name}")))
                        .h(r(32.))
                        .px(r(8.))
                        .gap(r(8.))
                        .rounded(r(6.))
                        .when(current, |d| d.bg(theme.primary.opacity(0.12)))
                        .when(clickable, |d| d.cursor_pointer().hover(|d| d.bg(theme.foreground.opacity(0.06))))
                        .child(branch_icon().size(r(14.)).text_color(if current { theme.primary } else { theme.muted_foreground }))
                        .child(div().flex_1().truncate().when(current, |d| d.font_weight(FontWeight::SEMIBOLD)).child(name.clone()))
                        .when(current, |d| d.child(Icon::new(IconName::Check).small().text_color(theme.primary)))
                        .when(clickable, |d| d.on_click(cx.listener(move |this, _, window, cx| this.switch_branch(target.clone(), window, cx))))
                })),
            )
            .child(
                h_flex()
                    .mt(r(6.))
                    .pt(r(8.))
                    .gap(r(6.))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(Input::new(&self.new_branch).small().flex_1())
                    .child(button("create-branch").small().primary().icon(IconName::Plus).label("Créer").disabled(self.busy.is_some()).on_click(cx.listener(|this, _, window, cx| this.create_branch(window, cx)))),
            )
    }

    /// "Modifications | Historique".
    fn render_panes(&self, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted, track, active) = (theme.foreground, theme.muted_foreground, theme.foreground.opacity(0.05), theme.foreground.opacity(0.1));
        let item = |id: &'static str, label: &'static str, n: usize, pane: Pane, cx: &mut Context<Self>| {
            let on = self.pane == pane;
            h_flex()
                .id(id)
                .flex_1()
                .h(r(30.))
                .justify_center()
                .gap(r(6.))
                .rounded(r(7.))
                .cursor_pointer()
                .text_size(r(13.))
                .map(|d| if on { d.bg(active).text_color(fg).font_weight(FontWeight::MEDIUM) } else { d.text_color(muted).hover(|d| d.text_color(fg)) })
                .child(label)
                .child(count(n, cx))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pane = pane;
                    cx.notify();
                }))
        };
        h_flex()
            .flex_shrink_0()
            .m(r(12.))
            .p(r(3.))
            .gap(r(3.))
            .rounded(r(9.))
            .bg(track)
            .child(item("pane-changes", "Modifications", s.status.changes.len(), Pane::Changes, cx))
            .child(item("pane-history", "Historique", s.log.len(), Pane::History, cx))
            .child(item("pane-stashes", "Remises", s.stashes.len(), Pane::Stashes, cx))
    }

    /// Changes put aside ("Remiser"): bring them back or drop them.
    fn render_stashes(&self, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        v_flex().gap(r(4.)).px(r(8.)).pb(r(8.)).children(s.stashes.iter().map(|st| {
            let (a, b, c) = (st.hash.clone(), st.hash.clone(), (st.hash.clone(), st.message.clone()));
            v_flex()
                .flex_shrink_0()
                .p(r(10.))
                .gap(r(6.))
                .rounded(r(8.))
                .border_1()
                .border_color(theme.border)
                .child(div().truncate().text_size(r(13.)).font_weight(FontWeight::MEDIUM).child(st.message.clone()))
                .child(div().text_size(r(11.5)).text_color(muted).child(format!("{} · {}", st.name, st.when)))
                .child(
                    h_flex()
                        .gap(r(6.))
                        .child(button(SharedString::from(format!("pop-{}", st.name))).primary().xsmall().label("Réappliquer").tooltip("Remet les modifications et retire la remise").on_click(cx.listener(move |this, _, window, cx| this.unstash(a.clone(), true, window, cx))))
                        .child(button(SharedString::from(format!("apply-{}", st.name))).ghost().xsmall().label("Appliquer sans retirer").on_click(cx.listener(move |this, _, window, cx| this.unstash(b.clone(), false, window, cx))))
                        .child(div().flex_1())
                        .child(button(SharedString::from(format!("drop-{}", st.name))).ghost().xsmall().icon(IconName::Delete).tooltip("Supprimer la remise").on_click(cx.listener(move |this, _, window, cx| this.drop_stash(c.0.clone(), c.1.clone(), window, cx)))),
                )
        }))
    }

    /// A conflicted file: each block with its two versions and the choices.
    fn render_conflict(&self, path: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, mono) = (theme.muted_foreground, theme.border, theme.mono_font_family.clone());
        let blocks: Vec<(usize, &git::Segment)> = self.conflict.iter().filter(|s| matches!(s, git::Segment::Conflict { .. })).enumerate().collect();
        let (name, _) = split_path(path);
        let (p1, p2, p3, p4, p5, p6) = (path.to_string(), path.to_string(), path.to_string(), path.to_string(), path.to_string(), path.to_string());
        // Deleted or added on one side only: no markers, the choice is to keep the file or not.
        let codes = self.snapshot().and_then(|s| s.status.changes.iter().find(|c| c.path == path)).map(|c| (c.index, c.worktree));
        let one_side = match codes {
            Some(('U', 'D')) => Some("Supprimé dans l'autre version, modifié dans la vôtre"),
            Some(('D', 'U')) => Some("Supprimé dans votre version, modifié dans l'autre"),
            Some(('A', 'U')) => Some("Ajouté dans votre version seulement"),
            Some(('U', 'A')) => Some("Ajouté dans l'autre version seulement"),
            _ => None,
        };
        // Both sides changed a binary file: no blocks, and staging it as is would silently keep "ours".
        // shortcut: a text file with a custom conflict-marker-size reads as "resolved by hand"; read the attribute if it shows up.
        let unreadable = self.conflict_unreadable && blocks.is_empty() && matches!(codes, Some(('U', 'U') | ('A', 'A')));
        let header = h_flex()
            .h(r(60.))
            .flex_shrink_0()
            .px(r(20.))
            .gap(r(12.))
            .border_b_1()
            .border_color(border)
            .child(icons::file(Path::new(name), 22., cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().text_size(r(14.)).font_weight(FontWeight::SEMIBOLD).child(name.to_string()))
                    .child(div().text_size(r(12.)).text_color(if blocks.is_empty() && one_side.is_none() && !unreadable { theme.green } else { theme.red }).child(match (blocks.len(), one_side) {
                        _ if unreadable => "Conflit sans blocs lisibles (fichier binaire ?) : gardez une des deux versions.".to_string(),
                        (0, Some(side)) => format!("{side} : gardez-le ou supprimez-le."),
                        (0, None) if codes == Some(('D', 'D')) => "Supprimé des deux côtés : marquez-le comme résolu.".to_string(),
                        (0, None) => "Plus de conflit : marquez-le comme résolu.".to_string(),
                        (1, _) => "1 conflit à résoudre".to_string(),
                        (n, _) => format!("{n} conflits à résoudre"),
                    })),
            )
            .child(button("conflict-open").ghost().small().icon(IconName::ExternalLink).label("Ouvrir").on_click(cx.listener(move |this, _, window, cx| this.open_file(&p1, window, cx))))
            .when(blocks.is_empty() && one_side.is_some(), |d| {
                d.child(button("conflict-keep-file").primary().small().icon(IconName::Check).label("Garder le fichier").on_click(cx.listener(move |this, _, window, cx| {
                    let path = p3.clone();
                    this.run("stage", Some("Conflit résolu."), move |root| git::stage(root, &[&path]), window, cx);
                })))
                .child(button("conflict-delete-file").outline().small().icon(IconName::Delete).label("Supprimer le fichier").on_click(cx.listener(move |this, _, window, cx| this.delete_conflicted(p4.clone(), window, cx))))
            })
            .when(unreadable, |d| {
                d.child(button("conflict-ours").primary().small().label("Garder la mienne").on_click(cx.listener(move |this, _, window, cx| this.take_side(p5.clone(), true, window, cx))))
                    .child(button("conflict-theirs").outline().small().label("Garder l'autre").on_click(cx.listener(move |this, _, window, cx| this.take_side(p6.clone(), false, window, cx))))
            })
            .when(blocks.is_empty() && one_side.is_none() && !unreadable, |d| {
                d.child(button("conflict-done").primary().small().icon(IconName::Check).label("Marquer comme résolu").on_click(cx.listener(move |this, _, window, cx| {
                    let path = p2.clone();
                    this.run("stage", Some("Conflit résolu."), move |root| git::stage(root, &[&path]), window, cx);
                })))
            });
        let side = |title: String, text: &str, color: Hsla| {
            v_flex()
                .flex_1()
                .min_w_0()
                .rounded(r(8.))
                .overflow_hidden()
                .border_1()
                .border_color(color.opacity(0.35))
                .child(div().px(r(10.)).py(r(5.)).bg(color.opacity(0.12)).text_size(r(12.)).font_weight(FontWeight::MEDIUM).text_color(color).child(title))
                .child(div().p(r(10.)).font_family(mono.clone()).text_size(theme.mono_font_size).whitespace_nowrap().overflow_hidden().child(text.trim_end().to_string()))
        };
        let cards: Vec<AnyElement> = blocks
            .iter()
            .map(|(i, seg)| {
                let git::Segment::Conflict { ours, theirs, ours_label, theirs_label, .. } = seg else { unreachable!() };
                let i = *i;
                let keep = |id: &str, label: &'static str, how: git::Keep, primary: bool| {
                    let path = path.to_string();
                    button(SharedString::from(format!("{id}-{i}")))
                        .small()
                        .map(|b| if primary { b.primary() } else { b.outline() })
                        .label(label)
                        .on_click(cx.listener(move |this, _, window, cx| this.keep(path.clone(), i, how, window, cx)))
                };
                v_flex()
                    .flex_shrink_0()
                    .gap(r(10.))
                    .p(r(14.))
                    .rounded(r(10.))
                    .border_1()
                    .border_color(border)
                    .child(div().text_size(r(12.)).text_color(muted).child(format!("Conflit {}", i + 1)))
                    .child(
                        h_flex()
                            .items_start()
                            .gap(r(10.))
                            .child(side(format!("Votre version ({})", if ours_label.is_empty() { "HEAD" } else { ours_label }), ours, theme.blue))
                            .child(side(format!("L'autre version ({})", if theirs_label.is_empty() { "fusionnée" } else { theirs_label }), theirs, theme.magenta)),
                    )
                    .child(h_flex().gap(r(8.)).child(keep("ours", "Garder la mienne", git::Keep::Ours, true)).child(keep("theirs", "Garder l'autre", git::Keep::Theirs, false)).child(keep("both", "Garder les deux", git::Keep::Both, false)))
                    .into_any_element()
            })
            .collect();
        v_flex().size_full().child(header).child(v_flex().id("conflict-blocks").flex_1().min_h_0().overflow_y_scroll().p(r(16.)).gap(r(12.)).children(cards)).into_any_element()
    }

    fn render_commit_box(&self, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let staged = s.status.changes.iter().filter(|c| c.staged() && !c.conflicted()).count();
        let empty_message = self.message.read(cx).value().trim().is_empty();
        let plural = |n: usize| if n > 1 { "s" } else { "" };
        let conflicts = s.status.changes.iter().filter(|c| c.conflicted()).count();
        let summary = match staged {
            _ if conflicts > 0 => "Résolvez d'abord les conflits".to_string(),
            0 => format!("Tout sera validé sur {}", s.status.branch),
            n => format!("{n} fichier{} indexé{} sur {}", plural(n), plural(n), s.status.branch),
        };
        v_flex()
            .flex_shrink_0()
            .mx(r(12.))
            .mb(r(8.))
            .rounded(r(10.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .overflow_hidden()
            .child(Input::new(&self.message).appearance(false))
            .child(
                h_flex()
                    .px(r(10.))
                    .py(r(8.))
                    .gap(r(8.))
                    .child(div().flex_1().min_w_0().truncate().text_size(r(12.)).text_color(theme.muted_foreground).child(summary))
                    .child(
                        button("commit")
                            .primary()
                            .small()
                            .icon(IconName::Check)
                            .label(if staged == 0 { "Tout valider" } else { "Valider" })
                            .tooltip("Valider (⌘⏎)")
                            .loading(self.busy == Some("commit"))
                            .disabled(empty_message || (s.status.changes.is_empty() && !s.merging) || conflicts > 0 || self.busy.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.commit(window, cx))),
                    ),
            )
    }

    fn render_change(&self, change: &Change, staged: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (name, dir) = split_path(&change.path);
        let letter = change.letter(staged);
        let selection = Selection::File { path: change.path.clone(), staged };
        let selected = self.selected.as_ref() == Some(&selection);
        let id = SharedString::from(format!("{}-{}", if staged { "s" } else { "w" }, change.path));
        let group = SharedString::from(format!("g{id}"));
        let path = change.path.clone();
        let tracked = !change.untracked();
        h_flex()
            .id(id)
            .group(group.clone())
            .h(r(34.))
            .px(r(10.))
            .gap(r(10.))
            .rounded(r(7.))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.primary.opacity(0.14)))
            .when(!selected, |d| d.hover(|d| d.bg(theme.foreground.opacity(0.05))))
            .on_click(cx.listener(move |this, _, _, cx| this.select(selection.clone(), cx)))
            .child(icons::file(Path::new(name), 16., cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(r(6.))
                    // The folder (flex basis 0) gives way before the name.
                    .child(div().min_w_0().truncate().text_size(r(13.)).when(letter == 'D', |d| d.line_through()).child(name.to_string()))
                    .child(div().flex_1().min_w_0().truncate().text_size(r(11.5)).text_color(theme.muted_foreground).child(dir.to_string())),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap(r(2.))
                    .invisible()
                    .group_hover(group, |d| d.visible())
                    .when(!staged && tracked, |d| {
                        let path = path.clone();
                        d.child(
                            button(SharedString::from(format!("discard-{path}")))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Undo)
                                .tooltip("Annuler les modifications")
                                .disabled(self.busy.is_some())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.discard(path.clone(), window, cx)
                                })),
                        )
                    })
                    .child(
                        button(SharedString::from(format!("toggle-{staged}-{path}")))
                            .ghost()
                            .xsmall()
                            .icon(if staged { IconName::Minus } else { IconName::Plus })
                            .tooltip(if staged { "Retirer de l'index" } else { "Indexer" })
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if staged { this.unstage(vec![path.clone()], window, cx) } else { this.stage(vec![path.clone()], window, cx) }
                            })),
                    ),
            )
            .child(badge(letter, cx))
            .into_any_element()
    }

    /// "Fusion en cours": finish with a commit, or abort.
    fn render_merge_banner(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .flex_shrink_0()
            .mx(r(12.))
            .mb(r(8.))
            .p(r(10.))
            .gap(r(8.))
            .rounded(r(8.))
            .bg(theme.yellow.opacity(0.12))
            .child(Icon::new(IconName::TriangleAlert).small().text_color(theme.yellow))
            .child(v_flex().flex_1().min_w_0().child(div().text_size(r(13.)).font_weight(FontWeight::MEDIUM).child("Fusion en cours")).child(div().text_size(r(11.5)).text_color(theme.muted_foreground).child("Résolvez les conflits puis validez pour la terminer.")))
            .child(button("abort-merge").ghost().xsmall().label("Annuler la fusion").on_click(cx.listener(|this, _, window, cx| this.abort_merge(window, cx))))
    }

    /// Files both sides changed: a click opens their blocks to choose from.
    fn render_conflicts(&self, conflicts: &[&Change], cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let red = theme.red;
        v_flex()
            .gap(r(1.))
            .child(h_flex().h(r(32.)).px(r(10.)).gap(r(8.)).child(section_title("Conflits", cx).text_color(red)).child(count(conflicts.len(), cx)))
            .children(conflicts.iter().map(|c| {
                let (name, dir) = split_path(&c.path);
                let selection = Selection::Conflict(c.path.clone());
                let selected = self.selected.as_ref() == Some(&selection);
                h_flex()
                    .id(SharedString::from(format!("conflict-{}", c.path)))
                    .h(r(34.))
                    .px(r(10.))
                    .gap(r(10.))
                    .rounded(r(7.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(red.opacity(0.14)))
                    .when(!selected, |d| d.hover(|d| d.bg(theme.foreground.opacity(0.05))))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(selection.clone(), cx)))
                    .child(icons::file(Path::new(name), 16., cx))
                    .child(h_flex().flex_1().min_w_0().gap(r(6.)).child(div().min_w_0().truncate().text_size(r(13.)).child(name.to_string())).child(div().flex_1().min_w_0().truncate().text_size(r(11.5)).text_color(theme.muted_foreground).child(dir.to_string())))
                    .child(badge('!', cx))
            }))
    }

    fn render_section(&self, title: &str, changes: Vec<&Change>, staged: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<AnyElement> = changes.iter().take(MAX_ROWS).map(|c| self.render_change(c, staged, cx)).collect();
        let hidden = changes.len().saturating_sub(MAX_ROWS);
        // During a merge "." would stage conflicted files markers and all (or unstage them, dropping the conflict): only the listed ones.
        let all: Vec<String> = if self.snapshot().is_some_and(|s| s.status.changes.iter().any(Change::conflicted)) { changes.iter().map(|c| c.path.clone()).collect() } else { vec![".".into()] };
        let theme = cx.theme();
        v_flex()
            .gap(r(1.))
            .child(
                h_flex()
                    .h(r(32.))
                    .px(r(10.))
                    .gap(r(8.))
                    .child(section_title(title, cx))
                    .child(count(changes.len(), cx))
                    .child(div().flex_1())
                    .child(
                        button(SharedString::from(format!("all-{staged}")))
                            .ghost()
                            .xsmall()
                            .icon(if staged { IconName::Minus } else { IconName::Plus })
                            .label(if staged { "Tout retirer" } else { "Tout indexer" })
                            .disabled(self.busy.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if staged { this.unstage(all.clone(), window, cx) } else { this.stage(all.clone(), window, cx) }
                            })),
                    ),
            )
            .children(rows)
            .when(hidden > 0, |d| d.child(div().px(r(10.)).py(r(6.)).text_size(r(12.)).text_color(theme.muted_foreground).child(format!("… et {hidden} autres fichiers"))))
    }

    fn render_history(&self, s: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex().gap(r(1.)).px(r(8.)).pb(r(8.)).children(s.log.iter().map(|c| {
            let selection = Selection::Commit(c.hash.clone());
            let selected = self.selected.as_ref() == Some(&selection);
            h_flex()
                .id(SharedString::from(format!("c-{}", c.hash)))
                .px(r(10.))
                .py(r(8.))
                .gap(r(10.))
                .rounded(r(8.))
                .cursor_pointer()
                .when(selected, |d| d.bg(theme.primary.opacity(0.14)))
                .when(!selected, |d| d.hover(|d| d.bg(theme.foreground.opacity(0.05))))
                .on_click(cx.listener(move |this, _, _, cx| this.select(selection.clone(), cx)))
                .child(avatar(&c.author, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(r(2.))
                        .child(div().truncate().text_size(r(13.)).font_weight(FontWeight::MEDIUM).child(c.subject.clone()))
                        .child(div().truncate().text_size(r(11.5)).text_color(theme.muted_foreground).child(format!("{} · {}", c.author, c.when))),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .px(r(6.))
                        .py(r(1.))
                        .rounded(r(5.))
                        .bg(theme.foreground.opacity(0.06))
                        .font_family(theme.mono_font_family.clone())
                        .text_size(r(11.))
                        .text_color(theme.muted_foreground)
                        .child(c.hash.clone()),
                )
        }))
    }

    /// What the diff pane is about: the file and its buttons, or the commit.
    fn render_diff_header(&self, s: &Snapshot, selected: &Selection, cx: &mut Context<Self>) -> AnyElement {
        let added = self.diff.iter().filter(|l| l.kind == LineKind::Added).count();
        let removed = self.diff.iter().filter(|l| l.kind == LineKind::Removed).count();
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let stats = h_flex()
            .flex_shrink_0()
            .gap(r(8.))
            .font_family(theme.mono_font_family.clone())
            .text_size(r(12.))
            .child(div().text_color(theme.green).child(format!("+{added}")))
            .child(div().text_color(theme.red).child(format!("−{removed}")));
        let row = h_flex().h(r(60.)).flex_shrink_0().px(r(20.)).gap(r(12.)).border_b_1().border_color(theme.border);
        let title = |main: String, sub: String| {
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(r(1.))
                .child(div().truncate().text_size(r(14.)).font_weight(FontWeight::SEMIBOLD).child(main))
                .child(div().truncate().text_size(r(12.)).text_color(muted).child(sub))
        };
        match selected {
            // Shown by `render_conflict` instead.
            Selection::Conflict(_) => row.into_any_element(),
            Selection::Commit(hash) => {
                let commit = s.log.iter().find(|c| &c.hash == hash);
                let author = commit.map_or(String::new(), |c| c.author.clone());
                row.child(avatar(&author, cx))
                    .child(title(commit.map_or(hash.clone(), |c| c.subject.clone()), commit.map_or(String::new(), |c| format!("{hash} · {} · {}", c.author, c.when))))
                    .child(stats)
                    .into_any_element()
            }
            Selection::File { path, staged } => {
                let (name, dir) = split_path(path);
                let sub = match (*staged, dir) {
                    (true, "") => "Indexé".to_string(),
                    (true, dir) => format!("{dir} · indexé"),
                    (false, "") => "Modifié".to_string(),
                    (false, dir) => format!("{dir} · modifié"),
                };
                let staged = *staged;
                let tracked = s.status.changes.iter().any(|c| &c.path == path && !c.untracked());
                let (p1, p2, p3) = (path.clone(), path.clone(), path.clone());
                row.child(icons::file(Path::new(name), 22., cx))
                    .child(title(name.to_string(), sub))
                    .child(stats)
                    .child(div().w(r(4.)))
                    .when(!staged && tracked, |d| {
                        d.child(button("diff-discard").ghost().small().icon(IconName::Undo).tooltip("Annuler les modifications").on_click(cx.listener(move |this, _, window, cx| this.discard(p1.clone(), window, cx))))
                    })
                    .child(
                        button("diff-stage")
                            .outline()
                            .small()
                            .icon(if staged { IconName::Minus } else { IconName::Plus })
                            .label(if staged { "Retirer" } else { "Indexer" })
                            .on_click(cx.listener(move |this, _, window, cx| if staged { this.unstage(vec![p2.clone()], window, cx) } else { this.stage(vec![p2.clone()], window, cx) })),
                    )
                    .child(button("open-file").ghost().small().icon(IconName::ExternalLink).label("Ouvrir").on_click(cx.listener(move |this, _, window, cx| this.open_file(&p3, window, cx))))
                    .into_any_element()
            }
        }
    }

    fn render_diff(&self, s: &Snapshot, cx: &mut Context<Self>) -> AnyElement {
        let Some(selected) = self.selected.clone() else {
            let text = "Choisissez un fichier modifié ou un commit de l'historique pour voir ses changements ligne par ligne.";
            return empty_state(branch_icon(), cx.theme().primary, "Aucune sélection", text, cx).into_any_element();
        };
        if let Selection::Conflict(path) = &selected {
            return self.render_conflict(path, cx);
        }
        let header = self.render_diff_header(s, &selected, cx);
        let theme = cx.theme();
        let lines = self.diff.clone();
        let (font, size) = (theme.mono_font_family.clone(), theme.mono_font_size);
        let (added, removed, info, muted) = (theme.green, theme.red, theme.primary, theme.muted_foreground);
        let gutter = theme.foreground.opacity(0.025);
        v_flex()
            .size_full()
            .child(header)
            .child(
                uniform_list("diff", lines.len(), move |range: Range<usize>, _, _| {
                    range
                        .map(|ix| {
                            let l = &lines[ix];
                            let (bg, side, fg, sign) = match l.kind {
                                LineKind::Added => (added.opacity(0.1), added.opacity(0.18), None, "+"),
                                LineKind::Removed => (removed.opacity(0.1), removed.opacity(0.18), None, "−"),
                                LineKind::Info => (info.opacity(0.07), info.opacity(0.07), Some(info), ""),
                                LineKind::Context => (gpui::transparent_black(), gutter, None, ""),
                            };
                            let num = |n: Option<u32>| div().w(r(42.)).flex_shrink_0().text_right().pr(r(8.)).child(n.map(|n| n.to_string()).unwrap_or_default());
                            h_flex()
                                .w_full()
                                .bg(bg)
                                .child(h_flex().h_full().flex_shrink_0().bg(side).text_color(muted.opacity(0.7)).child(num(l.old)).child(num(l.new)))
                                .child(div().w(r(22.)).flex_shrink_0().text_center().text_color(fg.unwrap_or(muted)).child(sign))
                                .child(div().whitespace_nowrap().pr(r(16.)).when_some(fg, |d, fg| d.text_color(fg)).child(l.text.replace('\t', "    ")))
                        })
                        .collect()
                })
                .flex_1()
                .min_h_0()
                .font_family(font)
                .text_size(size)
                .line_height(size * 1.65),
            )
            .into_any_element()
    }

    fn render_empty(&self, title: &str, text: &str, action: Option<AnyElement>, cx: &App) -> AnyElement {
        empty_state(branch_icon(), cx.theme().primary, title, text, cx).children(action).into_any_element()
    }
}

impl Render for GitView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rendered = true;
        if std::mem::take(&mut self.stale) {
            self.refresh(cx);
        }
        if !AppSettings::get(cx).tool_added("git") {
            let add = button("add-git").primary().small().label("Ajouter Git").on_click(|_, _, cx| AppSettings::update(cx, |s| s.tools.push("git".into())));
            return self.render_empty("Git n'est pas ajouté", "Ajoutez-le pour gérer le dépôt du projet ici et voir les fichiers modifiés dans l'explorateur.", Some(add.into_any_element()), cx);
        }
        let snapshot = match &self.state {
            State::Loading => return loading("Lecture du dépôt…", cx).into_any_element(),
            State::NoFolder => return self.render_empty("Aucun dossier ouvert", "Ouvrez un dossier (⌘⇧O) pour gérer son dépôt Git.", None, cx),
            State::NotARepo if let Some(what) = self.workspace.upgrade().and_then(|w| w.read(cx).root().and_then(git::paused)) => {
                let trust = button("git-trust").primary().small().label("Faire confiance au dossier…").on_click(cx.listener(|this, _, window, cx| {
                    let _ = this.workspace.update(cx, |w, cx| w.trust_folder(window, cx));
                }));
                let text = format!("Ce dépôt configure une commande que Git lancerait tout seul ({what}). Un projet téléchargé pourrait ainsi exécuter n'importe quoi : Git attend que vous fassiez confiance au dossier.");
                return self.render_empty("Git est en pause", &text, Some(trust.into_any_element()), cx);
            }
            State::NotARepo => {
                let init = button("git-init").primary().small().label("Initialiser un dépôt").on_click(cx.listener(|this, _, window, cx| {
                    this.state = State::Loading;
                    this.run("init", Some("Dépôt Git créé."), |root| git::git(root, &["init", "-q"]), window, cx);
                }));
                return self.render_empty("Pas encore de dépôt Git", "Ce dossier n'est pas suivi par Git. Créez un dépôt pour suivre l'historique de vos fichiers.", Some(init.into_any_element()), cx);
            }
            State::Ready(s) => s.clone(),
        };
        let conflicts: Vec<&Change> = snapshot.status.changes.iter().filter(|c| c.conflicted()).collect();
        let staged: Vec<&Change> = snapshot.status.changes.iter().filter(|c| c.staged() && !c.conflicted()).collect();
        let unstaged: Vec<&Change> = snapshot.status.changes.iter().filter(|c| c.unstaged() && !c.conflicted()).collect();

        let list = match self.pane {
            Pane::Changes if snapshot.status.changes.is_empty() && snapshot.merging => {
                empty_state(Icon::new(IconName::CircleCheck), cx.theme().green, "Fusion prête", "Validez pour la terminer.", cx).into_any_element()
            }
            Pane::Changes if snapshot.status.changes.is_empty() => {
                let text = format!("Aucune modification en attente sur {}.", snapshot.status.branch);
                empty_state(Icon::new(IconName::CircleCheck), cx.theme().green, "Tout est validé", &text, cx).into_any_element()
            }
            Pane::Changes => v_flex()
                .px(r(8.))
                .pb(r(8.))
                .gap(r(10.))
                .when(!conflicts.is_empty(), |d| d.child(self.render_conflicts(&conflicts, cx)))
                .when(!staged.is_empty(), |d| d.child(self.render_section("Indexées", staged.clone(), true, cx)))
                .when(!unstaged.is_empty(), |d| d.child(self.render_section("Modifications", unstaged.clone(), false, cx)))
                .into_any_element(),
            Pane::History if snapshot.log.is_empty() => empty_state(branch_icon(), cx.theme().primary, "Pas encore de commit", "Écrivez un message et validez vos fichiers pour créer le premier.", cx).into_any_element(),
            Pane::History => self.render_history(&snapshot, cx).into_any_element(),
            Pane::Stashes if snapshot.stashes.is_empty() => {
                empty_state(Icon::empty().path("icons/archive.svg"), cx.theme().primary, "Aucune remise", "« Remiser » met vos modifications de côté pour plus tard, sans les valider.", cx).into_any_element()
            }
            Pane::Stashes => self.render_stashes(&snapshot, cx).into_any_element(),
        };
        let theme = cx.theme();
        let (border, column) = (theme.border, theme.foreground.opacity(0.015));

        v_flex()
            .size_full()
            .child(self.render_header(&snapshot, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w(r(380.))
                            .flex_shrink_0()
                            .h_full()
                            .bg(column)
                            .border_r_1()
                            .border_color(border)
                            .child(self.render_panes(&snapshot, cx))
                            .when(snapshot.merging, |d| d.child(self.render_merge_banner(cx)))
                            .when(self.pane == Pane::Changes, |d| d.child(self.render_commit_box(&snapshot, cx)))
                            .child(div().id("git-list").flex_1().min_h_0().overflow_y_scroll().child(list)),
                    )
                    .child(div().flex_1().min_w_0().h_full().child(self.render_diff(&snapshot, cx))),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_into_name_and_folder() {
        assert_eq!(split_path("src/ui/button.rs"), ("button.rs", "src/ui"));
        assert_eq!(split_path("README.md"), ("README.md", ""));
    }

    #[test]
    fn unstaging_a_rename_takes_its_original_path_too() {
        let c = |path: &str, orig: Option<&str>| Change { path: path.into(), orig: orig.map(String::from), index: 'R', worktree: ' ' };
        let changes = [c("b é.txt", Some("a é.txt")), c("x", None)];
        assert_eq!(with_origins(&changes, vec!["b é.txt".into(), "x".into()]), ["b é.txt", "x", "a é.txt"]);
    }

    #[test]
    fn a_file_reached_through_a_symlink_has_its_repository_path() {
        let dir = std::env::temp_dir().join(format!("akolade-gitview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real/src")).unwrap();
        std::fs::write(dir.join("real/src/a.rs"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let top = dir.join("real").canonicalize().unwrap();
        assert_eq!(repo_path(&top, &dir.join("link/src/a.rs")).as_deref(), Some("src/a.rs"));
        assert_eq!(repo_path(&top, &top.join("gone.rs")).as_deref(), Some("gone.rs"), "deleted: the path as given");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
