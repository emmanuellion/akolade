//! "Multiplexeur": one tab driving N Claude Code sessions side by side, on one
//! project (several features at once, each session in its own worktree) or on
//! several (one session per project, the tab as a common dashboard), mixed at will.
//! Each panel is a `ClaudeSession` shown without the Claude Code tab's chrome.

pub mod coord;
pub mod copy;
pub mod mcp;
pub mod project;
pub mod queue;
pub mod stats;
pub mod worktree;

use std::path::{Path, PathBuf};

use gpui::{AnyElement, App, AppContext, Context, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, div, prelude::FluentBuilder, relative, uniform_list};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    resizable::{h_resizable, resizable_panel, v_resizable},
    tooltip::Tooltip,
    v_flex,
};

use crate::{
    settings::AppSettings,
    tools::{
        claude::{self, ClaudeSession, EFFORTS, MODELS, MODES, SessionOptions, SessionState},
        header, tool,
    },
    ui,
    workspace::{Workspace, button, r},
};

use project::WorkMode;

/// One session of the multiplexer.
struct Slot {
    id: String,
    name: String,
    role: String,
    /// Its project (a repository's root) and the folder it works in (the project's, or its worktree).
    project: PathBuf,
    cwd: PathBuf,
    mode: WorkMode,
    /// Isolated: its branch and worktree.
    branch: Option<String>,
    worktree: Option<PathBuf>,
    session: Entity<ClaudeSession>,
    /// Only its bar shows (its conversation is not drawn).
    folded: bool,
    /// Its turn was stopped by "Pause": "Reprendre" asks it to go on.
    paused: bool,
    /// Opened in the "À plat" view: shown there only (each view has its own sessions).
    flat: bool,
    /// Its own look (true: as in the terminal); None: the multiplexer's.
    terminal: Option<bool>,
    /// What its bar last showed: the tab redraws only when that changes, not at each streamed word.
    seen: (SessionState, u64),
    _sub: Subscription,
}

/// Who a message typed in "Échanges" goes to.
#[derive(Clone, PartialEq)]
enum Recipient {
    All,
    Project(PathBuf),
    Session(String),
}

/// A session to start, as the form gave it.
struct Launch {
    id: String,
    name: String,
    role: String,
    task: String,
    project: PathBuf,
    mode: WorkMode,
    model: usize,
    perm: usize,
    effort: usize,
    budget: Option<f64>,
    /// A session restored after a restart: its Claude Code conversation (it comes back paused).
    conversation: Option<String>,
    flat: bool,
    terminal: Option<bool>,
}

/// The "Nouvelle session" form.
struct Form {
    folder: Option<PathBuf>,
    /// A folder typed or pasted (taken once it exists).
    path: Entity<InputState>,
    name: Entity<InputState>,
    role: Entity<InputState>,
    task: Entity<InputState>,
    budget: Entity<InputState>,
    mode: WorkMode,
    model: usize,
    perm: usize,
    effort: usize,
    /// The optional fields shown (only the project is required).
    more: bool,
}

pub struct MuxView {
    workspace: WeakEntity<Workspace>,
    slots: Vec<Slot>,
    /// The view shown: one section per project, or all its sessions side by side ("À plat").
    /// Each has its own sessions; those of the other go on working meanwhile.
    by_project: bool,
    form: Option<Form>,
    _form_sub: Option<Subscription>,
    /// Isolated sessions whose worktree is being made.
    starting: Vec<String>,
    /// The project (key) whose last merge report is unfolded.
    report: Option<String>,
    /// The "Échanges" panel: shown, filtered on a project, and its box (to whom: None = all sessions).
    exchanges: bool,
    filter: Option<PathBuf>,
    compose: Entity<InputState>,
    to: Recipient,
    next_id: u64,
    /// The "Statistiques" panel: shown, and Claude Code's figures read when it opened.
    stats: bool,
    cc_stats: Option<stats::CcStats>,
    /// "Onglets": the session shown (its id).
    active: Option<String>,
}

/// A color per project, in the order they appear.
fn project_color(ix: usize, cx: &gpui::App) -> Hsla {
    let t = cx.theme();
    [t.blue, t.green, t.magenta, t.yellow, t.cyan, t.red][ix % 6]
}

/// A session's worktree rebased on its project's integration branch, said in a notification.
fn rebase(wt: PathBuf, window: &mut Window, cx: &mut App) {
    let task = cx.background_executor().spawn(async move { crate::git::rebase(&wt, queue::BRANCH) });
    window
        .spawn(cx, async move |cx| {
            let note = match task.await {
                crate::git::Integration::Done => gpui_component::notification::Notification::success("Branche rebasée sur akolade/integration."),
                crate::git::Integration::Conflict(files) => gpui_component::notification::Notification::error(format!("Conflit sur {} : le rebase est annulé, demandez à la session de le faire elle-même.", files.join(", "))),
                crate::git::Integration::Failed(e) => gpui_component::notification::Notification::error(format!("Rebase impossible : {e}")),
            };
            let _ = cx.update(|window, cx| window.push_notification(note, cx));
        })
        .detach();
}

