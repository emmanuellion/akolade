use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    ops::Range,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    process::Stdio,
    time::{Duration, SystemTime},
};

use gpui::{
    Action, Animation, AnimationExt, AnyElement, AnyView, App, ClickEvent, ClipboardItem, Div, MouseButton, AppContext, Context, DragMoveEvent, Pixels, ElementId, Entity, EntityId, EntityInputHandler, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    ParentElement, PathPromptOptions, Render, SharedString, StatefulInteractiveElement, Styled,
    Hsla, PromptLevel, ScrollHandle, ease_out_quint, linear_color_stop, linear_gradient, Rems, Rgba, Subscription, Task, Window, actions, div, relative, rems, uniform_list, prelude::FluentBuilder, px, rgb,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Root, Selectable, Sizable, TitleBar, WindowExt,
    button::{Button, ButtonRounded, ButtonVariants},
    dialog::DialogButtonProps,
    menu::{ContextMenuExt, PopupMenu, PopupMenuItem},
    h_flex,
    input::{Enter, IndentInline, Input, InputEvent, InputState, Position, Search},
    notification::Notification,
    highlighter::{Diagnostic, HighlightTheme},
    resizable::{resizable_panel, v_resizable},
    theme::Theme,
    text::TextView,
    spinner::Spinner,
    tooltip::Tooltip,
    v_flex,
};
use gpui_terminal::{ColorPalette, TerminalConfig, TerminalView};
use serde::Deserialize;

use crate::{
    trust,
    explorer::{Explorer, MAX_ENTRIES},
    files,
    format,
    lint,
    git::{self, Summary},
    git_view,
    icons,
    image_view::{self, ImageViewer, ImageViewerEvent},
    completion::{self, NativeCompletion},
    lsp::{self, Servers},
    languages::{self, Language},
    plugins::{Plugin, PluginsView},
    problems::{Problem, Severity},
    search::{self, SearchView},
    ui,
    session,
    settings::{self, AppSettings, SettingsView},
    tasks::TasksView,
    terminal::Terminal,
    tools,
};

actions!(
    akolade,
    [Save, Run, CloseTab, NewFile, OpenFile, OpenFolder, CloseFolder, ToggleTerminal, ToggleSidebar, OpenSettings, OpenPlugins, NextTab, PrevTab, ReopenTab, ZoomIn, ZoomOut, ZoomReset, Format, OpenGit, OpenSearch, GoToDefinitionKey, FindReferences, RenameSymbol, QuickFix, SplitEditor, LintFile, CopyFilePath, CopyRelativePath, RevealInExplorer, ShowFileChanges]
);

/// ⌘1…⌘9: that tab (⌘9 the last one, like browsers).
#[derive(Clone, PartialEq, Action)]
#[action(namespace = akolade, no_json)]
pub struct SelectTab(pub usize);

/// Notifications of one plugin's install or uninstall replace each other.
struct PluginJob;

/// A closed tab, shrinking in its place so the next ones slide left.
struct ClosingTab {
    at: usize,
    width: Pixels,
    title: String,
    /// Restarts the animation for each close.
    id: usize,
}

const TAB_CLOSE: Duration = Duration::from_millis(180);
/// The activity bar's width (px at the default text size).
const ACTIVITY_BAR: f32 = 56.;
/// The sidebar sliding in or out.
const SIDEBAR_SLIDE: Duration = Duration::from_millis(200);

/// A file or folder dragged in the explorer (shown as its name under the pointer).
#[derive(Clone)]
struct DraggedEntry {
    path: PathBuf,
    name: String,
}

impl Render for DraggedEntry {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div().px(r(10.)).py(r(4.)).rounded(r(6.)).bg(theme.background).border_1().border_color(theme.primary).text_size(r(12.5)).child(self.name.clone())
    }
}

/// Dragging the sidebar's edge (draws nothing under the pointer).
struct SidebarDrag;

impl Render for SidebarDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Per-project runtime setup, read from `<root>/akolade.json`.
#[derive(Default, Deserialize)]
struct Config {
    /// extension -> command, e.g. `{"ts": "deno run {file}"}`
    #[serde(default)]
    run: HashMap<String, String>,
    /// Environment variables for the run commands.
    #[serde(default)]
    env: HashMap<String, String>,
}

struct Buffer {
    /// None for an untitled buffer, until its first save.
    path: Option<PathBuf>,
    title: String,
    lang: &'static Language,
    view: View,
    dirty: bool,
    /// Modification time of the file when last read or written: saving over a
    /// file changed meanwhile (git checkout, formatter…) needs a confirmation.
    disk_mtime: Option<SystemTime>,
    /// Markdown only: source, live preview, or both.
    md_mode: MdMode,
    /// Its text is being replaced by the file's (see `reload_from_disk`): that change is not an edit.
    reloading: bool,
    _sub: Option<Subscription>,
}

#[derive(Clone, Copy, PartialEq)]
enum MdMode {
    Source,
    Split,
    Preview,
}

impl MdMode {
    fn from_setting(value: &str) -> Self {
        match value {
            "source" => Self::Source,
            "preview" => Self::Preview,
            _ => Self::Split,
        }
    }
}

enum View {
    Text(Entity<InputState>),
    Image(Entity<ImageViewer>),
    Settings(Entity<SettingsView>),
    Plugins(Entity<PluginsView>),
    /// A store tool's tab (`tools::TOOLS`).
    Tool { id: &'static str, view: AnyView },
}

/// What the sidebar shows (icons at the top of the activity bar).
#[derive(Clone, Copy, PartialEq)]
enum Panel {
    Explorer,
    Search,
    Todo,
    Scripts,
    Problems,
}

impl Buffer {
    fn id(&self) -> EntityId {
        match &self.view {
            View::Text(e) => e.entity_id(),
            View::Image(v) => v.entity_id(),
            View::Settings(v) => v.entity_id(),
            View::Plugins(v) => v.entity_id(),
            View::Tool { view, .. } => view.entity_id(),
        }
    }

    fn editor(&self) -> Option<&Entity<InputState>> {
        match &self.view {
            View::Text(e) => Some(e),
            _ => None,
        }
    }
}

pub struct Workspace {
    root: Option<PathBuf>,
    explorer: Explorer,
    config: Config,
    buffers: Vec<Buffer>,
    active: usize,
    /// The tab shown on the right (side by side, ⌘\\), and whether the right side was clicked last.
    split: Option<usize>,
    focus_right: bool,
    untitled: usize,
    /// The terminals (tabs of the bottom panel), the one shown, and how many were opened.
    terminals: Vec<Terminal>,
    active_terminal: usize,
    terminals_made: usize,
    show_terminal: bool,
    show_sidebar: bool,
    /// What a second, identical action may throw away (see `confirm`).
    discard: Option<Discard>,
    pub focus: FocusHandle,
    /// Language servers, shared by all buffers of a language.
    servers: Rc<Servers>,
    /// Prettier asked about for a file outside any folder: this run only.
    format_once: bool,
    /// Settings as last applied (to only redo what changed).
    applied: AppSettings,
    /// Pending automatic save (see `autosave`).
    _autosave: Option<Task<()>>,
    _settings: Subscription,
    /// Editor whose Markdown preview was drawn last frame (see `render_editor`).
    previewed: Cell<Option<EntityId>>,
    /// Files of the closed tabs, last closed at the end (⌘⇧T).
    closed: Vec<PathBuf>,
    /// Plugins being installed or uninstalled in the background.
    pub plugin_jobs: HashSet<&'static str>,
    sidebar_width: Pixels,
    /// The theme's syntax colors, and the language color they are tinted with now.
    base_syntax: Arc<HighlightTheme>,
    syntax_color: Option<u32>,
    /// The project's repository, read again every few seconds (None: not one, or Git off).
    git: Option<Summary>,
    _git_poll: Task<()>,
    /// The tabs' widths, measured each frame (a closed tab shrinks from its own width).
    tab_widths: Rc<std::cell::RefCell<Vec<Pixels>>>,
    closing: Option<ClosingTab>,
    closes: usize,
    _closing: Option<Task<()>>,
    /// Problems shown in the editors, as of this version of `servers.problems`.
    problems_version: u64,
    _problems_poll: Task<()>,
    _explorer_poll: Task<()>,
    /// Polls pause while the window isn't the active one.
    window_active: bool,
    _activation: Subscription,
    /// The last edit, sent to its language server once typing pauses.
    /// Per editor: a pending sync of one tab must not cancel another's.
    _lsp_sync: HashMap<EntityId, Task<()>>,
    /// One linter run per file (per project for Clippy) at a time, the newest text next.
    lint_gate: Arc<lint::Gate>,
    /// Reopening last time's tabs: saved once at the end, linted once (the active one).
    restoring: bool,
    /// The explorer row a context menu was opened on: outlined until the next click.
    menu_target: Option<PathBuf>,
    /// What the editor is waiting for, shown in the status bar with a spinner.
    busy: Option<SharedString>,
    /// The activity bar's tools list: is there more below (a fade says so)?
    act_scroll: ScrollHandle,
    /// Each time the terminal panel appears it fades in: counted to key the animation.
    term_opened: usize,
    term_was_shown: bool,
    /// The last tab shown before a tool took the editor area (tools have no tab).
    last_file: Option<EntityId>,
    /// The highlight of the line jumped to, fading out (a new jump replaces it).
    _flash: Option<Task<()>>,
    /// The sidebar slides when shown or hidden: the last state drawn, and the slide
    /// running (opening?, its id) until its end clears it.
    sidebar_was_shown: bool,
    sidebar_anim: Option<(bool, usize)>,
    sidebar_slides: usize,
    _sidebar_slide: Option<Task<()>>,
    /// The sidebar's panel, and the panels' views (kept between shows).
    panel: Panel,
    search: Entity<SearchView>,
    todo: Entity<SearchView>,
    tasks: Entity<TasksView>,
}

/// Something that loses unsaved work.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Discard {
    Tab(EntityId),
    Window,
    /// Closing the project with unsaved files.
    Folder,
    /// Saving over a file that changed on disk.
    Overwrite(EntityId),
}

/// Losing work takes the same action twice in a row: the first call arms
/// `target` and returns false (warn), a second call on the same target returns
/// true (go ahead). Another target, an edit or a tab switch disarms.
fn confirm(armed: &mut Option<Discard>, target: Discard) -> bool {
    if *armed == Some(target) {
        *armed = None;
        true
    } else {
        *armed = Some(target);
        false
    }
}

/// Writes through a temporary file then a rename: a failed write (disk full…)
/// leaves the old file whole. Follows symlinks and keeps the file's
/// permissions (a script stays executable).
pub fn write_file(path: &Path, text: &str) -> std::io::Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()); // a new file: as given
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let tmp = target.with_file_name(format!(".{name}.akolade-tmp"));
    if let Err(e) = fs::write(&tmp, text) {
        let _ = fs::remove_file(&tmp);
        // A folder we cannot add a file to, but a file we may write: in place, as before.
        return if e.kind() == std::io::ErrorKind::PermissionDenied { fs::write(&target, text) } else { Err(e) };
    }
    let kept = fs::metadata(&target).map_or(Ok(()), |m| fs::set_permissions(&tmp, m.permissions()));
    kept.and_then(|_| fs::rename(&tmp, &target)).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// The editor's right-click menu, after "Go to definition": code, file and Git
/// actions. Read when the menu opens: what is installed or enabled right now.
fn context_menu(menu: PopupMenu, ws: gpui::WeakEntity<Workspace>, lang: &'static Language, has_path: bool, cx: &mut App) -> PopupMenu {
    let settings = AppSettings::get(cx);
    let server = lang.server.as_ref().is_some_and(|s| settings.server_enabled(s.program));
    let formatter = lang.highlight == "json" || format::for_language(lang.name, settings).is_some();
    let root = ws.upgrade().and_then(|w| w.read(cx).root.clone());
    let linter = has_path && root.as_ref().is_some_and(|root| !lint::for_language(lang.name, root, settings).is_empty());
    let git = has_path && settings.tool_added("git");
    menu.menu_with_enable("Aller à la définition", Box::new(GoToDefinitionKey), server && has_path)
        .menu_with_enable("Trouver les références", Box::new(FindReferences), server && has_path)
        .menu_with_enable("Renommer le symbole…", Box::new(RenameSymbol), server && has_path)
        .menu_with_enable("Corrections rapides…", Box::new(QuickFix), server && has_path)
        .separator()
        .menu_with_enable("Formater le document", Box::new(Format), formatter)
        .menu_with_enable("Vérifier le fichier (linters)", Box::new(LintFile), linter)
        .separator()
        .menu_with_enable("Copier le chemin", Box::new(CopyFilePath), has_path)
        .menu_with_enable("Copier le chemin relatif", Box::new(CopyRelativePath), has_path && root.is_some())
        .menu_with_enable("Afficher dans l'explorateur", Box::new(RevealInExplorer), has_path && root.is_some())
        .when(git, |m| m.menu("Voir les changements Git", Box::new(ShowFileChanges)))
        .menu("Ouvrir à côté", Box::new(SplitEditor))
}

/// A file's problems as the editor's underlines (and their popups on hover).
fn set_diagnostics(editor: &Entity<InputState>, list: &[Problem], cx: &mut App) {
    editor.update(cx, |e, cx| {
        // Problem columns count UTF-16 units; the editor's count characters.
        let text = e.text().clone();
        let at = |(line, col): (u32, u32)| completion::editor_position(&text, Position::new(line, col));
        let Some(set) = e.diagnostics_mut() else { return };
        set.clear();
        set.extend(list.iter().map(|p| {
            let severity = match p.severity {
                Severity::Error => lsp_types::DiagnosticSeverity::ERROR,
                Severity::Warning => lsp_types::DiagnosticSeverity::WARNING,
                Severity::Info => lsp_types::DiagnosticSeverity::INFORMATION,
            };
            Diagnostic::new(at(p.start)..at(p.end), p.message.clone()).with_severity(severity).with_source(p.source.clone())
        }));
        cx.notify();
    });
}

