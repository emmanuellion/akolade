//! Completion plugins: the language servers, shown and managed as plugins.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gpui::{
    AnyElement, App, AppContext, Context, Div, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels, PromptLevel, Render,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, canvas, div, prelude::FluentBuilder, px, relative,
    rgb,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    switch::Switch,
    v_flex,
};

use crate::{
    format::{FORMATTERS, Formatter},
    lint::{LINTERS, Linter},
    icons,
    languages::{LANGUAGES, Language},
    lsp,
    settings::AppSettings,
    themes::{self, Palette},
    tools::{TOOLS, Tool, tile},
    workspace::{Workspace, button, r},
};

/// One installable unit: languages served by the same package (npm) or
/// program, or a formatter.
#[derive(Clone)]
pub struct Plugin {
    pub languages: Vec<&'static Language>,
    /// Programs, all started by this plugin.
    pub programs: Vec<&'static str>,
    pub install: Option<&'static str>,
    /// A formatter's own name and words (completion plugins are named after their languages).
    pub formatter: Option<&'static Formatter>,
    pub linter: Option<&'static Linter>,
}

/// The plugins, in the order of `LANGUAGES`. HTML, CSS and JSON come from one
/// npm package, so they are one plugin; Swift and C both install with Xcode's
/// tools but are different programs, so they are two.
pub fn plugins() -> Vec<Plugin> {
    let mut out: Vec<Plugin> = vec![];
    for lang in LANGUAGES {
        let Some(server) = &lang.server else { continue };
        let key = |install: Option<&str>, program: &str| match install {
            Some(i) if i.starts_with("npm ") => i.to_string(),
            _ => program.to_string(),
        };
        let mine = key(server.install, server.program);
        match out.iter_mut().find(|p| p.languages.iter().any(|l| l.server.as_ref().is_some_and(|s| key(s.install, s.program) == mine))) {
            Some(plugin) => {
                plugin.languages.push(lang);
                if !plugin.programs.contains(&server.program) {
                    plugin.programs.push(server.program);
                }
            }
            None => out.push(Plugin { languages: vec![lang], programs: vec![server.program], install: server.install, formatter: None, linter: None }),
        }
    }
    out
}

pub fn formatters() -> Vec<Plugin> {
    FORMATTERS
        .iter()
        .map(|f| Plugin {
            languages: f.languages.iter().filter_map(|name| LANGUAGES.iter().find(|l| l.name == *name)).collect(),
            programs: vec![f.program],
            install: f.install,
            formatter: Some(f),
            linter: None,
        })
        .collect()
}

pub fn linters() -> Vec<Plugin> {
    LINTERS
        .iter()
        .map(|l| Plugin {
            languages: l.languages.iter().filter_map(|name| LANGUAGES.iter().find(|lang| lang.name == *name)).collect(),
            programs: vec![l.program],
            install: l.install,
            formatter: None,
            linter: Some(l),
        })
        .collect()
}

impl Plugin {
    /// Its on/off switches: a linter has its own (Ruff formats and lints, separately).
    pub fn switches(&self) -> Vec<&'static str> {
        match self.linter {
            Some(l) => vec![l.id],
            None => self.programs.clone(),
        }
    }

    /// "Python", "JavaScript, TypeScript, JSX & TSX".
    pub fn name(&self) -> String {
        if let Some(f) = self.formatter {
            return f.name.into();
        }
        if let Some(l) = self.linter {
            return l.name.into();
        }
        let names: Vec<&str> = self.languages.iter().map(|l| l.name).collect();
        match names.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} & {last}", rest.join(", ")),
            _ => names.join(""),
        }
    }

    /// What gets installed: the npm package, else the program(s).
    pub fn engine(&self) -> String {
        match self.install.and_then(|i| i.strip_prefix("npm install --prefix {dir} ")) {
            Some(packages) => packages.split_whitespace().next().unwrap_or_default().to_string(),
            None => self.programs.join(", "),
        }
    }

    pub fn source(&self) -> &'static str {
        if let Some(f) = self.formatter {
            return f.source;
        }
        if let Some(l) = self.linter {
            return l.source;
        }
        match self.install {
            Some(i) if i.starts_with("npm ") => "npm",
            Some(i) if i.starts_with("brew ") => "Homebrew",
            Some(i) if i.starts_with("xcode-select") => "Outils Xcode",
            Some(i) if i.starts_with("dotnet ") => ".NET",
            _ => "installation manuelle",
        }
    }

    /// Uninstall command, for what Akolade installed itself (npm, in its own folder).
    /// Typed with `{dir}` still in it, like the install command.
    pub fn uninstall_command(&self) -> Option<String> {
        let owned = self.programs.iter().all(|p| lsp::find_program(p).is_some_and(|path| path.starts_with(lsp::servers_dir())));
        npm_uninstall(self.install?).filter(|_| owned)
    }
}