fn name_of(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

impl MuxView {
    pub fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // The sessions, messages and leases of last time (Akolade restarted): back, paused.
        let saved = coord::Coordinator::load(&crate::settings::app_dir());
        let restored = saved.sessions.clone();
        coord::with(|c| {
            if c.sessions.is_empty() {
                *c = saved;
            }
        });
        // Worktrees no session uses any more (a crash, a kill): removed, their branches kept.
        let base = worktree::base();
        let mut keep: Vec<PathBuf> = restored.iter().filter(|s| s.mode == WorkMode::Isolated).map(|s| s.checkout.clone()).collect();
        keep.extend(restored.iter().map(|s| queue::dir(&base, &s.project)));
        cx.background_executor().spawn(async move { worktree::cleanup(&base, &keep) }).detach();
        // Copies no session uses: removed, unless they hold work not brought back.
        let copies: Vec<PathBuf> = restored.iter().filter(|s| s.mode == WorkMode::Copy).map(|s| s.checkout.clone()).collect();
        cx.background_executor().spawn(async move { copy::cleanup(&copy::base(), &copies) }).detach();
        cx.defer_in(window, move |this, window, cx| this.restore(restored, window, cx));
        // Leases, messages, the queue: redrawn when the coordinator changes (from the MCP server too).
        let mut changes = coord::watch();
        cx.spawn_in(window, async move |this, cx| {
            use futures::StreamExt;
            while changes.next().await.is_some() {
                let alive = this.update_in(cx, |this, window, cx| {
                    this.deliver(window, cx);
                    this.pump(cx);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        let compose = cx.new(|cx| InputState::new(window, cx).placeholder("Écrire…"));
        Self { workspace, slots: vec![], by_project: !AppSettings::get(cx).mux_flat, form: None, _form_sub: None, starting: vec![], report: None, exchanges: false, filter: None, compose, to: Recipient::All, next_id: 1, stats: false, cc_stats: None, active: None }
    }

    /// The saved sessions as panels again, paused on their conversation; an Isolated one's
    /// worktree made again from its branch if it went (cleaned up, deleted by hand).
    fn restore(&mut self, sessions: Vec<coord::SessionInfo>, window: &mut Window, cx: &mut Context<Self>) {
        for mut info in sessions {
            let n = info.id.rsplit('-').next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
            self.next_id = self.next_id.max(n + 1);
            let worktree = match info.mode {
                WorkMode::Direct => None,
                WorkMode::Isolated => {
                    let mut wt = info.checkout.clone();
                    let canonical = |p: &Path| crate::platform::canonical(p).ok();
                    let known = crate::git::worktree_list(&info.project).iter().any(|(p, _)| canonical(p) == canonical(&wt));
                    // The project's own checkout is never an isolated session's worktree (it would work in yours).
                    let yours = canonical(&wt) == canonical(&info.project);
                    if !known || yours || !wt.join(".git").exists() {
                        let slug = info.branch.as_deref().and_then(|b| b.strip_prefix("akolade/")).unwrap_or(&info.id).to_string();
                        match worktree::create(&worktree::base(), &info.project, &slug) {
                            // Made again (maybe elsewhere): its folder, in the same subfolder as before.
                            Ok(made) => {
                                let rel = info.cwd.strip_prefix(&info.checkout).map(Path::to_path_buf).unwrap_or_default();
                                info.cwd = made.join(rel);
                                wt = made;
                            }
                            Err(e) => {
                                window.push_notification(gpui_component::notification::Notification::error(format!("« {} » n'a pas pu revenir : {e}", info.name)), cx);
                                coord::with(|c| c.remove(&info.id));
                                continue;
                            }
                        }
                    }
                    crate::trust::link(&wt, &info.project);
                    Some(wt)
                }
                // Its copy, as it was (made again from the project if it went).
                WorkMode::Copy => {
                    if !copy::exists(&info.checkout) {
                        match copy::create(&copy::base(), &info.project, &info.id) {
                            Ok(made) => {
                                let rel = info.cwd.strip_prefix(&info.checkout).map(Path::to_path_buf).unwrap_or_default();
                                info.cwd = made.join(rel);
                                info.checkout = made;
                            }
                            Err(e) => {
                                window.push_notification(gpui_component::notification::Notification::error(format!("« {} » n'a pas pu revenir : {e}", info.name)), cx);
                                coord::with(|c| c.remove(&info.id));
                                continue;
                            }
                        }
                    }
                    Some(info.checkout.clone())
                }
            };
            let launch = Launch {
                id: info.id,
                name: info.name,
                role: info.role,
                task: String::new(),
                project: info.project,
                mode: info.mode,
                model: info.model,
                perm: info.perm,
                effort: info.effort,
                budget: info.budget,
                conversation: Some(info.conversation.unwrap_or_default()).filter(|c| !c.is_empty()),
                flat: info.flat,
                terminal: info.terminal,
            };
            self.start(launch, info.cwd, worktree, window, cx);
        }
    }

    /// The sessions of the view shown (indexes in `slots`).
    fn shown(&self) -> Vec<usize> {
        (0..self.slots.len()).filter(|&i| self.slots[i].flat != self.by_project).collect()
    }

    /// The projects in order of first appearance (their colors).
    fn projects(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = vec![];
        for s in &self.slots {
            if !out.contains(&s.project) {
                out.push(s.project.clone());
            }
        }
        out
    }

    fn color_of(&self, project: &Path, cx: &gpui::App) -> Hsla {
        project_color(self.projects().iter().position(|p| p == project).unwrap_or(0), cx)
    }

    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = AppSettings::get(cx).clone();
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Automatique : d'après le projet"));
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("ou collez le chemin d'un dossier"));
        let sub = cx.subscribe(&path, |this, input, e: &gpui_component::input::InputEvent, cx| {
            if matches!(e, gpui_component::input::InputEvent::Change) {
                let typed = PathBuf::from(input.read(cx).value().trim());
                if typed.is_absolute() && typed.is_dir() {
                    this.set_folder(typed, cx);
                }
            }
        });
        self._form_sub = Some(sub);
        let form = Form {
            folder: None,
            path,
            name,
            role: input("Ex. : back-end, tests, documentation…", window, cx),
            task: cx.new(|cx| InputState::new(window, cx).multi_line(true).auto_grow(2, 6).placeholder("La première consigne (facultatif)")),
            budget: input("0 : sans limite", window, cx),
            mode: WorkMode::Direct,
            model: MODELS.iter().position(|m| m.1 == settings.claude_model).unwrap_or(0),
            perm: MODES.iter().position(|m| m.1 == settings.claude_mode).unwrap_or(0),
            effort: EFFORTS.iter().position(|e| e.0 == settings.claude_effort).unwrap_or(0),
            more: false,
        };
        self.form = Some(form);
        cx.notify();
    }

    /// A project picked in the form: alone there, in its folder; joining others, in a copy of its own
    /// (brought back when done), so that no command of one touches another's files.
    fn set_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let project = project::project_of(&folder);
        let joined = self.slots.iter().any(|s| s.project == project);
        if let Some(form) = &mut self.form {
            form.mode = if joined { WorkMode::Copy } else { WorkMode::Direct };
            form.folder = Some(folder);
        }
        cx.notify();
    }

    /// A folder from the system's picker: into the form, or (`at_once`) a session started there.
    fn pick_folder(&mut self, at_once: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Choisir".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = rx.await else { return };
            let Some(path) = paths.pop() else { return };
            let _ = this.update_in(cx, |this, window, cx| if at_once { this.quick(path, None, window, cx) } else { this.set_folder(path, cx) });
        })
        .detach();
    }

    /// A session on `folder` at once, everything else automatic: its name from the project's,
    /// its mode from who is there, the Claude Code tab's settings. The form stays open only
    /// to say why it cannot start (not a Git repository…).
    fn quick(&mut self, folder: PathBuf, mode: Option<WorkMode>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_form(window, cx);
        self.set_folder(folder, cx);
        if let (Some(f), Some(mode)) = (self.form.as_mut(), mode) {
            f.mode = mode;
        }
        self.launch(window, cx);
    }

    /// "+": another Claude on `project`: in a copy of its folder, in the folder itself, or in a worktree (git only).
    fn add_button(&self, id: SharedString, project: PathBuf, label: Option<&'static str>, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity().downgrade();
        let git = crate::git::toplevel(&project).is_some();
        button(id)
            .ghost()
            .xsmall()
            .icon(Icon::new(IconName::Plus).xsmall())
            .map(|b| match label {
                Some(l) => b.label(l),
                None => b.tooltip("Une autre session sur ce projet"),
            })
            .dropdown_menu_with_anchor(gpui::Corner::TopRight, move |menu, _, _| {
                let add = |mode: WorkMode| {
                    let (view, project) = (view.clone(), project.clone());
                    move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = view.update(cx, |this, cx| this.quick(project.clone(), Some(mode), window, cx));
                    }
                };
                let row = |title: &'static str, about: &'static str| move |_: &mut Window, cx: &mut App| v_flex().w(r(260.)).py(r(2.)).child(title).child(div().text_size(r(11.5)).text_color(cx.theme().muted_foreground).child(about));
                menu.label("Une autre session sur ce projet")
                    .item(PopupMenuItem::element(row("Dans sa propre copie", "Réintégrée quand elle a fini ; un Claude tranche les conflits")).on_click(add(WorkMode::Copy)))
                    .item(PopupMenuItem::element(row("Dans le même dossier", "Partagé : chacune réserve les fichiers qu'elle modifie")).on_click(add(WorkMode::Direct)))
                    .item(PopupMenuItem::element(row("Dans son propre worktree", if git { "Sa branche akolade/<nom>, fusionnée si les tests passent" } else { "Il faut un dépôt Git" })).disabled(!git).on_click(add(WorkMode::Isolated)))
            })
            .into_any_element()
    }

    /// A name no session has yet, from its project's: calc, calc-2, calc-3…
    fn auto_name(&self, project: &Path) -> String {
        let base = name_of(project);
        (1..).map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") }).find(|n| !self.slots.iter().any(|s| s.name == *n)).unwrap_or(base)
    }

    /// The editor's project, then the multiplexer's and the recent ones (6 at most).
    fn recent(&self, cx: &App) -> Vec<PathBuf> {
        let mut recent: Vec<PathBuf> = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf)).into_iter().collect();
        for p in self.projects().into_iter().chain(AppSettings::get(cx).recent_folders.iter().cloned()) {
            if !recent.contains(&p) && p.is_dir() {
                recent.push(p);
            }
        }
        recent.truncate(6);
        recent
    }

    /// Why the form's project cannot take this session now (None: it can). Several sessions in one
    /// folder are fine: the leases keep them off each other's files.
    fn refusal(&self, form: &Form) -> Option<String> {
        let folder = form.folder.as_ref()?;
        if form.mode == WorkMode::Isolated && crate::git::toplevel(folder).is_none() {
            return Some("Une session Isolée travaille dans un worktree Git : ce dossier n'est pas un dépôt.".into());
        }
        None
    }

    fn git_init(&mut self, cx: &mut Context<Self>) {
        let Some(folder) = self.form.as_ref().and_then(|f| f.folder.clone()) else { return };
        let _ = crate::git::git(&folder, &["init", "-q"]);
        self.set_folder(folder, cx);
    }

    /// The form's session, started (its first instruction sent, if any).
    fn launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = self.form.as_ref() else { return };
        if self.refusal(form).is_some() {
            return;
        }
        let Some(folder) = form.folder.clone() else { return };
        let read = |i: &Entity<InputState>, cx: &Context<Self>| i.read(cx).value().trim().to_string();
        let project = project::project_of(&folder);
        let name = Some(read(&form.name, cx)).filter(|n| !n.is_empty()).unwrap_or_else(|| self.auto_name(&project));
        let (role, task) = (read(&form.role, cx), read(&form.task, cx));
        let budget = read(&form.budget, cx).replace(',', ".").parse::<f64>().ok().filter(|b| *b > 0.);
        let (mode, model, perm, effort) = (form.mode, form.model, form.perm, form.effort);
        // Never an id whose copy, worktree or branch is still there (kept for its work, after a
        // restart the count starts again): the new session would replace it, or start on it.
        let taken = |id: &str| {
            copy::dir_for(&copy::base(), &project, id).exists()
                || worktree::dir_for(&worktree::base(), &project, id).exists()
                || (mode == WorkMode::Isolated && crate::git::git(&project, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", worktree::branch_for(id))]).is_ok())
        };
        let mut id = format!("{}-{}", project::slug(&name), self.next_id);
        while taken(&id) {
            self.next_id += 1;
            id = format!("{}-{}", project::slug(&name), self.next_id);
        }
        self.next_id += 1;
        let launch = Launch { id, name, role, task, project, mode, model, perm, effort, budget, conversation: None, flat: !self.by_project, terminal: None };
        self.form = None;
        if mode == WorkMode::Direct {
            return self.start(launch, folder, None, window, cx);
        }
        // Its worktree or its copy first (off the UI thread), in the same subfolder as picked.
        let rel = folder.strip_prefix(&launch.project).map(Path::to_path_buf).unwrap_or_default();
        let (project, slug) = (launch.project.clone(), launch.id.clone());
        self.starting.push(launch.name.clone());
        cx.notify();
        let made = cx.background_executor().spawn(async move { if mode == WorkMode::Copy { copy::create(&copy::base(), &project, &slug) } else { worktree::create(&worktree::base(), &project, &slug) } });
        cx.spawn_in(window, async move |this, cx| {
            let made = made.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.starting.retain(|n| *n != launch.name);
                match made {
                    Ok(wt) => this.start(launch, wt.join(rel), Some(wt), window, cx),
                    Err(e) => {
                        window.push_notification(gpui_component::notification::Notification::error(format!("Impossible de préparer le dossier de « {} » : {e}", launch.name)), cx);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// A session in `cwd` (its worktree's, if Isolated), its first instruction sent.
    fn start(&mut self, l: Launch, cwd: PathBuf, worktree: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let branch = worktree.as_ref().filter(|_| l.mode == WorkMode::Isolated).map(|_| worktree::branch_for(&l.id));
        // Its paths are relative to its checkout: its worktree, or the project.
        let checkout = crate::platform::canonical(worktree.as_deref().unwrap_or(&l.project)).unwrap_or_else(|_| worktree.clone().unwrap_or_else(|| l.project.clone()));
        // Known to the coordinator first: its instructions name the sessions it works with.
        coord::with(|c| {
            c.add(coord::SessionInfo {
                id: l.id.clone(),
                name: l.name.clone(),
                role: l.role.clone(),
                project: l.project.clone(),
                cwd: cwd.clone(),
                checkout,
                mode: l.mode,
                branch: branch.clone(),
                conversation: l.conversation.clone(),
                model: l.model,
                perm: l.perm,
                effort: l.effort,
                budget: l.budget,
                flat: l.flat,
                terminal: l.terminal,
            })
        });
        let opts = SessionOptions {
            chrome: false,
            follow_editor: false,
            own_settings: true,
            effort: Some(EFFORTS[l.effort].0.to_string()),
            budget: l.budget,
            system_prompt: coord::with(|c| coord_prompt(c, &l.id)),
            // The coordination server, without touching the project's .claude/ (see docs/multiplexeur.md).
            extra_args: vec!["--mcp-config".into(), mcp::config(&l.id), "--allowedTools".into(), mcp::allowed_tools()],
            gate: Some(gate_for(l.id.clone())),
        };
        let workspace = self.workspace.clone();
        let (model, perm) = (l.model, l.perm);
        let terminal = l.terminal.unwrap_or(AppSettings::get(cx).mux_terminal);
        let session = cx.new(|cx| {
            let mut s = ClaudeSession::new(workspace, Some(cwd.clone()), opts, window, cx);
            s.configure(model, perm, cx);
            s.set_terminal(terminal, cx);
            s
        });
        let sub = cx.observe(&session, |this, session, cx| {
            let s = session.read(cx);
            let now = (s.state(), (s.cost() * 10_000.) as u64);
            let (conversation, choices) = (s.conversation().map(str::to_string), s.choices(cx));
            let Some(slot) = this.slots.iter_mut().find(|slot| slot.session == session) else { return };
            // Its conversation and its menu's choices, saved: after a restart it goes on as it was.
            let id = slot.id.clone();
            let changed = coord::with(|c| {
                let Some(info) = c.sessions.iter_mut().find(|i| i.id == id) else { return false };
                let before = (info.conversation.clone(), info.model, info.perm, info.effort);
                info.conversation = conversation.or(info.conversation.take());
                (info.model, info.perm, info.effort) = choices;
                before != (info.conversation.clone(), info.model, info.perm, info.effort)
            });
            if changed {
                coord::changed();
            }
            if slot.seen != now {
                slot.seen = now;
                cx.notify();
            }
        });
        let restored = l.conversation.is_some();
        if let Some(conv) = l.conversation.clone() {
            session.update(cx, |s, cx| s.resume(conv, cx));
        } else if !l.task.is_empty() {
            session.update(cx, |s, cx| s.submit(l.task, window, cx));
        }
        let project = l.project.clone();
        self.slots.push(Slot { id: l.id, name: l.name, role: l.role, project: l.project, cwd, mode: l.mode, branch, worktree, session, folded: false, paused: restored, flat: l.flat, terminal: l.terminal, seen: (SessionState::Idle, 0), _sub: sub });
        self.refresh_prompts(&project, cx);
        coord::changed();
        cx.notify();
    }

    /// The sessions of `project` read who they work with again (one joined or left).
    fn refresh_prompts(&mut self, project: &Path, cx: &mut Context<Self>) {
        for slot in self.slots.iter().filter(|s| s.project == project) {
            let prompt = coord::with(|c| coord_prompt(c, &slot.id));
            slot.session.update(cx, |s, _| s.set_system_prompt(prompt));
        }
    }

    /// The integration queues: each idle project takes its next branch, merged and tested
    /// off the UI thread; the outcome goes to the session (see `coord::finish_merge`).
    fn pump(&mut self, cx: &mut Context<Self>) {
        while let Some((project, req, name)) = coord::with(|c| c.next_merge()) {
            if req.branch.is_empty() {
                self.bring_back(project, req, name, cx);
                continue;
            }
            // Its build and tests are its own code: only in a project you trust.
            if !crate::trust::is_trusted(&project) {
                coord::with(|c| c.finish_merge(&project, false, "Le projet est en mode restreint : Akolade ne lance pas son build ni ses tests. Approuvez-le, puis redemandez la fusion.".into(), vec![], coord::now()));
                continue;
            }
            coord::changed();
            let base = worktree::base();
            let task = cx.background_executor().spawn({
                let project = project.clone();
                async move {
                    match queue::prepare(&base, &project) {
                        Ok(dir) => queue::integrate(&dir, &req, &name, queue::project_check),
                        Err(e) => (false, format!("Impossible de préparer akolade/integration : {e}")),
                    }
                }
            });
            cx.spawn(async move |_, _| {
                let (merged, report) = task.await;
                coord::with(|c| c.finish_merge(&project, merged, report, vec![], coord::now()));
                coord::changed();
            })
            .detach();
        }
    }

    /// A copy's work into its project's folder (off the UI thread). A file both changed is settled
    /// by Claude Code, given the session's summary and, for the project's side, what the copies
    /// brought back since this one started said of it.
    fn bring_back(&mut self, project: PathBuf, req: coord::MergeRequest, name: String, cx: &mut Context<Self>) {
        let Some(dir) = coord::with(|c| c.session(&req.session).map(|s| s.checkout.clone())) else {
            coord::with(|c| c.finish_merge(&project, false, "La session n'est plus là.".into(), vec![], coord::now()));
            return;
        };
        let since = copy::since(&dir);
        let history: Vec<(String, String, Vec<String>)> = coord::with(|c| {
            let outcomes = c.projects.get(&project::key(&project)).map(|p| p.outcomes.clone()).unwrap_or_default();
            outcomes.into_iter().filter(|o| o.merged && o.at >= since && o.session != req.session).map(|o| (c.name_of(&o.session), o.summary, o.files)).collect()
        });
        let program = claude::find_claude();
        coord::changed();
        let task = cx.background_executor().spawn({
            let project = project.clone();
            async move {
                let project_name = name_of(&project);
                let done = copy::merge(&project, &dir, |conflict| {
                    let Some(program) = &program else { return Err("Claude Code est introuvable : personne pour trancher.".into()) };
                    let why_project: Vec<String> = history.iter().filter(|h| h.2.contains(&conflict.path)).map(|h| format!("la session « {} » : {}", h.0, h.1)).collect();
                    let why_project = if why_project.is_empty() { "modifié dans le dossier du projet (par l'utilisateur, ou une session qui y travaille directement), sans explication connue.".to_string() } else { why_project.join(" ; ") };
                    copy::resolve_with_claude(program, conflict, &project_name, &name, &req.summary, &why_project)
                });
                (done.left.is_empty(), done.report(&project), done.files())
            }
        });
        cx.spawn(async move |_, _| {
            let (merged, report, files) = task.await;
            coord::with(|c| c.finish_merge(&project, merged, report, files, coord::now()));
            coord::changed();
        })
        .detach();
    }

    /// Your action: `akolade/integration` merged into the project's current branch (never pushed).
    fn merge_into_yours(&mut self, project: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let branch = crate::git::git(&project, &["rev-parse", "--abbrev-ref", "HEAD"]).map(|b| b.trim().to_string()).unwrap_or_default();
        let detail = format!("Les branches des sessions déjà fusionnées et testées dans akolade/integration rejoignent « {branch} » dans {}. Rien n'est poussé.", project.display());
        let answer = window.prompt(gpui::PromptLevel::Info, "Fusionner dans votre branche ?", Some(&detail), &["Fusionner", "Annuler"], cx);
        cx.spawn_in(window, async move |_, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let done = cx.background_executor().spawn(async move { crate::git::merge(&project, queue::BRANCH, "akolade : intégration des sessions du multiplexeur") }).await;
            let _ = cx.update(|window, cx| {
                let note = match done {
                    crate::git::Integration::Done => gpui_component::notification::Notification::success(format!("akolade/integration est fusionnée dans « {branch} ».")),
                    crate::git::Integration::Conflict(files) => gpui_component::notification::Notification::error(format!("Conflit avec votre branche sur {} : rien n'a été fusionné.", files.join(", "))),
                    crate::git::Integration::Failed(e) => gpui_component::notification::Notification::error(format!("Fusion impossible : {e}")),
                };
                window.push_notification(note, cx);
            });
        })
        .detach();
    }

    /// A shared project's integration queue: merging now, waiting, the last outcome (its report on demand).
    fn render_queue(&self, project: &Path, cx: &mut Context<Self>) -> Option<AnyElement> {
        let key = project::key(project);
        let state = coord::with(|c| c.projects.get(&key).cloned()).unwrap_or_default();
        let names = coord::with(|c| c.sessions.iter().map(|s| (s.id.clone(), s.name.clone())).collect::<Vec<_>>());
        let name = |id: &str| names.iter().find(|(i, _)| i == id).map_or(id.to_string(), |(_, n)| n.clone());
        let has_branch = queue::dir(&worktree::base(), project).join(".git").exists();
        if state.queue.is_empty() && state.merging.is_none() && state.outcomes.is_empty() && !has_branch {
            return None;
        }
        let theme = cx.theme();
        let (muted, green, red, fg) = (theme.muted_foreground, theme.green, theme.red, theme.foreground);
        let last = state.outcomes.last().cloned();
        let open = self.report.as_deref() == Some(key.as_str());
        let project = project.to_path_buf();
        Some(
            v_flex()
                .flex_shrink_0()
                .gap(r(4.))
                .px(r(4.))
                .pb(r(6.))
                .text_size(r(11.5))
                .child(
                    h_flex()
                        .gap(r(8.))
                        .child(div().text_color(muted).child("Intégration :"))
                        .when_some(state.merging.as_ref(), |d, m| d.child(h_flex().gap(r(4.)).child(gpui_component::spinner::Spinner::new().xsmall()).child(format!("{} ({})", name(&m.session), if m.branch.is_empty() { "réintégration" } else { "fusion et tests" }))))
                        .when(!state.queue.is_empty(), |d| d.child(div().text_color(muted).child(format!("en attente : {}", state.queue.iter().map(|r| name(&r.session)).collect::<Vec<_>>().join(", ")))))
                        .when_some(last.clone(), |d, o| {
                            let k = key.clone();
                            d.child(
                                div()
                                    .id(SharedString::from(format!("mux-last-{key}")))
                                    .cursor_pointer()
                                    .text_color(if o.merged { green } else { red })
                                    .child(format!("{} {} {}", if o.merged { "✓" } else { "✗" }, name(&o.session), match (o.branch.is_empty(), o.merged) {
                                        (true, true) => "réintégrée",
                                        (true, false) => "en partie réintégrée",
                                        (false, true) => "fusionnée",
                                        (false, false) => "renvoyée",
                                    }))
                                    .tooltip(|window, cx| Tooltip::new("Voir le rapport").build(window, cx))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.report = if this.report.as_deref() == Some(k.as_str()) { None } else { Some(k.clone()) };
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(div().flex_1())
                        .when(has_branch, |d| {
                            d.child(button(SharedString::from(format!("mux-merge-{key}"))).xsmall().outline().label("Fusionner dans votre branche").tooltip("akolade/integration rejoint votre branche courante, sur votre action (jamais de push)").on_click(cx.listener(move |this, _, window, cx| this.merge_into_yours(project.clone(), window, cx))))
                        }),
                )
                .when_some(last.filter(|_| open), |d, o| {
                    d.child(div().id(SharedString::from(format!("mux-report-{key}"))).max_h(r(160.)).overflow_y_scroll().p(r(8.)).rounded(r(6.)).bg(fg.opacity(0.04)).font_family(cx.theme().mono_font_family.clone()).child(o.report))
                })
                .into_any_element(),
        )
    }

    /// Messages for the sessions: given to them at once (between two steps of a turn, or as a
    /// new one), as written to them; a paused session gets its own at "Reprendre".
    fn deliver(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for slot in self.slots.iter().filter(|s| !s.paused) {
            let text = coord::with(|c| {
                let msgs = c.inbox(&slot.id);
                msgs.iter().map(|m| c.render(m)).collect::<Vec<_>>().join("\n\n")
            });
            if !text.is_empty() {
                slot.session.update(cx, |s, cx| s.submit(text, window, cx));
            }
        }
    }

    /// Its turn stops; the conversation stays (resumed by `resume`).
    fn pause(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(slot) = self.slots.get_mut(ix) {
            slot.paused = true;
            slot.session.update(cx, |s, cx| s.interrupt(cx));
            cx.notify();
        }
    }

    fn resume(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(slot) = self.slots.get_mut(ix) {
            slot.paused = false;
            slot.session.update(cx, |s, cx| s.submit("Reprends là où tu t'étais arrêté.".into(), window, cx));
            // The messages it got meanwhile.
            self.deliver(window, cx);
            cx.notify();
        }
    }

    /// The session ends: its `claude` and what it runs stop (`Proc`'s drop).
    fn close(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.slots.len() {
            let slot = self.slots.remove(ix);
            coord::with(|c| c.remove(&slot.id));
            self.refresh_prompts(&slot.project.clone(), cx);
            coord::changed();
            // Its worktree goes once `claude` has stopped (its branch stays: nothing committed is lost).
            // A copy goes unless it holds work not brought back: then it stays, and says where.
            match (slot.mode, slot.worktree.clone()) {
                (WorkMode::Copy, Some(dir)) => {
                    let (name, project) = (slot.name.clone(), slot.project.clone());
                    drop(slot);
                    let kept = cx.background_executor().spawn({
                        let dir = dir.clone();
                        async move {
                            let kept = copy::has_changes(&dir);
                            if !kept {
                                copy::remove(&dir);
                            }
                            kept
                        }
                    });
                    cx.spawn(async move |_, _| {
                        if kept.await {
                            coord::with(|c| c.note(Some(&project), format!("La copie de {name} garde du travail non réintégré : {}", dir.display()), coord::now()));
                            coord::changed();
                        }
                    })
                    .detach();
                }
                (_, Some(wt)) => {
                    drop(slot);
                    cx.background_executor().spawn(async move { worktree::remove(&wt) }).detach();
                }
                _ => {}
            }
            cx.notify();
        }
    }

    fn render_form(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let form = self.form.as_ref()?;
        let theme = cx.theme();
        let (muted, border, fg, red) = (theme.muted_foreground, theme.border, theme.foreground, theme.red);
        let label = |text: &'static str| div().w(r(110.)).flex_shrink_0().text_size(r(12.5)).text_color(muted).child(text);
        let row = |l: &'static str, content: AnyElement| h_flex().gap(r(10.)).items_center().child(label(l)).child(div().flex_1().min_w_0().child(content));
        // The editor's project and the recent ones, then any folder.
        let recent = self.recent(cx);
        let shown = recent.clone();
        let folder = h_flex()
            .gap(r(6.))
            .flex_wrap()
            .children(recent.into_iter().enumerate().map(|(i, p)| {
                let on = form.folder.as_ref() == Some(&p);
                let path = p.clone();
                button(SharedString::from(format!("mux-recent-{i}"))).small().map(|b| if on { b.primary() } else { b.outline() }).label(name_of(&p)).tooltip(p.display().to_string()).on_click(cx.listener(move |this, _, _, cx| this.set_folder(path.clone(), cx)))
            }))
            .child({
                // A folder from elsewhere: its name, the whole path in the tooltip (a long one would push the form down).
                let other = form.folder.clone().filter(|f| !shown.contains(f));
                button("mux-pick")
                    .small()
                    .map(|b| if other.is_some() { b.primary() } else { b.ghost() })
                    .icon(IconName::FolderOpen)
                    .label(other.as_deref().map_or_else(|| "Autre dossier…".into(), name_of))
                    .tooltip(other.as_deref().map_or_else(|| "Choisir un dossier".into(), |p| p.display().to_string()))
                    .on_click(cx.listener(|this, _, window, cx| this.pick_folder(false, window, cx)))
            });
        let seg = |id: &'static str, labels: Vec<&'static str>, on: usize, pick: fn(&mut Form, usize), cx: &mut Context<Self>| {
            h_flex().gap(r(2.)).p(r(2.)).rounded(r(8.)).bg(cx.theme().foreground.opacity(0.05)).children(labels.into_iter().enumerate().map(|(i, l)| {
                button(SharedString::from(format!("{id}-{i}"))).small().map(|b| if i == on { b.primary() } else { b.ghost() }).label(l).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(f) = &mut this.form {
                        pick(f, i);
                    }
                    cx.notify();
                }))
            }))
        };
        let refusal = self.refusal(form);
        let no_git = form.folder.as_ref().is_some_and(|f| crate::git::toplevel(f).is_none());
        const FORM_MODES: [WorkMode; 3] = [WorkMode::Direct, WorkMode::Copy, WorkMode::Isolated];
        let mode = seg("mux-mode", FORM_MODES.iter().map(|m| m.label()).collect(), FORM_MODES.iter().position(|m| *m == form.mode).unwrap_or(0), |f, i| f.mode = FORM_MODES[i], cx);
        let mode_about = match form.mode {
            WorkMode::Direct => "Dans le dossier du projet, partagé avec les sessions qui y sont : chacune réserve les fichiers qu'elle modifie.",
            WorkMode::Isolated => "Dans son propre worktree Git, sur la branche akolade/<nom> : rien ne touche votre copie.",
            WorkMode::Copy => "Dans sa propre copie du dossier (sans Git), réintégrée quand elle a fini : un Claude tranche les fichiers modifiés des deux côtés.",
        };
        let models = self.render_model_picker(form.model, form.effort, cx);
        let perms = seg("mux-perm", MODES.iter().map(|m| m.0).collect(), form.perm, |f, i| f.perm = i, cx);
        let ready = form.folder.is_some() && refusal.is_none();
        Some(
            v_flex()
                .flex_shrink_0()
                .mx(r(24.))
                .mt(r(16.))
                .p(r(16.))
                .gap(r(10.))
                .rounded(r(12.))
                .border_1()
                .border_color(border)
                .bg(fg.opacity(0.015))
                .child(div().text_size(r(14.)).font_weight(FontWeight::SEMIBOLD).child("Nouvelle session"))
                .child(row("Projet", folder.into_any_element()))
                .child(row("", Input::new(&form.path).small().into_any_element()))
                .when(form.more, |d| {
                    d.child(row("Nom", Input::new(&form.name).small().into_any_element()))
                        .child(row("Rôle", Input::new(&form.role).small().into_any_element()))
                        .child(row("Tâche", Input::new(&form.task).small().into_any_element()))
                        .child(row("Mode de travail", h_flex().gap(r(10.)).child(mode).child(div().text_size(r(12.)).text_color(muted).child(mode_about)).into_any_element()))
                        .child(row("Modèle", models))
                        .child(row("Autorisations", perms.into_any_element()))
                        .child(row("Budget ($)", div().w(r(140.)).child(Input::new(&form.budget).small()).into_any_element()))
                })
                .when_some(refusal, |d, why| {
                    d.child(
                        h_flex()
                            .gap(r(10.))
                            .p(r(10.))
                            .rounded(r(8.))
                            .bg(red.opacity(0.08))
                            .text_size(r(12.5))
                            .text_color(red)
                            .child(div().flex_1().child(why))
                            .when(no_git, |d| d.child(button("mux-git-init").small().outline().label("Initialiser Git").on_click(cx.listener(|this, _, _, cx| this.git_init(cx))))),
                    )
                })
                .child(
                    h_flex()
                        .gap(r(8.))
                        .child(
                            button("mux-more")
                                .small()
                                .ghost()
                                .label(if form.more { "Moins d'options" } else { "Plus d'options : nom, rôle, tâche, mode, modèle…" })
                                .icon(Icon::new(if form.more { IconName::ChevronUp } else { IconName::ChevronDown }).xsmall())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(f) = &mut this.form {
                                        f.more = !f.more;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(div().flex_1())
                        .child(button("mux-cancel").small().ghost().label("Annuler").on_click(cx.listener(|this, _, _, cx| {
                            this.form = None;
                            cx.notify();
                        })))
                        .child(button("mux-launch").small().primary().icon(IconName::Plus).label("Lancer la session").disabled(!ready).on_click(cx.listener(|this, _, window, cx| this.launch(window, cx)))),
                )
                .into_any_element(),
        )
    }

    /// What you type in "Échanges", to its recipients (pushed to them at once, see `deliver`).
    fn send_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.compose.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let (ids, label): (Vec<String>, String) = match &self.to {
            Recipient::All => (self.slots.iter().map(|s| s.id.clone()).collect(), "toutes les sessions".into()),
            Recipient::Project(p) => (self.slots.iter().filter(|s| s.project == *p).map(|s| s.id.clone()).collect(), format!("projet {}", name_of(p))),
            Recipient::Session(id) => (vec![id.clone()], self.slots.iter().find(|s| s.id == *id).map_or(id.clone(), |s| s.name.clone())),
        };
        if ids.is_empty() {
            return;
        }
        coord::with(|c| c.send_from_user(ids, label, &text, coord::now()));
        self.compose.update(cx, |i, cx| i.set_value("", window, cx));
        coord::changed();
    }

    /// The thread of messages and coordination events, newest last, filtered on a project.
    fn render_exchanges(&self, cx: &mut Context<Self>) -> AnyElement {
        let (messages, notes, names) = coord::with(|c| (c.messages.clone(), c.notes.clone(), c.sessions.iter().map(|s| (s.id.clone(), s.name.clone(), s.project.clone())).collect::<Vec<_>>()));
        let who = |id: &str| match id {
            "vous" => "Vous".to_string(),
            "akolade" => "Akolade".to_string(),
            id => names.iter().find(|(i, _, _)| i == id).map_or(id.to_string(), |(_, n, _)| n.clone()),
        };
        let filter = self.filter.clone();
        let concerns = |project: &Option<PathBuf>, ids: &[String]| match &filter {
            None => true,
            Some(f) => project.as_ref() == Some(f) || ids.iter().any(|id| names.iter().any(|(i, _, p)| i == id && p == f)),
        };
        // (time, line, is a message)
        let mut lines: Vec<(u64, String, bool)> = messages
            .iter()
            .filter(|m| concerns(&m.project, std::slice::from_ref(&m.from)) || filter.as_ref().is_some_and(|f| names.iter().any(|(_, n, p)| *n == m.to && p == f)))
            .map(|m| (m.at, format!("{} → {} : {}", who(&m.from), m.to, m.text), true))
            .collect();
        lines.extend(notes.iter().filter(|n| concerns(&n.project, &[])).map(|n| (n.at, n.text.clone(), false)));
        lines.sort_by_key(|l| l.0);
        let lines = lines.split_off(lines.len().saturating_sub(200));
        let theme = cx.theme();
        let (muted, border, fg) = (theme.muted_foreground, theme.border, theme.foreground);
        let chip = |id: SharedString, label: String, on: bool| button(id).xsmall().map(|b| if on { b.primary() } else { b.ghost() }).label(label);
        let projects = self.projects();
        let filters = h_flex()
            .flex_wrap()
            .gap(r(4.))
            .child(chip("mux-filter-all".into(), "Tous".into(), self.filter.is_none()).on_click(cx.listener(|this, _, _, cx| {
                this.filter = None;
                cx.notify();
            })))
            .children(projects.iter().enumerate().map(|(i, p)| {
                let p2 = p.clone();
                chip(format!("mux-filter-{i}").into(), name_of(p), self.filter.as_ref() == Some(p)).on_click(cx.listener(move |this, _, _, cx| {
                    this.filter = Some(p2.clone());
                    cx.notify();
                }))
            }));
        let mut targets: Vec<(Recipient, String)> = vec![(Recipient::All, "Toutes".into())];
        targets.extend(projects.iter().map(|p| (Recipient::Project(p.clone()), format!("Projet {}", name_of(p)))));
        targets.extend(self.slots.iter().map(|s| (Recipient::Session(s.id.clone()), s.name.clone())));
        let to = h_flex().flex_wrap().gap(r(4.)).child(div().text_size(r(11.5)).text_color(muted).child("À :")).children(targets.into_iter().enumerate().map(|(i, (t, label))| {
            let on = self.to == t;
            chip(format!("mux-to-{i}").into(), label, on).on_click(cx.listener(move |this, _, _, cx| {
                this.to = t.clone();
                cx.notify();
            }))
        }));
        v_flex()
            .w(r(320.))
            .flex_shrink_0()
            .h_full()
            .border_l_1()
            .border_color(border)
            .child(v_flex().flex_shrink_0().gap(r(8.)).p(r(12.)).border_b_1().border_color(border).child(ui::section_title("Échanges", cx)).child(filters))
            .child(
                v_flex()
                    .id("mux-thread")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(r(12.))
                    .gap(r(6.))
                    .text_size(r(12.))
                    .when(lines.is_empty(), |d| d.child(div().text_color(muted).child("Les messages entre sessions et les événements de coordination (réservations, fusions) s'affichent ici.")))
                    .children(lines.into_iter().map(|(at, text, message)| {
                        let time = chrono::DateTime::from_timestamp(at as i64, 0).map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string()).unwrap_or_default();
                        h_flex().flex_shrink_0().items_start().gap(r(6.)).child(div().flex_shrink_0().text_color(muted).child(time)).child(div().min_w_0().text_color(if message { fg } else { muted }).child(text))
                    })),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .gap(r(6.))
                    .p(r(12.))
                    .border_t_1()
                    .border_color(border)
                    .child(to)
                    .child(h_flex().w_full().gap(r(6.)).child(Input::new(&self.compose).small().flex_1()).child(button("mux-send").small().primary().icon(IconName::ArrowUp).tooltip("Envoyer : le message arrive aussitôt aux sessions").on_click(cx.listener(|this, _, window, cx| this.send_compose(window, cx))))),
            )
            .into_any_element()
    }

    /// The "Statistiques" panel: the multiplexer's sessions (both views), then Claude Code's
    /// own figures, every use included (its `/stats`).
    fn render_stats(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, fg) = (theme.muted_foreground, theme.border, theme.foreground);
        let orange: Hsla = gpui::rgb(0xD97757).into();
        let tile = |value: String, name: &'static str| {
            v_flex().flex_1().min_w_0().p(r(10.)).rounded(r(8.)).bg(fg.opacity(0.04)).child(div().text_size(r(15.)).font_weight(FontWeight::SEMIBOLD).child(value)).child(div().text_size(r(11.5)).text_color(muted).child(name))
        };
        let row = |name: AnyElement, value: String| h_flex().gap(r(8.)).text_size(r(12.5)).child(div().flex_1().min_w_0().truncate().child(name)).child(div().flex_shrink_0().text_color(muted).child(value));
        let (cost, used) = self.slots.iter().map(|s| s.session.read(cx)).fold((0., 0), |(c, t), s| (c + s.cost(), t + s.tokens_used()));
        let mut panel = v_flex()
            .id("mux-stats-panel")
            .w(r(320.))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(border)
            .p(r(16.))
            .gap(r(10.))
            .child(ui::section_title("Sessions du multiplexeur", cx))
            .child(h_flex().gap(r(8.)).child(tile(claude::usd(cost), "Coût estimé")).child(tile(claude::tokens(used), "Tokens")))
            .children(self.slots.iter().map(|slot| {
                let s = slot.session.read(cx);
                let name = h_flex().gap(r(6.)).child(div().size(r(8.)).flex_shrink_0().rounded_full().bg(self.color_of(&slot.project, cx))).child(slot.name.clone()).when(slot.flat != !self.by_project, |d| d.child(div().text_color(muted).child(if slot.flat { "· à plat" } else { "· par projet" })));
                row(name.into_any_element(), format!("{} · {}", claude::tokens(s.tokens_used()), claude::usd(s.cost())))
            }));
        let Some(s) = &self.cc_stats else {
            return panel
                .child(ui::section_title("Claude Code", cx).mt(r(8.)))
                .child(div().text_size(r(12.5)).text_color(muted).child("Claude Code n'a pas encore calculé ses statistiques : tapez /stats une fois dans Claude Code (le terminal), puis rouvrez ce panneau."))
                .into_any_element();
        };
        let days = stats::last_days(s, 30);
        let top = days.iter().map(|d| d.1).max().unwrap_or(0);
        let mut models: Vec<(&String, u64)> = s.model_usage.iter().map(|(m, u)| (m, u.tokens())).collect();
        models.sort_by_key(|m| std::cmp::Reverse(m.1));
        panel = panel
            .child(ui::section_title("Claude Code, tous usages", cx).mt(r(8.)))
            .child(div().text_size(r(11.5)).text_color(muted).child(format!("Calculées par Claude Code le {} (sa commande /stats), depuis le {}.", stats::fr_date(&s.last_computed_date), stats::fr_date(&s.first_session_date))))
            .child(h_flex().gap(r(8.)).child(tile(s.total_sessions.to_string(), "Sessions")).child(tile(s.total_messages.to_string(), "Messages")))
            .child(h_flex().gap(r(8.)).child(tile(s.favorite_hour().map_or("—".into(), |h| format!("{h} h")), "Heure favorite")).child(tile(stats::duration(s.longest_session.duration), "Plus longue session")))
            .child(div().text_size(r(12.5)).mt(r(4.)).child("Messages, 30 jours"))
            .child(h_flex().flex_shrink_0().h(r(56.)).items_end().gap(r(2.)).children(days.into_iter().map(|(d, messages, tools)| {
                let part = if top > 0 { messages as f32 / top as f32 } else { 0. };
                div()
                    .id(SharedString::from(format!("mux-day-{d}")))
                    .flex_1()
                    .h(relative(part.max(0.04)))
                    .rounded_t(r(2.))
                    .bg(if messages > 0 { orange } else { fg.opacity(0.08) })
                    .tooltip(move |window, cx| Tooltip::new(format!("{} : {messages} messages · {tools} outils", stats::fr_date(&d))).build(window, cx))
            })))
            .child(h_flex().text_size(r(11.)).text_color(muted).child(div().flex_1().child("il y a 30 jours")).child(stats::fr_date(&s.last_computed_date)));
        if !models.is_empty() {
            panel = panel.child(div().text_size(r(12.5)).mt(r(4.)).child("Tokens par modèle")).children(models.into_iter().map(|(m, n)| row(div().child(stats::model_label(m)).into_any_element(), claude::tokens(n))));
        }
        panel.into_any_element()
    }

    /// "À plat": a Claude Code session in one click, on a project of the menu, without the form.
    fn render_quick_menu(&self, id: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let recent = self.recent(cx);
        let view = cx.entity().downgrade();
        type Act = fn(&mut MuxView, Option<PathBuf>, &mut Window, &mut Context<MuxView>);
        let on = move |act: Act, arg: Option<PathBuf>| {
            let view = view.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |this, cx| act(this, arg.clone(), window, cx));
            }
        };
        button(id)
            // Small in the header, full size in the empty view: as the "Par projet" view's buttons.
            .map(|b| if id == "mux-new" { b.small() } else { b })
            .primary()
            .icon(IconName::Plus)
            .label("Nouvelle session")
            .disabled(self.form.is_some())
            .dropdown_menu_with_anchor(if id == "mux-new" { gpui::Corner::TopRight } else { gpui::Corner::TopLeft }, move |mut menu, _, _| {
                menu = menu.min_w(gpui::px(300.)).max_w(gpui::px(380.)).label("Lancer Claude Code dans…");
                for p in &recent {
                    let (name, path) = (name_of(p), p.display().to_string().replacen(&crate::platform::home().display().to_string(), "~", 1));
                    menu = menu.item(
                        PopupMenuItem::element(move |_, cx: &mut App| v_flex().py(r(2.)).child(div().child(name.clone())).child(div().w(r(300.)).truncate().text_size(r(11.5)).text_color(cx.theme().muted_foreground).child(path.clone())))
                            .on_click(on(|this, p, window, cx| this.quick(p.unwrap_or_default(), None, window, cx), Some(p.clone()))),
                    );
                }
                menu.separator()
                    .item(PopupMenuItem::new("Autre dossier…").icon(IconName::FolderOpen).on_click(on(|this, _, window, cx| this.pick_folder(true, window, cx), None)))
                    .item(PopupMenuItem::new("Avec options…").icon(IconName::Settings).on_click(on(|this, _, window, cx| this.open_form(window, cx), None)))
            })
            .into_any_element()
    }

    /// The form's model and thinking effort, in the menu of the panels (and Claude's chat).
    fn render_model_picker(&self, model: usize, effort: usize, cx: &mut Context<Self>) -> AnyElement {
        let name = MODELS[model].0;
        let muted = cx.theme().muted_foreground;
        let label = h_flex().gap(r(6.)).child(name).when(effort > 0, |d| d.child(div().text_color(muted).child(EFFORTS[effort].1)));
        let view = cx.entity().downgrade();
        let pick: claude::OnPick = std::rc::Rc::new(move |p, _, cx| {
            let _ = view.update(cx, |this, cx| {
                if let Some(f) = &mut this.form {
                    match p {
                        claude::Pick::Model(i) => f.model = i,
                        claude::Pick::Effort(i) => f.effort = i,
                        claude::Pick::Mode(i) => f.perm = i,
                    }
                }
                cx.notify();
            });
        });
        // Its own width, the arrow after the words (the row would stretch it).
        h_flex()
            .child(button("mux-model").small().outline().child(label).dropdown_menu(move |menu, window, cx| claude::model_menu(menu, model, effort, pick.clone(), window, cx)))
            .into_any_element()
    }

    /// One session's look (`only`), or every session's (the multiplexer's, their own ones dropped).
    fn set_terminal(&mut self, only: Option<usize>, on: bool, cx: &mut Context<Self>) {
        match only {
            Some(ix) => {
                if let Some(slot) = self.slots.get_mut(ix) {
                    slot.terminal = Some(on);
                }
            }
            None => {
                AppSettings::update(cx, |s| s.mux_terminal = on);
                self.slots.iter_mut().for_each(|s| s.terminal = None);
            }
        }
        let all = AppSettings::get(cx).mux_terminal;
        for slot in &self.slots {
            let own = slot.terminal;
            slot.session.update(cx, |s, cx| s.set_terminal(own.unwrap_or(all), cx));
            coord::with(|c| c.sessions.iter_mut().filter(|i| i.id == slot.id).for_each(|i| i.terminal = own));
        }
        coord::changed();
        cx.notify();
    }

    /// `from` dropped on `to`: it takes its place (kept across restarts, the coordinator's order).
    fn move_slot(&mut self, from: &str, to: &str, cx: &mut Context<Self>) {
        let pos = |id: &str| self.slots.iter().position(|s| s.id == id);
        let (Some(a), Some(b)) = (pos(from), pos(to)) else { return };
        if a == b {
            return;
        }
        let slot = self.slots.remove(a);
        self.slots.insert(b, slot);
        let order: Vec<String> = self.slots.iter().map(|s| s.id.clone()).collect();
        coord::with(|c| c.sessions.sort_by_key(|i| order.iter().position(|id| *id == i.id).unwrap_or(usize::MAX)));
        coord::changed();
        cx.notify();
    }

    /// "Affichage": how the panels are laid out, and every conversation's look.
    fn render_display_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let (current, terminal) = (layout(cx), AppSettings::get(cx).mux_terminal);
        let mixed = self.slots.iter().any(|s| s.terminal.is_some_and(|t| t != terminal));
        let view = cx.entity().downgrade();
        button("mux-display")
            .small()
            .ghost()
            .icon(Icon::new(IconName::Eye))
            .tooltip("Affichage : disposition des sessions et style des discussions")
            .dropdown_menu_with_anchor(gpui::Corner::TopRight, move |mut menu, _, _| {
                menu = menu.min_w(gpui::px(240.)).label("Disposition des sessions");
                for (l, key, name) in LAYOUTS {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(name).checked(l == current).on_click(move |_, _, cx| {
                        AppSettings::update(cx, |s| s.mux_layout = key.into());
                        let _ = view.update(cx, |_, cx| cx.notify());
                    }));
                }
                menu = menu.separator().label(if mixed { "Style de toutes les discussions (certaines ont le leur)" } else { "Style de toutes les discussions" });
                for (on, name) in [(false, "Standard"), (true, "Terminal")] {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(name).checked(!mixed && on == terminal).on_click(move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.set_terminal(None, on, cx));
                    }));
                }
                menu
            })
            .into_any_element()
    }

    fn render_add(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let slot = &self.slots[ix];
        self.add_button(SharedString::from(format!("mux-add-{}", slot.id)), slot.project.clone(), None, cx)
    }

    /// A session's bar: who, where, how, what it does and costs, and its buttons.
    fn render_bar(&self, ix: usize, flat: bool, cx: &mut Context<Self>) -> AnyElement {
        let slot = &self.slots[ix];
        let s = slot.session.read(cx);
        let (state, cost, blocked) = (s.state(), s.cost(), s.blocked().map(str::to_string));
        let color = self.color_of(&slot.project, cx);
        let theme = cx.theme();
        let (muted, border, yellow, red, green) = (theme.muted_foreground, theme.border, theme.yellow, theme.red, theme.green);
        let (state_label, state_color) = match state {
            SessionState::Idle => ("Inactive", muted),
            SessionState::Working => ("En cours", green),
            SessionState::Waiting => ("Attend votre accord", yellow),
            SessionState::Blocked => ("Bloquée par un bail", yellow),
            SessionState::Error => ("Erreur", red),
        };
        let pill = |text: String, c: Hsla| div().flex_shrink_0().px(r(6.)).py(r(1.)).rounded(r(4.)).bg(c.opacity(0.14)).text_color(c).text_size(r(11.)).child(text);
        let working = state == SessionState::Working;
        // One line: who, its state when it says something, its cost, its buttons; how and where it
        // works in the name's tooltip.
        // Paused: « Reprendre » says it already.
        let state = (!slot.paused && state != SessionState::Idle).then_some((state_label, state_color));
        let tip = [
            Some(match &slot.branch {
                Some(b) => format!("{}, branche {b}", slot.mode.label()),
                None => slot.mode.label().into(),
            }),
            (!slot.role.is_empty()).then(|| format!("Rôle : {}", slot.role)),
            Some(format!("Travaille dans {}", slot.cwd.display())),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
        let view = cx.entity().downgrade();
        let (folded, wt) = (slot.folded, slot.worktree.clone().filter(|_| !working && slot.mode == WorkMode::Isolated));
        let terminal = slot.terminal.unwrap_or(AppSettings::get(cx).mux_terminal);
        let bar = h_flex()
            .gap(r(6.))
            .min_w_0()
            .child(div().size(r(8.)).flex_shrink_0().rounded_full().bg(color))
            .child(div().id(SharedString::from(format!("mux-name-{}", slot.id))).flex_shrink_0().max_w(r(160.)).truncate().text_size(r(13.)).font_weight(FontWeight::SEMIBOLD).child(slot.name.clone()).tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx)))
            .when(flat, |d| d.child(div().flex_shrink_0().text_size(r(12.)).text_color(color).child(name_of(&slot.project))))
            .when_some(state, |d, (label, c)| d.child(div().id(SharedString::from(format!("mux-state-{}", slot.id))).min_w_0().overflow_hidden().whitespace_nowrap().child(pill(label.into(), c)).when_some(blocked, |d, why| d.tooltip(move |window, cx| Tooltip::new(why.clone()).build(window, cx)))))
            .child(div().flex_1())
            .when(cost > 0., |d| d.child(div().flex_shrink_0().text_size(r(12.)).text_color(muted).child(claude::usd(cost))))
            .map(|d| {
                if slot.paused {
                    d.child(button(SharedString::from(format!("mux-resume-{}", slot.id))).ghost().xsmall().label("Reprendre").tooltip("Elle reprend là où elle s'était arrêtée").on_click(cx.listener(move |this, _, window, cx| this.resume(ix, window, cx))))
                } else if working {
                    d.child(button(SharedString::from(format!("mux-pause-{}", slot.id))).ghost().xsmall().label("Pause").tooltip("Interrompt son tour ; « Reprendre » la relance").on_click(cx.listener(move |this, _, _, cx| this.pause(ix, cx))))
                } else {
                    d
                }
            })
            .child(self.render_add(ix, cx))
            .child(button(SharedString::from(format!("mux-more-{}", slot.id))).ghost().xsmall().icon(Icon::new(IconName::Ellipsis).xsmall()).dropdown_menu_with_anchor(gpui::Corner::TopRight, move |menu, _, _| {
                let (view, fold_view) = (view.clone(), view.clone());
                let fold = move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = fold_view.update(cx, |this, cx| {
                        if let Some(slot) = this.slots.get_mut(ix) {
                            slot.folded = !slot.folded;
                        }
                        cx.notify();
                    });
                };
                let look = |on: bool| {
                    let view = view.clone();
                    move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                        let _ = view.update(cx, |this, cx| this.set_terminal(Some(ix), on, cx));
                    }
                };
                menu.item(PopupMenuItem::new(if folded { "Déplier" } else { "Replier" }).on_click(fold))
                    .when_some(wt.clone(), |m, wt| m.item(PopupMenuItem::new("Rebaser sur akolade/integration").on_click(move |_, window, cx| rebase(wt.clone(), window, cx))))
                    .separator()
                    .label("Style de cette discussion")
                    .item(PopupMenuItem::new("Standard").checked(!terminal).on_click(look(false)))
                    .item(PopupMenuItem::new("Terminal").checked(terminal).on_click(look(true)))
            }))
            .child(
                button(SharedString::from(format!("mux-close-{}", slot.id)))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Close).xsmall())
                    .tooltip("Arrêter et fermer la session")
                    .on_click(cx.listener(move |this, _, _, cx| this.close(ix, cx))),
            );
        // Dragged by its bar onto another panel: they swap places.
        let drag = PanelDrag { id: slot.id.clone(), name: slot.name.clone() };
        div()
            .id(SharedString::from(format!("mux-bar-{}", slot.id)))
            .flex_shrink_0()
            .px(r(10.))
            .py(r(7.))
            .border_b_1()
            .border_color(border)
            .cursor_grab()
            .when(state.is_some_and(|(l, _)| l == "Attend votre accord"), |d| d.bg(yellow.opacity(0.08)))
            .child(bar)
            .on_drag(drag, |d, _, _, cx| cx.new(|_| d.clone()))
            .into_any_element()
    }

    fn render_panel(&self, ix: usize, flat: bool, cx: &mut Context<Self>) -> AnyElement {
        let slot = &self.slots[ix];
        let (border, primary) = (cx.theme().border, cx.theme().primary);
        let target = slot.id.clone();
        v_flex()
            .size_full()
            .min_w_0()
            .border_1()
            .border_color(border)
            .drag_over::<PanelDrag>(move |s, _, _, _| s.border_color(primary).bg(primary.opacity(0.06)))
            .on_drop(cx.listener(move |this, d: &PanelDrag, _, cx| this.move_slot(&d.id, &target, cx)))
            .rounded(r(8.))
            .overflow_hidden()
            .child(self.render_bar(ix, flat, cx))
            // Cached: a panel is drawn again only when its own session changed, not when another one streams.
            .when(!slot.folded, |d| d.child(div().flex_1().min_h_0().child(gpui::AnyView::from(slot.session.clone()).cached(gpui::StyleRefinement::default().size_full()))))
            .into_any_element()
    }

    fn render_leases(&self, project: &Path, cx: &mut Context<Self>) -> Option<AnyElement> {
        let leases = coord::with(|c| c.leases(project, coord::now()));
        if leases.is_empty() {
            return None;
        }
        let names: Vec<(String, String)> = self.slots.iter().map(|s| (s.id.clone(), s.name.clone())).collect();
        let theme = cx.theme();
        let (muted, fg) = (theme.muted_foreground, theme.foreground);
        h_flex()
            .flex_shrink_0()
            .flex_wrap()
            .gap(r(6.))
            .px(r(4.))
            .pb(r(6.))
            .text_size(r(11.5))
            .child(div().text_color(muted).child("Fichiers réservés :"))
            .children(leases.into_iter().enumerate().map(|(i, l)| {
                let owner = names.iter().find(|(id, _)| *id == l.owner).map_or(l.owner.clone(), |(_, n)| n.clone());
                let (project, path) = (project.to_path_buf(), l.path.clone());
                h_flex()
                    .gap(r(4.))
                    .pl(r(8.))
                    .pr(r(2.))
                    .py(r(1.))
                    .rounded(r(6.))
                    .bg(fg.opacity(0.05))
                    .child(div().font_family(cx.theme().mono_font_family.clone()).child(l.path.clone()))
                    .child(div().text_color(muted).child(format!("· {owner}")))
                    .child(
                        button(SharedString::from(format!("mux-free-{i}-{}", l.path)))
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Close).xsmall())
                            .tooltip(format!("Libérer de force ({})", l.intent))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                coord::with(|c| c.force_release(&project, &path, coord::now()));
                                coord::changed();
                                cx.notify();
                            })),
                    )
            }))
            .into_any_element()
            .into()
    }

    /// The sessions' panels: rows of up to three, resizable both ways; one section per project, or flat.
    fn render_grid(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        // By project: a section per shared project (its leases, its queue); the projects with one
        // session and nothing to show share rows, each bar naming its project (usage B side by side).
        let groups: Vec<(Option<PathBuf>, Vec<usize>)> = if self.by_project {
            let mut groups = vec![];
            let mut solo = vec![];
            let shown = self.shown();
            for p in self.projects() {
                let ixs: Vec<usize> = shown.iter().copied().filter(|&i| self.slots[i].project == p).collect();
                if ixs.is_empty() {
                    continue;
                }
                let busy_queue = coord::with(|c| c.projects.get(&project::key(&p)).is_some_and(|s| !s.queue.is_empty() || s.merging.is_some() || !s.outcomes.is_empty()));
                if ixs.len() == 1 && !busy_queue {
                    solo.extend(ixs);
                } else {
                    groups.push((Some(p), ixs));
                }
            }
            if !solo.is_empty() {
                groups.push((None, solo));
            }
            groups
        } else {
            vec![(None, self.shown())]
        };
        let layout = layout(cx);
        if layout == Layout::Tabs {
            return self.render_tabs(groups, cx);
        }
        // One row per chunk: three panels (grid), all of a group (columns) or one (rows).
        let per_row = match layout {
            Layout::Columns => usize::MAX,
            Layout::Rows => 1,
            _ => 3,
        };
        let rows: Vec<Row> = groups.into_iter().flat_map(|(project, ixs)| {
            ixs.chunks(per_row).enumerate().map(|(n, chunk)| Row { project: project.clone(), n, panels: chunk.to_vec(), group: ixs.clone() }).collect::<Vec<_>>()
        }).collect();
        match rows.len() {
            0 => div().into_any_element(),
            1 => self.render_row(&rows[0], cx),
            // Two rows share the height, resizable, when both keep room for their conversation.
            2 if window.viewport_size().height >= r(1100.).to_pixels(window.rem_size()) => {
                let both: Vec<AnyElement> = rows.iter().map(|row| self.render_row(row, cx)).collect();
                v_resizable("mux-rows").children(both.into_iter().map(|row| resizable_panel().child(div().absolute().size_full().child(row)))).into_any_element()
            }
            // More: each its own height, the grid scrolls; only the rows on screen are built and drawn.
            n => uniform_list("mux-grid", n, cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                range.filter_map(|i| rows.get(i)).map(|row| div().h(r(460.)).child(this.render_row(row, cx)).into_any_element()).collect::<Vec<_>>()
            }))
            .size_full()
            .into_any_element(),
        }
    }

    /// "Onglets": a tab per session (dragged to reorder), the chosen one's panel below,
    /// with its project's header when it shares the project.
    fn render_tabs(&self, groups: Vec<(Option<PathBuf>, Vec<usize>)>, cx: &mut Context<Self>) -> AnyElement {
        let all: Vec<usize> = groups.iter().flat_map(|g| g.1.iter().copied()).collect();
        let Some(&on) = all.iter().find(|&&i| self.active.as_deref() == Some(self.slots[i].id.as_str())).or(all.first()) else { return div().into_any_element() };
        let theme = cx.theme();
        let (muted, fg, primary, border) = (theme.muted_foreground, theme.foreground, theme.primary, theme.border);
        let tabs = h_flex().flex_shrink_0().flex_wrap().gap(r(4.)).pb(r(8.)).children(all.iter().map(|&i| {
            let slot = &self.slots[i];
            let s = slot.session.read(cx);
            let waiting = s.state() == SessionState::Waiting;
            let (id, target) = (slot.id.clone(), slot.id.clone());
            h_flex()
                .id(SharedString::from(format!("mux-tab-{}", slot.id)))
                .gap(r(6.))
                .px(r(10.))
                .py(r(5.))
                .rounded(r(8.))
                .border_1()
                .cursor_pointer()
                .text_size(r(12.5))
                .map(|d| if i == on { d.bg(fg.opacity(0.08)).border_color(border).text_color(fg) } else { d.border_color(gpui::transparent_black()).text_color(muted).hover(|d| d.bg(fg.opacity(0.04))) })
                .child(div().size(r(7.)).rounded_full().bg(self.color_of(&slot.project, cx)))
                .child(slot.name.clone())
                .when(s.state() == SessionState::Working, |d| d.child(div().size(r(5.)).rounded_full().bg(cx.theme().green)))
                .when(waiting, |d| d.child(div().text_color(cx.theme().yellow).child("•")))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.active = Some(id.clone());
                    cx.notify();
                }))
                .on_drag(PanelDrag { id: slot.id.clone(), name: slot.name.clone() }, |d, _, _, cx| cx.new(|_| d.clone()))
                .drag_over::<PanelDrag>(move |s, _, _, _| s.border_color(primary))
                .on_drop(cx.listener(move |this, d: &PanelDrag, _, cx| this.move_slot(&d.id, &target, cx)))
        }));
        let group = groups.iter().find(|g| g.1.contains(&on)).map(|g| (g.0.clone(), g.1.clone())).unwrap_or((None, vec![on]));
        let row = Row { project: group.0, n: 0, panels: vec![on], group: group.1 };
        v_flex().size_full().child(tabs).child(div().flex_1().min_h_0().child(self.render_row(&row, cx))).into_any_element()
    }

    /// A row of the grid: its project's header first (leases, queue) when it starts a shared project's section.
    fn render_row(&self, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (project, n, ixs) = (&row.project, row.n, &row.group);
        let key = project.as_deref().map(project::key).unwrap_or_else(|| "flat".into());
        let panels: Vec<AnyElement> = row.panels.iter().map(|&i| self.render_panel(i, project.is_none(), cx)).collect();
        let section = project.as_ref().filter(|_| n == 0).map(|p| {
            let cost: f64 = ixs.iter().map(|&i| self.slots[i].session.read(cx).cost()).sum();
            let color = self.color_of(p, cx);
            h_flex()
                .flex_shrink_0()
                .gap(r(8.))
                .px(r(4.))
                .pb(r(6.))
                .child(div().size(r(10.)).rounded_full().bg(color))
                .child({
                    let path = p.display().to_string().replacen(&crate::platform::home().display().to_string(), "~", 1);
                    div().id(SharedString::from(format!("mux-project-{key}"))).text_size(r(13.5)).font_weight(FontWeight::SEMIBOLD).child(name_of(p)).tooltip(move |window, cx| Tooltip::new(path.clone()).build(window, cx))
                })
                .child(div().flex_shrink_0().text_size(r(12.)).text_color(muted).child(format!("{} · {}", ui::plural(ixs.len(), "session"), claude::usd(cost))))
                .child(self.add_button(SharedString::from(format!("mux-add-project-{key}")), p.clone(), Some("Ajouter une session"), cx))
        });
        // Panels out of the flow (absolute): the content's width never sets the starting sizes, all start equal.
        // Named after its sessions: a row of other panels (another view, one closed) starts afresh.
        let members: String = row.panels.iter().map(|&i| self.slots[i].id.as_str()).collect::<Vec<_>>().join("+");
        let grid = h_resizable(SharedString::from(format!("mux-row-{key}-{n}-{members}"))).children(panels.into_iter().map(|p| resizable_panel().child(div().absolute().size_full().p(r(4.)).child(p))));
        // A shared project: its leases, each with its forced release.
        let leases = project.as_ref().filter(|_| n == 0 && ixs.len() > 1).and_then(|p| self.render_leases(p, cx));
        let queue = project.as_ref().filter(|_| n == 0).and_then(|p| self.render_queue(p, cx));
        v_flex().size_full().p(r(6.)).children(section).children(leases).children(queue).child(div().flex_1().min_h_0().child(grid)).into_any_element()
    }
}

