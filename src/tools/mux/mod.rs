//! "Multiplexeur": one tab driving N Claude Code sessions side by side, on one
//! project (several features at once, each session in its own worktree) or on
//! several (one session per project, the tab as a common dashboard), mixed at will.
//! Each panel is a `ClaudeSession` shown without the Claude Code tab's chrome.

pub mod coord;
pub mod mcp;
pub mod project;
pub mod queue;
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

use project::{Admission, WorkMode};

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
}

pub struct MuxView {
    workspace: WeakEntity<Workspace>,
    slots: Vec<Slot>,
    /// One section per project, or all sessions side by side.
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
}

/// A color per project, in the order they appear.
fn project_color(ix: usize, cx: &gpui::App) -> Hsla {
    let t = cx.theme();
    [t.blue, t.green, t.magenta, t.yellow, t.cyan, t.red][ix % 6]
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
        Self { workspace, slots: vec![], by_project: true, form: None, _form_sub: None, starting: vec![], report: None, exchanges: false, filter: None, compose, to: Recipient::All, next_id: 1 }
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
            };
            self.start(launch, info.cwd, worktree, window, cx);
        }
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

    /// The modes of the sessions already on `project`.
    fn modes_on(&self, project: &Path) -> Vec<WorkMode> {
        self.slots.iter().filter(|s| s.project == project).map(|s| s.mode).collect()
    }

    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = AppSettings::get(cx).clone();
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let n = self.next_id;
        let name = cx.new(|cx| InputState::new(window, cx).default_value(format!("session-{n}")));
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
        };
        self.form = Some(form);
        cx.notify();
    }

    /// A project picked in the form: its default mode follows who works there already.
    fn set_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let project = project::project_of(&folder);
        let others = self.modes_on(&project);
        if let Some(form) = &mut self.form {
            form.mode = match project::admission(crate::git::toplevel(&folder).is_some(), &others) {
                Admission::Open(mode) => mode,
                Admission::NoGit => WorkMode::Direct,
            };
            form.folder = Some(folder);
        }
        cx.notify();
    }

    fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Choisir".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = rx.await else { return };
            let Some(path) = paths.pop() else { return };
            let _ = this.update(cx, |this, cx| this.set_folder(path, cx));
        })
        .detach();
    }

    /// Why the form's project cannot take this session now (None: it can).
    fn refusal(&self, form: &Form) -> Option<String> {
        let folder = form.folder.as_ref()?;
        let project = project::project_of(folder);
        let others = self.modes_on(&project);
        let git = crate::git::toplevel(folder).is_some();
        if matches!(project::admission(git, &others), Admission::NoGit) {
            return Some(format!("« {} » n'est pas un dépôt Git : sans branche ni worktree, deux sessions y écriraient les mêmes fichiers. Une seule session à la fois, ou initialisez Git.", name_of(&project)));
        }
        if form.mode == WorkMode::Direct && !project::direct_allowed(&others) {
            return Some("Une session Directe travaille déjà dans ce projet : la nouvelle doit être Isolée, dans son propre worktree.".into());
        }
        if form.mode == WorkMode::Isolated && !git {
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
        let name = Some(read(&form.name, cx)).filter(|n| !n.is_empty()).unwrap_or_else(|| format!("session-{}", self.next_id));
        let (role, task) = (read(&form.role, cx), read(&form.task, cx));
        let budget = read(&form.budget, cx).replace(',', ".").parse::<f64>().ok().filter(|b| *b > 0.);
        let (mode, model, perm, effort) = (form.mode, form.model, form.perm, form.effort);
        let project = project::project_of(&folder);
        let id = format!("{}-{}", project::slug(&name), self.next_id);
        self.next_id += 1;
        let launch = Launch { id, name, role, task, project, mode, model, perm, effort, budget, conversation: None };
        self.form = None;
        if mode == WorkMode::Direct {
            return self.start(launch, folder, None, window, cx);
        }
        // Isolated: its worktree first (a checkout: off the UI thread), in the same subfolder as picked.
        let rel = folder.strip_prefix(&launch.project).map(Path::to_path_buf).unwrap_or_default();
        let (project, slug) = (launch.project.clone(), launch.id.clone());
        self.starting.push(launch.name.clone());
        cx.notify();
        let made = cx.background_executor().spawn(async move { worktree::create(&worktree::base(), &project, &slug) });
        cx.spawn_in(window, async move |this, cx| {
            let made = made.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.starting.retain(|n| *n != launch.name);
                match made {
                    Ok(wt) => this.start(launch, wt.join(rel), Some(wt), window, cx),
                    Err(e) => {
                        window.push_notification(gpui_component::notification::Notification::error(format!("Impossible de créer le worktree de « {} » : {e}", launch.name)), cx);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// A session in `cwd` (its worktree's, if Isolated), its first instruction sent.
    fn start(&mut self, l: Launch, cwd: PathBuf, worktree: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let branch = worktree.as_ref().map(|_| worktree::branch_for(&l.id));
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
        let session = cx.new(|cx| {
            let mut s = ClaudeSession::new(workspace, Some(cwd.clone()), opts, window, cx);
            s.configure(model, perm, cx);
            s
        });
        let sub = cx.observe(&session, |this, session, cx| {
            let s = session.read(cx);
            let now = (s.state(), (s.cost() * 10_000.) as u64);
            let conversation = s.conversation().map(str::to_string);
            let Some(slot) = this.slots.iter_mut().find(|slot| slot.session == session) else { return };
            // Its conversation's id, saved: after a restart it goes on with it.
            let id = slot.id.clone();
            if conversation.is_some() && coord::with(|c| c.session(&id).map(|i| i.conversation.clone())) != Some(conversation.clone()) {
                coord::with(|c| {
                    if let Some(info) = c.sessions.iter_mut().find(|i| i.id == id) {
                        info.conversation = conversation;
                    }
                });
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
        self.slots.push(Slot { id: l.id, name: l.name, role: l.role, project: l.project, cwd, mode: l.mode, branch, worktree, session, folded: false, paused: restored, seen: (SessionState::Idle, 0), _sub: sub });
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
            // Its build and tests are its own code: only in a project you trust.
            if !crate::trust::is_trusted(&project) {
                coord::with(|c| c.finish_merge(&project, false, "Le projet est en mode restreint : Akolade ne lance pas son build ni ses tests. Approuvez-le, puis redemandez la fusion.".into(), coord::now()));
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
                coord::with(|c| c.finish_merge(&project, merged, report, coord::now()));
                coord::changed();
            })
            .detach();
        }
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
                        .child(div().text_color(muted).child("File d'intégration :"))
                        .when_some(state.merging.as_ref(), |d, m| d.child(h_flex().gap(r(4.)).child(gpui_component::spinner::Spinner::new().xsmall()).child(format!("{} (fusion et tests)", name(&m.session)))))
                        .when(!state.queue.is_empty(), |d| d.child(div().text_color(muted).child(format!("en attente : {}", state.queue.iter().map(|r| name(&r.session)).collect::<Vec<_>>().join(", ")))))
                        .when(state.merging.is_none() && state.queue.is_empty(), |d| d.child(div().text_color(muted).child("vide")))
                        .when_some(last.clone(), |d, o| {
                            let k = key.clone();
                            d.child(
                                div()
                                    .id(SharedString::from(format!("mux-last-{key}")))
                                    .cursor_pointer()
                                    .text_color(if o.merged { green } else { red })
                                    .child(format!("{} {} {}", if o.merged { "✓" } else { "✗" }, name(&o.session), if o.merged { "fusionnée" } else { "renvoyée" }))
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
            if let Some(wt) = slot.worktree.clone() {
                drop(slot);
                cx.background_executor().spawn(async move { worktree::remove(&wt) }).detach();
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
        let settings = AppSettings::get(cx);
        let mut recent: Vec<PathBuf> = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf)).into_iter().collect();
        for p in self.projects().into_iter().chain(settings.recent_folders.iter().cloned()) {
            if !recent.contains(&p) && p.is_dir() {
                recent.push(p);
            }
        }
        recent.truncate(6);
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
                    .on_click(cx.listener(|this, _, window, cx| this.pick_folder(window, cx)))
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
        let mode = seg("mux-mode", vec!["Directe", "Isolée"], (form.mode == WorkMode::Isolated) as usize, |f, i| f.mode = if i == 0 { WorkMode::Direct } else { WorkMode::Isolated }, cx);
        let mode_about = match form.mode {
            WorkMode::Direct => "Dans le dossier du projet lui-même, comme l'onglet Claude Code.",
            WorkMode::Isolated => "Dans son propre worktree, sur la branche akolade/<nom> : rien ne touche votre copie.",
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
                .child(row("Nom", Input::new(&form.name).small().into_any_element()))
                .child(row("Rôle", Input::new(&form.role).small().into_any_element()))
                .child(row("Tâche", Input::new(&form.task).small().into_any_element()))
                .child(row("Mode de travail", h_flex().gap(r(10.)).child(mode).child(div().text_size(r(12.)).text_color(muted).child(mode_about)).into_any_element()))
                .child(row("Modèle", models))
                .child(row("Autorisations", perms.into_any_element()))
                .child(row("Budget ($)", div().w(r(140.)).child(Input::new(&form.budget).small()).into_any_element()))
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
                        .justify_end()
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

    /// The model and its thinking effort in one menu, as in Claude's chat: each model with what
    /// it is for, then the effort; the choice is checked.
    fn render_model_picker(&self, model: usize, effort: usize, cx: &mut Context<Self>) -> AnyElement {
        const ABOUT: [&str; 4] = ["Celui de vos réglages Claude Code", "Le plus capable, pour les tâches complexes", "Rapide et précis, pour le travail de tous les jours", "Le plus rapide et le moins cher, pour les tâches simples"];
        let name = if model == 0 { "Modèle par défaut".to_string() } else { MODELS[model].0.to_string() };
        let label = format!("{name} · effort {}", EFFORTS[effort].1.to_lowercase());
        let view = cx.entity().downgrade();
        let pick = move |model: Option<usize>, effort: Option<usize>| {
            let view = view.clone();
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |this, cx| {
                    if let Some(f) = &mut this.form {
                        f.model = model.unwrap_or(f.model);
                        f.effort = effort.unwrap_or(f.effort);
                    }
                    cx.notify();
                });
            }
        };
        // Its own width, the arrow after the words (the row would stretch it).
        h_flex().child(button("mux-model")
            .small()
            .outline()
            .child(h_flex().gap(r(6.)).child(label).child(Icon::new(IconName::ChevronDown).xsmall()))
            .dropdown_menu(move |mut menu, _, _| {
                menu = menu.min_w(gpui::px(320.)).check_side(gpui_component::Side::Right).label("Modèle");
                for (i, (m, about)) in MODELS.iter().zip(ABOUT).enumerate() {
                    let title = m.0;
                    menu = menu.item(
                        PopupMenuItem::element(move |_, cx: &mut App| v_flex().py(r(2.)).child(div().child(title)).child(div().text_size(r(11.5)).text_color(cx.theme().muted_foreground).child(about)))
                            .checked(i == model)
                            .on_click(pick(Some(i), None)),
                    );
                }
                menu = menu.separator().label("Effort de réflexion");
                for (i, e) in EFFORTS.iter().enumerate() {
                    menu = menu.item(PopupMenuItem::new(e.1).checked(i == effort).on_click(pick(None, Some(i))));
                }
                menu
            }))
            .into_any_element()
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
        // Second line: how and where it works, its role (narrow panels keep the first line's buttons).
        let details = h_flex()
            .gap(r(6.))
            .min_w_0()
            .child({
                let (label, c) = if slot.paused { ("En pause", muted) } else { (state_label, state_color) };
                div().id(SharedString::from(format!("mux-state-{}", slot.id))).flex_shrink_0().child(pill(label.into(), c)).when_some(blocked, |d, why| d.tooltip(move |window, cx| Tooltip::new(why.clone()).build(window, cx)))
            })
            .child(pill(match &slot.branch {
                Some(b) => format!("{} · {b}", slot.mode.label()),
                None => slot.mode.label().into(),
            }, muted))
            .child(div().flex_shrink_0().text_size(r(12.)).text_color(muted).child(claude::usd(cost)))
            .when(!slot.role.is_empty(), |d| d.child(div().min_w_0().truncate().text_size(r(12.)).text_color(muted).child(slot.role.clone())));
        let top = h_flex()
            .gap(r(6.))
            .min_w_0()
            .child(div().size(r(8.)).flex_shrink_0().rounded_full().bg(color))
            .child({
                let tip = format!("Travaille dans {}", slot.cwd.display());
                div().id(SharedString::from(format!("mux-name-{}", slot.id))).flex_shrink_0().max_w(r(160.)).truncate().text_size(r(13.)).font_weight(FontWeight::SEMIBOLD).child(slot.name.clone()).tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .when(flat, |d| d.child(div().flex_shrink_0().text_size(r(12.)).text_color(color).child(name_of(&slot.project))))
            .child(div().flex_1())
            .map(|d| {
                if slot.paused {
                    d.child(button(SharedString::from(format!("mux-resume-{}", slot.id))).ghost().xsmall().label("Reprendre").tooltip("Elle reprend là où elle s'était arrêtée").on_click(cx.listener(move |this, _, window, cx| this.resume(ix, window, cx))))
                } else if working {
                    d.child(button(SharedString::from(format!("mux-pause-{}", slot.id))).ghost().xsmall().label("Pause").tooltip("Interrompt son tour ; « Reprendre » la relance").on_click(cx.listener(move |this, _, _, cx| this.pause(ix, cx))))
                } else {
                    d
                }
            })
            .when_some(slot.worktree.clone().filter(|_| !working), |d, wt| {
                d.child(button(SharedString::from(format!("mux-rebase-{}", slot.id))).ghost().xsmall().icon(Icon::empty().path("icons/git-pull-request.svg").xsmall()).tooltip("Rebaser sur akolade/integration : sa branche reprend ce que les autres ont déjà fait fusionner (annulé en cas de conflit)").on_click(cx.listener(move |_, _, window, cx| {
                    let wt = wt.clone();
                    let task = cx.background_executor().spawn(async move { crate::git::rebase(&wt, queue::BRANCH) });
                    cx.spawn_in(window, async move |_, cx| {
                        let done = task.await;
                        let _ = cx.update(|window, cx| {
                            let note = match done {
                                crate::git::Integration::Done => gpui_component::notification::Notification::success("Branche rebasée sur akolade/integration."),
                                crate::git::Integration::Conflict(files) => gpui_component::notification::Notification::error(format!("Conflit sur {} : le rebase est annulé, demandez à la session de le faire elle-même.", files.join(", "))),
                                crate::git::Integration::Failed(e) => gpui_component::notification::Notification::error(format!("Rebase impossible : {e}")),
                            };
                            window.push_notification(note, cx);
                        });
                    })
                    .detach();
                })))
            })
            .child(
                button(SharedString::from(format!("mux-fold-{}", slot.id)))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(if slot.folded { IconName::ChevronDown } else { IconName::ChevronUp }).xsmall())
                    .tooltip(if slot.folded { "Déplier" } else { "Replier (la conversation n'est plus dessinée)" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(slot) = this.slots.get_mut(ix) {
                            slot.folded = !slot.folded;
                        }
                        cx.notify();
                    })),
            )
            .child(
                button(SharedString::from(format!("mux-close-{}", slot.id)))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Close).xsmall())
                    .tooltip("Arrêter et fermer la session")
                    .on_click(cx.listener(move |this, _, _, cx| this.close(ix, cx))),
            );
        v_flex()
            .flex_shrink_0()
            .gap(r(4.))
            .px(r(10.))
            .py(r(6.))
            .border_b_1()
            .border_color(border)
            .when(state == SessionState::Waiting, |d| d.bg(yellow.opacity(0.08)))
            .child(top)
            .child(details)
            .into_any_element()
    }

    fn render_panel(&self, ix: usize, flat: bool, cx: &mut Context<Self>) -> AnyElement {
        let slot = &self.slots[ix];
        let border = cx.theme().border;
        v_flex()
            .size_full()
            .min_w_0()
            .border_1()
            .border_color(border)
            .rounded(r(8.))
            .overflow_hidden()
            .child(self.render_bar(ix, flat, cx))
            // Cached: a panel is drawn again only when its own session changed, not when another one streams.
            .when(!slot.folded, |d| d.child(div().flex_1().min_h_0().child(gpui::AnyView::from(slot.session.clone()).cached(gpui::StyleRefinement::default().size_full()))))
            .into_any_element()
    }

    fn render_leases(&self, project: &Path, cx: &mut Context<Self>) -> AnyElement {
        let leases = coord::with(|c| c.leases(project, coord::now()));
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
            .child(div().text_color(muted).child(if leases.is_empty() { "Aucun bail en cours".to_string() } else { format!("{} :", ui::plural(leases.len(), "bail")).replace("bails", "baux") }))
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
    }

    /// The sessions' panels: rows of up to three, resizable both ways; one section per project, or flat.
    fn render_grid(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        // By project: a section per shared project (its leases, its queue); the projects with one
        // session and nothing to show share rows, each bar naming its project (usage B side by side).
        let groups: Vec<(Option<PathBuf>, Vec<usize>)> = if self.by_project {
            let mut groups = vec![];
            let mut solo = vec![];
            for p in self.projects() {
                let ixs: Vec<usize> = (0..self.slots.len()).filter(|&i| self.slots[i].project == p).collect();
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
            vec![(None, (0..self.slots.len()).collect())]
        };
        // One row per chunk of up to three panels: (its group's project, its index in the group, its panels, the group's).
        let rows: Vec<Row> = groups.into_iter().flat_map(|(project, ixs)| {
            ixs.chunks(3).enumerate().map(|(n, chunk)| Row { project: project.clone(), n, panels: chunk.to_vec(), group: ixs.clone() }).collect::<Vec<_>>()
        }).collect();
        match rows.len() {
            0 => div().into_any_element(),
            1 => self.render_row(&rows[0], cx),
            // Two rows share the height, resizable, when both keep room for their conversation.
            2 if window.viewport_size().height >= r(1100.).to_pixels(window.rem_size()) => {
                let both: Vec<AnyElement> = rows.iter().map(|row| self.render_row(row, cx)).collect();
                v_resizable("mux-rows").children(both.into_iter().map(|row| resizable_panel().child(row))).into_any_element()
            }
            // More: each its own height, the grid scrolls; only the rows on screen are built and drawn.
            n => uniform_list("mux-grid", n, cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                range.filter_map(|i| rows.get(i)).map(|row| div().h(r(460.)).child(this.render_row(row, cx)).into_any_element()).collect::<Vec<_>>()
            }))
            .size_full()
            .into_any_element(),
        }
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
                .child(div().text_size(r(13.5)).font_weight(FontWeight::SEMIBOLD).child(name_of(p)))
                .child(div().min_w_0().truncate().text_size(r(12.)).text_color(muted).child(p.display().to_string().replacen(&crate::platform::home().display().to_string(), "~", 1)))
                .child(div().flex_1())
                .child(div().flex_shrink_0().text_size(r(12.)).text_color(muted).child(format!("{} · {}", ui::plural(ixs.len(), "session"), claude::usd(cost))))
        });
        let grid = h_resizable(SharedString::from(format!("mux-row-{key}-{n}"))).children(panels.into_iter().map(|p| resizable_panel().child(div().size_full().p(r(4.)).child(p))));
        // A shared project: its leases, each with its forced release.
        let leases = project.as_ref().filter(|_| n == 0 && ixs.len() > 1).map(|p| self.render_leases(p, cx));
        let queue = project.as_ref().filter(|_| n == 0).and_then(|p| self.render_queue(p, cx));
        v_flex().size_full().p(r(6.)).children(section).children(leases).children(queue).child(div().flex_1().min_h_0().child(grid)).into_any_element()
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
    let peers = c.peers(id);
    if !peers.is_empty() {
        let who: Vec<String> = peers.iter().map(|p| if p.role.is_empty() { p.name.clone() } else { format!("{} ({})", p.name, p.role) }).collect();
        out += &format!(
            " Ton projet est partagé avec : {}. Règles : réserve les fichiers avant de les modifier (claim_files) et libère-les quand tu as fini (release_files) — une écriture sur un fichier réservé par une autre session est refusée ; lis ta boîte (read_inbox) quand on t'écrit ; annonce par broadcast toute modification d'une interface partagée (API, types, schéma) ; quand ta tâche est terminée et commitée, appelle request_merge : ta branche n'est fusionnée dans akolade/integration que si le build et les tests passent.",
            who.join(", ")
        );
    }
    out
}

impl Render for MuxView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tool("mux");
        let projects = self.projects().len();
        let total: f64 = self.slots.iter().map(|s| s.session.read(cx).cost()).sum();
        let waiting = self.slots.iter().filter(|s| s.session.read(cx).state() == SessionState::Waiting).count();
        // The subscription's windows are the account's: the same for every session.
        let limits: Vec<(&'static str, f64, i64)> = self.slots.iter().map(|s| s.session.read(cx).limits().to_vec()).find(|l| !l.is_empty()).unwrap_or_default();
        let subtitle = if self.slots.is_empty() {
            "Plusieurs sessions Claude Code en parallèle, sur un projet ou sur plusieurs.".to_string()
        } else {
            format!("{} · {} · {} au total", ui::plural(self.slots.len(), "session"), ui::plural(projects, "projet"), claude::usd(total))
        };
        let theme = cx.theme();
        let (muted, yellow, fg) = (theme.muted_foreground, theme.yellow, theme.foreground);
        let by_project = self.by_project;
        let toggle = h_flex()
            .gap(r(2.))
            .p(r(2.))
            .rounded(r(8.))
            .bg(fg.opacity(0.05))
            .child(button("mux-by-project").small().map(|b| if by_project { b.primary() } else { b.ghost() }).label("Par projet").on_click(cx.listener(|this, _, _, cx| {
                this.by_project = true;
                cx.notify();
            })))
            .child(button("mux-flat").small().map(|b| if by_project { b.ghost() } else { b.primary() }).label("À plat").on_click(cx.listener(|this, _, _, cx| {
                this.by_project = false;
                cx.notify();
            })));
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
        if !self.slots.is_empty() {
            actions.push(toggle.into_any_element());
            let unread = coord::with(|c| c.messages.len() + c.notes.len());
            actions.push(
                button("mux-exchanges")
                    .small()
                    .map(|b| if self.exchanges { b.primary() } else { b.ghost() })
                    .icon(Icon::new(IconName::Inbox))
                    .label(if unread > 0 { format!("Échanges ({unread})") } else { "Échanges".into() })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.exchanges = !this.exchanges;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        actions.push(button("mux-new").small().primary().icon(IconName::Plus).label("Nouvelle session").disabled(self.form.is_some()).on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))).into_any_element());
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
        let body: AnyElement = if self.slots.is_empty() && self.form.is_none() {
            ui::empty_state(Icon::new(IconName::LayoutDashboard), (t.accent)(cx.theme()), "Aucune session", "Lancez plusieurs sessions Claude Code côte à côte : sur des projets différents, chacune dans son dossier, ou sur un même projet, chacune dans son worktree.", cx)
                .child(button("mux-first").primary().mt(r(8.)).icon(IconName::Plus).label("Nouvelle session").on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))))
                .into_any_element()
        } else {
            h_flex()
                .flex_1()
                .min_h_0()
                .size_full()
                .child(div().flex_1().min_w_0().h_full().p(r(10.)).child(self.render_grid(window, cx)))
                .when(self.exchanges, |d| d.child(self.render_exchanges(cx)))
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(header(t, subtitle, actions, cx))
            .children(limits_line)
            .children(self.starting.iter().map(|name| {
                h_flex().flex_shrink_0().gap(r(8.)).px(r(24.)).py(r(6.)).text_size(r(12.5)).text_color(muted).child(gpui_component::spinner::Spinner::new().xsmall()).child(format!("Création du worktree de « {name} »…"))
            }))
            .children(self.render_form(cx))
            .when(self.form.is_none(), |d| d.child(div().flex_1().min_h_0().w_full().child(body).map(|d| d.h(relative(1.)))))
    }
}