/// Where the plugin and tool programs were found. PATH lookups are slow:
/// done now and then, off the UI thread; renders read this.
#[derive(Default, PartialEq)]
pub struct Found {
    programs: HashMap<&'static str, PathBuf>,
    /// Tools whose program is there (or that need none).
    tools: HashSet<&'static str>,
}

impl Found {
    pub fn lookup() -> Self {
        let programs = plugins().into_iter().chain(formatters()).chain(linters()).flat_map(|p| p.programs);
        Self { programs: programs.filter_map(|p| Some((p, lsp::find_program(p)?))).collect(), tools: TOOLS.iter().filter(|t| t.available()).map(|t| t.id).collect() }
    }

    pub fn installed(&self, plugin: &Plugin) -> bool {
        plugin.programs.iter().all(|p| self.programs.contains_key(p))
    }

    /// Same rule as `Plugin::uninstall_command`, from the lookup.
    fn can_uninstall(&self, plugin: &Plugin) -> bool {
        let dir = lsp::servers_dir();
        plugin.install.and_then(npm_uninstall).is_some() && plugin.programs.iter().all(|p| self.programs.get(p).is_some_and(|path| path.starts_with(&dir)))
    }

    fn available(&self, t: &Tool) -> bool {
        self.tools.contains(t.id)
    }
}

/// `npm install --prefix {dir} a b@5` → `npm uninstall --prefix {dir} a b`.
fn npm_uninstall(install: &str) -> Option<String> {
    let packages = install.strip_prefix("npm install --prefix {dir} ")?;
    // Without their version (`typescript@5`), keeping scopes (`@scope/pkg`).
    let names: Vec<&str> = packages.split_whitespace().map(|p| p.rsplit_once('@').filter(|(name, _)| !name.is_empty()).map_or(p, |(name, _)| name)).collect();
    Some(format!("npm uninstall --prefix {{dir}} {}", names.join(" ")))
}

/// Store sections.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Category {
    All,
    Tools,
    Completion,
    Format,
    Lint,
    Themes,
    Icons,
    /// What is installed, turned on or applied.
    Mine,
}

impl Category {
    const ALL: [Category; 8] = [Category::All, Category::Tools, Category::Completion, Category::Format, Category::Lint, Category::Themes, Category::Icons, Category::Mine];

    fn label(self) -> &'static str {
        match self {
            Category::All => "Tout",
            Category::Tools => "Outils",
            Category::Completion => "Complétion",
            Category::Format => "Formatage",
            Category::Lint => "Linters",
            Category::Themes => "Thèmes",
            Category::Icons => "Icônes",
            Category::Mine => "Installés",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Category::All => Icon::new(IconName::LayoutDashboard),
            Category::Tools => Icon::empty().path("icons/wrench.svg"),
            Category::Completion => Icon::empty().path("icons/sparkles.svg"),
            Category::Format => Icon::empty().path("icons/indent.svg"),
            Category::Lint => Icon::new(IconName::TriangleAlert),
            Category::Themes => Icon::new(IconName::Palette),
            Category::Icons => Icon::new(IconName::File),
            Category::Mine => Icon::new(IconName::CircleCheck),
        }
    }

    fn about(self) -> &'static str {
        match self {
            Category::Tools => "Chaque outil ajouté a son icône dans la barre de gauche : il s'affiche à la place de l'éditeur, un clic sur un fichier y revient.",
            Category::Completion => "La vraie complétion d'un langage : méthodes des objets, types, modules importés, documentation au survol.",
            Category::Format => "Remettent tout un fichier en forme d'un raccourci (⌥⇧F) ou du bouton « Formater ».",
            Category::Lint => "Soulignent les erreurs et les mauvaises pratiques pendant la frappe ; tout est listé dans « Problèmes ».",
            Category::Themes => "Couleurs de toute l'application, du code et du terminal.",
            Category::Icons => "La façon dont les fichiers sont dessinés : explorateur, onglets, recherche, Git.",
            _ => "",
        }
    }
}

/// The store tab: tools, completion plugins, formatters and themes.
pub struct PluginsView {
    workspace: WeakEntity<Workspace>,
    category: Category,
    search: Entity<InputState>,
    /// Sections folded (in "Tout" and "Installés").
    folded: HashSet<Category>,
    /// Width of the card area, measured while painting: sets the grid's columns.
    width: Rc<Cell<Pixels>>,
    found: Found,
    /// Plugin jobs running when `found` was last looked up: one ending looks again.
    jobs: usize,
    _lookup: Task<()>,
    /// Installs happen outside too (brew, npm): look at the plugins again now and then.
    _refresh: Task<()>,
    _search: Subscription,
}