/// How the panels are laid out (`AppSettings::mux_layout`).
#[derive(Clone, Copy, PartialEq)]
enum Layout {
    Grid,
    Columns,
    Rows,
    Tabs,
}

/// Each layout: its setting, its name in the menu.
const LAYOUTS: [(Layout, &str, &str); 4] = [(Layout::Grid, "", "Grille"), (Layout::Columns, "columns", "Colonnes"), (Layout::Rows, "rows", "Lignes"), (Layout::Tabs, "tabs", "Onglets")];

fn layout(cx: &App) -> Layout {
    let set = &AppSettings::get(cx).mux_layout;
    LAYOUTS.iter().find(|l| l.1 == set).map_or(Layout::Grid, |l| l.0)
}

/// A session's bar or tab dragged onto another: it takes that one's place.
#[derive(Clone)]
struct PanelDrag {
    id: String,
    name: String,
}

impl Render for PanelDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div().px(r(10.)).py(r(4.)).rounded(r(6.)).bg(theme.background).border_1().border_color(theme.primary).text_size(r(12.5)).child(self.name.clone())
    }
}

/// A row of the grid (see `render_grid`).
#[derive(Clone)]
struct Row {
    project: Option<PathBuf>,
    n: usize,
    panels: Vec<usize>,
    group: Vec<usize>,
}