/// The word (letters, digits, `_`, `$`) at an LSP position of the text (the
/// column in UTF-16 units, as servers count: an emoji is 2).
fn word_at(text: &str, line: usize, col: usize) -> String {
    let line = text.lines().nth(line).unwrap_or_default();
    let chars: Vec<char> = line.chars().collect();
    let is_word = |c: &char| c.is_alphanumeric() || *c == '_' || *c == '$';
    let mut units = 0;
    let col = chars.iter().take_while(|c| {
        units += c.len_utf16();
        units <= col
    }).count();
    let start = chars[..col].iter().rposition(|c| !is_word(c)).map_or(0, |i| i + 1);
    let end = chars[col..].iter().position(|c| !is_word(c)).map_or(chars.len(), |i| col + i);
    chars[start..end].iter().collect()
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// One word for the terminal's shell, whatever it holds: PowerShell on Windows
/// (`''` inside), a POSIX shell elsewhere (`'\''` inside).
pub fn shell_quote(s: &str) -> String {
    if cfg!(windows) { format!("'{}'", s.replace('\'', "''")) } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// Code blocks of a preview that get syntax colors. gpui-component compiles the
/// highlight queries again for every block (~30 ms each for JavaScript), on every
/// re-parse: past this many, blocks are shown as plain code.
const MAX_HIGHLIGHTED_BLOCKS: usize = 30;

/// The Markdown given to the preview: language tags dropped after the
/// first `MAX_HIGHLIGHTED_BLOCKS` code fences. Fences follow CommonMark: a
/// block opened by N backticks (or tildes) is closed by a bare run of at
/// least N, so ``` lines inside a ```` block stay content.
fn preview_source(md: &str) -> SharedString {
    let mut out = String::with_capacity(md.len());
    let mut open: Option<(char, usize)> = None;
    let mut blocks = 0;
    for line in md.split_inclusive('\n') {
        let fence = line.trim_start();
        let first = fence.chars().next().filter(|c| matches!(c, '`' | '~'));
        let run = first.map_or(0, |c| fence.chars().take_while(|x| *x == c).count());
        match (open, first) {
            (None, Some(c)) if run >= 3 => {
                open = Some((c, run));
                blocks += 1;
                if blocks > MAX_HIGHLIGHTED_BLOCKS {
                    out.push_str(&line[..line.len() - fence.len()]); // indent
                    out.push_str(&fence[..run]);
                    out.push('\n');
                    continue;
                }
            }
            (Some((c, n)), Some(x)) if x == c && run >= n && fence[run..].trim().is_empty() => open = None,
            _ => {}
        }
        out.push_str(line);
    }
    out.into()
}

/// JSON re-indented with 2 spaces, every value kept as written (key order,
/// number spelling). Err: where the JSON is invalid.
fn pretty_json(src: &str) -> Result<String, String> {
    serde_json::from_str::<serde::de::IgnoredAny>(src).map_err(|e| format!("ligne {}, colonne {}", e.line(), e.column()))?;
    let mut out = String::with_capacity(src.len() * 2);
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    };
    let (mut depth, mut in_string, mut escaped) = (0, false, false);
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            (escaped, in_string) = (!escaped && c == '\\', escaped || c != '"');
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(|c| c.is_whitespace()).is_some() {}
                // `{}` and `[]` stay on one line.
                match chars.next_if(|c| matches!(c, '}' | ']')) {
                    Some(close) => out.push(close),
                    None => {
                        depth += 1;
                        newline(&mut out, depth);
                    }
                }
            }
            '}' | ']' => {
                depth -= 1;
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out.push('\n');
    Ok(out)
}

/// `{file}` → the quoted file, `{out}` → a temp path for compiled binaries,
/// preceded by the project's variables. They are typed with the command, in
/// plain sight, never given to the shell at startup (where `ZDOTDIR`,
/// `BASH_ENV`… would run the project's code as soon as the terminal opens).
fn expand_run(cmd: &str, file: &Path, env: &HashMap<String, String>) -> String {
    let stem = file.file_stem().unwrap_or_default().to_string_lossy();
    let out = std::env::temp_dir().join(format!("akolade-{stem}"));
    let cmd = cmd.replace("{file}", &shell_quote(&file.to_string_lossy())).replace("{out}", &shell_quote(&out.to_string_lossy()));
    let is_name = |k: &str| k.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let mut vars: Vec<String> = env.iter().filter(|(k, _)| is_name(k)).map(|(k, v)| format!("{k}={}", shell_quote(v))).collect();
    if vars.is_empty() {
        return cmd;
    }
    vars.sort();
    // PowerShell has no `export`: each variable is set on its own.
    if cfg!(windows) { format!("{}; {cmd}", vars.iter().map(|v| format!("$env:{v}")).collect::<Vec<_>>().join("; ")) } else { format!("export {}; {cmd}", vars.join(" ")) }
}

fn home() -> PathBuf {
    crate::platform::home()
}

/// Theme color as 8-bit RGB, for the terminal's palette.
fn rgb8(color: Hsla) -> (u8, u8, u8) {
    let c = Rgba::from(color);
    ((c.r * 255.).round() as u8, (c.g * 255.).round() as u8, (c.b * 255.).round() as u8)
}

fn terminal_config(cx: &gpui::App) -> TerminalConfig {
    let theme = cx.theme();
    let ((br, bg, bb), (fr, fg, fb), (cr, cg, cb)) = (rgb8(theme.background), rgb8(theme.foreground), rgb8(theme.caret));
    // Background, text and cursor follow the theme; the 16 ANSI colors are the terminal's own.
    let colors = ColorPalette::builder()
        .background(br, bg, bb)
        .foreground(fr, fg, fb)
        .cursor(cr, cg, cb)
        .black(0x3a, 0x3f, 0x4b)
        .red(0xf0, 0x71, 0x78)
        .green(0xa8, 0xd6, 0x8a)
        .yellow(0xff, 0xcb, 0x6b)
        .blue(0x82, 0xaa, 0xff)
        .magenta(0xc7, 0x92, 0xea)
        .cyan(0x89, 0xdd, 0xff)
        .white(0xd7, 0xda, 0xe0)
        .bright_black(0x5c, 0x67, 0x73)
        .bright_white(0xff, 0xff, 0xff)
        .build();
    TerminalConfig {
        font_family: cx.theme().mono_font_family.to_string(),
        font_size: px(AppSettings::get(cx).terminal_font_size as f32),
        padding: gpui::Edges { top: px(4.), right: px(12.), bottom: px(4.), left: px(12.) },
        colors,
        ..Default::default()
    }
}

/// Colored file-type icon (full-color SVG).
fn file_icon(path: &Path, cx: &App) -> AnyElement {
    icons::file(path, 16., cx)
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _settings = cx.observe_global_in::<AppSettings>(window, |this, window, cx| this.apply_settings(window, cx));
        // shortcut: polls `git status` every 3 s; watch the files instead if huge repositories make it slow.
        // Both polls pause while the window is in the background (a `git status` and a tree
        // walk every few seconds for nobody); coming back refreshes at once (`_activation`).
        let _git_poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(task) = this.update(cx, |this, cx| this.window_active.then(|| this.refresh_git(cx))) else { break };
                if let Some(task) = task {
                    task.await;
                }
                cx.background_executor().timer(Duration::from_secs(3)).await;
            }
        });
        // Servers and linters report from their own threads: look often, it is one atomic read.
        let _problems_poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(300)).await;
                if this.update(cx, |this, cx| this.show_problems(cx)).is_err() {
                    break;
                }
            }
        });
        // Files created, deleted or renamed elsewhere (terminal, git…) show up on their own:
        // the project's open folders are read again off the UI thread.
        let _explorer_poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let Ok(task) = this.update(cx, |this, cx| this.window_active.then(|| this.refresh_explorer(cx))) else { break };
                if let Some(task) = task {
                    task.await;
                }
            }
        });
        let _activation = cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            if this.window_active {
                this.refresh_git(cx).detach();
                this.refresh_explorer(cx).detach();
            }
        });
        let weak = cx.entity().downgrade();
        let search = cx.new(|cx| SearchView::new(weak.clone(), false, window, cx));
        let todo = cx.new(|cx| SearchView::new(weak.clone(), true, window, cx));
        let tasks = cx.new(|_| TasksView::new(weak));
        Self {
            root: None,
            explorer: Explorer::default(),
            config: Config::default(),
            buffers: vec![],
            active: 0,
            split: None,
            focus_right: false,
            untitled: 0,
            terminals: vec![],
            active_terminal: 0,
            terminals_made: 0,
            show_terminal: false,
            show_sidebar: true,
            discard: None,
            focus: cx.focus_handle(),
            servers: Rc::default(),
            format_once: false,
            applied: AppSettings::get(cx).clone(),
            _autosave: None,
            _settings,
            previewed: Cell::new(None),
            closed: vec![],
            plugin_jobs: HashSet::new(),
            sidebar_width: r(260.).to_pixels(window.rem_size()),
            base_syntax: cx.theme().highlight_theme.clone(),
            syntax_color: None,
            git: None,
            _git_poll,
            tab_widths: Rc::default(),
            closing: None,
            closes: 0,
            _closing: None,
            problems_version: 0,
            _problems_poll,
            _explorer_poll,
            window_active: true,
            _activation,
            _lsp_sync: HashMap::new(),
            lint_gate: Arc::default(),
            restoring: false,
            menu_target: None,
            busy: None,
            act_scroll: ScrollHandle::new(),
            term_opened: 0,
            term_was_shown: false,
            last_file: None,
            _flash: None,
            sidebar_was_shown: true,
            sidebar_anim: None,
            sidebar_slides: 0,
            _sidebar_slide: None,
            panel: Panel::Explorer,
            search,
            todo,
            tasks,
        }
    }

    /// The window may close: nothing unsaved, or the user insisted.
    pub fn confirm_window_close(&mut self) -> bool {
        !self.buffers.iter().any(|b| b.dirty) || confirm(&mut self.discard, Discard::Window)
    }

    pub fn set_root(&mut self, root: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let changed = root != self.root;
        if changed {
            self.explorer.clear();
            // The old project's plugins would only use memory.
            self.servers.stop_all();
            // The shell must restart in the new folder with the new env.
            self.terminals.clear();
            self.show_terminal = false;
        }
        self.config = Config::default();
        if let Some(root) = &root {
            self.explorer.load(root, &AppSettings::get(cx).scan_options());
            let last = root.clone();
            AppSettings::update(cx, |s| {
                s.recent_folders.retain(|p| *p != last);
                s.recent_folders.insert(0, last.clone());
                s.recent_folders.truncate(12);
                s.last_folder = Some(last);
            });
            if let Ok(s) = fs::read_to_string(root.join("akolade.json")) {
                match serde_json::from_str(&s) {
                    Ok(c) => self.config = c,
                    Err(e) => window.push_notification(format!("akolade.json invalide : {e}"), cx),
                }
            }
        }
        self.root = root;
        // The panels follow the new folder (the hidden ones read it when shown).
        let root = self.root.clone();
        self.search.update(cx, |v, cx| v.search_in(root.clone(), cx));
        match self.panel {
            Panel::Todo => self.todo.update(cx, |v, cx| v.search_in(root, cx)),
            Panel::Scripts => self.tasks.update(cx, |v, cx| v.load(root.as_deref(), cx)),
            _ => {}
        }
        // Another project: its own tabs from last time (not on a rescan).
        if changed && AppSettings::get(cx).reopen_last_folder {
            self.restore_session(window, cx);
        }
        cx.notify();
    }

    fn rescan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_root(self.root.clone(), window, cx);
    }

    /// Settings changed: update what is already open.
    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = AppSettings::get(cx).clone();
        let old = std::mem::replace(&mut self.applied, s.clone());
        if (old.line_numbers, old.soft_wrap) != (s.line_numbers, s.soft_wrap) {
            for editor in self.buffers.iter().filter_map(Buffer::editor) {
                editor.update(cx, |e, cx| {
                    e.set_line_number(s.line_numbers, window, cx);
                    e.set_soft_wrap(s.soft_wrap, window, cx);
                });
            }
        }
        // The theme was applied again (see `apply_appearance`): tint its colors anew.
        self.base_syntax = cx.theme().highlight_theme.clone();
        self.syntax_color = None;
        if (old.terminal_font_size, &old.code_font, old.ui_font_size, &old.theme) != (s.terminal_font_size, &s.code_font, s.ui_font_size, &s.theme)
        {
            for t in &self.terminals {
                let config = terminal_config(cx);
                t.view.update(cx, |v, cx| v.update_config(config, cx));
            }
        }
        if old.scan_options() != s.scan_options() {
            self.rescan(window, cx);
        }
        for l in lint::LINTERS.iter().filter(|l| old.plugin_enabled(l.id) && !s.plugin_enabled(l.id)) {
            // Stopped first: a run in flight would publish after the clear.
            self.lint_gate.stop(l.id);
            self.servers.problems.clear_origin(l.id);
        }
        // A plugin turned off stops for real (it can hold hundreds of MB).
        for lang in languages::LANGUAGES {
            if let Some(server) = &lang.server
                && old.server_enabled(server.program)
                && !s.server_enabled(server.program)
            {
                self.servers.stop(server.program);
            }
        }
        cx.notify();
    }

    /// Writes the edited files one second after the last keystroke. Never over
    /// a file changed on disk meanwhile, and errors leave the tab dirty (the
    /// next ⌘S shows them).
    fn schedule_autosave(&mut self, cx: &mut Context<Self>) {
        if !AppSettings::get(cx).autosave {
            return;
        }
        self._autosave = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
            let _ = this.update(cx, |this, cx| {
                let mut saved = vec![];
                for b in this.buffers.iter_mut().filter(|b| b.dirty) {
                    let (Some(path), Some(editor)) = (b.path.clone(), b.editor().cloned()) else { continue };
                    if modified(&path) != b.disk_mtime {
                        continue;
                    }
                    let text = editor.read(cx).value().to_string();
                    if write_file(&path, &text).is_ok() {
                        b.dirty = false;
                        b.disk_mtime = modified(&path);
                        saved.push((path, b.lang, text));
                    }
                }
                for (path, lang, text) in saved {
                    this.lint(path, lang, text, true, cx);
                }
                cx.notify();
            });
        }));
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.buffers.iter().position(|b| matches!(b.view, View::Settings(_))) {
            return self.activate(ix, window, cx);
        }
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| SettingsView::new(workspace, cx));
        let sub = cx.observe(&view, |_, _, cx| cx.notify());
        self.open_page("Paramètres", View::Settings(view), Some(sub), window, cx);
    }

    pub fn open_plugins(&mut self, _: &OpenPlugins, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.buffers.iter().position(|b| matches!(b.view, View::Plugins(_))) {
            return self.activate(ix, window, cx);
        }
        let workspace = cx.entity().downgrade();
        let view = cx.new(|cx| PluginsView::new(workspace, window, cx));
        let sub = cx.observe(&view, |_, _, cx| cx.notify());
        self.open_page("Store", View::Plugins(view), Some(sub), window, cx);
    }

    pub fn open_git(&mut self, _: &OpenGit, window: &mut Window, cx: &mut Context<Self>) {
        self.open_tool("git", window, cx);
    }

    /// The Git tab, on this file's changes (or its conflict).
    fn show_changes(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_tool("git", window, cx);
        let view = self.buffers.iter().find_map(|b| match &b.view {
            View::Tool { id: "git", view } => view.clone().downcast::<git_view::GitView>().ok(),
            _ => None,
        });
        // After this update: the Git view reads the workspace (its folder) when it refreshes.
        if let Some(view) = view {
            window.defer(cx, move |_, cx| view.update(cx, |g, cx| g.show_file(path, cx)));
        }
    }

    /// A store tool's tab, opened once.
    pub fn open_tool(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.buffers.iter().position(|b| matches!(b.view, View::Tool { id: i, .. } if i == id)) {
            return self.activate(ix, window, cx);
        }
        let Some(tool) = tools::find(id) else { return };
        let view = (tool.build)(cx.entity().downgrade(), self.root.clone(), window, cx);
        self.open_page(tool.name, View::Tool { id: tool.id, view }, None, window, cx);
    }

    /// Its activity bar icon: shows the tool, or back to the files if it shows it already.
    fn toggle_tool(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.buffers.get(self.active).map(|b| &b.view), Some(View::Tool { id: i, .. }) if *i == id) {
            return self.hide_tool(window, cx);
        }
        self.open_tool(id, window, cx);
    }

    /// Leaving a tool by its icon shows the last file tab again (or the welcome page when there is none).
    fn hide_tool(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let last = self.last_file.and_then(|id| self.buffers.iter().position(|b| b.id() == id));
        match last.or_else(|| self.buffers.iter().rposition(|b| !matches!(b.view, View::Tool { .. }))) {
            Some(ix) => self.activate(ix, window, cx),
            None => {
                self.active = self.buffers.len(); // no tab: the welcome page
                self.focus.focus(window);
                cx.notify();
            }
        }
    }

    /// The buffers shown as tabs, in order: files, settings, the Store, tools.
    fn tab_indices(&self) -> Vec<usize> {
        (0..self.buffers.len()).collect()
    }

    pub fn open_search(&mut self, _: &OpenSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.show_panel(Panel::Search, window, cx);
    }

    /// Its activity bar icon: shows the panel, or hides the sidebar if it shows it already.
    fn toggle_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_sidebar && self.panel == panel {
            self.show_sidebar = false;
            return cx.notify();
        }
        self.show_panel(panel, window, cx);
    }

    /// The panels read the project when shown (the folder given: they cannot read the workspace now).
    fn show_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        (self.show_sidebar, self.panel) = (true, panel);
        let root = self.root.clone();
        match panel {
            Panel::Explorer => {}
            Panel::Search => {
                let input = self.search.read(cx).input.clone();
                input.update(cx, |i, cx| i.focus(window, cx));
            }
            Panel::Todo => self.todo.update(cx, |v, cx| v.search_in(root, cx)),
            Panel::Scripts => self.tasks.update(cx, |v, cx| v.load(root.as_deref(), cx)),
            Panel::Problems => {}
        }
        cx.notify();
    }

    /// New problems from the servers or linters: into the editors (underlines) and the panel.
    fn show_problems(&mut self, cx: &mut Context<Self>) {
        let version = self.servers.problems.version();
        if version == self.problems_version {
            return;
        }
        self.problems_version = version;
        for b in &self.buffers {
            if let (Some(path), Some(editor)) = (&b.path, b.editor()) {
                set_diagnostics(editor, &self.servers.problems.of(path), cx);
            }
        }
        cx.notify();
    }

    /// The active editor's symbol under the cursor, its file and language server (the
    /// text sent first): for F12, ⇧F12, F2.
    fn symbol_at_cursor(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<(Entity<InputState>, PathBuf, Arc<lsp::Client>, (usize, usize))> {
        let found = self.buffers.get(self.active).and_then(|b| {
            let (editor, path) = (b.editor()?.clone(), b.path.clone()?);
            let server = b.lang.server.as_ref().filter(|s| AppSettings::get(cx).server_enabled(s.program))?;
            let client = self.servers.client(b.lang, &self.project_root(Some(&path)))?;
            let text = editor.read(cx).value().to_string();
            client.sync(&path, server.language_id, &text);
            // In UTF-16 units, as servers count (the editor counts characters).
            let pos = lsp::position(&text, editor.read(cx).cursor());
            Some((editor, path, client, pos))
        });
        if found.is_none() {
            window.push_notification("Il faut le plugin de complétion de ce langage (Store › Complétion), et un fichier enregistré.", cx);
        }
        found
    }

    /// An answer from the language server, or Null after a few seconds.
    async fn answer(rx: futures::channel::oneshot::Receiver<serde_json::Value>, timer: Task<()>) -> serde_json::Value {
        match futures::future::select(rx, Box::pin(timer)).await {
            futures::future::Either::Left((Ok(v), _)) => v,
            _ => serde_json::Value::Null,
        }
    }

    /// F12: where the symbol under the cursor is defined.
    fn go_to_definition(&mut self, _: &GoToDefinitionKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, path, client, (line, col))) = self.symbol_at_cursor(window, cx) else { return };
        let (rx, timer) = (client.definition(&path, line, col), cx.background_executor().timer(Duration::from_secs(5)));
        self.set_busy(Some("Recherche de la définition…"), cx);
        cx.spawn_in(window, async move |this, cx| {
            let found = completion::places(&Self::answer(rx, timer).await);
            let _ = this.update_in(cx, |this, window, cx| {
                this.set_busy(None, cx);
                match found.first() {
                    Some(p) => this.open_at_lsp(p.path.clone(), p.start.0, p.start.1, window, cx),
                    None => window.push_notification("Définition introuvable.", cx),
                }
            });
        })
        .detach();
    }

    /// Right-click › Vérifier le fichier: every linter now (project ones too), then the Problems panel.
    fn lint_file(&mut self, _: &LintFile, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers.get(self.active) else { return };
        let (Some(path), Some(editor)) = (b.path.clone(), b.editor()) else { return };
        let (lang, text) = (b.lang, editor.read(cx).value().to_string());
        self.lint(path, lang, text, true, cx);
        self.show_panel(Panel::Problems, window, cx);
    }

    fn copy_file_path(&mut self, _: &CopyFilePath, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.buffers.get(self.active).and_then(|b| b.path.clone()) {
            cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
            window.push_notification("Chemin copié.", cx);
        }
    }

    fn copy_relative_path(&mut self, _: &CopyRelativePath, window: &mut Window, cx: &mut Context<Self>) {
        let rel = self.buffers.get(self.active).and_then(|b| b.path.as_ref()).zip(self.root.as_ref()).and_then(|(p, root)| p.strip_prefix(root).ok());
        if let Some(rel) = rel {
            cx.write_to_clipboard(ClipboardItem::new_string(rel.display().to_string()));
            window.push_notification("Chemin relatif copié.", cx);
        }
    }

    /// Its folders open in the explorer, which shows.
    fn reveal_in_explorer(&mut self, _: &RevealInExplorer, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.buffers.get(self.active).and_then(|b| b.path.clone()) {
            self.show_panel(Panel::Explorer, window, cx);
            self.show_in_explorer(&path, cx);
        }
    }

    fn show_file_changes(&mut self, _: &ShowFileChanges, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.buffers.get(self.active).and_then(|b| b.path.clone()) {
            self.show_changes(path, window, cx);
        }
    }

    /// `git status` of the project, off the UI thread; the margin marks follow a change.
    fn refresh_git(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let root = self.root.clone().filter(|_| AppSettings::get(cx).tool_added("git"));
        cx.spawn(async move |this, cx| {
            let summary = match root.clone() {
                Some(root) => cx.background_executor().spawn(async move { git::summary(&root) }).await,
                None => None,
            };
            let _ = this.update(cx, |this, cx| {
                // Not if the folder changed meanwhile.
                if this.root == root && this.git != summary {
                    this.git = summary;
                    // A commit, a checkout…: the margin marks of open files change.
                    this.refresh_all_marks(cx);
                    cx.notify();
                }
            });
        })
    }

    /// The explorer's open folders read again off the UI thread (files made elsewhere).
    fn refresh_explorer(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let copy = self.root.is_some().then(|| self.explorer.fresh());
        cx.spawn(async move |this, cx| {
            let Some(mut copy) = copy else { return };
            let copy = cx
                .background_executor()
                .spawn(async move {
                    copy.reload();
                    copy
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.explorer.take_if_changed(copy) {
                    cx.notify();
                }
            });
        })
    }

    /// The status bar's "waiting for…" (None: done).
    fn set_busy(&mut self, what: Option<&'static str>, cx: &mut Context<Self>) {
        self.busy = what.map(SharedString::from);
        cx.notify();
    }

    /// ⇧F12: every use of the symbol under the cursor, listed in the Search panel.
    fn find_references(&mut self, _: &FindReferences, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, path, client, (line, col))) = self.symbol_at_cursor(window, cx) else { return };
        let word = word_at(&editor.read(cx).value(), line, col);
        let (rx, timer) = (client.references(&path, line, col), cx.background_executor().timer(Duration::from_secs(8)));
        self.set_busy(Some("Recherche des références…"), cx);
        cx.spawn_in(window, async move |this, cx| {
            let found = completion::places(&Self::answer(rx, timer).await);
            let _ = this.update_in(cx, |this, window, cx| {
                this.set_busy(None, cx);
                if found.is_empty() {
                    return window.push_notification(format!("Aucune référence à « {word} »."), cx);
                }
                let files = this.reference_hits(found, cx);
                let title = format!("Références à « {word} »");
                this.show_panel(Panel::Search, window, cx);
                this.search.update(cx, |v, cx| v.show_found(title, files, cx));
            });
        })
        .detach();
    }

    /// Search › Replace: in open tabs like typed (⌘Z undoes), else written to the files.
    pub fn replace_in_files(&mut self, rels: Vec<PathBuf>, needle: &str, replacement: &str, case: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        let (mut count, mut changed, mut on_disk) = (0, 0, vec![]);
        for rel in rels {
            let path = root.join(&rel);
            match self.buffers.iter().find(|b| b.path.as_ref() == Some(&path)).and_then(Buffer::editor).cloned() {
                Some(editor) => {
                    let src = editor.read(cx).value().to_string();
                    let (text, n) = search::replace_all(&src, needle, replacement, case);
                    if n > 0 {
                        editor.update(cx, |e, cx| e.replace_text_in_range(Some(0..src.encode_utf16().count()), &text, window, cx));
                        (count, changed) = (count + n, changed + 1);
                    }
                }
                None => on_disk.push((rel, path)),
            }
        }
        // The other files off the UI thread: hundreds of them would freeze the window.
        let (needle, replacement) = (needle.to_string(), replacement.to_string());
        let job = cx.background_executor().spawn(async move {
            let (mut count, mut changed, mut failed) = (count, changed, vec![]);
            for (rel, path) in on_disk {
                match fs::read_to_string(&path) {
                    Ok(src) => {
                        let (text, n) = search::replace_all(&src, &needle, &replacement, case);
                        if n > 0 {
                            match write_file(&path, &text) {
                                Ok(()) => (count, changed) = (count + n, changed + 1),
                                Err(_) => failed.push(rel.display().to_string()),
                            }
                        }
                    }
                    Err(_) => failed.push(rel.display().to_string()),
                }
            }
            (count, changed, failed)
        });
        self.set_busy(Some("Remplacement…"), cx);
        cx.spawn_in(window, async move |this, cx| {
            let (count, changed, failed) = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.set_busy(None, cx);
                // The results again, now that the files on disk changed too (given the
                // folder: `refresh` would read the workspace, being updated here).
                let root = this.root.clone();
                this.search.update(cx, |v, cx| v.search_in(root, cx));
                let plural = |n: usize| if n > 1 { "s" } else { "" };
                let note = format!("{count} remplacement{} dans {changed} fichier{}.", plural(count), plural(changed));
                match failed.is_empty() {
                    true => window.push_notification(Notification::success(note), cx),
                    false => window.push_notification(Notification::error(format!("{note} Impossible de modifier : {}", failed.join(", "))), cx),
                }
            });
        })
        .detach();
    }

    /// Places as search results: each one's line, read from its open tab or its file.
    fn reference_hits(&self, places: Vec<completion::Place>, cx: &App) -> Vec<search::FileHits> {
        let mut files: Vec<search::FileHits> = vec![];
        // Each file's lines once, however many references it has.
        let mut texts: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for p in places {
            let lines = texts.entry(p.path.clone()).or_insert_with(|| {
                let text = match self.buffers.iter().find(|b| b.path.as_ref() == Some(&p.path)).and_then(Buffer::editor) {
                    Some(editor) => editor.read(cx).value().to_string(),
                    None => fs::read_to_string(&p.path).unwrap_or_default(),
                };
                text.lines().map(str::to_string).collect()
            });
            let line = lines.get(p.start.0 as usize).map_or("", String::as_str);
            let rel = self.root.as_ref().and_then(|r| p.path.strip_prefix(r).ok()).map_or(p.path.clone(), Path::to_path_buf);
            let hit = search::hit_at(line, p.start.0, p.start.1, p.end.1);
            match files.iter_mut().find(|f| f.rel == rel) {
                Some(f) => f.hits.push(hit),
                None => files.push(search::FileHits { rel, hits: vec![hit] }),
            }
        }
        files
    }

    /// F2: renames the symbol under the cursor everywhere the language server knows.
    fn rename_symbol(&mut self, _: &RenameSymbol, window: &mut Window, cx: &mut Context<Self>) {
        let Some((editor, path, client, (line, col))) = self.symbol_at_cursor(window, cx) else { return };
        let word = word_at(&editor.read(cx).value(), line, col);
        if word.is_empty() {
            return window.push_notification("Placez le curseur sur un nom à renommer.", cx);
        }
        let (title, initial) = (format!("Renommer « {word} » partout"), word.clone());
        let then = Rc::new(move |ws: &mut Self, new: String, window: &mut Window, cx: &mut Context<Self>| {
            let new = new.trim().to_string();
            if !new.is_empty() && new != word {
                ws.rename_with(path.clone(), client.clone(), (line, col), word.clone(), new, window, cx);
            }
        });
        self.ask_text(title, initial, "Renommer", then, window, cx);
    }

    fn rename_with(&mut self, path: PathBuf, client: Arc<lsp::Client>, (line, col): (usize, usize), old: String, new: String, window: &mut Window, cx: &mut Context<Self>) {
        let sent = self.sync_tabs_of(&path, &client, cx);
        let (rx, timer) = (client.rename(&path, line, col, &new), cx.background_executor().timer(Duration::from_secs(8)));
        self.set_busy(Some("Renommage…"), cx);
        cx.spawn_in(window, async move |this, cx| {
            let files = completion::workspace_edits(&Self::answer(rx, timer).await);
            let _ = this.update_in(cx, |this, window, cx| {
                this.set_busy(None, cx);
                if files.is_empty() {
                    return window.push_notification(format!("Le plugin ne sait pas renommer « {old} » ici."), cx);
                }
                match this.apply_workspace_edit(&files, &sent, &client, window, cx) {
                    Err(refused) => window.push_notification(Notification::error(format!("Renommage annulé, rien n'a changé : {}. Réessayez.", refused.join(", "))), cx),
                    Ok((count, failed)) => {
                        let note = format!("« {old} » → « {new} » : {count} modification{} dans {} fichier{}.", if count > 1 { "s" } else { "" }, files.len(), if files.len() > 1 { "s" } else { "" });
                        match failed.is_empty() {
                            true => window.push_notification(Notification::success(note), cx),
                            false => window.push_notification(Notification::error(format!("{note} Impossible d'écrire : {}", failed.join(", "))), cx),
                        }
                    }
                }
            });
        })
        .detach();
    }

    /// ⌘. : the language server's fixes and refactorings at the cursor, one to pick.
    fn quick_fix(&mut self, _: &QuickFix, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, path, client, (line, col))) = self.symbol_at_cursor(window, cx) else { return };
        let sent = Rc::new(self.sync_tabs_of(&path, &client, cx));
        let (rx, timer) = (client.code_actions(&path, line, col), cx.background_executor().timer(Duration::from_secs(8)));
        self.set_busy(Some("Corrections…"), cx);
        cx.spawn_in(window, async move |this, cx| {
            let answer = Self::answer(rx, timer).await;
            // shortcut: actions that only run a server command (no edit to resolve) are left out; answer `workspace/applyEdit` if a server needs them.
            let actions: Rc<Vec<serde_json::Value>> = Rc::new(answer.as_array().into_iter().flatten().filter(|a| a["disabled"].is_null() && (a["edit"].is_object() || !a["data"].is_null())).cloned().collect());
            let _ = this.update_in(cx, |this, window, cx| {
                this.set_busy(None, cx);
                if actions.is_empty() {
                    return window.push_notification("Aucune correction proposée ici.", cx);
                }
                let ws = cx.entity().downgrade();
                window.open_dialog(cx, move |dialog, _, _| {
                    let rows = actions.iter().enumerate().map(|(i, action)| {
                        let (ws, action, client, sent) = (ws.clone(), action.clone(), client.clone(), sent.clone());
                        button(("quick-fix", i)).ghost().w_full().justify_start().label(action["title"].as_str().unwrap_or_default().to_string()).on_click(move |_, window, cx| {
                            window.close_dialog(cx);
                            let _ = ws.update(cx, |ws, cx| ws.run_code_action(action.clone(), client.clone(), sent.clone(), window, cx));
                        })
                    });
                    dialog.title("Corrections rapides").child(v_flex().gap(r(2.)).children(rows))
                });
            });
        })
        .detach();
    }

    /// A chosen action's edit (asked for first if the server sends it only now), applied.
    fn run_code_action(&mut self, action: serde_json::Value, client: Arc<lsp::Client>, sent: Rc<HashMap<PathBuf, String>>, window: &mut Window, cx: &mut Context<Self>) {
        let rx = (!action["edit"].is_object()).then(|| client.resolve_action(&action));
        let timer = cx.background_executor().timer(Duration::from_secs(8));
        cx.spawn_in(window, async move |this, cx| {
            let title = action["title"].as_str().unwrap_or_default().to_string();
            let action = match rx {
                Some(rx) => Self::answer(rx, timer).await,
                None => action,
            };
            let files = completion::workspace_edits(&action["edit"]);
            let _ = this.update_in(cx, |this, window, cx| {
                if files.is_empty() {
                    return window.push_notification(format!("« {title} » : le plugin n'a rien modifié."), cx);
                }
                match this.apply_workspace_edit(&files, &sent, &client, window, cx) {
                    Err(refused) => window.push_notification(Notification::error(format!("Correction annulée, rien n'a changé : {}. Réessayez.", refused.join(", "))), cx),
                    Ok((_, failed)) if !failed.is_empty() => window.push_notification(Notification::error(format!("« {title} » : impossible d'écrire {}", failed.join(", "))), cx),
                    Ok(_) => {}
                }
            });
        })
        .detach();
    }

    /// The server gets every open tab of `path`'s language as it is now, and we keep
    /// that text: its edits are offsets in it (see `apply_workspace_edit`).
    fn sync_tabs_of(&self, path: &Path, client: &lsp::Client, cx: &App) -> HashMap<PathBuf, String> {
        let mut sent: HashMap<PathBuf, String> = HashMap::new();
        if let Some(lang) = self.buffers.iter().find(|b| b.path.as_deref() == Some(path)).map(|b| b.lang)
            && let Some(server) = lang.server.as_ref()
        {
            for b in self.buffers.iter().filter(|b| b.lang.name == lang.name) {
                if let (Some(p), Some(editor)) = (b.path.clone(), b.editor()) {
                    let text = editor.read(cx).value().to_string();
                    client.sync(&p, server.language_id, &text);
                    sent.insert(p, text);
                }
            }
        }
        sent
    }

    /// A language server's edits (rename, quick fix), in every file or none: a tab
    /// typed in during the wait (its sync would hide the change) or a file changed
    /// elsewhere refuses them all (Err: which ones). Ok: edits made, files not written.
    fn apply_workspace_edit(&mut self, files: &[(PathBuf, Vec<completion::Edit>)], sent: &HashMap<PathBuf, String>, client: &lsp::Client, window: &mut Window, cx: &mut Context<Self>) -> Result<(usize, Vec<String>), Vec<String>> {
        // All files checked first: edits in some and not others would break the code.
        let (mut plan, mut refused) = (vec![], vec![]);
        for (file, edits) in files {
            // An open tab changes like typed (⌘Z undoes it, ⌘S saves); other files are written.
            let buffer = self.buffers.iter().find(|b| b.path.as_ref() == Some(file));
            let editor = buffer.and_then(Buffer::editor).cloned();
            let src = match &editor {
                Some(editor) => editor.read(cx).value().to_string(),
                None => match fs::read_to_string(file) {
                    Ok(src) => src,
                    Err(_) => {
                        refused.push(format!("{} (illisible)", file.display()));
                        continue;
                    }
                },
            };
            let stale = match sent.get(file) {
                Some(text) => *text != src,
                // Not sent now: the server has our last sync of it, or reads the disk
                // (then an unsaved tab differs from what it read).
                None => client.synced_text(file).map_or(buffer.is_some_and(|b| b.dirty), |t| t != src),
            };
            if stale {
                refused.push(format!("{} (modifié entre-temps)", file.display()));
                continue;
            }
            plan.push((file, src, editor, edits));
        }
        if !refused.is_empty() {
            return Err(refused);
        }
        let (mut count, mut failed) = (0, vec![]);
        for (file, src, editor, edits) in plan {
            count += edits.len();
            let text = completion::apply_edits(&src, edits);
            match editor {
                Some(editor) => editor.update(cx, |e, cx| e.replace_text_in_range(Some(0..src.encode_utf16().count()), &text, window, cx)),
                None => {
                    if write_file(file, &text).is_err() {
                        failed.push(file.display().to_string());
                    }
                }
            }
        }
        self.reload_from_disk(window, cx);
        Ok((count, failed))
    }

    /// Git margin marks of a file's editor (added, modified, deleted lines), off the UI thread.
    fn refresh_marks(&self, editor: Entity<InputState>, path: PathBuf, cx: &mut Context<Self>) {
        if !AppSettings::get(cx).tool_added("git") {
            return;
        }
        let text = editor.read(cx).value().to_string();
        let theme = cx.theme();
        let (added, modified, deleted) = (theme.green, theme.blue, theme.red);
        let task = cx.background_executor().spawn(async move { git::line_changes(&path, &text) });
        cx.spawn(async move |_, cx| {
            let marks: Vec<_> = task
                .await
                .into_iter()
                .map(|(rows, change)| (rows, match change {
                    git::LineChange::Added => added,
                    git::LineChange::Modified => modified,
                    git::LineChange::Deleted => deleted,
                }))
                .collect();
            let _ = editor.update(cx, |e, cx| {
                if e.line_markers != marks {
                    e.line_markers = marks;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn refresh_all_marks(&self, cx: &mut Context<Self>) {
        for b in &self.buffers {
            if let (Some(path), Some(editor)) = (b.path.clone(), b.editor().cloned()) {
                self.refresh_marks(editor, path, cx);
            }
        }
    }


    /// Runs the file's linters in the background, through the gate: an 8 s Clippy
    /// no longer piles up behind every pause, and an older run can't finish last
    /// and bring back a fixed error. `saved`: also the ones that read the project.
    fn lint(&mut self, path: PathBuf, lang: &'static Language, text: String, saved: bool, cx: &mut Context<Self>) {
        let root = self.project_root(Some(&path));
        let linters: Vec<_> = lint::for_language(lang.name, &root, AppSettings::get(cx)).into_iter().filter(|(l, _)| saved || !l.project).collect();
        if linters.is_empty() {
            return;
        }
        let (problems, gate) = (self.servers.problems.clone(), self.lint_gate.clone());
        // Enabled (again): `for_language` only gives those. Here, before the run: a
        // linter turned off after this point stays stopped.
        for (l, _) in &linters {
            gate.resume(l.id);
        }
        cx.background_executor()
            .spawn(async move {
                for (l, program) in linters {
                    lint::run_gated(&gate, l, &program, &path, &root, text.clone(), |mut found| {
                        if l.project {
                            problems.replace_origin(l.id, found);
                        } else {
                            problems.set(path.clone(), l.id, found.remove(&path).unwrap_or_default());
                        }
                    });
                }
            })
            .detach();
    }

    /// Sends an edited file to its language server once typing pauses: its diagnostics follow.
    fn schedule_sync(&mut self, editor: Entity<InputState>, cx: &mut Context<Self>) {
        let id = editor.entity_id();
        self._lsp_sync.insert(id, cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let _ = this.update(cx, |this, cx| {
                let Some(b) = this.buffers.iter().find(|b| b.editor() == Some(&editor)) else { return };
                let (Some(path), lang) = (b.path.clone(), b.lang) else { return };
                let text = editor.read(cx).value().to_string();
                this.lint(path.clone(), lang, text.clone(), false, cx);
                this.refresh_marks(editor.clone(), path.clone(), cx);
                let Some(server) = lang.server.as_ref().filter(|s| AppSettings::get(cx).server_enabled(s.program)) else { return };
                if let Some(client) = this.servers.client(lang, &this.project_root(Some(&path))) {
                    client.sync(&path, server.language_id, &text);
                }
            });
        }));
    }

    /// Opens a file with the cursor on that line and column (0-based).
    pub fn open_at(&mut self, path: PathBuf, line: u32, column: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path(path.clone(), window, cx);
        if let Some(editor) = self.buffers.get(self.active).filter(|b| b.path.as_ref() == Some(&path)).and_then(Buffer::editor).cloned() {
            editor.update(cx, |e, cx| {
                e.set_cursor_position(Position::new(line, column), window, cx);
                // In the middle, even in a file just opened (not laid out yet: done after its first paint).
                e.center_row(line as usize, cx);
            });
            self.flash_line(editor, line as usize, cx);
        }
    }

    /// The line jumped to lights up, then fades out: the eye finds it.
    fn flash_line(&mut self, editor: Entity<InputState>, row: usize, cx: &mut Context<Self>) {
        let color = cx.theme().primary;
        editor.update(cx, |e, cx| {
            e.flash_row = Some((row, color.opacity(0.28)));
            cx.notify();
        });
        let animations = AppSettings::get(cx).animations;
        self._flash = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_millis(700)).await;
            // No animations: it stays a moment, then goes at once.
            if !animations {
                cx.background_executor().timer(Duration::from_millis(600)).await;
                let _ = editor.update(cx, |e, cx| {
                    e.flash_row = None;
                    cx.notify();
                });
                return;
            }
            // About 0.9 s of fading, in small steps.
            for step in (0..12).rev() {
                let alpha = 0.28 * step as f32 / 12.;
                let done = editor.update(cx, |e, cx| {
                    e.flash_row = (step > 0).then_some((row, color.opacity(alpha)));
                    cx.notify();
                });
                if done.is_err() {
                    return;
                }
                cx.background_executor().timer(Duration::from_millis(75)).await;
            }
        }));
    }

    /// `open_at` for a place a language server or linter gave: its column counts
    /// UTF-16 units (an emoji is 2), the editor's counts characters.
    pub fn open_at_lsp(&mut self, path: PathBuf, line: u32, utf16_col: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.open_at(path.clone(), line, 0, window, cx);
        if let Some(editor) = self.buffers.get(self.active).filter(|b| b.path.as_ref() == Some(&path)).and_then(Buffer::editor).cloned() {
            editor.update(cx, |e, cx| {
                let at = completion::editor_position(e.text(), Position::new(line, utf16_col));
                e.set_cursor_position(at, window, cx);
                e.center_row(line as usize, cx); // moving the cursor scrolled it to an edge
            });
        }
    }

    /// Types a command into the integrated terminal (opened if needed).
    /// A program already runs in the shown terminal (a dev server…): in a new one, next to it.
    pub fn run_in_terminal(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.ensure_terminal(window, cx).is_some_and(|t| t.run(command)) {
            return;
        }
        if self.new_terminal(window, cx) {
            let ix = self.active_terminal;
            self.terminals[ix].run(command);
        }
    }

    /// A command the project defines itself (package.json, Makefile…): asked once per folder, like Run.
    pub fn run_project_command(&mut self, command: String, runs: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.trusted() {
            let shown = if runs == command { command.clone() } else { format!("{command}\n→ {runs}") };
            let what = format!("Ce dossier définit ses propres scripts :\n\n{shown}");
            return self.ask_trust(what, move |this, window, cx| this.run_project_command(command, runs, window, cx), window, cx);
        }
        self.run_in_terminal(&command, window, cx);
    }

    /// A cloned project could ship any command: the user accepts this folder's, once.
    fn ask_trust(&mut self, what: String, then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "Faire confiance à ce projet ?",
            Some(&format!("{what}\n\nN'acceptez que si vous faites confiance à ce projet : ce sera retenu pour ce dossier (Paramètres › Sécurité).")),
            &["Faire confiance", "Annuler"],
            cx,
        );
        let asked_for = self.root.clone();
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| {
                    // Another folder opened while asking: that one wasn't asked about.
                    if this.root != asked_for {
                        return;
                    }
                    this.trust(cx);
                    then(this, window, cx);
                });
            }
        })
        .detach();
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The file tab shown last (a tool hides the tabs, not this).
    pub fn last_file_path(&self) -> Option<PathBuf> {
        let id = self.last_file?;
        self.buffers.iter().find(|b| b.id() == id)?.path.clone()
    }

    /// The user accepted this folder's own commands (see `ask_trust`).
    pub fn trusted(&self) -> bool {
        self.root.as_deref().is_some_and(trust::is_trusted)
    }

    /// Remembered for this folder: what was paused (Git with risky settings,
    /// ESLint, Clippy…) starts again.
    pub fn trust(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone().filter(|r| !trust::is_trusted(r)) else { return };
        AppSettings::update(cx, |s| s.trusted_folders.push(root));
        self.refresh_git(cx).detach();
        if let Some(b) = self.buffers.get(self.active)
            && let (Some(path), Some(editor)) = (b.path.clone(), b.editor())
        {
            let (lang, text) = (b.lang, editor.read(cx).value().to_string());
            self.lint(path, lang, text, false, cx);
        }
        cx.notify();
    }

    /// The status bar's "Mode restreint": what it pauses, and the way out.
    pub fn trust_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let what = "Ce dossier est en mode restreint : son propre code ne s'exécute pas tout seul.\n\nEn pause : ESLint et Clippy (ils exécutent la configuration et les scripts du projet), Git si le dépôt configure des commandes (hooks, filtres, fsmonitor), certains serveurs de langage (Ruby, GraphQL). Claude Code, les tests, les scripts et Prettier vous demandent avant de démarrer.".to_string();
        self.ask_trust(what, |_, _, _| {}, window, cx);
    }

    /// Unmodified tabs whose file changed on disk (git checkout, pull…) show it again.
    pub fn reload_from_disk(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for b in self.buffers.iter_mut().filter(|b| !b.dirty) {
            let (Some(path), Some(editor)) = (b.path.clone(), b.editor().cloned()) else { continue };
            let Some(mtime) = modified(&path).filter(|m| Some(*m) != b.disk_mtime) else { continue };
            let Ok(text) = fs::read_to_string(&path) else { continue };
            b.disk_mtime = Some(mtime);
            if text != editor.read(cx).value().as_str() {
                b.reloading = true;
                editor.update(cx, |e, cx| e.set_value(text, window, cx));
            }
        }
    }

    /// A tab that is not a file (settings, plugins).
    fn open_page(&mut self, title: &str, view: View, _sub: Option<Subscription>, window: &mut Window, cx: &mut Context<Self>) {
        self.buffers.push(Buffer { path: None, title: title.into(), lang: &languages::PLAIN, view, dirty: false, disk_mtime: None, md_mode: MdMode::Split, reloading: false, _sub });
        self.activate(self.buffers.len() - 1, window, cx);
    }

    pub fn plugin_running(&self, program: &str) -> bool {
        self.servers.is_running(program)
    }

    /// Installs (or uninstalls) a plugin in the background, into Akolade's own
    /// folder for npm packages; a notification follows it until the end.
    pub fn plugin_job(&mut self, plugin: &Plugin, install: bool, window: &mut Window, cx: &mut Context<Self>) {
        let id = plugin.programs[0];
        let command = if install { plugin.install.map(String::from) } else { plugin.uninstall_command() };
        let Some(command) = command else { return };
        if !self.plugin_jobs.insert(id) {
            return;
        }
        if !install {
            for program in &plugin.programs {
                self.servers.stop(program);
            }
        }
        // The folder made here: `mkdir -p` and `&&` are not PowerShell's.
        let _ = fs::create_dir_all(lsp::servers_dir());
        let dir = shell_quote(&lsp::servers_dir().to_string_lossy());
        let command = command.replace("{dir}", &dir);
        let (name, doing, done) = (plugin.name(), if install { "Installation" } else { "Désinstallation" }, if install { "installé" } else { "désinstallé" });
        window.push_notification(Notification::info(format!("{doing} du plugin {name}…")).id1::<PluginJob>(id).autohide(false), cx);
        // Through the user's shell, interactive and login like the terminal: a
        // Finder-launched app has a bare PATH, without npm or brew.
        let job = cx.background_executor().spawn(async move { crate::platform::shell_command(&command).stdin(Stdio::null()).output() });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |ws, window, cx| {
                ws.plugin_jobs.remove(id);
                let note = match result {
                    Ok(out) if out.status.success() => Notification::success(format!("Plugin {name} {done}.")),
                    Ok(out) => {
                        let err = String::from_utf8_lossy(&out.stderr);
                        let reason = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("erreur inconnue").trim().to_string();
                        Notification::error(format!("{doing} du plugin {name} impossible : {reason}"))
                    }
                    Err(e) => Notification::error(format!("{doing} du plugin {name} impossible : {e}")),
                };
                window.push_notification(note.id1::<PluginJob>(id), cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// The open folder, else the file's folder, else home.
    fn project_root(&self, path: Option<&Path>) -> PathBuf {
        self.root.clone().or_else(|| path.and_then(Path::parent).map(Path::to_path_buf)).unwrap_or_else(home)
    }

    fn completion(&self, lang: &'static Language, path: Option<&Path>, cx: &Context<Self>) -> Rc<NativeCompletion> {
        let ws = cx.entity().downgrade();
        // ⌘-click on a name defined in another file: opened after the editor's own click handling.
        let open = Rc::new(move |path: PathBuf, line: u32, col: u32, window: &mut Window, cx: &mut App| {
            let ws = ws.clone();
            window.defer(cx, move |window, cx| {
                let _ = ws.update(cx, |ws, cx| ws.open_at_lsp(path, line, col, window, cx));
            });
        });
        Rc::new(NativeCompletion { lang, path: path.map(Path::to_path_buf), root: self.project_root(path), servers: self.servers.clone(), open })
    }

    fn new_editor(&self, lang: &'static Language, path: Option<&Path>, content: String, window: &mut Window, cx: &mut Context<Self>) -> (Entity<InputState>, Subscription) {
        let completion = self.completion(lang, path, cx);
        let settings = AppSettings::get(cx).clone();
        let (ws, has_path) = (cx.entity().downgrade(), path.is_some());
        let editor = cx.new(|cx| {
            let mut s = InputState::new(window, cx)
                .code_editor(lang.highlight)
                .line_number(settings.line_numbers)
                .soft_wrap(settings.soft_wrap)
                .default_value(content);
            s.lsp.hover_provider = Some(completion.clone());
            s.lsp.definition_provider = Some(completion.clone());
            s.lsp.completion_provider = Some(completion);
            s.extra_context_menu = Some(Rc::new(move |menu, _, cx| context_menu(menu, ws.clone(), lang, has_path, cx)));
            s
        });
        let sub = cx.subscribe(&editor, |this, editor, ev: &InputEvent, cx| {
            if let (InputEvent::Change, Some(b)) = (ev, this.buffers.iter_mut().find(|b| b.editor() == Some(&editor))) {
                if std::mem::take(&mut b.reloading) {
                    // Read again from the disk (Git checkout, conflict resolved): not
                    // modified, but the server and linters must see the new text.
                    this.schedule_sync(editor.clone(), cx);
                    return;
                }
                b.dirty = true;
                this.discard = None;
                this.schedule_autosave(cx);
                this.schedule_sync(editor.clone(), cx);
                cx.notify();
            }
        });
        (editor, sub)
    }

    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.buffers.iter().position(|b| b.path.as_ref() == Some(&path)) {
            return self.activate(ix, window, cx);
        }
        let title = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let lang = languages::for_path(&path);
        let (view, _sub) = if image_view::is_image(&path) {
            let viewer = cx.new(|cx| ImageViewer::new(path.clone(), cx));
            let sub = cx.subscribe_in(&viewer, window, |this, viewer, ev: &ImageViewerEvent, window, cx| match ev {
                ImageViewerEvent::ShowSource => this.show_source(viewer, window, cx),
            });
            (View::Image(viewer), sub)
        } else {
            let Some((editor, sub)) = self.load_text(&path, lang, window, cx) else { return };
            (View::Text(editor), sub)
        };
        let disk_mtime = modified(&path);
        let md_mode = MdMode::from_setting(&AppSettings::get(cx).markdown_mode);
        if let View::Text(editor) = &view {
            set_diagnostics(editor, &self.servers.problems.of(&path), cx);
            if !self.restoring {
                let text = editor.read(cx).value().to_string();
                self.lint(path.clone(), lang, text, false, cx);
            }
            self.refresh_marks(editor.clone(), path.clone(), cx);
        }
        self.buffers.push(Buffer { path: Some(path), title, lang, view, dirty: false, disk_mtime, md_mode, reloading: false, _sub: Some(_sub) });
        self.activate(self.buffers.len() - 1, window, cx);
    }

    fn load_text(&self, path: &Path, lang: &'static Language, window: &mut Window, cx: &mut Context<Self>) -> Option<(Entity<InputState>, Subscription)> {
        match fs::read_to_string(path) {
            Ok(content) => {
                // Start the language server now: it is warm by the first `.`.
                let enabled = lang.server.as_ref().is_some_and(|s| AppSettings::get(cx).server_enabled(s.program));
                if let (true, Some(server), Some(client)) = (enabled, &lang.server, self.servers.client(lang, &self.project_root(Some(path)))) {
                    client.sync(path, server.language_id, &content);
                }
                Some(self.new_editor(lang, Some(path), content, window, cx))
            }
            Err(e) => {
                window.push_notification(format!("Impossible d'ouvrir {} : {e}", path.display()), cx);
                None
            }
        }
    }

    /// SVG viewer → code editor, in the same tab.
    fn show_source(&mut self, viewer: &Entity<ImageViewer>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.buffers.iter().position(|b| matches!(&b.view, View::Image(v) if v == viewer)) else { return };
        let (Some(path), lang) = (self.buffers[ix].path.clone(), self.buffers[ix].lang) else { return };
        let Some((editor, sub)) = self.load_text(&path, lang, window, cx) else { return };
        let b = &mut self.buffers[ix];
        b.view = View::Text(editor);
        b.disk_mtime = modified(&path);
        b._sub = Some(sub);
        self.activate(ix, window, cx);
    }

    fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        self.untitled += 1;
        let (editor, _sub) = self.new_editor(&languages::PLAIN, None, String::new(), window, cx);
        let title = format!("Sans titre-{}", self.untitled);
        self.buffers.push(Buffer { path: None, title, lang: &languages::PLAIN, view: View::Text(editor), dirty: false, disk_mtime: None, md_mode: MdMode::Split, reloading: false, _sub: Some(_sub) });
        self.activate(self.buffers.len() - 1, window, cx);
    }

    /// Reopens the project's tabs from last time (at launch).
    fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        let s = session::load(&settings::app_dir(), &root);
        // shortcut: only the active tab is linted now (40 tabs = 40 ESLint at once);
        // the others on their first edit or save.
        self.restoring = true;
        for f in s.files {
            self.open_path(f, window, cx);
        }
        if let Some(ix) = s.active.and_then(|a| self.buffers.iter().position(|b| b.path.as_ref() == Some(&a))) {
            self.activate(ix, window, cx);
        }
        self.restoring = false;
        self.save_session();
        if let Some(b) = self.buffers.get(self.active)
            && let (Some(path), Some(editor)) = (b.path.clone(), b.editor())
        {
            let (lang, text) = (b.lang, editor.read(cx).value().to_string());
            self.lint(path, lang, text, false, cx);
        }
    }

    /// Remembers the project's file tabs, for the next launch.
    fn save_session(&self) {
        let Some(root) = self.root.as_ref().filter(|_| !self.restoring) else { return };
        // Only this project's files: tabs of the previous folder or opened from
        // elsewhere would replace its own list.
        let files = self.buffers.iter().filter_map(|b| b.path.clone()).filter(|p| p.starts_with(root)).collect();
        let active = self.buffers.get(self.active).and_then(|b| b.path.clone()).filter(|p| p.starts_with(root));
        session::save(&settings::app_dir(), root, session::Session { files, active });
    }

    /// ⌘\\: the current tab and another one side by side, or back to one.
    fn split_editor(&mut self, _: &SplitEditor, window: &mut Window, cx: &mut Context<Self>) {
        if self.split.take().is_some() {
            return self.activate(self.active, window, cx);
        }
        if self.buffers.len() < 2 {
            return window.push_notification("Ouvrez un deuxième onglet pour l'afficher à côté.", cx);
        }
        self.split = Some(if self.active + 1 < self.buffers.len() { self.active + 1 } else { self.active - 1 });
        self.focus_right = true;
        cx.notify();
    }

    /// A tab clicked: on the right side if it was clicked last (or ⌥-click), else on the left.
    fn click_tab(&mut self, ix: usize, right: bool, window: &mut Window, cx: &mut Context<Self>) {
        if (right || (self.split.is_some() && self.focus_right)) && ix != self.active {
            self.split = Some(ix);
            self.focus_right = true;
            if let Some(editor) = self.buffers[ix].editor() {
                editor.update(cx, |e, cx| e.focus(window, cx));
            }
            return cx.notify();
        }
        self.focus_right = false;
        self.activate(ix, window, cx);
    }

    fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.active;
        self.active = ix.min(self.buffers.len().saturating_sub(1));
        // A tab can't be on both sides: the left one takes the right's place.
        if self.split == Some(self.active) {
            self.split = (before != self.active && before < self.buffers.len()).then_some(before);
        }
        self.save_session();
        let ix = self.active;
        self.discard = None;
        if let Some(b) = self.buffers.get(ix).filter(|b| !matches!(b.view, View::Tool { .. })) {
            self.last_file = Some(b.id());
        }
        // The active file shows in the explorer: its folders open.
        if let Some(rel) = self.buffers.get(ix).and_then(|b| b.path.as_ref()).zip(self.root.as_ref()).and_then(|(p, root)| p.strip_prefix(root).ok()).map(Path::to_path_buf) {
            self.explorer.open_ancestors(&rel);
        }
        match self.buffers.get(ix).map(|b| &b.view) {
            Some(View::Text(editor)) => editor.update(cx, |e, cx| e.focus(window, cx)),
            _ => self.focus.focus(window),
        }
        cx.notify();
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        self.save_buffer(self.active, window, cx);
    }

    /// Returns false if not saved (error, or a Save As dialog was opened instead).
    fn save_buffer(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(b) = self.buffers.get_mut(ix) else { return false };
        let Some(editor) = b.editor().cloned() else { return true }; // images are read-only
        let Some(path) = b.path.clone() else {
            self.save_as(ix, window, cx);
            return false;
        };
        if b.disk_mtime.is_some() && modified(&path) != b.disk_mtime && !confirm(&mut self.discard, Discard::Overwrite(b.id())) {
            window.push_notification("Le fichier a changé sur le disque depuis son ouverture : ⌘S à nouveau pour l'écraser.", cx);
            return false;
        }
        let text = editor.read(cx).value().to_string();
        match write_file(&path, &text) {
            Ok(()) => {
                b.dirty = false;
                b.disk_mtime = modified(&path);
                let lang = b.lang;
                self.lint(path, lang, text, true, cx);
                cx.notify();
                true
            }
            Err(e) => {
                window.push_notification(format!("Échec de l'enregistrement : {e}"), cx);
                false
            }
        }
    }

    fn save_as(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers.get(ix) else { return };
        let Some(editor) = b.editor().cloned() else { return };
        let dir = self.root.clone().unwrap_or_else(home);
        let rx = cx.prompt_for_new_path(&dir, Some(&format!("{}.js", b.title)));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(ix) = this.buffers.iter().position(|b| b.editor() == Some(&editor)) else { return };
                this.set_buffer_path(ix, path, cx);
                if this.save_buffer(ix, window, cx) {
                    this.rescan(window, cx);
                }
                this.save_session(); // the new file's tab comes back next time
            });
        })
        .detach();
    }

    /// A tab's file has a new path (Save As, renamed or moved): its name, language and plugins follow.
    fn set_buffer_path(&mut self, ix: usize, path: PathBuf, cx: &mut Context<Self>) {
        let lang = languages::for_path(&path);
        let completion = self.completion(lang, Some(&path), cx);
        let b = &mut self.buffers[ix];
        b.title = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        b.disk_mtime = modified(&path).or(b.disk_mtime);
        b.path = Some(path);
        b.lang = lang;
        if let Some(editor) = b.editor().cloned() {
            editor.update(cx, |e, cx| {
                e.set_highlighter(lang.highlight, cx);
                e.lsp.hover_provider = Some(completion.clone());
                e.lsp.definition_provider = Some(completion.clone());
                e.lsp.completion_provider = Some(completion);
            });
        }
    }

    /// Tabs of a renamed or moved file or folder follow it.
    fn retarget(&mut self, from: &Path, to: &Path, cx: &mut Context<Self>) {
        self.forget_files(from);
        for ix in 0..self.buffers.len() {
            let Some(rest) = self.buffers[ix].path.as_ref().and_then(|p| p.strip_prefix(from).ok()).map(Path::to_path_buf) else { continue };
            if let Some(old) = &self.buffers[ix].path {
                self.servers.close(old);
            }
            let path = if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) };
            self.set_buffer_path(ix, path, cx);
        }
        self.save_session();
    }

    /// A file or folder gone from `path` (trashed, renamed, moved): its problems
    /// leave the panel (the new place gets its own on the next check).
    fn forget_files(&self, path: &Path) {
        for (file, _) in self.servers.problems.all() {
            if file.starts_with(path) {
                self.servers.problems.forget(&file);
            }
        }
    }

    /// A small dialog asking for a text (a name); `then` gets it once confirmed.
    fn ask_text(&mut self, title: String, initial: String, ok: &'static str, then: Rc<dyn Fn(&mut Self, String, &mut Window, &mut Context<Self>)>, window: &mut Window, cx: &mut Context<Self>) {
        // A file name: its name selected, not its extension (like Finder). ".env": all of it.
        let selected = initial.rfind('.').filter(|&dot| dot > 0).unwrap_or(initial.len());
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        // Once the dialog is up (it takes the focus when it opens): the text, selected, ready to retype.
        let field = input.clone();
        window.defer(cx, move |window, cx| {
            field.update(cx, |i, cx| {
                i.focus(window, cx);
                i.select_bytes(0..selected, cx);
            })
        });
        let ws = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, ws, then) = (input.clone(), ws.clone(), then.clone());
            dialog
                .title(title.clone())
                .child(Input::new(&input))
                .confirm()
                .button_props(DialogButtonProps::default().ok_text(ok).cancel_text("Annuler"))
                .on_ok(move |_, window, cx| {
                    let text = input.read(cx).value().to_string();
                    let then = then.clone();
                    let _ = ws.update(cx, |ws, cx| then(ws, text, window, cx));
                    true
                })
        });
    }

    /// The explorer reads the disk again now, showing `path` (its folders opened).
    fn show_in_explorer(&mut self, path: &Path, cx: &mut Context<Self>) {
        if let Some(rel) = self.root.as_ref().and_then(|r| path.strip_prefix(r).ok()) {
            self.explorer.reveal(rel);
        }
        cx.notify();
    }

    fn new_entry(&mut self, dir: PathBuf, folder: bool, window: &mut Window, cx: &mut Context<Self>) {
        let then = Rc::new(move |ws: &mut Self, name: String, window: &mut Window, cx: &mut Context<Self>| {
            let made = files::target(&dir, &name).and_then(|p| if folder { fs::create_dir(&p) } else { fs::write(&p, "") }.map(|_| p).map_err(|e| e.to_string()));
            match made {
                Ok(p) => {
                    ws.show_in_explorer(&p, cx);
                    if !folder {
                        ws.open_path(p, window, cx);
                    }
                }
                Err(e) => window.push_notification(Notification::error(e), cx),
            }
        });
        self.ask_text((if folder { "Nouveau dossier" } else { "Nouveau fichier" }).into(), String::new(), "Créer", then, window, cx);
    }

    fn rename_entry(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let (title, initial) = (format!("Renommer « {name} »"), name.clone());
        let then = Rc::new(move |ws: &mut Self, new: String, window: &mut Window, cx: &mut Context<Self>| {
            if new.trim() == name {
                return;
            }
            match files::rename(&path, &new) {
                Ok(to) => {
                    ws.retarget(&path, &to, cx);
                    ws.show_in_explorer(&to, cx);
                }
                Err(e) => window.push_notification(Notification::error(e), cx),
            }
        });
        self.ask_text(title, initial, "Renommer", then, window, cx);
    }

    fn duplicate_entry(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // Off the UI thread: a folder with node_modules is tens of thousands of files.
        let copy = cx.background_executor().spawn(async move { files::duplicate(&path) });
        cx.spawn_in(window, async move |this, cx| {
            let done = copy.await;
            let _ = this.update_in(cx, |this, window, cx| match done {
                Ok(copy) => this.show_in_explorer(&copy, cx),
                Err(e) => window.push_notification(Notification::error(format!("Duplication impossible : {e}")), cx),
            });
        })
        .detach();
    }

    fn trash_entry(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let answer = window.prompt(PromptLevel::Warning, &format!("Mettre « {name} » à la corbeille ?"), Some("Vous pourrez le récupérer depuis la Corbeille du Finder."), &["Mettre à la corbeille", "Annuler"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| match files::trash(&path) {
                Ok(()) => {
                    this.forget_files(&path);
                    // Its tabs close, except unsaved ones (⌘S would bring the file back).
                    while let Some(ix) = this.buffers.iter().position(|b| !b.dirty && b.path.as_ref().is_some_and(|p| p.starts_with(&path))) {
                        this.close_buffer(ix, window, cx);
                    }
                    this.show_in_explorer(&path, cx);
                }
                Err(e) => window.push_notification(Notification::error(format!("Impossible de mettre « {name} » à la corbeille : {e}")), cx),
            });
        })
        .detach();
    }

    fn move_entry(&mut self, from: PathBuf, to_dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match files::move_into(&from, &to_dir) {
            Ok(to) if to != from => {
                self.retarget(&from, &to, cx);
                self.show_in_explorer(&to, cx);
            }
            Ok(_) => {}
            Err(e) => window.push_notification(Notification::error(e), cx),
        }
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_buffer(self.active, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(true, window, cx);
    }

    fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(false, window, cx);
    }

    /// The next (or previous) tab; from the welcome page, the first (or last) one.
    fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let tabs = self.tab_indices();
        let n = tabs.len();
        if n == 0 {
            return;
        }
        let next = match tabs.iter().position(|&ix| ix == self.active) {
            Some(pos) if forward => tabs[(pos + 1) % n],
            Some(pos) => tabs[(pos + n - 1) % n],
            None if forward => tabs[0],
            None => tabs[n - 1],
        };
        self.activate(next, window, cx);
    }

    fn select_tab(&mut self, action: &SelectTab, window: &mut Window, cx: &mut Context<Self>) {
        let tabs = self.tab_indices();
        let pos = if action.0 == 9 { tabs.len().saturating_sub(1) } else { action.0.saturating_sub(1) };
        if let Some(&ix) = tabs.get(pos) {
            self.activate(ix, window, cx);
        }
    }

    fn reopen_tab(&mut self, _: &ReopenTab, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(path) = self.closed.pop() {
            if path.exists() {
                return self.open_path(path, window, cx);
            }
        }
    }

    /// ⌥⇧F or the "Formater" button: the language's formatter from the store,
    /// else the built-in JSON one. Undoable (⌘Z).
    fn format(&mut self, _: &Format, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers.get(self.active) else { return };
        let Some(editor) = b.editor().cloned() else { return };
        let src = editor.read(cx).value();
        let replace = move |editor: &Entity<InputState>, src: &str, text: String, window: &mut Window, cx: &mut App| {
            // Through the input handler, like typing: ⌘Z undoes it.
            editor.update(cx, |e, cx| e.replace_text_in_range(Some(0..src.encode_utf16().count()), &text, window, cx));
        };
        let Some((formatter, program)) = format::for_language(b.lang.name, AppSettings::get(cx)) else {
            return match (b.lang.highlight, pretty_json(&src)) {
                ("json", Ok(pretty)) if pretty != src.as_str() => replace(&editor, &src, pretty, window, cx),
                ("json", Ok(_)) => {}
                ("json", Err(at)) => window.push_notification(Notification::error(format!("JSON invalide ({at}) : corrigez-le pour pouvoir le formater.")), cx),
                _ => window.push_notification(format!("Aucun formateur pour {} : voyez le Store (Formatage).", b.lang.name), cx),
            };
        };
        let file = b.path.clone().unwrap_or_else(|| PathBuf::from(format!("sans-titre.{}", b.lang.extensions.first().unwrap_or(&"txt"))));
        let root = self.project_root(b.path.as_deref());
        // Prettier runs the project's config files (prettier.config.js) and plugins.
        if formatter.program == "prettier" && !trust::is_trusted(&root) && !self.format_once {
            let what = "Prettier exécute la configuration du projet (prettier.config.js, plugins).".to_string();
            return self.ask_trust(
                what,
                |this, window, cx| {
                    // A file outside any folder: this once (nothing to remember).
                    this.format_once = true;
                    this.format(&Format, window, cx);
                    this.format_once = false;
                },
                window,
                cx,
            );
        }
        let text = src.to_string();
        let job = cx.background_executor().spawn(async move { format::run(formatter, &program, &file, &root, text) });
        self.set_busy(Some("Formatage…"), cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| this.set_busy(None, cx));
            let _ = cx.update(|window, cx| match result {
                // Typed meanwhile: the formatted text is already stale.
                Ok(_) if editor.read(cx).value() != src => window.push_notification("Le fichier a changé pendant le formatage : relancez ⌥⇧F.", cx),
                Ok(text) if text != src.as_str() => replace(&editor, &src, text, window, cx),
                Ok(_) => {}
                Err(e) => window.push_notification(Notification::error(format!("{} : {e}", formatter.name)), cx),
            });
        })
        .detach();
    }

    fn zoom(&mut self, step: f64, cx: &mut Context<Self>) {
        let size = if step == 0. { AppSettings::default().code_font_size } else { AppSettings::get(cx).code_font_size + step };
        AppSettings::update(cx, |s| s.code_font_size = size.clamp(10., 24.));
    }

    fn close_buffer(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers.get(ix) else { return };
        // Closing Claude's tab stops it (and what it runs): asked like an unsaved file.
        let working = matches!(&b.view, View::Tool { id: "claude", view } if view.clone().downcast::<crate::tools::claude::ClaudeView>().is_ok_and(|c| c.read(cx).is_busy(cx)));
        if (b.dirty || working) && !confirm(&mut self.discard, Discard::Tab(b.id())) {
            let armed = self.discard;
            self.activate(ix, window, cx); // shows the tab, but must stay armed
            self.discard = armed;
            let why = if working { "Claude travaille : fermer l'onglet l'arrête. Fermez à nouveau pour l'arrêter." } else { "Fichier non enregistré : ⌘S pour enregistrer, fermez à nouveau pour abandonner." };
            return window.push_notification(why, cx);
        }
        // Its place in the tab bar; a shrinking tab is a child
        // too: the ones after it are one further.
        let slot = self.tab_indices().iter().position(|&i| i == ix);
        let child = slot.map(|slot| if self.closing.as_ref().is_some_and(|c| slot >= c.at) { slot + 1 } else { slot });
        // No animations: no shrinking ghost, the tabs move at once.
        let width = child.and_then(|child| self.tab_widths.borrow().get(child).copied()).filter(|_| AppSettings::get(cx).animations);
        let closed = self.buffers.remove(ix);
        if let Some(editor) = closed.editor() {
            self._lsp_sync.remove(&editor.entity_id());
        }
        // The servers drop their copy: a later rename then reads the file from the disk.
        if let Some(path) = closed.path.as_ref().filter(|p| !self.buffers.iter().any(|b| b.path.as_ref() == Some(*p))) {
            self.servers.close(path);
            // Changes dropped: the linters' problems were about them, the disk says otherwise.
            if closed.dirty
                && let Ok(text) = fs::read_to_string(path)
            {
                self.lint(path.clone(), closed.lang, text, false, cx);
            }
        }
        self.split = match self.split {
            Some(s) if s == ix => None,
            Some(s) if s > ix => Some(s - 1),
            s => s,
        };
        if let Some(width) = width {
            self.closes += 1;
            self.closing = Some(ClosingTab { at: slot.unwrap_or(0), width, title: closed.title.clone(), id: self.closes });
            let id = self.closes;
            self._closing = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(TAB_CLOSE).await;
                let _ = this.update(cx, |this, cx| {
                    if this.closing.as_ref().is_some_and(|c| c.id == id) {
                        this.closing = None;
                        cx.notify();
                    }
                });
            }));
        }
        if let Some(path) = closed.path {
            self.closed.push(path);
        }
        // Another tab closed: the shown one stays (one place earlier if after it).
        if self.active != ix {
            let active = if self.active > ix { self.active - 1 } else { self.active };
            self.active = active;
            if active < self.buffers.len() {
                self.activate(active, window, cx);
            }
            return cx.notify();
        }
        // The shown tab closed: its neighbour, or the welcome page.
        let tabs = self.tab_indices();
        match tabs.iter().find(|&&i| i >= ix).or(tabs.last()) {
            Some(&next) => self.activate(next, window, cx),
            None => {
                self.active = self.buffers.len();
                self.focus.focus(window);
                cx.notify();
            }
        }
    }

    /// The shown terminal, a first one opened if needed.
    fn ensure_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<&mut Terminal> {
        if self.terminals.is_empty() {
            self.new_terminal(window, cx);
        }
        self.show_terminal = !self.terminals.is_empty();
        cx.notify();
        self.terminals.get_mut(self.active_terminal)
    }

    /// Opens another terminal (a tab of the panel) and shows it. False if the shell failed.
    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let ws = cx.entity().downgrade();
        // `exit` in a shell closes its tab.
        let on_exit = move |_: &mut Window, cx: &mut Context<TerminalView>| {
            let id = cx.entity_id();
            let _ = ws.update(cx, |ws, cx| {
                if let Some(ix) = ws.terminals.iter().position(|t| t.view.entity_id() == id) {
                    ws.remove_terminal(ix, cx);
                }
            });
        };
        let cwd = self.root.clone().unwrap_or_else(home);
        match Terminal::spawn(&cwd, terminal_config(cx), on_exit, cx) {
            Ok(mut t) => {
                self.terminals_made += 1;
                // "/bin/zsh" → "zsh", "powershell.exe" → "powershell".
                let shell = crate::platform::default_shell();
                let shell = shell.rsplit(['/', '\\']).next().unwrap_or("sh").trim_end_matches(".exe").to_string();
                t.title = format!("{shell} {}", self.terminals_made);
                self.terminals.push(t);
                self.active_terminal = self.terminals.len() - 1;
                self.show_terminal = true;
                self.focus_terminal(window, cx);
                true
            }
            Err(e) => {
                window.push_notification(format!("Impossible de lancer le terminal : {e}"), cx);
                false
            }
        }
    }

    fn focus_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.terminals.get(self.active_terminal) {
            t.view.read(cx).focus_handle().clone().focus(window);
        }
        cx.notify();
    }

    /// Closes a terminal (its shell is stopped); the panel hides with the last one.
    fn remove_terminal(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.terminals.len() {
            return;
        }
        self.terminals.remove(ix);
        if self.active_terminal >= ix && self.active_terminal > 0 {
            self.active_terminal -= 1;
        }
        self.show_terminal &= !self.terminals.is_empty();
        cx.notify();
    }

    fn toggle_terminal(&mut self, _: &ToggleTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_terminal {
            self.show_terminal = false;
            return self.activate(self.active, window, cx);
        }
        if self.ensure_terminal(window, cx).is_some() {
            self.focus_terminal(window, cx);
        }
    }

    fn kill_terminal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_terminal(ix, cx);
        match self.show_terminal {
            true => self.focus_terminal(window, cx),
            false => self.activate(self.active, window, cx),
        }
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar = !self.show_sidebar;
        cx.notify();
    }

    /// Command that ⌘R runs for this buffer (`{file}` not yet substituted).
    fn run_command(&self, b: &Buffer) -> Option<String> {
        let ext = b.path.as_ref()?.extension()?.to_str()?.to_lowercase();
        self.config.run.get(&ext).cloned().or(b.lang.run.map(String::from))
    }

    fn run(&mut self, _: &Run, window: &mut Window, cx: &mut Context<Self>) {
        let Some(b) = self.buffers.get(self.active) else {
            return window.push_notification("Ouvrez un fichier à exécuter.", cx);
        };
        let Some(path) = b.path.clone() else {
            self.save_as(self.active, window, cx);
            return window.push_notification("Enregistrez le fichier pour pouvoir l'exécuter.", cx);
        };
        let Some(cmd) = self.run_command(b) else {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
            return window.push_notification(
                format!("Aucun environnement d'exécution pour .{ext} — ajoutez-le dans akolade.json : {{\"run\": {{\"{ext}\": \"commande {{file}}\"}}}}"),
                cx,
            );
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
        let from_project = self.config.run.contains_key(&ext) || !self.config.env.is_empty();
        let cmd = expand_run(&cmd, &path, &self.config.env);
        if from_project && !self.trusted() {
            return self.ask_trust(format!("Ce dossier définit ses propres commandes dans akolade.json :\n\n{cmd}"), |this, window, cx| this.run(&Run, window, cx), window, cx);
        }
        if !self.save_buffer(self.active, window, cx) {
            return;
        }
        self.run_in_terminal(&cmd, window, cx);
    }

    fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_open(false, window, cx);
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_open(true, window, cx);
    }

    /// ⌥⌘W: back to the welcome page, the project's tabs closed (and remembered
    /// for when it is opened again); the next launch starts without it.
    fn close_folder(&mut self, _: &CloseFolder, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        let mine = |b: &Buffer| b.path.as_ref().is_some_and(|p| p.starts_with(&root));
        if self.buffers.iter().any(|b| mine(b) && b.dirty) && !confirm(&mut self.discard, Discard::Folder) {
            return window.push_notification("Fichiers non enregistrés : ⌘S pour les garder, fermez à nouveau le dossier pour abandonner.", cx);
        }
        self.save_session();
        for b in self.buffers.iter().filter(|b| mine(b)) {
            if let Some(editor) = b.editor() {
                self._lsp_sync.remove(&editor.entity_id());
            }
        }
        self.buffers.retain(|b| !mine(b));
        (self.split, self.closing, self.last_file) = (None, None, None);
        self.active = self.tab_indices().last().copied().unwrap_or(self.buffers.len());
        AppSettings::update(cx, |s| s.last_folder = None);
        self.set_root(None, window, cx);
        self.refresh_git(cx).detach();
        self.focus.focus(window);
    }

    fn prompt_open(&mut self, folder: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: !folder, directories: folder, multiple: !folder, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                for path in paths {
                    if folder {
                        this.set_root(Some(path), window, cx);
                    } else {
                        this.open_path(path, window, cx);
                    }
                }
            });
        })
        .detach();
    }

    fn click_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(root), Some(e)) = (&self.root, self.explorer.entries.get(ix)) else { return };
        if e.is_dir {
            self.explorer.toggle(ix);
            cx.notify();
        } else {
            self.open_path(root.join(&e.rel), window, cx);
        }
    }

    // ---------- rendering ----------

    /// Where the active tab lives: project-relative path, else the last folders.
    fn crumbs(&self) -> Vec<String> {
        let Some(b) = self.buffers.get(self.active) else { return vec![] };
        match (&b.path, &self.root) {
            (Some(p), Some(r)) if p.starts_with(r) => p.strip_prefix(r).unwrap().iter().map(|s| s.to_string_lossy().into_owned()).collect(),
            (Some(p), _) => {
                let parts: Vec<String> = p.iter().skip(1).map(|s| s.to_string_lossy().into_owned()).collect();
                let skip = parts.len().saturating_sub(3);
                (skip > 0).then(|| "…".to_string()).into_iter().chain(parts.into_iter().skip(skip)).collect()
            }
            (None, _) => vec![b.title.clone()],
        }
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let project: SharedString = self
            .root
            .as_ref()
            .map_or("Ouvrir un dossier".into(), |r| r.file_name().unwrap_or_default().to_string_lossy().to_string())
            // A very long folder name would push "Exécuter" out of a narrow window.
            .map(|n: String| if n.chars().count() > 32 { format!("{}…", n.chars().take(31).collect::<String>()) } else { n })
            .into();
        let can_run = self.buffers.get(self.active).is_some_and(|b| b.editor().is_some() && (b.path.is_none() || self.run_command(b).is_some()));
        let crumbs = self.crumbs();
        let last = crumbs.len().saturating_sub(1);

        // Past the traffic lights (moved to x = 16 to center them in the taller bar). Elsewhere
        // the window buttons are on the right, drawn by TitleBar itself.
        TitleBar::new().h(r(46.)).pl(px(if cfg!(target_os = "macos") { 92. } else { 12. })).child(
            h_flex()
                .w_full()
                .h_full()
                .pr(r(10.))
                .gap(r(6.))
                // Left: logo + project switcher
                .child(icons::asset(icons::LOGO).size(r(18.)).flex_shrink_0())
                .child(
                    button("project")
                        .ghost()
                        .small()
                        .label(project)
                        // A folder, not a chevron: this opens the folder picker, not a menu.
                        .icon(Icon::new(IconName::FolderOpen).small().text_color(theme.muted_foreground))
                        .tooltip("Ouvrir un autre dossier (⇧⌘O)")
                        .on_click(cx.listener(|this, _, window, cx| this.prompt_open(true, window, cx))),
                )
                .when(self.root.is_some(), |d| {
                    d.child(
                        button("close-folder")
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Close).xsmall().text_color(theme.muted_foreground))
                            .tooltip("Fermer le dossier (⌥⌘W)")
                            .on_click(cx.listener(|this, _, window, cx| this.close_folder(&CloseFolder, window, cx))),
                    )
                })
                // Center: where you are
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .justify_center()
                        .gap(r(6.))
                        .text_size(r(12.5))
                        .text_color(theme.muted_foreground)
                        .overflow_hidden()
                        .children(crumbs.into_iter().enumerate().flat_map(|(i, c)| {
                            let seg = div().whitespace_nowrap().when(i == last, |d| d.text_color(theme.foreground).font_weight(FontWeight::MEDIUM)).child(c);
                            (i > 0).then(|| div().opacity(0.5).child("/")).into_iter().chain([seg])
                        })),
                )
                // Right: run + panels
                .child(
                    button("run")
                        // Nothing to run: a quiet ghost, not a muddy disabled primary.
                        .map(|b| if can_run { b.primary() } else { b.ghost() })
                        .small()
                        .rounded(ButtonRounded::Large)
                        .px(r(14.))
                        .disabled(!can_run)
                        .icon(Icon::empty().path("icons/play.svg"))
                        .label("Exécuter")
                        .tooltip("Exécuter le fichier (⌘R)")
                        .on_click(cx.listener(|this, _, window, cx| this.run(&Run, window, cx))),
                )
        )
    }

    /// The column of big icons on the left: panels and tools, the active one marked.
    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted, primary, on_primary) = (theme.foreground, theme.muted_foreground, theme.primary, theme.primary_foreground);
        let settings = AppSettings::get(cx);
        let view = self.buffers.get(self.active).map(|b| &b.view);
        let chrome = theme.sidebar; // the window chrome behind the bar
        // From the last layout: scrolled less than its max (offsets go negative downwards).
        let more_below = self.act_scroll.max_offset().height - (-self.act_scroll.offset().y) > px(2.);
        let item = |id: &'static str, icon: Icon, tip: &'static str, on: bool, badge: Option<usize>| {
            div()
                .id(id)
                .relative()
                .size(r(40.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(r(10.))
                .cursor_pointer()
                .text_color(if on { fg } else { muted })
                .when(on, |d| d.bg(fg.opacity(0.08)))
                .when(!on, |d| d.hover(|d| d.text_color(fg).bg(fg.opacity(0.06))))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(icon.size(r(20.)))
                .when(on, |d| d.child(div().absolute().left(r(-8.)).top(r(10.)).h(r(20.)).w(r(3.)).rounded_full().bg(primary)))
                .when_some(badge.filter(|n| *n > 0), |d, n| {
                    d.child(
                        div()
                            .absolute()
                            .top(r(2.))
                            .right(r(1.))
                            .h(r(16.))
                            .min_w(r(16.))
                            .px(r(4.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(primary)
                            .text_color(on_primary)
                            .text_size(r(10.))
                            .font_weight(FontWeight::BOLD)
                            .child(if n > 99 { "99+".to_string() } else { n.to_string() }),
                    )
                })
        };
        v_flex()
            .w(r(ACTIVITY_BAR))
            .h_full()
            .flex_shrink_0()
            .items_center()
            .pt(r(4.))
            .pb(r(8.))
            .gap(r(6.))
            .child(
                item("act-explorer", Icon::empty().path("icons/files.svg"), "Explorateur (⌘B)", self.show_sidebar && self.panel == Panel::Explorer, None)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_panel(Panel::Explorer, window, cx))),
            )
            .child(
                item("act-search", Icon::new(IconName::Search), "Rechercher dans le projet (⌘⇧F)", self.show_sidebar && self.panel == Panel::Search, None)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_panel(Panel::Search, window, cx))),
            )
            .child(
                item("act-todo", Icon::empty().path("icons/list-todo.svg"), "TODO : les notes laissées dans le code", self.show_sidebar && self.panel == Panel::Todo, None)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_panel(Panel::Todo, window, cx))),
            )
            .child(
                item("act-scripts", Icon::empty().path("icons/list-play.svg"), "Scripts du projet (npm, Make, Cargo, Go)", self.show_sidebar && self.panel == Panel::Scripts, None)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_panel(Panel::Scripts, window, cx))),
            )
            .child(
                item("act-problems", Icon::new(IconName::TriangleAlert), "Problèmes : erreurs et avertissements", self.show_sidebar && self.panel == Panel::Problems, Some(self.servers.problems.counts().0))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_panel(Panel::Problems, window, cx))),
            )
            // The tools added from the store, then the store itself.
            .child(div().my(r(2.)).w(r(22.)).h(px(1.)).bg(fg.opacity(0.1)))
            // Many tools added: this part scrolls, the panels and the bottom icons stay.
            .child(
                div().relative().flex_1().min_h_0().w_full().child(
                v_flex()
                    .id("act-tools")
                    .size_full()
                    .items_center()
                    .overflow_y_scroll()
                    .track_scroll(&self.act_scroll)
                    .gap(r(6.))
                    .pl(r(8.))
                    .pr(r(8.))
                    .children(
                        tools::TOOLS
                            .iter()
                            .filter(|t| settings.tool_added(t.id))
                            .map(|t| {
                                let id = t.id;
                                let on = matches!(view, Some(View::Tool { id: i, .. }) if *i == id);
                                let badge = if id == "git" { self.git.as_ref().map(|g| g.count) } else { None };
                                let tip = if id == "git" { "Git (⌘⇧G)" } else { t.name };
                                item(id, (t.icon)(), tip, on, badge).flex_shrink_0().on_click(cx.listener(move |this, _, window, cx| this.toggle_tool(id, window, cx)))
                            })
                            .collect::<Vec<_>>(),
                    ),
                )
                // More tools below: the list fades out at the bottom.
                .when(more_below, |d| d.child(div().absolute().bottom_0().left_0().right_0().h(r(28.)).bg(linear_gradient(180., linear_color_stop(chrome.opacity(0.), 0.), linear_color_stop(chrome, 1.))))),
            )
            // Always in reach, however many tools are added.
            .child(
                item("act-store", Icon::empty().path("icons/puzzle.svg"), "Store : plugins, outils et thèmes", matches!(view, Some(View::Plugins(_))), None)
                    .on_click(cx.listener(|this, _, window, cx| this.open_plugins(&OpenPlugins, window, cx))),
            )
            .child(
                item("act-terminal", Icon::new(IconName::SquareTerminal), "Terminal (⌃`)", self.show_terminal, None)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(&ToggleTerminal, window, cx))),
            )
            .child(
                item("act-settings", Icon::new(IconName::Settings), "Paramètres (⌘,)", matches!(view, Some(View::Settings(_))), None)
                    .on_click(cx.listener(|this, _, window, cx| this.open_settings(&OpenSettings, window, cx))),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let title = match self.panel {
            Panel::Explorer => "EXPLORATEUR",
            Panel::Search => "RECHERCHE",
            Panel::Todo => "TODO",
            Panel::Scripts => "SCRIPTS",
            Panel::Problems => "PROBLÈMES",
        };
        let refresh = |id: &'static str, tip: &'static str| button(id).ghost().xsmall().icon(Icon::empty().path("icons/refresh.svg")).tooltip(tip);
        let header = h_flex()
            .h(r(44.))
            .flex_shrink_0()
            .pl(r(16.))
            .pr(r(10.))
            .gap(r(4.))
            .text_size(r(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.muted_foreground)
            .child(title)
            .child(div().flex_1())
            .map(|this| match self.panel {
                Panel::Explorer => this
                    .child(button("new-file").ghost().xsmall().icon(IconName::Plus).tooltip("Nouveau fichier").on_click(cx.listener(|this, _, window, cx| match this.root.clone() {
                        Some(root) => this.new_entry(root, false, window, cx),
                        None => this.new_file(&NewFile, window, cx),
                    })))
                    .when(self.root.is_some(), |this| {
                        this.child(button("new-folder").ghost().xsmall().icon(Icon::empty().path("icons/folder-plus.svg")).tooltip("Nouveau dossier").on_click(cx.listener(|this, _, window, cx| {
                            if let Some(root) = this.root.clone() {
                                this.new_entry(root, true, window, cx);
                            }
                        })))
                    })
                    .when(self.root.is_some(), |this| this.child(refresh("rescan", "Actualiser").on_click(cx.listener(|this, _, window, cx| this.rescan(window, cx))))),
                Panel::Search => this,
                Panel::Todo => this.child(refresh("todo-again", "Actualiser").on_click(cx.listener(|this, _, window, cx| this.show_panel(Panel::Todo, window, cx)))),
                Panel::Scripts => this.child(refresh("scripts-again", "Actualiser").on_click(cx.listener(|this, _, window, cx| this.show_panel(Panel::Scripts, window, cx)))),
                Panel::Problems => this,
            });

        let sidebar = v_flex().size_full().bg(theme.sidebar).text_color(theme.sidebar_foreground).child(header);
        match self.panel {
            Panel::Search => return sidebar.child(self.search.clone()),
            Panel::Todo => return sidebar.child(self.todo.clone()),
            Panel::Scripts => return sidebar.child(self.tasks.clone()),
            Panel::Problems => return sidebar.child(self.render_problems(cx)),
            Panel::Explorer => {}
        }
        sidebar
            .map(|this| match &self.root {
                None => this.child(
                    v_flex()
                        .px(r(14.))
                        .pt(r(4.))
                        .gap(r(12.))
                        .text_size(r(13.))
                        .text_color(theme.muted_foreground)
                        .child("Aucun dossier ouvert.")
                        .child(button("open-folder").outline().small().w_full().label("Ouvrir un dossier")
                            .on_click(cx.listener(|this, _, window, cx| this.prompt_open(true, window, cx)))),
                ),
                // Only the rows on screen are built (a folder of thousands of
                // files would otherwise be rebuilt on every keystroke).
                Some(_) if self.explorer.visible().is_empty() => {
                    this.child(div().px(r(14.)).pt(r(4.)).text_size(r(13.)).text_color(theme.muted_foreground).child("Dossier vide."))
                }
                Some(_) => this
                    .child(
                        uniform_list("files", self.explorer.visible().len(), cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)))
                            .flex_1()
                            .min_h_0()
                            .px(r(8.))
                            .pb(r(8.))
                            .text_size(r(13.)),
                    )
                    .when(self.explorer.truncated(), |this| {
                        this.child(div().px(r(16.)).py(r(8.)).text_size(r(11.5)).text_color(theme.muted_foreground).child(format!("Un dossier dépasse {MAX_ENTRIES} éléments : seuls les premiers sont listés.")))
                    }),
            })
    }

    /// Errors and warnings of the project, by file; a click opens the line.
    fn render_problems(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, hover) = (theme.muted_foreground, ui::hover_bg(cx));
        let all = self.servers.problems.all();
        let root = self.root.clone();
        let mut rows: Vec<AnyElement> = vec![];
        // shortcut: lists at most 1000 problems; switch to uniform_list if projects have more.
        for (path, list) in all.iter() {
            if rows.len() > 1000 {
                break;
            }
            let shown = root.as_ref().and_then(|r| path.strip_prefix(r).ok()).unwrap_or(path);
            let name = shown.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let dir = shown.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            rows.push(
                h_flex()
                    .h(r(28.))
                    .flex_shrink_0()
                    .px(r(8.))
                    .gap(r(6.))
                    .child(div().flex_shrink_0().child(file_icon(path, cx)))
                    // A long name truncates too, after its folder: the count stays visible.
                    .child(div().min_w_0().truncate().text_size(r(13.)).font_weight(FontWeight::MEDIUM).child(name))
                    .child(div().flex_1().min_w_0().truncate().text_size(r(11.5)).text_color(muted).child(dir))
                    .child(div().flex_shrink_0().px(r(6.)).rounded_full().bg(theme.foreground.opacity(0.08)).text_size(r(11.)).text_color(muted).child(list.len().to_string()))
                    .into_any_element(),
            );
            for (i, p) in list.iter().enumerate() {
                let (icon, color) = match p.severity {
                    Severity::Error => (IconName::CircleX, theme.red),
                    Severity::Warning => (IconName::TriangleAlert, theme.yellow),
                    Severity::Info => (IconName::Info, theme.blue),
                };
                let (path, line, col) = (path.clone(), p.start.0, p.start.1);
                rows.push(
                    h_flex()
                        .id(SharedString::from(format!("problem-{}-{i}", path.display())))
                        .flex_shrink_0()
                        .py(r(4.))
                        .pl(r(22.))
                        .pr(r(8.))
                        .gap(r(6.))
                        .items_start()
                        .rounded(r(5.))
                        .cursor_pointer()
                        .hover(|d| d.bg(hover))
                        .child(Icon::new(icon).size(r(14.)).flex_shrink_0().mt(r(2.)).text_color(color))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                // Whole words, 2 lines at most (gpui's line_clamp cuts the last line mid-word).
                                .child(div().max_h(r(12.5 * 1.5 * 2.)).overflow_hidden().text_size(r(12.5)).line_height(relative(1.5)).child(p.message.clone()))
                                .child(div().text_size(r(11.)).text_color(muted).child(format!("{} · ligne {}", p.source, line + 1))),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| this.open_at_lsp(path.clone(), line, col, window, cx)))
                        .into_any_element(),
                );
            }
        }
        let empty = all.is_empty().then(|| {
            div().px(r(8.)).pt(r(4.)).text_size(r(12.5)).text_color(muted).child(
                "Aucun problème. Les erreurs viennent des plugins de complétion (fichiers ouverts) et des linters du Store.",
            )
        });
        v_flex().id("problems").flex_1().min_h_0().overflow_y_scroll().px(r(6.)).pb(r(8.)).children(empty).children(rows)
    }

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = cx.theme();
        let (hover, guide) = (ui::hover_bg(cx), theme.foreground.opacity(0.06));
        let selected_bg = theme.primary.opacity(0.14);
        let active = self.buffers.get(self.active).and_then(|b| b.path.as_deref());
        let Some(root) = &self.root else { return vec![] };
        let visible = self.explorer.visible();
        visible[range.start.min(visible.len())..range.end.min(visible.len())]
            .iter()
            .map(|&ix| {
                let e = &self.explorer.entries[ix];
                let selected = !e.is_dir && active == Some(&root.join(&e.rel));
                let name = e.rel.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let open = self.explorer.is_open(&e.rel);
                // Git: a changed file shows its letter and color, its folders a dot.
                let letter = self.git.as_ref().and_then(|g| g.files.get(&e.rel)).copied();
                let changed_dir = e.is_dir && self.git.as_ref().is_some_and(|g| g.dirs.contains(&e.rel));
                let tint = letter.or(changed_dir.then_some('M')).map(|l| git_view::letter_color(l, cx));
                let path = root.join(&e.rel);
                // Dropped here or "New file" here: in this folder, or next to this file.
                let dir = if e.is_dir { path.clone() } else { path.parent().unwrap_or(root).to_path_buf() };
                let (dragged, drop_dir, menu) = (DraggedEntry { path: path.clone(), name: name.clone() }, dir.clone(), self.entry_menu(path.clone(), e.rel.clone(), dir, e.is_dir, cx));
                let target = path.clone();
                h_flex()
                    .id(ix)
                    .relative()
                    .w_full()
                    .h(r(30.))
                    .flex_shrink_0()
                    .pl(r(8. + e.depth as f32 * 14.))
                    .pr(r(8.))
                    .gap(r(6.))
                    .rounded(r(6.))
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .cursor_pointer()
                    // Indent guides: one thin line per parent folder.
                    .children((0..e.depth).map(|level| div().absolute().top_0().bottom_0().left(r(8. + level as f32 * 14. + 6.)).w(px(1.)).bg(guide)))
                    .when(selected, |d| d.bg(selected_bg).text_color(theme.foreground))
                    .when(!selected, |d| d.hover(|d| d.bg(hover)))
                    // The row a right-click menu acts on.
                    .when(self.menu_target.as_ref() == Some(&path), |d| d.border_1().border_color(theme.primary.opacity(0.6)))
                    .map(|d| {
                        if e.is_dir {
                            d.child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).size(r(14.)).flex_shrink_0().text_color(theme.muted_foreground))
                                .child(icons::asset(if open { icons::FOLDER_OPEN } else { icons::FOLDER }).size(r(16.)).flex_shrink_0())
                        } else {
                            d.child(div().w(r(14.)).flex_shrink_0()).child(file_icon(&e.rel, cx))
                        }
                    })
                    .child(div().ml(r(2.)).flex_1().min_w_0().truncate().when_some(tint, |d, c| d.text_color(c.opacity(0.95))).child(name))
                    .when_some(letter, |d, l| d.child(div().flex_shrink_0().text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(git_view::letter_color(l, cx)).child(l.to_string())))
                    .when(changed_dir && letter.is_none(), |d| d.child(div().size(r(6.)).mr(r(3.)).flex_shrink_0().rounded_full().bg(git_view::letter_color('M', cx))))
                    .on_click(cx.listener(move |this, _, window, cx| this.click_entry(ix, window, cx)))
                    .on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
                    .drag_over::<DraggedEntry>(move |s, _, _, _| s.bg(selected_bg))
                    .on_drop(cx.listener(move |this, d: &DraggedEntry, window, cx| this.move_entry(d.path.clone(), drop_dir.clone(), window, cx)))
                    .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, _, cx| {
                        this.menu_target = Some(target.clone());
                        cx.notify();
                    }))
                    .context_menu(menu)
                    .into_any_element()
            })
            .collect()
    }

    /// Right-click on a file or folder of the explorer.
    fn entry_menu(&self, path: PathBuf, rel: PathBuf, dir: PathBuf, is_dir: bool, cx: &Context<Self>) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        type Act = fn(&mut Workspace, PathBuf, &mut Window, &mut Context<Workspace>);
        let ws = cx.entity().downgrade();
        move |menu, _, _| {
            let on_click = |act: Act, arg: PathBuf| {
                let ws = ws.clone();
                // The menu sits above the window: the row's outline is cleared here.
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = ws.update(cx, |ws, cx| {
                        ws.menu_target = None;
                        act(ws, arg.clone(), window, cx)
                    });
                }
            };
            let item = |label: &'static str, act: Act, arg: PathBuf| PopupMenuItem::new(label).on_click(on_click(act, arg));
            // Destructive: in red.
            let danger = |label: &'static str, act: Act, arg: PathBuf| {
                PopupMenuItem::element(move |_, cx: &mut App| div().text_color(cx.theme().red).child(label)).on_click(on_click(act, arg))
            };
            menu.item(item("Nouveau fichier…", |ws, d, w, cx| ws.new_entry(d, false, w, cx), dir.clone()))
                .item(item("Nouveau dossier…", |ws, d, w, cx| ws.new_entry(d, true, w, cx), dir.clone()))
                .separator()
                .item(item("Renommer…", Workspace::rename_entry, path.clone()))
                .item(item("Dupliquer", Workspace::duplicate_entry, path.clone()))
                .item(danger("Mettre à la corbeille", Workspace::trash_entry, path.clone()))
                .separator()
                .item(item("Copier le chemin", |_, p, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(p.display().to_string())), path.clone()))
                .item(item("Copier le chemin relatif", |_, p, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(p.display().to_string())), rel.clone()))
                .item(item(crate::platform::REVEAL_LABEL, |_, p, _, _| crate::platform::reveal(&p), path.clone()))
                .item(item(if is_dir { "Ouvrir dans le terminal" } else { "Ouvrir son dossier dans le terminal" }, |ws, d, w, cx| ws.run_in_terminal(&format!("cd {}", shell_quote(&d.to_string_lossy())), w, cx), dir.clone()))
        }
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let card_bg = theme.muted;
        let card = |id: &'static str, icon: IconName, label: &'static str, keys: &'static str, f: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            h_flex()
                .id(id)
                .w(r(248.))
                .p(r(14.))
                .gap(r(12.))
                .rounded(r(12.))
                .bg(card_bg)
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|d| d.border_color(theme.primary.opacity(0.5)))
                .child(
                    div()
                        .size(r(34.))
                        .flex_shrink_0()
                        .rounded(r(9.))
                        .bg(theme.primary.opacity(0.14))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(icon).text_color(theme.primary)),
                )
                .child(
                    v_flex()
                        .gap(r(4.))
                        .child(div().text_size(r(13.5)).font_weight(FontWeight::MEDIUM).child(label))
                        .child(kbd(keys, cx)),
                )
                .on_click(cx.listener(move |this, _, window, cx| f(this, window, cx)))
        };
        let recent = AppSettings::get(cx).last_folder.clone().filter(|p| p.is_dir() && self.root.as_ref() != Some(p));
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .child(icons::asset(icons::LOGO).size(r(64.)).mb(r(18.)))
            .child(div().text_size(r(26.)).font_weight(FontWeight::SEMIBOLD).child("Akolade"))
            .child(div().mt(r(4.)).text_size(r(14.)).text_color(theme.muted_foreground).child("Un éditeur de code rapide et natif"))
            .child(
                v_flex()
                    .mt(r(32.))
                    .gap(r(12.))
                    .child(
                        h_flex()
                            .gap(r(12.))
                            .child(card("w-new", IconName::Plus, "Nouveau fichier", "⌘N", |t, w, cx| t.new_file(&NewFile, w, cx)))
                            .child(card("w-file", IconName::File, "Ouvrir un fichier", "⌘O", |t, w, cx| t.prompt_open(false, w, cx))),
                    )
                    .child(
                        h_flex()
                            .gap(r(12.))
                            .child(card("w-folder", IconName::FolderOpen, "Ouvrir un dossier", "⇧⌘O", |t, w, cx| t.prompt_open(true, w, cx)))
                            .child(card("w-term", IconName::SquareTerminal, "Terminal", "⌃`", |t, w, cx| t.toggle_terminal(&ToggleTerminal, w, cx))),
                    ),
            )
            .when_some(recent, |this, folder| {
                let name = folder.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let shown = folder.display().to_string().replacen(&home().display().to_string(), "~", 1);
                this.child(
                    v_flex()
                        .mt(r(28.))
                        .w(r(508.))
                        .gap(r(6.))
                        .child(div().text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.muted_foreground).child("RÉCENT"))
                        .child(
                            h_flex()
                                .id("w-recent")
                                .px(r(12.))
                                .py(r(10.))
                                .gap(r(10.))
                                .rounded(r(10.))
                                .cursor_pointer()
                                .hover(|d| d.bg(ui::hover_bg(cx)))
                                .child(icons::asset(icons::FOLDER).size(r(18.)).flex_shrink_0())
                                .child(div().flex_shrink_0().text_size(r(13.5)).font_weight(FontWeight::MEDIUM).child(name))
                                .child(div().flex_1().min_w_0().truncate().text_size(r(12.5)).text_color(theme.muted_foreground).child(shown))
                                .on_click(cx.listener(move |this, _, window, cx| this.set_root(Some(folder.clone()), window, cx))),
                        ),
                )
            })
    }

    fn render_tabs(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let animations = AppSettings::get(cx).animations;
        let theme = cx.theme();
        let widths = self.tab_widths.clone();
        let mut tabs: Vec<AnyElement> = self.buffers.iter().enumerate().map(|(ix, b)| {
                let active = ix == self.active;
                let group: SharedString = format!("tab-{ix}").into();
                h_flex()
                    .id(("tab", ix))
                    .group(group.clone())
                    .flex_shrink_0()
                    .h(r(34.))
                    .pl(r(12.))
                    .pr(r(5.))
                    .gap(r(8.))
                    .rounded(r(9.))
                    .text_size(r(13.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .text_color(if active { theme.foreground } else { theme.muted_foreground })
                    .when(active, |d| d.bg(theme.foreground.opacity(0.06)).border_color(theme.foreground.opacity(0.06)))
                    .when(!active, |d| d.hover(|d| d.bg(ui::hover_bg(cx)).text_color(theme.foreground)))
                    // Two "mod.rs": the full path tells them apart.
                    .when_some(b.path.clone(), |d, p| d.tooltip(move |window, cx| Tooltip::new(p.display().to_string()).build(window, cx)))
                    .map(|d| match &b.view {
                        View::Settings(_) => d.child(Icon::new(IconName::Settings).small().text_color(theme.muted_foreground)),
                        View::Plugins(_) => d.child(Icon::empty().path("icons/puzzle.svg").small().text_color(theme.muted_foreground)),
                        View::Tool { id: "git", .. } => d.child(icons::asset("icons/files/git.svg").size(r(16.))),
                        View::Tool { id, .. } => d.children(tools::find(id).map(|t| (t.icon)().small().text_color((t.accent)(theme)))),
                        _ => d.child(file_icon(b.path.as_deref().unwrap_or(Path::new(&b.title)), cx)),
                    })
                    .child(div().max_w(r(200.)).truncate().child(b.title.clone()))
                    .child(
                        // Close button: always on the active clean tab, on hover elsewhere.
                        // A dirty tab shows a dot instead, which turns into the button on hover.
                        div()
                            .size(r(26.))
                            .relative()
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(b.dirty, |d| {
                                d.child(
                                    div()
                                        .absolute()
                                        .size(r(7.))
                                        .rounded_full()
                                        .bg(theme.foreground.opacity(0.6))
                                        .group_hover(group.clone(), |s| s.opacity(0.)),
                                )
                            })
                            .child(
                                button(("close", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Close)
                                    .tooltip("Fermer (⌘W)")
                                    .when(!active || b.dirty, |b| b.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        // Not also a click on the tab: it would select the closed tab's place.
                                        cx.stop_propagation();
                                        this.close_buffer(ix, window, cx)
                                    })),
                            ),
                    )
                    .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| this.click_tab(ix, e.modifiers().alt, window, cx)))
                    // A new tab fades in (once: keyed by the buffer, not its place).
                    .map(|tab| match animations {
                        true => tab.with_animation(("tab-open", b.id()), Animation::new(Duration::from_millis(140)).with_easing(ease_out_quint()), |el, d| el.opacity(d)).into_any_element(),
                        false => tab.into_any_element(),
                    })
            }).collect();
        if let Some(c) = self.closing.as_ref().filter(|c| c.at <= tabs.len()) {
            let (width, gap) = (c.width, r(4.).to_pixels(window.rem_size()));
            let ghost = div()
                .flex_shrink_0()
                .h(r(34.))
                .pl(r(12.))
                .overflow_hidden()
                .whitespace_nowrap()
                .flex()
                .items_center()
                .text_size(r(13.))
                .text_color(theme.muted_foreground)
                .child(c.title.clone())
                // Its width and the gap after it go to zero: the next tabs slide into place.
                .with_animation(("tab-close", c.id), Animation::new(TAB_CLOSE).with_easing(ease_out_quint()), move |el, d| el.w(width * (1. - d)).mr(gap * -d).opacity(1. - d));
            tabs.insert(c.at, ghost.into_any_element());
        }
        h_flex()
            .on_children_prepainted(move |bounds, _, _| *widths.borrow_mut() = bounds.iter().map(|b| b.size.width).collect())
            .id("tabs")
            .flex_1()
            .min_w_0()
            .h_full()
            .gap(r(4.))
            .overflow_x_scroll()
            .children(tabs)
            .into_any_element()
    }

    fn render_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(b) = self.buffers.get(self.active) else {
            return self.render_welcome(cx).into_any_element();
        };
        let ui_font = cx.theme().font_family.clone();
        // Live Markdown preview, re-rendered from the editor's text as you type.
        let markdown = match &b.view {
            View::Text(editor) if languages::is_markdown(b.lang) => Some(editor),
            _ => None,
        };
        let shown = markdown.filter(|_| b.md_mode != MdMode::Source);
        // gpui-component parses a preview's first text synchronously, in layout:
        // a long document froze the window for seconds. So its first frame gets
        // an empty text, and the real one comes next frame, parsed in the background.
        let ready = shown.is_some_and(|e| self.previewed.get() == Some(e.entity_id()));
        self.previewed.set(shown.map(|e| e.entity_id()));
        if shown.is_some() && !ready {
            window.request_animation_frame();
        }
        let preview = shown.map(|editor| {
            let text = if ready { preview_source(&editor.read(cx).value()) } else { SharedString::default() };
            div().size_full().font_family(ui_font.clone()).text_size(r(15.)).child(
                TextView::markdown(("md-preview", editor.entity_id().as_u64() as usize), text, window, cx)
                    .scrollable(true)
                    .selectable(true)
                    .size_full()
                    .px(r(32.))
                    .py(r(24.)),
            )
        });
        // Semantic completion needs the language's server: offer to install it.
        // A completion plugin exists for this language but is not installed.
        let missing_plugin = match (&b.view, &b.lang.server) {
            (View::Text(_), Some(server)) => server.install.is_some() && AppSettings::get(cx).server_enabled(server.program) && !self.servers.available(b.lang),
            _ => false,
        };
        let lang = b.lang;
        let formatter = format::for_language(lang.name, AppSettings::get(cx)).map(|(f, _)| f.name).or((lang.highlight == "json").then_some("le formateur JSON intégré"));
        // The file differs from its last commit (margin marks): its diff is one click away.
        let changed = b.editor().filter(|e| !e.read(cx).line_markers.is_empty()).and(b.path.clone());
        let tabs = self.render_tabs(window, cx);
        let left = self.render_content(b, preview, cx);
        let right = self.split.filter(|s| *s != self.active).and_then(|ix| self.buffers.get(ix)).map(|rb| (rb.title.clone(), rb.path.clone(), self.render_content(rb, None, cx)));
        let theme = cx.theme();
        let mode_button = |id: &'static str, label: &'static str, mode: MdMode| {
            button(id)
                .ghost()
                .small()
                .label(label)
                .selected(b.md_mode == mode)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(b) = this.buffers.get_mut(this.active) {
                        b.md_mode = mode;
                        cx.notify();
                    }
                }))
        };

        v_flex()
            .size_full()
            // Tabs, then the tab's own tools on the right.
            .child(
                h_flex()
                    .h(r(48.))
                    .flex_shrink_0()
                    .px(r(10.))
                    .gap(r(8.))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(tabs)
                    .when(missing_plugin, |this| {
                        this.child(
                            button("plugin-available")
                                .ghost()
                                .small()
                                .text_color(theme.primary)
                                .icon(Icon::empty().path("icons/puzzle.svg"))
                                .label(format!("Plugin {}", lang.name))
                                .tooltip(format!("Un plugin de complétion existe pour {} : après « objet. », les vraies méthodes de l'objet et le contenu des modules importés.", lang.name))
                                .on_click(cx.listener(|this, _, window, cx| this.open_plugins(&OpenPlugins, window, cx))),
                        )
                    })
                    .when_some(changed.filter(|_| AppSettings::get(cx).tool_added("git")), |this, path| {
                        this.child(
                            button("git-changes")
                                .ghost()
                                .small()
                                .icon(Icon::empty().path("icons/git-branch.svg"))
                                .label("Changements")
                                .tooltip("Voir ce qui a changé depuis le dernier commit")
                                .on_click(cx.listener(move |this, _, window, cx| this.show_changes(path.clone(), window, cx))),
                        )
                    })
                    .when_some(formatter.filter(|_| matches!(b.view, View::Text(_))), |this, name| {
                        this.child(
                            button("format")
                                .ghost()
                                .small()
                                .icon(Icon::empty().path("icons/indent.svg"))
                                .label("Formater")
                                .tooltip(format!("Mettre en forme avec {name} (⌥⇧F)"))
                                .on_click(cx.listener(|this, _, window, cx| this.format(&Format, window, cx))),
                        )
                    })
                    .when(markdown.is_some(), |this| {
                        this.child(
                            h_flex()
                                .flex_shrink_0()
                                .p(r(2.))
                                .gap(r(2.))
                                .rounded(r(8.))
                                .bg(theme.foreground.opacity(0.04))
                                .child(mode_button("md-source", "Code", MdMode::Source))
                                .child(mode_button("md-split", "Côte à côte", MdMode::Split))
                                .child(mode_button("md-preview", "Aperçu", MdMode::Preview)),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("pane-left")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, _| this.focus_right = false))
                            .child(left),
                    )
                    .when_some(right, |row, (title, path, content)| {
                        let on = self.focus_right;
                        row.child(
                            v_flex()
                                .id("pane-right")
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .border_l_1()
                                .border_color(theme.border)
                                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, _| this.focus_right = true))
                                .child(
                                    h_flex()
                                        .h(r(32.))
                                        .flex_shrink_0()
                                        .px(r(10.))
                                        .gap(r(6.))
                                        .border_b_1()
                                        .border_color(theme.border)
                                        .text_size(r(12.5))
                                        .text_color(if on { theme.foreground } else { theme.muted_foreground })
                                        .child(file_icon(path.as_deref().unwrap_or(Path::new(&title)), cx))
                                        .child(div().flex_1().truncate().child(title))
                                        .child(button("close-split").ghost().xsmall().icon(IconName::Close).tooltip("Fermer la vue de droite (⌘⇧D)").on_click(cx.listener(|this, _, window, cx| {
                                            this.split = None;
                                            this.activate(this.active, window, cx);
                                        }))),
                                )
                                .child(div().flex_1().min_h_0().child(content)),
                        )
                    }),
            )
            .into_any_element()
    }

    /// A tab's content: the code editor (with its Markdown preview), an image, a page or a tool.
    fn render_content(&self, b: &Buffer, preview: Option<Div>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        div().size_full().map(|this| match &b.view {
                View::Text(editor) => {
                    let (tab_editor, search_editor) = (editor.clone(), editor.clone());
                    let code = div()
                        .h_full()
                        // Tab accepts the highlighted completion, like Enter; else it indents.
                        .capture_action(move |_: &IndentInline, window, cx| {
                            if tab_editor.update(cx, |e, cx| e.handle_action_for_context_menu(Box::new(Enter { secondary: false }), window, cx)) {
                                cx.stop_propagation();
                            }
                        })
                        // ⌘F copies the selection into the single-line search field:
                        // a selection over several lines made gpui panic.
                        .capture_action(move |_: &Search, window, cx| {
                            search_editor.update(cx, |e, cx| {
                                let selection = e.selected_text_range(true, window, cx).and_then(|s| e.text_for_range(s.range, &mut None, window, cx));
                                if selection.is_some_and(|text| text.contains('\n')) {
                                    e.unselect(window, cx);
                                }
                            })
                        })
                        .pt(r(6.))
                        .font_family(theme.mono_font_family.clone())
                        .child(
                            // gpui-component's code editor packs lines at 1.25: give them air.
                            Input::new(editor).h_full().appearance(false).text_size(theme.mono_font_size).line_height(relative(1.65)),
                        );
                    match (b.md_mode, preview) {
                        (MdMode::Split, Some(preview)) => this.child(
                            h_flex()
                                .size_full()
                                .child(code.w_1_2())
                                .child(div().w_1_2().h_full().border_l_1().border_color(theme.border).child(preview)),
                        ),
                        (_, Some(preview)) => this.child(preview),
                        (_, None) => this.child(code),
                    }
                }
                View::Image(viewer) => this.child(viewer.clone()),
                View::Settings(view) => this.child(view.clone()),
                View::Plugins(view) => this.child(view.clone()),
                View::Tool { view, .. } => this.child(view.clone()),
            })
        .into_any_element()
    }

    fn render_terminal_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let animations = AppSettings::get(cx).animations;
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let tabs: Vec<AnyElement> = self
            .terminals
            .iter()
            .enumerate()
            .map(|(ix, t)| {
                let on = ix == self.active_terminal;
                let group = SharedString::from(format!("term-tab-{ix}"));
                h_flex()
                    .id(group.clone())
                    .group(group.clone())
                    .h(r(26.))
                    .pl(r(10.))
                    .pr(r(4.))
                    .gap(r(6.))
                    .rounded(r(7.))
                    .cursor_pointer()
                    .text_size(r(12.5))
                    .map(|d| if on { d.bg(fg.opacity(0.06)).text_color(fg) } else { d.text_color(muted).hover(|d| d.bg(fg.opacity(0.05)).text_color(fg)) })
                    .child(Icon::new(IconName::SquareTerminal).small())
                    .child(t.title.clone())
                    .child(
                        button(SharedString::from(format!("term-close-{ix}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Fermer ce terminal")
                            .when(!on, |b| b.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.kill_terminal(ix, window, cx)
                            })),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.active_terminal = ix;
                        this.focus_terminal(window, cx);
                    }))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .size_full()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .h(r(38.))
                    .flex_shrink_0()
                    .px(r(10.))
                    .gap(r(4.))
                    .child(h_flex().id("term-tabs").min_w_0().gap(r(4.)).overflow_x_scroll().children(tabs))
                    .child(button("term-new").ghost().xsmall().icon(IconName::Plus).tooltip("Nouveau terminal").on_click(cx.listener(|this, _, window, cx| {
                        this.new_terminal(window, cx);
                    })))
                    .child(div().flex_1())
                    .child(button("term-hide").ghost().xsmall().icon(IconName::ChevronDown).tooltip("Masquer (⌃`)")
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_terminal(&ToggleTerminal, window, cx)))),
            )
            .child(div().flex_1().min_h_0().px(r(4.)).children(self.terminals.get(self.active_terminal).map(|t| t.view.clone())))
            .map(|panel| match animations {
                true => panel.with_animation(("term-open", self.term_opened), Animation::new(Duration::from_millis(140)).with_easing(ease_out_quint()), |el, d| el.opacity(d)).into_any_element(),
                false => panel.into_any_element(),
            })
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let b = self.buffers.get(self.active);
        let item = |text: String| div().whitespace_nowrap().child(text);
        h_flex()
            .h(r(28.))
            .flex_shrink_0()
            .px(r(16.))
            .gap(r(18.))
            .text_size(r(11.5))
            .text_color(theme.muted_foreground)
            .child(
                h_flex()
                    .gap(r(6.))
                    .min_w_0()
                    .overflow_hidden()
                    .child(Icon::new(IconName::Folder).size(r(13.)).flex_shrink_0())
                    .child(div().min_w_0().truncate().child(self.root.as_ref().map_or("Aucun projet".into(), |r| r.display().to_string().replacen(&home().display().to_string(), "~", 1)))),
            )
            .when(self.root.is_some() && !self.trusted(), |d| {
                d.child(
                    h_flex()
                        .id("restricted")
                        .flex_shrink_0()
                        .gap(r(5.))
                        .px(r(6.))
                        .rounded(r(4.))
                        .cursor_pointer()
                        .text_color(theme.yellow)
                        .hover(|d| d.bg(theme.yellow.opacity(0.12)))
                        .child(Icon::new(IconName::TriangleAlert).size(r(12.)))
                        .child("Mode restreint")
                        .tooltip(|window, cx| Tooltip::new("Le code du projet ne s'exécute pas tout seul (ESLint, Clippy, Git avec hooks…). Cliquez pour faire confiance à ce dossier.").build(window, cx))
                        .on_click(cx.listener(|this, _, window, cx| this.trust_folder(window, cx))),
                )
            })
            .map(|this| {
                let (errors, warnings) = self.servers.problems.counts();
                this.child(
                    h_flex()
                        .id("status-problems")
                        .flex_shrink_0()
                        .gap(r(8.))
                        .px(r(6.))
                        .rounded(r(5.))
                        .cursor_pointer()
                        .hover(|d| d.bg(theme.foreground.opacity(0.06)).text_color(theme.foreground))
                        .tooltip(|window, cx| Tooltip::new("Problèmes").build(window, cx))
                        .child(h_flex().gap(r(3.)).child(Icon::new(IconName::CircleX).size(r(13.)).when(errors > 0, |i| i.text_color(theme.red))).child(item(errors.to_string())))
                        .child(h_flex().gap(r(3.)).child(Icon::new(IconName::TriangleAlert).size(r(13.)).when(warnings > 0, |i| i.text_color(theme.yellow))).child(item(warnings.to_string())))
                        .on_click(cx.listener(|this, _, window, cx| this.show_panel(Panel::Problems, window, cx))),
                )
            })
            .when_some(self.git.as_ref(), |this, g| {
                let sync = [(g.ahead, "↑"), (g.behind, "↓")].into_iter().filter(|(n, _)| *n > 0).map(|(n, arrow)| format!("{arrow}{n}")).collect::<Vec<_>>().join(" ");
                this.child(
                    h_flex()
                        .id("status-git")
                        .flex_shrink_0()
                        .gap(r(5.))
                        .px(r(6.))
                        .rounded(r(5.))
                        .cursor_pointer()
                        .hover(|d| d.bg(theme.foreground.opacity(0.06)).text_color(theme.foreground))
                        .tooltip(|window, cx| Tooltip::new("Ouvrir Git (⌘⇧G)").build(window, cx))
                        .child(Icon::empty().path("icons/git-branch.svg").size(r(13.)))
                        .child(div().max_w(r(200.)).truncate().child(if g.branch.is_empty() { "HEAD".to_string() } else { g.branch.clone() }))
                        .when(!sync.is_empty(), |d| d.child(item(sync)))
                        .when(g.count > 0, |d| d.child(item(format!("● {}", g.count))))
                        .on_click(cx.listener(|this, _, window, cx| this.open_git(&OpenGit, window, cx))),
                )
            })
            .child(div().flex_1())
            // A slow answer on its way (definition, references, rename, formatting).
            .when_some(self.busy.clone(), |this, text| this.child(h_flex().flex_shrink_0().gap(r(6.)).child(Spinner::new().xsmall().color(theme.muted_foreground)).child(item(text.to_string()))))
            .when_some(b, |this, b| {
                let runtime = self.run_command(b).and_then(|c| c.split_whitespace().next().map(String::from));
                match &b.view {
                    View::Text(editor) => {
                        let pos = editor.read(cx).cursor_position();
                        this.child(item(format!("Ligne {}, col. {}", pos.line + 1, pos.character + 1)))
                    }
                    View::Image(viewer) => this.children(viewer.read(cx).dimensions().map(|d| item(format!("{d} px")))),
                    _ => this,
                }
                .when_some(runtime, |this, r| this.child(item(format!("▶ {r}"))))
                .map(|this| match &b.view {
                    View::Text(_) => this.child(h_flex().gap(r(6.)).child(div().size(r(7.)).rounded_full().bg(rgb(b.lang.color))).child(b.lang.name)),
                    View::Image(_) => this.child(item("Image".into())),
                    _ => this,
                })
            })
    }
}