impl PluginsView {
    pub fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _refresh = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                if this.update(cx, |this, cx| this.look_again(cx)).is_err() {
                    break;
                }
            }
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Rechercher…"));
        let _search = cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify());
        // Once on the UI thread at open: the first frame shows the right states, unsorted cards never flash.
        let found = Found::lookup();
        Self { workspace, category: Category::All, search, folded: HashSet::new(), width: Rc::default(), found, jobs: 0, _lookup: Task::ready(()), _refresh, _search }
    }

    fn look_again(&mut self, cx: &mut Context<Self>) {
        self._lookup = cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async { Found::lookup() }).await;
            let _ = this.update(cx, |this, cx| {
                if this.found != found {
                    this.found = found;
                    cx.notify();
                }
            });
        });
    }
}

/// "● Actif" on a tinted pill.
fn status(label: &'static str, color: Hsla) -> impl IntoElement {
    // A state worth noticing (added, active…) in its color with a dot; a neutral one
    // (not installed, off) just outlined: grey on grey read as a dull blob.
    let neutral = color.s < 0.15;
    h_flex()
        .h(r(26.))
        .px(r(11.))
        .gap(r(7.))
        .rounded_full()
        .text_size(r(12.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .map(|d| if neutral { d.border_1().border_color(color.opacity(0.35)) } else { d.bg(color.opacity(0.14)).child(div().size(r(7.)).rounded_full().bg(color)) })
        .child(label)
}

/// Every card has the same size: the grid stays even.
/// `clickable`: the whole card does something on click (hover border, pointer).
fn card(id: impl Into<SharedString>, clickable: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let (border, hover) = (theme.border, theme.foreground.opacity(0.18));
    v_flex()
        .id(id.into())
        .h(r(212.))
        .p(r(18.))
        .gap(r(12.))
        .rounded(r(14.))
        .bg(theme.foreground.opacity(0.025))
        .border_1()
        .border_color(border)
        .when(clickable, |d| d.cursor_pointer().hover(move |d| d.border_color(hover)))
}

/// Icon tile, name and one line under it.
fn card_head(tile: impl IntoElement, name: String, meta: String, right: Option<AnyElement>, cx: &App) -> impl IntoElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .gap(r(12.))
        .child(tile)
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap(r(2.))
                .child(div().truncate().text_size(r(15.)).font_weight(FontWeight::SEMIBOLD).child(name))
                .child(div().truncate().text_size(r(12.)).text_color(muted).child(meta)),
        )
        .children(right)
}

/// Wrapped on whole words, at most 3 lines. Not `line_clamp`: gpui doesn't
/// wrap a clamped last line, it runs past the card and is cut mid-word.
fn description(text: String, cx: &App) -> impl IntoElement {
    div().flex_1().min_h_0().child(div().max_h(r(13. * 1.5 * 3.)).overflow_hidden().text_size(r(13.)).line_height(relative(1.5)).text_color(cx.theme().muted_foreground).child(text))
}

fn footer() -> Div {
    h_flex().gap(r(8.))
}

fn toggle(id: String, checked: bool, programs: Vec<&'static str>) -> Switch {
    Switch::new(SharedString::from(id)).checked(checked).on_click(move |on, _, cx| {
        let (on, programs) = (*on, programs.clone());
        AppSettings::update(cx, move |s| {
            s.disabled_plugins.retain(|p| !programs.iter().any(|q| q == p));
            if !on {
                s.disabled_plugins.extend(programs.iter().map(|p| p.to_string()));
            }
        });
    })
}

/// "Python, JavaScript & TypeScript".
fn and_list(names: &[&str]) -> String {
    match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} et {last}", rest.join(", ")),
        _ => names.join(""),
    }
}