/// A file as `claude` names it, made comparable with the checkout's canonical root
/// (macOS: /var is /private/var); a new file through its folder.
fn canonical_file(path: &str) -> String {
    let p = Path::new(path);
    let canon = crate::platform::canonical(p).ok().or_else(|| {
        let parent = crate::platform::canonical(p.parent()?).ok()?;
        Some(parent.join(p.file_name()?))
    });
    canon.map_or_else(|| path.to_string(), |c| c.to_string_lossy().into_owned())
}

/// The leases, at each permission request: a write to a file another session of the
/// project holds is refused (with who and why); a free one is taken. The modes that would
/// skip `can_use_tool` are played here ("Modifs auto": writes allowed; "Tout autoriser": all).
fn gate_for(id: String) -> claude::Gate {
    std::rc::Rc::new(move |tool, input, mode, _| {
        let write = matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit");
        if write {
            let path = canonical_file(input["file_path"].as_str().or(input["notebook_path"].as_str()).unwrap_or_default());
            let check = coord::with(|c| c.check_write(&id, &path, coord::now()));
            coord::changed();
            if let coord::WriteCheck::Held { name, intent, .. } = check {
                return claude::Verdict::Deny(format!(
                    "Refusé par Akolade : ce fichier est réservé par la session « {name} » ({intent}). Ne le modifie pas maintenant : avance sur autre chose, ou écris-lui (send_message) pour qu'elle le libère, puis réessaie."
                ));
            }
        }
        match mode {
            "bypassPermissions" => claude::Verdict::Allow,
            "acceptEdits" if write => claude::Verdict::Allow,
            _ => claude::Verdict::Ask,
        }
    })
}

