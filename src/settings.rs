//! User settings: stored as JSON next to the language servers, applied live,
//! edited in the "Paramètres" tab (⌘,).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::{
    App, Context, Global, Hsla, IntoElement, ParentElement, Render, SharedString, Styled, Task, WeakEntity,
    Window, div, hsla, prelude::FluentBuilder, px, rgb,
};
use gpui_component::{
    ActiveTheme, Sizable,
    button::ButtonVariants,
    h_flex, v_flex,
    highlighter::{HighlightTheme, ThemeStyle},
    setting::{NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings},
    theme::Theme,
};
use serde::{Deserialize, Serialize};

use crate::{
    icons,
    explorer::ScanOptions,
    plugins, themes,
    tools::claude,
    workspace::{OpenPlugins, Workspace, button, r},
};

/// Akolade's own folder (settings, language servers).
/// A JSON file of ours, or the defaults. A file that exists but doesn't parse
/// (edited by hand) is kept aside as `<name>.bak` first: the next save would
/// otherwise replace it with the defaults.
pub fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    let Ok(bytes) = std::fs::read(path) else { return T::default() };
    serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        let _ = std::fs::rename(path, bak);
        T::default()
    })
}

pub fn app_dir() -> PathBuf {
    let home = crate::platform::home();
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Akolade")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Roaming")).join("Akolade")
    } else {
        std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".local/share")).join("akolade")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Name of one of `themes::all()`.
    pub theme: String,
    /// Keywords, functions and types tinted with each language's color.
    pub language_colors: bool,
    pub ui_font_size: f64,
    /// Empty: the nicest installed code font.
    pub code_font: String,
    pub code_font_size: f64,
    pub line_numbers: bool,
    pub soft_wrap: bool,
    /// Save one second after the last keystroke.
    pub autosave: bool,
    /// "source", "split" or "preview".
    pub markdown_mode: String,
    pub completion: bool,
    /// Ask the language servers (types, members of objects and modules).
    pub language_servers: bool,
    pub terminal_font_size: f64,
    pub show_hidden: bool,
    /// Comma-separated folder names hidden from the explorer.
    pub ignored: String,
    pub reopen_last_folder: bool,
    pub last_folder: Option<PathBuf>,
    /// The projects opened lately, the last first (the multiplexer offers them).
    pub recent_folders: Vec<PathBuf>,
    /// Programs of the completion plugins turned off.
    pub disabled_plugins: Vec<String>,
    /// Store tools added to the activity bar (`tools::TOOLS` ids).
    pub tools: Vec<String>,
    /// The file icons: one of `icons::PACKS`.
    pub icon_pack: String,
    /// Slides and fades (tabs, sidebar, terminal, the line jumped to).
    pub animations: bool,
    /// Claude Code: model and effort ("": its own default), permission mode (`claude::MODES`).
    pub claude_model: String,
    pub claude_effort: String,
    pub claude_mode: String,
    /// Tell Claude which file is open.
    pub claude_with_file: bool,
    /// The files a turn changed are kept at once (still listed, and undoable from a message).
    pub claude_auto_keep: bool,
    /// The multiplexer shows its sessions side by side, not grouped by project.
    pub mux_flat: bool,
    /// Dollars a session may spend, 0: no limit.
    pub claude_budget: f64,
    /// Added to Claude's own instructions.
    pub claude_instructions: String,
    /// Client HTTP: seconds before a request gives up.
    pub http_timeout: f64,
    /// Folders whose own code may run (see `trust`).
    pub trusted_folders: Vec<PathBuf>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: themes::DEFAULT.into(),
            language_colors: true,
            ui_font_size: 14.,
            code_font: String::new(),
            code_font_size: 13.5,
            line_numbers: true,
            soft_wrap: false,
            autosave: false,
            markdown_mode: "split".into(),
            completion: true,
            language_servers: true,
            terminal_font_size: 13.,
            show_hidden: false,
            ignored: "node_modules, target, dist, build".into(),
            reopen_last_folder: true,
            last_folder: None,
            recent_folders: vec![],
            disabled_plugins: vec![],
            tools: vec!["git".into()],
            icon_pack: "couleurs".into(),
            animations: true,
            claude_model: String::new(),
            claude_effort: String::new(),
            claude_mode: "manual".into(),
            claude_with_file: true,
            claude_auto_keep: false,
            mux_flat: false,
            claude_budget: 0.,
            claude_instructions: String::new(),
            http_timeout: 60.,
            trusted_folders: vec![],
        }
    }
}