impl PluginsView {
    /// A completion plugin or a formatter. `is_installed`: looked up once per render (PATH lookups).
    fn plugin_card(&self, plugin: Plugin, is_installed: bool, cx: &App) -> Stateful<Div> {
        let theme = cx.theme();
        let settings = AppSettings::get(cx);
        let ws = self.workspace.upgrade();
        let id = plugin.programs[0];
        let completion = plugin.formatter.is_none() && plugin.linter.is_none();
        let enabled = plugin.switches().iter().all(|p| if completion { settings.server_enabled(p) } else { settings.plugin_enabled(p) });
        let active = completion && plugin.programs.iter().any(|p| ws.as_ref().is_some_and(|w| w.read(cx).plugin_running(p)));
        let busy = ws.as_ref().is_some_and(|w| w.read(cx).plugin_jobs.contains(id));
        let (label, color) = match (is_installed, enabled, active) {
            (false, _, _) => ("Non installé", theme.muted_foreground),
            (true, false, _) => ("Désactivé", theme.muted_foreground),
            (true, true, true) => ("Actif", theme.green),
            (true, true, false) => ("Installé", theme.primary),
        };
        let langs: Vec<&str> = plugin.languages.iter().map(|l| l.name).collect();
        let about = match (plugin.formatter, plugin.linter) {
            (Some(f), _) => f.about.to_string(),
            (_, Some(l)) => l.about.to_string(),
            (None, None) => format!("Complétion intelligente pour {} : méthodes, types, imports et documentation au survol.", and_list(&langs)),
        };
        let lead = plugin.languages.first().map_or(icons::LOGO, |l| l.icon);
        let others = plugin.languages.iter().skip(1).take(4).map(|l| icons::asset(l.icon).size(r(16.)));
        let uninstall = is_installed && self.found.can_uninstall(&plugin);
        let ws = self.workspace.clone();
        let job = plugin.clone();
        let switch = is_installed.then(|| toggle(format!("toggle-{}", plugin.switches()[0]), enabled, plugin.switches()).into_any_element());
        card(format!("card-{id}"), false, cx)
            .child(card_head(tile(theme.foreground, 44., icons::asset(lead).size(r(26.))), plugin.name(), format!("{} · {}", plugin.engine(), plugin.source()), switch, cx))
            .child(description(about, cx))
            .child(
                footer()
                    .child(status(label, color))
                    .children(others)
                    .child(div().flex_1())
                    .map(|this| match (is_installed, plugin.install, uninstall) {
                        (true, _, true) => this.child(
                            button(SharedString::from(format!("uninstall-{id}"))).ghost().small().label("Désinstaller").loading(busy).disabled(busy).on_click(move |_, window, cx| {
                                let _ = ws.update(cx, |ws, cx| ws.plugin_job(&job, false, window, cx));
                            }),
                        ),
                        (true, _, false) => this,
                        (false, Some(_), _) => this.child(
                            button(SharedString::from(format!("install-{id}")))
                                .primary()
                                .small()
                                .icon(IconName::ArrowDown)
                                .label(if busy { "Installation…" } else { "Installer" })
                                .loading(busy)
                                .disabled(busy)
                                .on_click(move |_, window, cx| {
                                    let _ = ws.update(cx, |ws, cx| ws.plugin_job(&job, true, window, cx));
                                }),
                        ),
                        (false, None, _) => this.child(div().text_size(r(12.)).text_color(theme.muted_foreground).child(format!("À installer : {id}"))),
                    }),
            )
    }

    fn tool_card(&self, t: &'static Tool, cx: &App) -> Stateful<Div> {
        let theme = cx.theme();
        let added = AppSettings::get(cx).tool_added(t.id);
        let available = self.found.available(t);
        let (label, color) = match (available, added) {
            (false, _) => ("Indisponible", theme.muted_foreground),
            (true, false) => ("Pas ajouté", theme.muted_foreground),
            (true, true) => ("Ajouté", theme.green),
        };
        let accent = (t.accent)(theme);
        let (ws, id) = (self.workspace.clone(), t.id);
        let about = match t.needs {
            Some((program, how)) if !available => format!("Demande {program} : {how}."),
            _ => t.about.to_string(),
        };
        card(format!("card-{id}"), false, cx)
            .child(card_head(tile(accent, 44., (t.icon)().size(r(22.)).text_color(accent)), t.name.into(), t.meta.into(), None, cx))
            .child(description(about, cx))
            .child(footer().child(status(label, color)).child(div().flex_1()).map(|this| {
                if added {
                    this.child(button(SharedString::from(format!("remove-{id}"))).ghost().small().label("Retirer").on_click(move |_, _, cx| {
                        AppSettings::update(cx, |s| s.tools.retain(|t| t != id));
                    }))
                    .child(button(SharedString::from(format!("open-{id}"))).outline().small().label("Ouvrir").on_click(move |_, window, cx| {
                        let _ = ws.update(cx, |ws, cx| ws.open_tool(id, window, cx));
                    }))
                } else {
                    this.child(button(SharedString::from(format!("add-{id}"))).primary().small().icon(IconName::Plus).label("Ajouter").disabled(!available).on_click(move |_, window, cx| {
                        AppSettings::update(cx, |s| s.tools.push(id.into()));
                        let _ = ws.update(cx, |ws, cx| ws.open_tool(id, window, cx));
                    }))
                }
            }))
    }