/// What a session is told about itself (its role), added to its instructions.
/// What a session is told of its situation (added to its instructions): alone on its
/// project, its role and how to reach the others; sharing it, also who works with it and the rules.
fn coord_prompt(c: &coord::Coordinator, id: &str) -> String {
    let Some(me) = c.session(id) else { return String::new() };
    let role = if me.role.is_empty() { String::new() } else { format!(" Ton rôle : {}.", me.role) };
    let mut out = format!(
        "Tu es la session « {} » du multiplexeur d'Akolade, qui fait travailler plusieurs sessions Claude Code en parallèle.{role} Les outils MCP `akolade` te relient aux autres sessions : list_sessions (scope \"tout\" pour tous les projets) et send_message pour leur écrire, même dans un autre projet. Les messages qu'on t'écrit t'arrivent comme des messages utilisateur commençant par « [Message de … ] ».",
        me.name
    );
    if let Some(branch) = &me.branch {
        out += &format!(" Tu travailles dans ton propre worktree, sur la branche {branch} : commite ton travail au fil de l'eau.");
    }
    // Even alone: its work reaches the project only through request_merge.
    if me.mode == WorkMode::Copy {
        out += " Tu travailles dans ta propre copie du projet : tes modifications ne touchent pas le dossier du projet tant qu'elles n'y sont pas réintégrées. Quand ta tâche est terminée, appelle request_merge avec un résumé qui dit, fichier par fichier, ce que tu as changé et pourquoi : Akolade réintègre ta copie, et si un fichier a aussi changé dans le projet, un Claude tranche à partir de ce résumé. Attends sa réponse avant de modifier à nouveau ta copie : elle repart ensuite de l'état du projet.";
    }
    let peers = c.peers(id);
    if !peers.is_empty() {
        let who: Vec<String> = peers.iter().map(|p| if p.role.is_empty() { p.name.clone() } else { format!("{} ({})", p.name, p.role) }).collect();
        out += &format!(
            " Ton projet est partagé avec : {}. Règles : réserve les fichiers avant de les modifier (claim_files) et libère-les quand tu as fini (release_files) — une écriture sur un fichier réservé par une autre session est refusée ; lis ta boîte (read_inbox) quand on t'écrit ; annonce par broadcast toute modification d'une interface partagée (API, types, schéma).",
            who.join(", ")
        );
        out += if me.mode == WorkMode::Copy {
            ""
        } else if me.branch.is_some() {
            " Quand ta tâche est terminée et commitée, appelle request_merge : ta branche n'est fusionnée dans akolade/integration que si le build et les tests passent."
        } else {
            // Several in one folder: git's state is everyone's.
            " Tu travailles dans le dossier du projet, que d'autres sessions partagent peut-être : ne change pas de branche, ne fais ni git stash, ni git reset, ni git checkout de fichiers, et si tu commites, n'ajoute que les fichiers que tu as modifiés toi-même."
        };
    }
    out
}