impl Global for AppSettings {}

impl AppSettings {
    fn path() -> PathBuf {
        app_dir().join("settings.json")
    }

    /// Settings saved on disk; defaults for anything missing or unreadable.
    pub fn load() -> Self {
        let settings: Self = read_json(&Self::path());
        crate::trust::set(&settings.trusted_folders);
        settings
    }

    pub fn get(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// Changes the settings, saves them, and applies them everywhere.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut Self)) {
        let mut settings = Self::get(cx).clone();
        change(&mut settings);
        if &settings == Self::get(cx) {
            return;
        }
        // A failed write only loses the change for the next launch.
        let _ = std::fs::create_dir_all(app_dir());
        let _ = crate::workspace::write_file(&Self::path(), &serde_json::to_string_pretty(&settings).unwrap_or_default());
        crate::trust::set(&settings.trusted_folders);
        cx.set_global(settings);
        apply_appearance(cx);
        cx.refresh_windows();
    }

    /// May this language server (completion plugin) run?
    pub fn server_enabled(&self, program: &str) -> bool {
        self.language_servers && self.plugin_enabled(program)
    }

    /// Not turned off in the store (`id`: the plugin's program, or "git").
    pub fn plugin_enabled(&self, id: &str) -> bool {
        !self.disabled_plugins.iter().any(|p| p == id)
    }

    /// This store tool is added (icon in the activity bar, Git colors…).
    pub fn tool_added(&self, id: &str) -> bool {
        self.tools.iter().any(|t| t == id)
    }

    pub fn scan_options(&self) -> ScanOptions {
        ScanOptions {
            show_hidden: self.show_hidden,
            ignored: self.ignored.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect(),
        }
    }
}

/// Code fonts offered in the settings, nicest first; the first installed one is the default.
const CODE_FONTS: &[&str] = &["JetBrains Mono", "JetBrainsMono Nerd Font", "Fira Code", "SF Mono", "Cascadia Code", "Source Code Pro", "Hack", "Menlo", "Monaco", "Courier New"];

fn installed_code_fonts(cx: &App) -> Vec<&'static str> {
    let installed = cx.text_system().all_font_names();
    CODE_FONTS.iter().copied().filter(|f| installed.iter().any(|n| n == f)).collect()
}

/// Theme, font sizes and code font from the settings.
pub fn apply_appearance(cx: &mut App) {
    let s = AppSettings::get(cx).clone();
    themes::apply(&s.theme, cx);
    let fonts = installed_code_fonts(cx);
    let font = fonts.iter().find(|f| **f == s.code_font).or(fonts.first()).copied().unwrap_or("Menlo");
    let theme = Theme::global_mut(cx);
    theme.font_size = px(s.ui_font_size as f32);
    theme.mono_font_size = px(s.code_font_size as f32);
    theme.mono_font_family = font.into();
}

/// A language's syntax colors: keywords in its own color (Python blue,
/// JavaScript yellow, Rust orange…), functions and types in the two
/// split-complementary hues. Strings, numbers and comments keep the theme's.
pub fn syntax_theme(base: &HighlightTheme, color: u32) -> HighlightTheme {
    let brand = Hsla::from(rgb(color));
    let mut theme = base.clone();
    if brand.s < 0.25 {
        return theme; // grey: plain text
    }
    // Light on a dark background, dark on a light one.
    let dark = base.appearance.is_dark();
    // Saturated, a bit darker than a pastel: the language's color reads at a glance.
    let style = |turn: f32, l: f32| -> Option<ThemeStyle> {
        let (s, l) = if dark { (0.88, l) } else { (0.85, l - 0.3) };
        let hex = themes::hsla_hex(hsla((brand.h + turn).rem_euclid(1.), s, l, 1.));
        serde_json::from_value(serde_json::json!({ "color": hex })).ok()
    };
    let syntax = &mut theme.style.syntax;
    syntax.keyword = style(0., 0.66);
    syntax.function = style(150. / 360., 0.7);
    let types = style(-150. / 360., 0.68);
    (syntax.type_, syntax.constructor, syntax.enum_) = (types, types, types);
    theme
}

/// The "Paramètres" tab.
pub struct SettingsView {
    workspace: WeakEntity<Workspace>,
    fonts: Vec<(SharedString, SharedString)>,
    /// Plugins installed, for the Store line: PATH lookups, kept off the renders.
    installed: usize,
    _lookup: Task<()>,
}