/// Sizes are designed at 14 px and follow "Taille du texte de l'interface".
pub fn r(px_at_14: f32) -> Rems {
    rems(px_at_14 / 14.)
}

/// Keyboard shortcut drawn as keycaps: "⇧⌘O" → ⇧ ⌘ O.
fn kbd(keys: &'static str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex().gap(r(3.)).children(keys.chars().map(|k| {
        div()
            .min_w(r(18.))
            .h(r(18.))
            .px(r(4.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(r(4.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.foreground.opacity(0.03))
            .text_size(r(11.))
            .text_color(theme.muted_foreground)
            .child(k.to_string())
    }))
}

/// The app's button. The (patched) component shows the pointer itself, and
/// only while enabled: a disabled button keeps the arrow.
pub fn button(id: impl Into<ElementId>) -> Button {
    Button::new(id)
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.show_terminal && !self.term_was_shown {
            self.term_opened += 1;
        }
        self.term_was_shown = self.show_terminal;
        if self.show_sidebar != self.sidebar_was_shown {
            self.sidebar_was_shown = self.show_sidebar;
            if AppSettings::get(cx).animations {
                self.sidebar_slides += 1;
                let id = self.sidebar_slides;
                self.sidebar_anim = Some((self.show_sidebar, id));
                // Its end: the hidden sidebar stops being drawn.
                self._sidebar_slide = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(SIDEBAR_SLIDE).await;
                    let _ = this.update(cx, |this, cx| {
                        if this.sidebar_anim.is_some_and(|(_, i)| i == id) {
                            this.sidebar_anim = None;
                            cx.notify();
                        }
                    });
                }));
            }
        }
        // One code editor shows at a time: the global syntax colors follow its language.
        let color = self.buffers.get(self.active).map(|b| b.lang.color).filter(|_| AppSettings::get(cx).language_colors);
        if self.syntax_color != color {
            self.syntax_color = color;
            Theme::global_mut(cx).highlight_theme = Arc::new(color.map_or_else(|| (*self.base_syntax).clone(), |c| settings::syntax_theme(&self.base_syntax, c)));
        }
        let theme = cx.theme();
        let (chrome, card, border) = (theme.sidebar, theme.background, theme.border);
        v_flex()
            .size_full()
            .key_context("Workspace")
            // The right-click outline goes once the menu is used or dismissed.
            .capture_any_mouse_down(cx.listener(|this, e: &gpui::MouseDownEvent, _, cx| {
                if e.button == MouseButton::Left && this.menu_target.take().is_some() {
                    cx.notify();
                }
            }))
            .capture_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" && this.menu_target.take().is_some() {
                    cx.notify();
                }
            }))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::lint_file))
            .on_action(cx.listener(Self::copy_file_path))
            .on_action(cx.listener(Self::copy_relative_path))
            .on_action(cx.listener(Self::reveal_in_explorer))
            .on_action(cx.listener(Self::show_file_changes))
            .on_action(cx.listener(Self::run))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::close_folder))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::open_plugins))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::select_tab))
            .on_action(cx.listener(Self::reopen_tab))
            .on_action(cx.listener(Self::format))
            .on_action(cx.listener(Self::open_git))
            .on_action(cx.listener(Self::open_search))
            .on_action(cx.listener(Self::go_to_definition))
            .on_action(cx.listener(Self::find_references))
            .on_action(cx.listener(Self::rename_symbol))
            .on_action(cx.listener(Self::quick_fix))
            .on_action(cx.listener(Self::split_editor))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(1., cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(-1., cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(0., cx)))
            .bg(chrome)
            .text_color(theme.foreground)
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    // Focus is tracked here, below the title bar: a focusable element takes every
                    // mouse down inside it, and on Windows that keeps the title bar's drag area and
                    // its minimize/maximize/close buttons from reaching the system.
                    .track_focus(&self.focus)
                    // Our own sidebar resizing: gpui-component's handle draws a line
                    // that juts out of the card's rounded corners.
                    .on_drag_move(cx.listener(|this, e: &DragMoveEvent<SidebarDrag>, window, cx| {
                        // Limits in rems: a bigger UI text gets a wider sidebar.
                        let rem = window.rem_size();
                        // The row starts with the activity bar: the sidebar's left edge is after it
                        // (without this, the edge jumped 56 px away from the cursor).
                        let left = e.bounds.left() + r(ACTIVITY_BAR).to_pixels(rem);
                        this.sidebar_width = (e.event.position.x - left).clamp(r(180.).to_pixels(rem), r(520.).to_pixels(rem));
                        cx.notify();
                    }))
                    .child(self.render_activity_bar(cx))
                    .when(self.show_sidebar || self.sidebar_anim.is_some_and(|(open, _)| !open), |this| {
                        let width = self.sidebar_width;
                        let sidebar = div().relative().h_full().w(width).flex_shrink_0().child(self.render_sidebar(cx)).child(
                                div()
                                    .id("sidebar-resize")
                                    .absolute()
                                    .top_0()
                                    .right(px(-4.))
                                    .h_full()
                                    .w(px(8.))
                                    .flex()
                                    .justify_center()
                                    .cursor_col_resize()
                                    // A thin line shows where to drag.
                                    .child(div().w(px(2.)).h_full().rounded_full().hover(|d| d.bg(cx.theme().primary.opacity(0.5))))
                                    .on_drag(SidebarDrag, |_, _, _, cx| cx.new(|_| SidebarDrag)),
                            );
                        match self.sidebar_anim {
                            // Sliding: its width goes from 0 to full (or back), the content is
                            // revealed, not squeezed.
                            Some((open, id)) => this.child(
                                div().h_full().flex_shrink_0().overflow_hidden().child(sidebar).with_animation(
                                    ("sidebar-slide", id),
                                    Animation::new(SIDEBAR_SLIDE).with_easing(ease_out_quint()),
                                    move |el, d| el.w(width * if open { d } else { 1. - d }),
                                ),
                            ),
                            None => this.child(sidebar),
                        }
                    })
                    .child(
                        div().flex_1().min_w_0().h_full().child(
                                // The editor sits in a rounded card inside the window chrome.
                                div().size_full().pr(r(8.)).child(
                                    div()
                                        .size_full()
                                        .rounded(r(12.))
                                        .overflow_hidden()
                                        .border_1()
                                        .border_color(border)
                                        .bg(card)
                                        .shadow_lg()
                                        .child(
                                            // A fresh layout each time the terminal opens: hidden, the
                                            // group shrank it to its 100 px minimum and kept that.
                                            // shortcut: a height dragged by hand is lost when hidden; keep it in a field if missed.
                                            v_resizable(SharedString::from(format!("center-{}", self.term_opened)))
                                                .child(resizable_panel().child(self.render_editor(window, cx)))
                                                .child(
                                                    resizable_panel()
                                                        .size(r(280.).to_pixels(window.rem_size()))
                                                        .size_range(r(100.).to_pixels(window.rem_size())..px(2000.))
                                                        .visible(self.show_terminal)
                                                        .child(self.render_terminal_panel(cx)),
                                                ),
                                        ),
                                ),
                            ),
                        ),
            )
            .child(self.render_status_bar(cx))
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_word_at_a_server_position_counts_utf16() {
        let text = "x\nf(\"🎉\", a, b)";
        // "b" is at character 10 but UTF-16 unit 11 (the emoji counts 2).
        assert_eq!(word_at(text, 1, 11), "b");
        assert_eq!(word_at(text, 1, 8), "a");
        assert_eq!(word_at(text, 0, 0), "x");
        assert_eq!(word_at(text, 1, 99), "");
    }

    #[test]
    fn losing_work_needs_the_same_action_twice() {
        let (a, b) = (EntityId::from(1u64), EntityId::from(2u64));
        let mut armed = None;
        // ⌘W on a dirty tab warns; then closing the window must warn again.
        assert!(!confirm(&mut armed, Discard::Tab(a)));
        assert!(!confirm(&mut armed, Discard::Window));
        assert!(confirm(&mut armed, Discard::Window));
        // The other way round, and with another tab.
        assert!(!confirm(&mut armed, Discard::Window));
        assert!(!confirm(&mut armed, Discard::Tab(a)));
        assert!(!confirm(&mut armed, Discard::Tab(b)));
        assert!(confirm(&mut armed, Discard::Tab(b)));
        // Overwriting a changed file is its own confirmation.
        assert!(!confirm(&mut armed, Discard::Overwrite(a)));
        assert!(confirm(&mut armed, Discard::Overwrite(a)));
        assert_eq!(armed, None);
    }

    #[test]
    fn changes_on_disk_are_noticed() {
        let path = std::env::temp_dir().join(format!("akolade-mtime-{}", std::process::id()));
        fs::write(&path, "a").unwrap();
        let known = modified(&path);
        assert!(known.is_some() && modified(&path) == known);
        // Someone else writes the file one second later (git checkout, formatter…).
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(known.unwrap() + std::time::Duration::from_secs(1)).unwrap();
        assert_ne!(modified(&path), known);
        fs::remove_file(&path).unwrap();
        assert_eq!(modified(&path), None, "a deleted file counts as changed");
    }

    #[test]
    fn preview_highlights_only_the_first_code_blocks() {
        let block = |i: usize| format!("```js\nconst a{i} = 1;\n```\n\ntext\n");
        let md: String = (0..MAX_HIGHLIGHTED_BLOCKS + 2).map(block).collect();
        let out = preview_source(&md);
        assert_eq!(out.matches("```js").count(), MAX_HIGHLIGHTED_BLOCKS);
        assert_eq!(out.matches("const a").count(), MAX_HIGHLIGHTED_BLOCKS + 2, "code is kept");
        assert_eq!(out.len(), md.len() - 2 * 2, "only the 2 extra `js` tags are removed");
        assert_eq!(preview_source("# hi\n").as_ref(), "# hi\n");
    }

    #[test]
    fn preview_keeps_long_fences_and_their_content() {
        let first: String = (0..MAX_HIGHLIGHTED_BLOCKS).map(|i| format!("```js\nconst a{i} = 1;\n```\n")).collect();
        // Markdown shown inside Markdown: the inner ``` lines are content, not fences.
        let nested = "````md\n```js\ninner\n```\n````\n";
        let out = preview_source(&format!("{first}{nested}```rust\nlast\n```\n"));
        assert!(out.ends_with("````\n```js\ninner\n```\n````\n```\nlast\n```\n"), "{}", &out[out.len() - 60..]);
    }

    #[cfg(unix)]
    #[test]
    fn saving_keeps_permissions_symlinks_and_no_leftover() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("akolade-save-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("run.sh");
        fs::write(&script, "old").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&script, dir.join("link.sh")).unwrap();
        write_file(&dir.join("link.sh"), "#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(fs::read_to_string(&script).unwrap(), "#!/bin/sh\necho hi\n", "written through the link");
        assert!(fs::symlink_metadata(dir.join("link.sh")).unwrap().file_type().is_symlink(), "the link stays a link");
        assert_eq!(fs::metadata(&script).unwrap().permissions().mode() & 0o777, 0o755, "still executable");
        write_file(&dir.join("new.txt"), "neuf").unwrap();
        assert_eq!(fs::read_to_string(dir.join("new.txt")).unwrap(), "neuf");
        assert!(write_file(&dir.join("missing/x.txt"), "x").is_err());
        let names: Vec<String> = fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(!names.iter().any(|n| n.contains("akolade-tmp")), "{names:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn json_is_indented_without_changing_values() {
        let src = r#"{"b":1.50,"a":[1,2,{}],"s":"x,{\"y\":[]}\\","e":[ ],"n":null}"#;
        let pretty = pretty_json(src).unwrap();
        assert_eq!(
            pretty,
            "{\n  \"b\": 1.50,\n  \"a\": [\n    1,\n    2,\n    {}\n  ],\n  \"s\": \"x,{\\\"y\\\":[]}\\\\\",\n  \"e\": [],\n  \"n\": null\n}\n",
            "key order, number spelling and strings untouched"
        );
        assert_eq!(pretty_json(&pretty).unwrap(), pretty, "formatting twice changes nothing");
        assert_eq!(pretty_json("{\"a\": [1,\n 2"), Err("ligne 2, colonne 2".into()));
    }

    #[test]
    fn run_commands_expand_placeholders() {
        let cmd = expand_run("cc {file} -o {out} && {out}", Path::new("/p/it's main.c"), &HashMap::new());
        let out = shell_quote(&std::env::temp_dir().join("akolade-it's main").to_string_lossy());
        let file = if cfg!(windows) { "'/p/it''s main.c'" } else { "'/p/it'\\''s main.c'" };
        assert_eq!(cmd, format!("cc {file} -o {out} && {out}"));
    }

    #[test]
    fn quotes_keep_a_name_one_word_in_the_terminal_shell() {
        let quoted = shell_quote("l'outil; calc; '.py");
        if cfg!(windows) {
            assert_eq!(quoted, "'l''outil; calc; ''.py'"); // PowerShell: '' is a quote inside quotes
        } else {
            assert_eq!(quoted, "'l'\\''outil; calc; '\\''.py'");
            let echoed = crate::platform::command("sh").arg("-c").arg(format!("printf %s {quoted}")).output().unwrap();
            assert_eq!(String::from_utf8_lossy(&echoed.stdout), "l'outil; calc; '.py");
        }
    }

    #[test]
    fn project_variables_are_typed_with_the_command() {
        let env = HashMap::from([
            ("NODE_ENV".to_string(), "dev; rm -rf ~".to_string()), // the value stays a value
            ("BAD KEY;ls".to_string(), "x".to_string()),           // not a variable name: dropped
            ("API_URL".to_string(), "http://x".to_string()),
        ]);
        let cmd = expand_run("node {file}", Path::new("/p/a.js"), &env);
        let expected = if cfg!(windows) { "$env:API_URL='http://x'; $env:NODE_ENV='dev; rm -rf ~'; node '/p/a.js'" } else { "export API_URL='http://x' NODE_ENV='dev; rm -rf ~'; node '/p/a.js'" };
        assert_eq!(cmd, expected);
    }

    #[test]
    fn terminal_colors_follow_the_theme() {
        assert_eq!(rgb8(gpui::rgb(0x1b1c22).into()), (0x1b, 0x1c, 0x22));
    }
}