impl Render for MuxView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tool("mux");
        let shown = self.shown();
        let projects = shown.iter().map(|&i| &self.slots[i].project).collect::<std::collections::HashSet<_>>().len();
        let total: f64 = shown.iter().map(|&i| self.slots[i].session.read(cx).cost()).sum();
        let waiting = self.slots.iter().filter(|s| s.session.read(cx).state() == SessionState::Waiting).count();
        // The subscription's windows are the account's: the same for every session.
        let limits: Vec<(&'static str, f64, i64)> = self.slots.iter().map(|s| s.session.read(cx).limits().to_vec()).find(|l| !l.is_empty()).unwrap_or_default();
        let subtitle = if shown.is_empty() {
            "Plusieurs sessions Claude Code en parallèle, sur un projet ou sur plusieurs.".to_string()
        } else {
            format!("{} · {} · {}", ui::plural(shown.len(), "session"), ui::plural(projects, "projet"), claude::usd(total))
        };
        let theme = cx.theme();
        let (muted, yellow, fg) = (theme.muted_foreground, theme.yellow, theme.foreground);
        let by_project = self.by_project;
        // Two views, each with its own sessions (counted): both halves the same width, whichever is on.
        let in_flat = self.slots.iter().filter(|s| s.flat).count();
        let segment = |id: &'static str, name: &str, n: usize, on: bool, flat: bool| {
            button(id).small().w(r(104.)).map(|b| if on { b.primary() } else { b.ghost() }).label(if n > 0 { format!("{name} ({n})") } else { name.to_string() }).on_click(cx.listener(move |this, _, _, cx| {
                this.by_project = !flat;
                AppSettings::update(cx, |s| s.mux_flat = flat);
                cx.notify();
            }))
        };
        let toggle = h_flex()
            .gap(r(2.))
            .p(r(2.))
            .rounded(r(8.))
            .bg(fg.opacity(0.05))
            .child(segment("mux-by-project", "Par projet", self.slots.len() - in_flat, by_project, false))
            .child(segment("mux-flat", "À plat", in_flat, !by_project, true));
        let mut actions: Vec<AnyElement> = vec![];
        if waiting > 0 {
            actions.push(
                h_flex()
                    .id("mux-waiting")
                    .gap(r(6.))
                    .px(r(10.))
                    .py(r(4.))
                    .rounded(r(8.))
                    .bg(yellow.opacity(0.14))
                    .text_color(yellow)
                    .text_size(r(12.5))
                    .child(Icon::empty().path("icons/circle-help.svg").xsmall())
                    .child(format!("{waiting} en attente"))
                    .tooltip(move |window, cx| Tooltip::new(format!("{} attendent votre accord : leurs barres sont surlignées, autorisez ou refusez dans leur panneau.", ui::plural(waiting, "session"))).build(window, cx))
                    .into_any_element(),
            );
        }
        actions.push(toggle.into_any_element());
        if !self.slots.is_empty() {
            let unread = coord::with(|c| c.messages.len() + c.notes.len());
            actions.push(
                button("mux-exchanges")
                    .small()
                    .map(|b| if self.exchanges { b.primary() } else { b.ghost() })
                    .icon(Icon::new(IconName::Inbox))
                    .label(if unread > 0 { format!("Échanges ({unread})") } else { "Échanges".into() })
                    .on_click(cx.listener(|this, _, _, cx| {
                        (this.exchanges, this.stats) = (!this.exchanges, false);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        actions.push(
            button("mux-stats")
                .small()
                .map(|b| if self.stats { b.primary() } else { b.ghost() })
                .icon(Icon::new(IconName::ChartPie))
                .tooltip("Statistiques : vos sessions et Claude Code")
                .on_click(cx.listener(|this, _, _, cx| {
                    (this.stats, this.exchanges) = (!this.stats, false);
                    // Read again at each opening: Claude Code may have computed them meanwhile.
                    this.cc_stats = if this.stats { stats::CcStats::load() } else { None };
                    cx.notify();
                }))
                .into_any_element(),
        );
        actions.push(self.render_display_menu(cx));
        actions.push(if self.by_project {
            button("mux-new").small().primary().icon(IconName::Plus).label("Nouvelle session").disabled(self.form.is_some()).on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))).into_any_element()
        } else {
            self.render_quick_menu("mux-new", cx)
        });
        let limits_line = (!limits.is_empty()).then(|| {
            h_flex()
                .flex_shrink_0()
                .gap(r(14.))
                .px(r(24.))
                .py(r(6.))
                .text_size(r(12.))
                .text_color(muted)
                .child("Limites de l'abonnement, communes à toutes les sessions :")
                .children(limits.iter().map(|&(name, part, _)| div().text_color(fg).child(format!("{name} {:.0} %", part * 100.))))
        });
        let side: Option<AnyElement> = if self.stats {
            Some(self.render_stats(cx))
        } else if self.exchanges && !self.slots.is_empty() {
            Some(self.render_exchanges(cx))
        } else {
            None
        };
        let body: AnyElement = if shown.is_empty() && self.form.is_none() {
            let empty = ui::empty_state(Icon::new(IconName::LayoutDashboard), (t.accent)(cx.theme()), if self.slots.is_empty() { "Aucune session" } else { "Aucune session dans cette vue" }, "Lancez plusieurs sessions Claude Code côte à côte : sur des projets différents, chacune dans son dossier, ou sur un même projet, chacune dans son worktree.", cx)
                .child(div().mt(r(8.)).child(if self.by_project {
                    button("mux-first").primary().icon(IconName::Plus).label("Nouvelle session").on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))).into_any_element()
                } else {
                    self.render_quick_menu("mux-first", cx)
                }));
            h_flex().size_full().child(div().flex_1().min_w_0().h_full().child(empty)).children(side).into_any_element()
        } else {
            h_flex()
                .flex_1()
                .min_h_0()
                .size_full()
                .child(div().flex_1().min_w_0().h_full().p(r(10.)).child(self.render_grid(window, cx)))
                .children(side)
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(header(t, subtitle, actions, cx))
            .children(limits_line)
            .children(self.starting.iter().map(|name| {
                h_flex().flex_shrink_0().gap(r(8.)).px(r(24.)).py(r(6.)).text_size(r(12.5)).text_color(muted).child(gpui_component::spinner::Spinner::new().xsmall()).child(format!("Préparation du dossier de « {name} »…"))
            }))
            .children(self.render_form(cx))
            .when(self.form.is_none(), |d| d.child(div().flex_1().min_h_0().w_full().child(body).map(|d| d.h(relative(1.)))))
    }
}