    fn theme_card(&self, p: &'static Palette, cx: &App) -> Stateful<Div> {
        let theme = cx.theme();
        let current = AppSettings::get(cx).theme == p.name;
        let line = |w: f32, color: u32| div().h(r(5.)).w(r(w)).rounded_full().bg(rgb(color));
        // A tiny window in the theme's colors: chrome, card, a few lines of code.
        let preview = h_flex()
            .flex_1()
            .rounded(r(10.))
            .overflow_hidden()
            .bg(rgb(p.chrome))
            .border_1()
            .border_color(rgb(p.border))
            .p(r(8.))
            .gap(r(8.))
            .child(v_flex().w(r(18.)).gap(r(6.)).pt(r(4.)).items_center().children([p.primary, p.muted, p.muted].map(|c| div().size(r(7.)).rounded_full().bg(rgb(c)))))
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .rounded(r(7.))
                    .bg(rgb(p.card))
                    .p(r(10.))
                    .gap(r(7.))
                    .child(h_flex().gap(r(5.)).child(line(26., p.keyword)).child(line(44., p.function)).child(line(18., p.fg)))
                    .child(h_flex().gap(r(5.)).pl(r(14.)).child(line(22., p.types)).child(line(60., p.string)))
                    .child(h_flex().gap(r(5.)).pl(r(14.)).child(line(30., p.keyword)).child(line(16., p.number)))
                    .child(h_flex().gap(r(5.)).child(line(70., p.comment))),
            );
        let name = p.name;
        card(format!("card-theme-{name}"), !current, cx)
            .when(current, |d| d.border_color(theme.primary))
            .when(!current, |d| d.on_click(move |_, _, cx| AppSettings::update(cx, |s| s.theme = name.into())))
            .child(preview)
            .child(
                footer()
                    .child(v_flex().min_w_0().child(div().truncate().text_size(r(15.)).font_weight(FontWeight::SEMIBOLD).child(name)).child(div().text_size(r(12.)).text_color(theme.muted_foreground).child(match (p.dark, p.custom) {
                        (true, false) => "Sombre",
                        (false, false) => "Clair",
                        (true, true) => "Sombre · importé",
                        (false, true) => "Clair · importé",
                    })))
                    .child(div().flex_1())
                    .when(p.custom, |this| {
                        this.child(button(SharedString::from(format!("theme-remove-{name}"))).ghost().small().icon(IconName::Delete).tooltip("Supprimer ce thème importé").on_click(move |_, window, cx| {
                            cx.stop_propagation(); // the card's click would apply it
                            let answer = window.prompt(PromptLevel::Warning, &format!("Supprimer le thème « {name} » ?"), Some("Il faudra l'importer de nouveau pour le retrouver."), &["Supprimer", "Annuler"], cx);
                            cx.spawn(async move |cx| {
                                if answer.await == Ok(0) {
                                    let _ = cx.update(|cx| {
                                        themes::remove_custom(name);
                                        if AppSettings::get(cx).theme == name {
                                            AppSettings::update(cx, |s| s.theme = themes::DEFAULT.into());
                                        }
                                        cx.refresh_windows();
                                    });
                                }
                            })
                            .detach();
                        }))
                    })
                    .map(|this| {
                        if current {
                            this.child(status("Appliqué", theme.primary))
                        } else {
                            this.child(button(SharedString::from(format!("theme-{name}"))).small().outline().label("Appliquer").on_click(move |_, _, cx| {
                                AppSettings::update(cx, |s| s.theme = name.into());
                            }))
                        }
                    }),
            )
    }

    /// An icon pack: a few files drawn with it, and "Appliquer".
    fn pack_card(&self, (id, name, about): (&'static str, &'static str, &'static str), cx: &App) -> Stateful<Div> {
        let theme = cx.theme();
        let current = AppSettings::get(cx).icon_pack == id;
        let samples = ["main.rs", "app.tsx", "script.py", "index.html", "package.json", "logo.png"];
        card(format!("card-pack-{id}"), !current, cx)
            .when(current, |d| d.border_color(theme.primary))
            .when(!current, |d| d.on_click(move |_, _, cx| AppSettings::update(cx, |s| s.icon_pack = id.into())))
            .child(h_flex().flex_1().flex_wrap().items_center().justify_center().gap(r(10.)).rounded(r(10.)).bg(theme.foreground.opacity(0.04)).p(r(12.)).children(samples.map(|f| icons::file_in(id, Path::new(f), 24., cx))))
            .child(
                footer()
                    // The text takes the free width (a spacer here split it in two: "Les logos des").
                    .gap(r(10.))
                    .child(v_flex().flex_1().min_w_0().child(div().text_size(r(15.)).font_weight(FontWeight::SEMIBOLD).child(name)).child(div().max_h(r(12. * 1.5 * 2.)).overflow_hidden().text_size(r(12.)).line_height(relative(1.5)).text_color(theme.muted_foreground).child(about)))
                    .map(|this| {
                        if current {
                            this.child(status("Appliqué", theme.primary))
                        } else {
                            this.child(button(SharedString::from(format!("pack-{id}"))).small().outline().label("Appliquer").on_click(move |_, _, cx| {
                                AppSettings::update(cx, |s| s.icon_pack = id.into());
                            }))
                        }
                    }),
            )
    }

    /// A card that imports a VS Code color theme (.json) and applies it.
    fn import_card(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        card("card-theme-import", true, cx)
            .border_dashed()
            .items_center()
            .justify_center()
            .gap(r(8.))
            .child(tile(theme.primary, 44., Icon::new(IconName::Plus).size(r(22.)).text_color(theme.primary)))
            .child(div().text_size(r(15.)).font_weight(FontWeight::SEMIBOLD).child("Importer un thème VS Code"))
            .child(div().text_size(r(12.5)).text_color(muted).text_center().child("Choisissez son fichier .json : ses couleurs et sa coloration du code sont reprises."))
            .on_click(cx.listener(|this, _, window, cx| this.import_theme(window, cx)))
    }

    fn import_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let result = std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|text| themes::from_vscode(&text, &stem)).and_then(themes::save_custom);
            let _ = this.update_in(cx, |_, window, cx| match result {
                Ok(name) => {
                    AppSettings::update(cx, |s| s.theme = name.into());
                    window.push_notification(Notification::success(format!("Thème « {name} » importé et appliqué.")), cx);
                }
                Err(e) => window.push_notification(Notification::error(e), cx),
            });
        })
        .detach();
    }

    fn render_nav(&self, counts: &[usize], cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let item = |c: Category, n: usize, cx: &mut Context<Self>| {
            let on = self.category == c;
            h_flex()
                .id(SharedString::from(format!("cat-{}", c.label())))
                .h(r(36.))
                .px(r(10.))
                .gap(r(10.))
                .rounded(r(8.))
                .cursor_pointer()
                .text_size(r(13.5))
                .map(|d| if on { d.bg(fg.opacity(0.08)).text_color(fg).font_weight(FontWeight::MEDIUM) } else { d.text_color(muted).hover(|d| d.bg(fg.opacity(0.04)).text_color(fg)) })
                .child(c.icon().size(r(17.)))
                .child(div().flex_1().child(c.label()))
                .child(div().text_size(r(12.)).text_color(muted).child(n.to_string()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.category = c;
                    cx.notify();
                }))
        };
        let master = Switch::new("plugins-master").checked(AppSettings::get(cx).language_servers).small().on_click(|on, _, cx| {
            let on = *on;
            AppSettings::update(cx, |s| s.language_servers = on);
        });
        v_flex()
            .w(r(236.))
            .h_full()
            .flex_shrink_0()
            .p(r(16.))
            .gap(r(4.))
            .border_r_1()
            .border_color(theme.border)
            .bg(fg.opacity(0.015))
            .child(
                h_flex()
                    .gap(r(10.))
                    .pb(r(14.))
                    .child(tile(theme.primary, 38., Icon::empty().path("icons/puzzle.svg").size(r(20.)).text_color(theme.primary)))
                    .child(v_flex().child(div().text_size(r(17.)).font_weight(FontWeight::SEMIBOLD).child("Store")).child(div().text_size(r(12.)).text_color(muted).child("Plugins, outils et thèmes"))),
            )
            .child(v_flex().pb(r(10.)).child(Input::new(&self.search).cleanable(true).prefix(Icon::new(IconName::Search).small().text_color(muted))))
            .children(Category::ALL.into_iter().zip(counts.iter().copied()).map(|(c, n)| item(c, n, cx)).collect::<Vec<_>>())
            .child(div().flex_1())
            .child(
                v_flex()
                    .p(r(12.))
                    .gap(r(6.))
                    .rounded(r(10.))
                    .bg(fg.opacity(0.04))
                    .child(h_flex().gap(r(8.)).child(div().flex_1().text_size(r(13.)).font_weight(FontWeight::MEDIUM).child("Complétion avancée")).child(master))
                    .child(div().text_size(r(12.)).text_color(muted).child("Coupe ou rallume d'un coup tous les plugins de complétion.")),
            )
    }
}

