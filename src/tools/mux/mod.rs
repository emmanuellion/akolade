//! "Multiplexeur": one tab driving N Claude Code sessions side by side, on one
//! project (several features at once, each session in its own worktree) or on
//! several (one session per project, the tab as a common dashboard), mixed at will.
//! Each panel is a `ClaudeSession` shown without the Claude Code tab's chrome.

pub mod project;

use std::path::{Path, PathBuf};

use gpui::{AnyElement, AppContext, Context, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, div, prelude::FluentBuilder, relative};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputState},
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
    session: Entity<ClaudeSession>,
    /// Only its bar shows (its conversation is not drawn).
    folded: bool,
    /// Its turn was stopped by "Pause": "Reprendre" asks it to go on.
    paused: bool,
    /// What its bar last showed: the tab redraws only when that changes, not at each streamed word.
    seen: (SessionState, u64),
    _sub: Subscription,
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
    pub fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self { workspace, slots: vec![], by_project: true, form: None, _form_sub: None, next_id: 1 }
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
        let cwd = folder.clone();
        let opts = SessionOptions {
            chrome: false,
            follow_editor: false,
            own_settings: true,
            effort: Some(EFFORTS[effort].0.to_string()),
            budget,
            system_prompt: role_prompt(&name, &role),
            extra_args: vec![],
        };
        let workspace = self.workspace.clone();
        let session = cx.new(|cx| {
            let mut s = ClaudeSession::new(workspace, Some(cwd.clone()), opts, window, cx);
            s.configure(model, perm, cx);
            s
        });
        let sub = cx.observe(&session, |this, session, cx| {
            let s = session.read(cx);
            let now = (s.state(), (s.cost() * 10_000.) as u64);
            if let Some(slot) = this.slots.iter_mut().find(|slot| slot.session == session)
                && slot.seen != now
            {
                slot.seen = now;
                cx.notify();
            }
        });
        if !task.is_empty() {
            session.update(cx, |s, cx| s.submit(task, window, cx));
        }
        self.slots.push(Slot { id, name, role, project, cwd, mode, session, folded: false, paused: false, seen: (SessionState::Idle, 0), _sub: sub });
        self.form = None;
        cx.notify();
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
            cx.notify();
        }
    }

    /// The session ends: its `claude` and what it runs stop (`Proc`'s drop).
    fn close(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.slots.len() {
            self.slots.remove(ix);
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
        let models = seg("mux-model", MODELS.iter().map(|m| m.0).collect(), form.model, |f, i| f.model = i, cx);
        let perms = seg("mux-perm", MODES.iter().map(|m| m.0).collect(), form.perm, |f, i| f.perm = i, cx);
        let efforts = seg("mux-effort", EFFORTS.iter().map(|e| e.1).collect(), form.effort, |f, i| f.effort = i, cx);
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
                .child(row("Modèle", models.into_any_element()))
                .child(row("Autorisations", perms.into_any_element()))
                .child(row("Effort", efforts.into_any_element()))
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

    /// A session's bar: who, where, how, what it does and costs, and its buttons.
    fn render_bar(&self, ix: usize, flat: bool, cx: &mut Context<Self>) -> AnyElement {
        let slot = &self.slots[ix];
        let s = slot.session.read(cx);
        let (state, cost) = (s.state(), s.cost());
        let color = self.color_of(&slot.project, cx);
        let theme = cx.theme();
        let (muted, border, yellow, red, green) = (theme.muted_foreground, theme.border, theme.yellow, theme.red, theme.green);
        let (state_label, state_color) = match state {
            SessionState::Idle => ("Inactive", muted),
            SessionState::Working => ("En cours", green),
            SessionState::Waiting => ("Attend votre accord", yellow),
            SessionState::Error => ("Erreur", red),
        };
        let pill = |text: String, c: Hsla| div().flex_shrink_0().px(r(6.)).py(r(1.)).rounded(r(4.)).bg(c.opacity(0.14)).text_color(c).text_size(r(11.)).child(text);
        let working = state == SessionState::Working;
        h_flex()
            .flex_shrink_0()
            .gap(r(8.))
            .px(r(10.))
            .py(r(6.))
            .border_b_1()
            .border_color(border)
            .when(state == SessionState::Waiting, |d| d.bg(yellow.opacity(0.08)))
            .child(div().size(r(8.)).flex_shrink_0().rounded_full().bg(color))
            .child(div().flex_shrink_0().text_size(r(13.)).font_weight(FontWeight::SEMIBOLD).child(slot.name.clone()))
            .when(flat, |d| d.child(div().flex_shrink_0().text_size(r(12.)).text_color(color).child(name_of(&slot.project))))
            .when(!slot.role.is_empty(), |d| d.child(div().min_w_0().truncate().text_size(r(12.)).text_color(muted).child(slot.role.clone())))
            .child(div().flex_1())
            .child(pill(slot.mode.label().into(), muted))
            .child(pill(state_label.into(), state_color))
            .when(slot.paused, |d| d.child(pill("En pause".into(), muted)))
            .child(div().flex_shrink_0().text_size(r(12.)).text_color(muted).child(claude::usd(cost)))
            .map(|d| {
                if slot.paused {
                    d.child(button(SharedString::from(format!("mux-resume-{}", slot.id))).ghost().xsmall().label("Reprendre").tooltip("Elle reprend là où elle s'était arrêtée").on_click(cx.listener(move |this, _, window, cx| this.resume(ix, window, cx))))
                } else {
                    d.child(button(SharedString::from(format!("mux-pause-{}", slot.id))).ghost().xsmall().label("Pause").tooltip("Interrompt son tour ; « Reprendre » la relance").disabled(!working).on_click(cx.listener(move |this, _, _, cx| this.pause(ix, cx))))
                }
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
            )
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
            .when(!slot.folded, |d| d.child(div().flex_1().min_h_0().child(slot.session.clone())))
            .into_any_element()
    }

    /// The sessions' panels: rows of up to three, resizable both ways; one section per project, or flat.
    fn render_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        let groups: Vec<(Option<PathBuf>, Vec<usize>)> = if self.by_project {
            self.projects().into_iter().map(|p| {
                let ixs = (0..self.slots.len()).filter(|&i| self.slots[i].project == p).collect();
                (Some(p), ixs)
            }).collect()
        } else {
            vec![(None, (0..self.slots.len()).collect())]
        };
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mut rows: Vec<AnyElement> = vec![];
        for (project, ixs) in groups {
            for (n, chunk) in ixs.chunks(3).enumerate() {
                let key = project.as_deref().map(project::key).unwrap_or_else(|| "flat".into());
                let panels: Vec<AnyElement> = chunk.iter().map(|&i| self.render_panel(i, project.is_none(), cx)).collect();
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
                        .child(div().min_w_0().truncate().text_size(r(12.)).text_color(muted).child(p.display().to_string()))
                        .child(div().flex_1())
                        .child(div().flex_shrink_0().text_size(r(12.)).text_color(muted).child(format!("{} · {}", ui::plural(ixs.len(), "session"), claude::usd(cost))))
                });
                let row = h_resizable(SharedString::from(format!("mux-row-{key}-{n}"))).children(panels.into_iter().map(|p| resizable_panel().child(div().size_full().p(r(4.)).child(p))));
                rows.push(v_flex().size_full().p(r(6.)).children(section).child(div().flex_1().min_h_0().child(row)).into_any_element());
            }
        }
        if rows.len() == 1 {
            return rows.pop().unwrap_or_else(|| div().into_any_element());
        }
        v_resizable("mux-rows").children(rows.into_iter().map(|row| resizable_panel().child(row))).into_any_element()
    }
}

/// What a session is told about itself (its role), added to its instructions.
fn role_prompt(name: &str, role: &str) -> String {
    let role = if role.is_empty() { String::new() } else { format!(" Ton rôle : {role}.") };
    format!("Tu es la session « {name} » du multiplexeur d'Akolade, qui fait travailler plusieurs sessions Claude Code en parallèle.{role}")
}

impl Render for MuxView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(format!("{} en attente de votre accord", ui::plural(waiting, "session")))
                    .tooltip(|window, cx| Tooltip::new("Leurs barres sont surlignées : autorisez ou refusez dans leur panneau.").build(window, cx))
                    .into_any_element(),
            );
        }
        if !self.slots.is_empty() {
            actions.push(toggle.into_any_element());
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
            div().flex_1().min_h_0().p(r(10.)).child(self.render_grid(cx)).into_any_element()
        };
        v_flex()
            .size_full()
            .child(header(t, subtitle, actions, cx))
            .children(limits_line)
            .children(self.render_form(cx))
            .child(div().flex_1().min_h_0().w_full().child(body).map(|d| d.h(relative(1.))))
    }
}