fn installed_plugins() -> usize {
    let found = plugins::Found::lookup();
    plugins::plugins().iter().filter(|p| found.installed(p)).count()
}

impl SettingsView {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let fonts = std::iter::once(("".into(), "Automatique".into()))
            .chain(installed_code_fonts(cx).into_iter().map(|f| (f.into(), f.into())))
            .collect();
        // Looked up again now and then, off the UI thread: installs end in the background.
        let _lookup = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let n = cx.background_executor().spawn(async { installed_plugins() }).await;
                let updated = this.update(cx, |this, cx| {
                    if this.installed != n {
                        this.installed = n;
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        Self { workspace, fonts, installed: installed_plugins(), _lookup }
    }
}

fn switch(get: fn(&AppSettings) -> bool, set: fn(&mut AppSettings, bool)) -> SettingField<bool> {
    SettingField::switch(move |cx| get(AppSettings::get(cx)), move |v, cx| AppSettings::update(cx, |s| set(s, v)))
}

fn number(min: f64, max: f64, step: f64, get: fn(&AppSettings) -> f64, set: fn(&mut AppSettings, f64)) -> SettingField<f64> {
    SettingField::number_input(
        NumberFieldOptions { min, max, step },
        move |cx| get(AppSettings::get(cx)),
        move |v, cx| AppSettings::update(cx, |s| set(s, v.clamp(min, max))),
    )
}

/// A dropdown over fixed choices: (value saved, label shown).
fn choices(options: &[(&'static str, &'static str)], get: fn(&AppSettings) -> &String, set: fn(&mut AppSettings, String)) -> SettingField<SharedString> {
    let options = options.iter().map(|(v, l)| (SharedString::from(*v), SharedString::from(*l))).collect();
    SettingField::dropdown(options, move |cx| get(AppSettings::get(cx)).clone().into(), move |v, cx| AppSettings::update(cx, |s| set(s, v.to_string())))
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let d = AppSettings::default();
        let fonts = self.fonts.clone();
        let themes: Vec<(SharedString, SharedString)> = themes::all().into_iter().map(|t| (t.name.into(), t.name.into())).collect();
        let md_modes: Vec<(SharedString, SharedString)> =
            vec![("source".into(), "Code".into()), ("split".into(), "Côte à côte".into()), ("preview".into(), "Aperçu".into())];

        let workspace = self.workspace.clone();
        let installed = self.installed;
        let store = SettingItem::render(move |_, _, cx| {
            let all = plugins::plugins();
            let workspace = workspace.clone();
            h_flex()
                .w_full()
                .gap(r(12.))
                .child(
                    div()
                        .flex_1()
                        .child(div().child(format!("{installed} plugin{} installé{} sur {}", if installed > 1 { "s" } else { "" }, if installed > 1 { "s" } else { "" }, all.len())))
                        .child(div().text_size(r(12.)).text_color(cx.theme().muted_foreground).child("Les plugins de complétion s'installent depuis le Store, un par langage.")),
                )
                .child(button("manage-plugins").small().outline().label("Ouvrir le Store").on_click(move |_, window, cx| {
                    let _ = workspace.update(cx, |ws, cx| ws.open_plugins(&OpenPlugins, window, cx));
                }))
        });

        let trusted = SettingItem::render(|_, _, cx| {
            let folders = AppSettings::get(cx).trusted_folders.clone();
            let (muted, mono) = (cx.theme().muted_foreground, cx.theme().mono_font_family.clone());
            v_flex()
                .w_full()
                .gap(r(6.))
                .child(div().child("Dossiers de confiance"))
                .child(
                    div()
                        .text_size(r(12.))
                        .text_color(muted)
                        .child("Leur propre code peut s'exécuter tout seul : ESLint, Clippy, Git avec hooks, Claude Code… Les autres dossiers s'ouvrent en mode restreint."),
                )
                .when(folders.is_empty(), |d| d.child(div().text_size(r(12.5)).text_color(muted).child("Aucun pour l'instant.")))
                .children(folders.into_iter().enumerate().map(|(i, folder)| {
                    let shown = folder.display().to_string().replacen(&crate::platform::home().display().to_string(), "~", 1);
                    h_flex()
                        .gap(r(8.))
                        .child(div().flex_1().min_w_0().truncate().font_family(mono.clone()).text_size(r(12.5)).child(shown))
                        .child(button(("untrust", i)).ghost().xsmall().label("Retirer").on_click(move |_, _, cx| AppSettings::update(cx, |s| s.trusted_folders.retain(|t| *t != folder))))
                }))
        });

        Settings::new("settings").sidebar_width(r(200.).to_pixels(window.rem_size())).pages([
            SettingPage::new("Apparence").default_open(true).groups([
                SettingGroup::new()
                    .title("Thème")
                    .item(SettingItem::new(
                        "Thème",
                        SettingField::dropdown(themes, |cx| AppSettings::get(cx).theme.clone().into(), |v, cx| AppSettings::update(cx, |s| s.theme = v.to_string())),
                    ))
                    .item(SettingItem::new(
                        "Icônes des fichiers",
                        SettingField::dropdown(
                            icons::PACKS.iter().map(|(id, name, _)| (SharedString::from(*id), SharedString::from(*name))).collect(),
                            |cx| AppSettings::get(cx).icon_pack.clone().into(),
                            |v, cx| AppSettings::update(cx, |s| s.icon_pack = v.to_string()),
                        ),
                    ))
                    .item(
                        SettingItem::new("Couleurs propres à chaque langage", switch(|s| s.language_colors, |s, v| s.language_colors = v))
                            .description("Mots-clés, fonctions et types prennent la couleur du langage (Python bleu, JavaScript jaune…)."),
                    )
                    .item(
                        SettingItem::new("Animations", switch(|s| s.animations, |s, v| s.animations = v))
                            .description("Glissements et fondus : onglets, barre latérale, terminal, ligne atteinte. Désactivées, tout change d'un coup."),
                    ),
                SettingGroup::new()
                    .title("Texte")
                    .item(SettingItem::new("Taille du texte de l'interface", number(11., 20., 1., |s| s.ui_font_size, |s, v| s.ui_font_size = v).default_value(d.ui_font_size)))
                    .item(SettingItem::new(
                        "Police du code",
                        SettingField::dropdown(fonts, |cx| AppSettings::get(cx).code_font.clone().into(), |v, cx| AppSettings::update(cx, |s| s.code_font = v.to_string())),
                    ))
                    .item(SettingItem::new("Taille du code", number(10., 24., 0.5, |s| s.code_font_size, |s, v| s.code_font_size = v).default_value(d.code_font_size))),
            ]),
            SettingPage::new("Éditeur").group(
                SettingGroup::new()
                    .item(SettingItem::new("Numéros de ligne", switch(|s| s.line_numbers, |s, v| s.line_numbers = v)))
                    .item(SettingItem::new("Retour à la ligne automatique", switch(|s| s.soft_wrap, |s, v| s.soft_wrap = v)).description("Les longues lignes passent à la ligne au lieu de défiler."))
                    .item(SettingItem::new("Enregistrement automatique", switch(|s| s.autosave, |s, v| s.autosave = v)).description("Enregistre une seconde après la dernière frappe (jamais par-dessus un fichier modifié ailleurs)."))
                    .item(SettingItem::new(
                        "Affichage des fichiers Markdown",
                        SettingField::dropdown(md_modes, |cx| AppSettings::get(cx).markdown_mode.clone().into(), |v, cx| AppSettings::update(cx, |s| s.markdown_mode = v.to_string())),
                    )),
            ),
            SettingPage::new("Complétion").group(
                SettingGroup::new()
                    .item(SettingItem::new("Complétion automatique", switch(|s| s.completion, |s, v| s.completion = v)).description("Propose des mots pendant la frappe, classés par probabilité."))
                    .item(
                        SettingItem::new("Complétion avancée", switch(|s| s.language_servers, |s, v| s.language_servers = v))
                            .description("Avec les plugins installés : méthodes des objets, types, modules importés, documentation au survol."),
                    )
                    .item(store),
            ),
            SettingPage::new("Terminal").group(
                SettingGroup::new().item(SettingItem::new("Taille du texte", number(10., 22., 1., |s| s.terminal_font_size, |s, v| s.terminal_font_size = v).default_value(d.terminal_font_size))),
            ),
            SettingPage::new("Explorateur").group(
                SettingGroup::new()
                    .item(SettingItem::new("Afficher les fichiers cachés", switch(|s| s.show_hidden, |s, v| s.show_hidden = v)).description("Fichiers et dossiers commençant par un point (.env, .github…)."))
                    .item(
                        SettingItem::new(
                            "Dossiers ignorés",
                            SettingField::input(|cx| AppSettings::get(cx).ignored.clone().into(), |v, cx| AppSettings::update(cx, |s| s.ignored = v.to_string())),
                        )
                        .description("Noms séparés par des virgules."),
                    ),
            ),
            SettingPage::new("Outils").groups([
                SettingGroup::new()
                    .title("Claude Code")
                    .description("Pour chaque nouvelle conversation ; les choix du moment restent possibles sous la zone de saisie.")
                    .item(SettingItem::new("Modèle par défaut", choices(&claude::MODELS.map(|m| (m.1, m.0)), |s| &s.claude_model, |s, v| s.claude_model = v)))
                    .item(
                        SettingItem::new("Effort", choices(&claude::EFFORTS, |s| &s.claude_effort, |s, v| s.claude_effort = v))
                            .description("Plus d'effort : Claude réfléchit davantage avant d'agir, plus lentement et pour plus de tokens."),
                    )
                    .item(
                        SettingItem::new("Mode par défaut", choices(&claude::MODES.map(|m| (m.1, m.0)), |s| &s.claude_mode, |s, v| s.claude_mode = v))
                            .description("« Tout autoriser » : Claude agit sans rien demander, dès l'ouverture de l'outil."),
                    )
                    .item(SettingItem::new("Indiquer le fichier ouvert", switch(|s| s.claude_with_file, |s, v| s.claude_with_file = v)).description("Claude sait quel fichier vous regardez (désactivable message par message)."))
                    .item(
                        SettingItem::new("Toujours garder les modifications", switch(|s| s.claude_auto_keep, |s, v| s.claude_auto_keep = v))
                            .description("Les fichiers modifiés par Claude sont gardés d'office à la fin de chaque tour. Leur liste et leur diff restent affichés, et « Revenir avant ce message » les annule toujours."),
                    )
                    .item(
                        SettingItem::new("Budget maximum par session ($)", number(0., 1000., 0.5, |s| s.claude_budget, |s, v| s.claude_budget = v).default_value(d.claude_budget))
                            .description("Claude s'arrête une fois ce montant dépensé, au tarif de l'API. 0 : sans limite."),
                    )
                    .item(
                        SettingItem::new(
                            "Consignes supplémentaires",
                            SettingField::input(|cx| AppSettings::get(cx).claude_instructions.clone().into(), |v, cx| AppSettings::update(cx, |s| s.claude_instructions = v.to_string())),
                        )
                        .description("Ajoutées aux instructions de Claude à chaque conversation (ex. « Réponds en français, sois concis »)."),
                    ),
                SettingGroup::new()
                    .title("Client HTTP")
                    .item(SettingItem::new("Délai maximum d'une requête (s)", number(1., 600., 5., |s| s.http_timeout, |s, v| s.http_timeout = v).default_value(d.http_timeout))),
            ]),
            SettingPage::new("Sécurité").group(SettingGroup::new().item(trusted)),
            SettingPage::new("Démarrage").group(
                SettingGroup::new().item(SettingItem::new("Rouvrir le dernier dossier", switch(|s| s.reopen_last_folder, |s, v| s.reopen_last_folder = v))),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_old_settings_files_fall_back_to_defaults() {
        let partial: AppSettings = serde_json::from_str(r#"{ "autosave": true, "unknown_from_a_newer_version": 1 }"#).unwrap();
        assert!(partial.autosave);
        assert_eq!(partial.code_font_size, AppSettings::default().code_font_size);
    }

    #[test]
    fn each_language_gets_its_own_syntax_colors() {
        let base: HighlightTheme = serde_json::from_value(serde_json::json!({
            "name": "t", "style": { "syntax": { "keyword": { "color": "#c792ea" }, "string": { "color": "#c3e88d" } } }
        }))
        .unwrap();
        let (python, js) = (syntax_theme(&base, 0x4b8bbe), syntax_theme(&base, 0xf1d756));
        assert_ne!(python.keyword, js.keyword);
        assert_ne!(python.keyword, base.keyword);
        assert!(python.function.is_some() && python.type_.is_some());
        assert_eq!(python.string, base.string, "strings keep the theme's color");
        assert_eq!(syntax_theme(&base, 0x7d8590).keyword, base.keyword, "grey: plain text keeps the theme");
    }

    #[test]
    fn ignored_folders_are_parsed() {
        let s = AppSettings { ignored: " Node_Modules ,, vendor ".into(), ..Default::default() };
        assert_eq!(s.scan_options().ignored, ["node_modules", "vendor"]);
    }
}