impl Render for PluginsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let jobs = self.workspace.upgrade().map_or(0, |w| w.read(cx).plugin_jobs.len());
        if jobs != self.jobs {
            self.jobs = jobs;
            self.look_again(cx);
        }
        let query = self.search.read(cx).value().trim().to_lowercase();
        let matches = |words: &[&str]| query.is_empty() || words.iter().any(|w| w.to_lowercase().contains(&query));
        let keep = |p: &Plugin| {
            let mut words = vec![p.name(), p.engine()];
            words.extend(p.languages.iter().map(|l| l.name.to_string()));
            words.extend(p.formatter.map(|f| f.about.to_string()));
            words.extend(p.linter.map(|l| l.about.to_string()));
            matches(&words.iter().map(String::as_str).collect::<Vec<_>>())
        };
        let settings = AppSettings::get(cx);
        // By name; installed or not looked up once each (PATH lookups).
        let with_installed = |list: Vec<Plugin>| {
            let mut list: Vec<(Plugin, bool)> = list.into_iter().filter(&keep).map(|p| {
                let i = self.found.installed(&p);
                (p, i)
            }).collect();
            list.sort_by_cached_key(|(p, _)| crate::ui::sort_key(&p.name()));
            list
        };
        let (completion, format, lint) = (with_installed(plugins()), with_installed(formatters()), with_installed(linters()));
        let mut tools: Vec<&'static Tool> = TOOLS.iter().filter(|t| matches(&[t.name, t.about, t.meta, "outils"])).collect();
        tools.sort_by_cached_key(|t| crate::ui::sort_key(t.name));
        let mut packs: Vec<(&'static str, &'static str, &'static str)> = icons::PACKS.into_iter().filter(|(_, name, about)| matches(&[name, about, "icônes"])).collect();
        packs.sort_by_cached_key(|p| crate::ui::sort_key(p.1));
        let themes: Vec<&'static Palette> = themes::all().into_iter().filter(|t| matches(&[t.name, "thème", if t.dark { "sombre" } else { "clair" }])).collect();
        let mine = |p: &&(Plugin, bool)| p.1;
        let mine_count = completion.iter().filter(mine).count() + format.iter().filter(mine).count() + lint.iter().filter(mine).count() + tools.iter().filter(|t| settings.tool_added(t.id)).count() + themes.iter().filter(|t| t.name == settings.theme).count();
        let mine_count = mine_count + packs.iter().filter(|p| p.0 == settings.icon_pack).count();
        let counts = [completion.len() + format.len() + lint.len() + tools.len() + themes.len() + packs.len(), tools.len(), completion.len(), format.len(), lint.len(), themes.len(), packs.len(), mine_count];

        // As many columns of at least 290 px-at-14 as fit.
        let rem = window.rem_size();
        let (min, gap) = (r(290.).to_pixels(rem), r(16.).to_pixels(rem));
        let cols = ((f32::from(self.width.get()) + f32::from(gap)) / f32::from(min + gap)).floor().clamp(1., 6.) as u16;

        let mine_only = self.category == Category::Mine;
        let show = |c: Category| matches!(self.category, Category::All | Category::Mine) || self.category == c;
        let mut sections: Vec<(Category, Vec<Stateful<Div>>)> = vec![];
        if show(Category::Tools) {
            let cards = tools.into_iter().filter(|t| !mine_only || settings.tool_added(t.id)).map(|t| self.tool_card(t, cx)).collect();
            sections.push((Category::Tools, cards));
        }
        if show(Category::Completion) {
            sections.push((Category::Completion, completion.into_iter().filter(|(_, i)| !mine_only || *i).map(|(p, i)| self.plugin_card(p, i, cx)).collect()));
        }
        if show(Category::Format) {
            sections.push((Category::Format, format.into_iter().filter(|(_, i)| !mine_only || *i).map(|(p, i)| self.plugin_card(p, i, cx)).collect()));
        }
        if show(Category::Lint) {
            sections.push((Category::Lint, lint.into_iter().filter(|(_, i)| !mine_only || *i).map(|(p, i)| self.plugin_card(p, i, cx)).collect()));
        }
        if show(Category::Themes) {
            let mut cards: Vec<Stateful<Div>> = themes.into_iter().filter(|t| !mine_only || t.name == settings.theme).map(|t| self.theme_card(t, cx)).collect();
            if !mine_only && query.is_empty() {
                cards.push(self.import_card(cx));
            }
            sections.push((Category::Themes, cards));
        }
        if show(Category::Icons) {
            let pack = AppSettings::get(cx).icon_pack.clone();
            sections.push((Category::Icons, packs.into_iter().filter(|p| !mine_only || p.0 == pack).map(|p| self.pack_card(p, cx)).collect()));
        }
        sections.retain(|(_, cards)| !cards.is_empty());

        let nav = self.render_nav(&counts, cx);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let width = self.width.clone();
        let (title, subtitle) = match self.category {
            Category::All => ("Tout le Store", "Tout ce qui s'ajoute à Akolade. Les installations se font en arrière-plan."),
            Category::Mine => ("Installés", "Ce qui est installé, activé ou appliqué."),
            c => (c.label(), c.about()),
        };
        let empty = sections.is_empty();
        h_flex()
            .size_full()
            .child(nav)
            .child(
                v_flex()
                        .id("store-content")
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_y_scroll()
                        .relative()
                        .px(r(32.))
                        .py(r(28.))
                        .gap(r(30.))
                        // Measures the card area for the grid's columns.
                        .child(
                            canvas(
                                move |bounds, window, _| {
                                    if width.get() != bounds.size.width {
                                        width.set(bounds.size.width);
                                        window.refresh();
                                    }
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .left(r(32.))
                            .right(r(32.))
                            .h(px(1.)),
                        )
                        .child(v_flex().flex_shrink_0().gap(r(4.)).child(div().text_size(r(24.)).font_weight(FontWeight::SEMIBOLD).child(title)).child(div().text_size(r(13.5)).text_color(muted).child(subtitle)))
                        .children(sections.into_iter().map(|(c, cards)| {
                            let n = cards.len();
                            let titled = matches!(self.category, Category::All | Category::Mine);
                            let folded = titled && self.folded.contains(&c);
                            // Not shrunk to the visible height: the area scrolls instead.
                            v_flex()
                                .flex_shrink_0()
                                .gap(r(14.))
                                .when(titled, |d| {
                                    // A click on the title folds or unfolds the section.
                                    d.child(
                                        v_flex()
                                            .id(SharedString::from(format!("section-{}", c.label())))
                                            .gap(r(3.))
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if !this.folded.remove(&c) {
                                                    this.folded.insert(c);
                                                }
                                                cx.notify();
                                            }))
                                            .child(
                                                h_flex()
                                                    .gap(r(10.))
                                                    .child(Icon::new(if folded { IconName::ChevronRight } else { IconName::ChevronDown }).small().text_color(muted))
                                                    .child(c.icon().size(r(17.)).text_color(muted))
                                                    .child(div().text_size(r(16.)).font_weight(FontWeight::SEMIBOLD).child(c.label()))
                                                    .child(div().text_size(r(13.)).text_color(muted).child(n.to_string()))
                                                    .child(div().flex_1())
                                                    .child(div().text_size(r(12.)).text_color(muted).child(if folded { "Afficher" } else { "Replier" })),
                                            )
                                            .when(!folded, |d| d.child(div().pl(r(51.)).text_size(r(13.)).text_color(muted).child(c.about()))),
                                    )
                                })
                                .when(!folded, |d| d.child(div().grid().grid_cols(cols).gap(gap).children(cards)))
                        }))
                        .when(empty, |d| {
                            let (title, text) = if query.is_empty() { ("Rien ici pour l'instant", "Installez un plugin depuis les autres catégories.".to_string()) } else { ("Aucun résultat", format!("Rien ne correspond à « {query} ».")) };
                            d.child(div().pt(r(36.)).child(crate::ui::empty_state(if query.is_empty() { Category::Mine.icon() } else { Icon::new(IconName::Search) }, muted, title, &text, cx)))
                        }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(name: &str) -> Plugin {
        plugins().into_iter().find(|p| p.languages.iter().any(|l| l.name == name)).unwrap()
    }

    #[test]
    fn languages_sharing_a_package_or_program_are_one_plugin() {
        assert_eq!(find("TSX").name(), "JavaScript, TypeScript, JSX & TSX");
        assert_eq!(find("JSON").name(), "HTML, CSS, SCSS & JSON", "one npm package, three programs");
        assert_eq!(find("JSON").programs.len(), 3);
        assert_eq!(find("C++").name(), "C & C++");
        assert_eq!(find("Swift").name(), "Swift", "also from Xcode, but another program");
        assert_eq!(find("Python").source(), "npm");
        assert_eq!(find("JSON").engine(), "vscode-langservers-extracted");
        assert_eq!(find("C").engine(), "clangd");
        assert_eq!(find("Rust").source(), "Homebrew");
    }

    #[test]
    fn formatters_are_plugins_of_their_own_name() {
        let all = formatters();
        let prettier = all.iter().find(|p| p.name() == "Prettier").unwrap();
        assert_eq!((prettier.engine().as_str(), prettier.source()), ("prettier", "npm"));
        assert!(prettier.languages.iter().any(|l| l.name == "TSX"));
        let rustfmt = all.iter().find(|p| p.name() == "rustfmt").unwrap();
        assert_eq!((rustfmt.source(), rustfmt.uninstall_command()), ("fourni avec Rust", None), "a toolchain's own tool is never ours to remove");
    }

    #[test]
    fn uninstall_command_drops_versions() {
        assert_eq!(npm_uninstall("npm install --prefix {dir} typescript-language-server typescript@5").as_deref(), Some("npm uninstall --prefix {dir} typescript-language-server typescript"));
        assert_eq!(npm_uninstall("npm install --prefix {dir} @scope/pkg@2").as_deref(), Some("npm uninstall --prefix {dir} @scope/pkg"));
        assert_eq!(npm_uninstall("brew install gopls"), None);
    }

    #[test]
    fn only_what_akolade_installed_can_be_uninstalled() {
        // clangd comes with Xcode: never ours to remove.
        assert_eq!(find("C").uninstall_command(), None);
        // Not installed in our folder (here) → nothing to remove either.
        let ts = find("TypeScript");
        if !ts.programs.iter().all(|p| lsp::find_program(p).is_some_and(|path| path.starts_with(lsp::servers_dir()))) {
            assert_eq!(ts.uninstall_command(), None);
        }
    }
}
