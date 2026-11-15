//! "Variables d'environnement": the project's `.env*` files as tables, secrets
//! masked, edited in place (untouched lines are written back byte for byte),
//! and compared with `.env.example`.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use gpui::{
    AppContext, ClipboardItem, Context, Div, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    v_flex,
};

use crate::{
    tools::{header, tool},
    ui,
    workspace::{Save, Workspace, button, r, write_file},
};

const EXAMPLE: &str = ".env.example";
const SECRET_WORDS: [&str; 9] = ["KEY", "SECRET", "TOKEN", "PASSWORD", "PASS", "PWD", "PRIVATE", "CREDENTIAL", "AUTH"];
/// Bigger files are not env files: not read.
const MAX_SIZE: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Var {
    pub key: String,
    pub value: String,
    export: bool,
    /// The quote it was written with, kept when the value changes.
    quote: Option<char>,
    /// What follows the value (an inline comment), kept when it changes.
    tail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// As in the file (without its `\n`): written back unchanged unless edited.
    raw: String,
    pub var: Option<Var>,
}

pub fn is_secret(key: &str) -> bool {
    let key = key.to_uppercase();
    SECRET_WORDS.iter().any(|w| key.contains(w))
}

pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && !key.starts_with(|c: char| c.is_ascii_digit()) && key.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// A `KEY=value` line, or `None` (comment, blank, anything else: kept as is).
pub fn parse_line(raw: &str) -> Option<Var> {
    let line = raw.trim_end_matches('\r').trim_start();
    let (export, line) = match line.strip_prefix("export ") {
        Some(rest) => (true, rest.trim_start()),
        None => (false, line),
    };
    let (key, rest) = line.split_once('=')?;
    let key = key.trim();
    if !valid_key(key) {
        return None;
    }
    let trimmed = rest.trim_start();
    if let Some(q) = trimmed.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let body = &trimmed[1..];
        let mut escaped = false;
        // In double quotes (like sh) `\"` and `\\` are escapes; others (`\n`) stay as text.
        let end = body.char_indices().find(|&(_, c)| {
            let closes = c == q && !escaped;
            escaped = q == '"' && c == '\\' && !escaped;
            closes
        });
        if let Some((i, _)) = end {
            let inner = &body[..i];
            let value = if q == '"' { unescape(inner) } else { inner.to_string() };
            return Some(Var { key: key.into(), value, export, quote: Some(q), tail: body[i + 1..].into() });
        }
        // shortcut: an unclosed (multi-line) quoted value reads as plain text on its first line; parse continuation lines if asked.
    }
    // Unquoted: a `#` after a space starts a comment.
    let cut = rest.char_indices().find(|&(i, c)| c == '#' && rest[..i].ends_with(char::is_whitespace)).map_or(rest.len(), |(i, _)| i);
    let value = rest[..cut].trim();
    let tail = if cut < rest.len() { &rest[rest[..cut].trim_end().len()..] } else { "" };
    Some(Var { key: key.into(), value: value.into(), export, quote: None, tail: tail.into() })
}

/// `\"` → `"` and `\\` → `\`; any other backslash is kept.
fn unescape(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('\\', Some(&next @ ('\\' | '"'))) => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

/// The inverse of `unescape`. A backslash before another, a quote or the end is
/// doubled (else a final `\` would escape the closing quote); others (`\n`) stay.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' if matches!(chars.peek(), None | Some('\\' | '"')) => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out
}

/// The line for a changed variable: its `export`, quote and inline comment kept.
fn render_var(v: &Var, cr: bool) -> String {
    let needs_quotes = v.value.contains(char::is_whitespace) || v.value.starts_with(['"', '\'', '#']);
    let quote = match v.quote {
        Some('\'') if v.value.contains('\'') => Some('"'),
        None if needs_quotes => Some('"'),
        q => q,
    };
    let value = match quote {
        Some('"') => format!("\"{}\"", escape(&v.value)),
        Some(q) => format!("{q}{}{q}", v.value),
        None => v.value.clone(),
    };
    format!("{}{}={value}{}{}", if v.export { "export " } else { "" }, v.key, v.tail, if cr { "\r" } else { "" })
}

/// A whole env file, line by line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Doc {
    pub lines: Vec<Line>,
}

impl Doc {
    pub fn parse(text: &str) -> Self {
        Self { lines: text.split('\n').map(|raw| Line { var: parse_line(raw), raw: raw.into() }).collect() }
    }

    pub fn text(&self) -> String {
        self.lines.iter().map(|l| l.raw.as_str()).collect::<Vec<_>>().join("\n")
    }

    pub fn vars(&self) -> impl Iterator<Item = (usize, &Var)> {
        self.lines.iter().enumerate().filter_map(|(i, l)| l.var.as_ref().map(|v| (i, v)))
    }

    pub fn get(&self, key: &str) -> Option<&Var> {
        self.vars().map(|(_, v)| v).find(|v| v.key == key)
    }

    /// Changes the value of line `ix` (only that line is rewritten).
    pub fn set(&mut self, ix: usize, value: String) {
        if let Some(line) = self.lines.get_mut(ix)
            && let Some(var) = &mut line.var
            && var.value != value
        {
            var.value = value;
            line.raw = render_var(var, line.raw.ends_with('\r'));
        }
    }

    /// A new variable at the end (before the final newline).
    pub fn add(&mut self, key: &str, value: &str) {
        let var = Var { key: key.into(), value: value.into(), export: false, quote: None, tail: String::new() };
        let cr = self.lines.first().is_some_and(|l| l.raw.ends_with('\r'));
        let line = Line { raw: render_var(&var, cr), var: Some(var) };
        let at = if self.lines.last().is_some_and(|l| l.raw.is_empty()) { self.lines.len() - 1 } else { self.lines.len() };
        self.lines.insert(at, line);
    }

    pub fn remove(&mut self, ix: usize) {
        if ix < self.lines.len() {
            self.lines.remove(ix);
        }
    }
}

/// Keys of `example` missing from `doc` (with the example's value), and keys of
/// `doc` that `example` does not have.
pub fn compare(doc: &Doc, example: &Doc) -> (Vec<(String, String)>, Vec<String>) {
    let mut missing: Vec<(String, String)> = vec![];
    for (_, v) in example.vars() {
        if doc.get(&v.key).is_none() && !missing.iter().any(|(k, _)| *k == v.key) {
            missing.push((v.key.clone(), v.value.clone()));
        }
    }
    let mut extra: Vec<String> = vec![];
    for (_, v) in doc.vars() {
        if example.get(&v.key).is_none() && !extra.contains(&v.key) {
            extra.push(v.key.clone());
        }
    }
    (missing, extra)
}

/// The `.env` and `.env.*` files at the root, with their text (blocking).
pub fn load(root: &Path) -> Vec<(String, Result<String, String>)> {
    let mut names: Vec<String> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n == ".env" || n.starts_with(".env."))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let path = root.join(&name);
            let text = match fs::metadata(&path) {
                Ok(m) if m.len() > MAX_SIZE => Err("Fichier trop gros pour un fichier .env (plus de 1 Mo).".into()),
                _ => fs::read_to_string(&path).map_err(|e| format!("Lecture impossible : {e}")),
            };
            (name, text)
        })
        .collect()
}

/// Edited elsewhere (or gone) since the tab read it: writing would drop those edits.
fn changed_on_disk(path: &Path, loaded: &str) -> bool {
    fs::read_to_string(path).ok().as_deref() != Some(loaded)
}

pub struct EnvView {
    workspace: WeakEntity<Workspace>,
    root: Option<PathBuf>,
    /// The files as on disk.
    files: Vec<(String, Result<String, String>)>,
    loaded: bool,
    selected: Option<String>,
    /// The selected file, with its edits not saved yet.
    doc: Doc,
    /// Lines whose secret value is shown.
    revealed: HashSet<usize>,
    editing: Option<(usize, Entity<InputState>)>,
    new_key: Entity<InputState>,
    new_value: Entity<InputState>,
    _edit_sub: Option<Subscription>,
    _subs: Vec<Subscription>,
    _load: Task<()>,
}

impl EnvView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let new_key = cx.new(|cx| InputState::new(window, cx).placeholder("NOUVELLE_VARIABLE"));
        let new_value = cx.new(|cx| InputState::new(window, cx).placeholder("valeur"));
        let add_on_enter = |this: &mut Self, _: &Entity<InputState>, ev: &InputEvent, window: &mut Window, cx: &mut Context<Self>| {
            if let InputEvent::PressEnter { .. } = ev {
                this.add(window, cx);
            }
        };
        let _subs = vec![cx.subscribe_in(&new_key, window, add_on_enter), cx.subscribe_in(&new_value, window, add_on_enter)];
        let mut view = Self {
            workspace,
            root: None,
            files: vec![],
            loaded: false,
            selected: None,
            doc: Doc::default(),
            revealed: HashSet::new(),
            editing: None,
            new_key,
            new_value,
            _edit_sub: None,
            _subs,
            _load: Task::ready(()),
        };
        view.reload(root, cx);
        view
    }

    /// Reads the files again (edits not saved are dropped).
    fn reload(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.root = root.clone();
        self._load = cx.spawn(async move |this, cx| {
            let files = match root {
                Some(root) => cx.background_executor().spawn(async move { load(&root) }).await,
                None => vec![],
            };
            let _ = this.update(cx, |this, cx| {
                this.files = files;
                this.loaded = true;
                let keep = this.selected.clone().filter(|s| this.files.iter().any(|(n, _)| n == s));
                this.select(keep.or_else(|| this.files.first().map(|(n, _)| n.clone())));
                cx.notify();
            });
        });
    }

    fn select(&mut self, name: Option<String>) {
        self.doc = Doc::parse(name.as_deref().and_then(|n| self.disk(n)).and_then(|t| t.as_ref().ok()).map_or("", String::as_str));
        self.selected = name;
        self.revealed.clear();
        (self.editing, self._edit_sub) = (None, None);
    }

    fn disk(&self, name: &str) -> Option<&Result<String, String>> {
        self.files.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }

    fn modified(&self) -> bool {
        self.selected.as_deref().and_then(|n| self.disk(n)).and_then(|t| t.as_ref().ok()).is_some_and(|t| *t != self.doc.text())
    }

    /// The value being typed goes into the file (not saved yet).
    fn commit(&mut self, cx: &mut Context<Self>) {
        if let Some((ix, input)) = self.editing.take() {
            let value = input.read(cx).value().to_string();
            self.doc.set(ix, value);
            cx.notify();
        }
    }

    fn edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(cx);
        let Some(var) = self.doc.lines.get(ix).and_then(|l| l.var.clone()) else { return };
        let masked = is_secret(&var.key) && !self.revealed.contains(&ix);
        let input = cx.new(|cx| InputState::new(window, cx).masked(masked).default_value(var.value));
        input.update(cx, |i, cx| i.focus(window, cx));
        self._edit_sub = Some(cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit(cx);
            }
        }));
        self.editing = Some((ix, input));
        cx.notify();
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.new_key.read(cx).value().trim().to_string();
        let value = self.new_value.read(cx).value().to_string();
        if !valid_key(&key) {
            return window.push_notification("Nom de variable invalide : lettres, chiffres et _ (sans espace, pas de chiffre au début).", cx);
        }
        if self.doc.get(&key).is_some() {
            return window.push_notification(format!("{key} existe déjà dans ce fichier."), cx);
        }
        self.commit(cx);
        self.doc.add(&key, &value);
        self.new_key.update(cx, |i, cx| i.set_value("", window, cx));
        self.new_value.update(cx, |i, cx| i.set_value("", window, cx));
        self.new_key.update(cx, |i, cx| i.focus(window, cx));
        cx.notify();
    }

    fn delete(&mut self, ix: usize, key: String, window: &mut Window, cx: &mut Context<Self>) {
        let file = self.selected.clone().unwrap_or_default();
        let answer = window.prompt(PromptLevel::Warning, &format!("Supprimer {key} ?"), Some(&format!("Elle est retirée de {file} quand vous enregistrez.")), &["Supprimer", "Annuler"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.commit(cx);
                if this.doc.lines.get(ix).and_then(|l| l.var.as_ref()).is_some_and(|v| v.key == key) {
                    this.doc.remove(ix);
                    this.revealed.clear();
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// ⌘S: writes the selected file.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit(cx);
        let (Some(root), Some(name)) = (self.root.clone(), self.selected.clone()) else { return };
        let Some(Ok(loaded)) = self.disk(&name) else {
            return window.push_notification(Notification::error(format!("{name} n'a pas pu être lu : Akolade ne l'écrase pas.")), cx);
        };
        if changed_on_disk(&root.join(&name), loaded) {
            return window.push_notification(Notification::error(format!("{name} a changé sur le disque : cliquez sur Actualiser (↻) avant d'enregistrer.")), cx);
        }
        let text = self.doc.text();
        match write_file(&root.join(&name), &text) {
            Ok(()) => {
                if let Some((_, t)) = self.files.iter_mut().find(|(n, _)| *n == name) {
                    *t = Ok(text);
                }
                window.push_notification(Notification::success(format!("{name} enregistré.")), cx);
            }
            Err(e) => window.push_notification(Notification::error(format!("Enregistrement impossible : {e}")), cx),
        }
        cx.notify();
    }

    /// Runs `then` now, or once the user agrees to drop the edits not saved.
    fn discard_then(&mut self, window: &mut Window, cx: &mut Context<Self>, then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static) {
        self.commit(cx);
        if !self.modified() {
            return then(self, cx);
        }
        let file = self.selected.clone().unwrap_or_default();
        let answer = window.prompt(PromptLevel::Warning, "Abandonner les modifications ?", Some(&format!("{file} a des changements pas enregistrés.")), &["Abandonner", "Annuler"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| then(this, cx));
            }
        })
        .detach();
    }

    fn render_files(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        v_flex()
            .w(r(220.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(border)
            .bg(fg.opacity(0.015))
            .child(h_flex().flex_shrink_0().h(r(40.)).px(r(16.)).text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(format!("FICHIERS · {}", self.files.len())))
            .child(
                v_flex().id("env-files").flex_1().min_h_0().overflow_y_scroll().px(r(6.)).gap(r(1.)).children(self.files.iter().map(|(name, _)| {
                    let on = self.selected.as_ref() == Some(name);
                    let pick = name.clone();
                    h_flex()
                        .id(SharedString::from(format!("env-file-{name}")))
                        .flex_shrink_0()
                        .h(r(32.))
                        .px(r(10.))
                        .rounded(r(6.))
                        .cursor_pointer()
                        .font_family(theme.mono_font_family.clone())
                        .text_size(r(12.5))
                        .when(on, |d| d.bg(theme.primary.opacity(0.14)))
                        .when(!on, |d| d.hover(|d| d.bg(fg.opacity(0.05))))
                        .child(div().min_w_0().truncate().child(name.clone()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let pick = pick.clone();
                            this.discard_then(window, cx, move |this, cx| {
                                this.select(Some(pick));
                                cx.notify();
                            });
                        }))
                })),
            )
    }

    fn render_row(&self, ix: usize, var: &Var, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let secret = is_secret(&var.key);
        let shown = !secret || self.revealed.contains(&ix);
        let btn = |id: &str| button(SharedString::from(format!("{id}-{ix}"))).ghost().xsmall();
        let value = match &self.editing {
            Some((i, input)) if *i == ix => h_flex().flex_1().min_w_0().child(Input::new(input).small().flex_1()),
            _ => div().flex_1().min_w_0().child(
                div()
                    .id(SharedString::from(format!("value-{ix}")))
                    .px(r(6.))
                    .py(r(3.))
                    .rounded(r(4.))
                    .truncate()
                    .cursor_pointer()
                    .hover(|d| d.bg(fg.opacity(0.05)))
                    .map(|d| match () {
                        _ if var.value.is_empty() => d.text_color(muted).child("(vide)"),
                        _ if shown => d.child(var.value.clone()),
                        _ => d.text_color(muted).child("••••••"),
                    })
                    .on_click(cx.listener(move |this, _, window, cx| this.edit(ix, window, cx))),
            ),
        };
        let copied = var.value.clone();
        let key = var.key.clone();
        h_flex()
            .flex_shrink_0()
            .min_h(r(36.))
            .px(r(8.))
            .gap(r(12.))
            .border_b_1()
            .border_color(border.opacity(0.6))
            .font_family(theme.mono_font_family.clone())
            .text_size(r(12.5))
            .child(div().w(r(240.)).flex_shrink_0().truncate().font_weight(FontWeight::MEDIUM).child(var.key.clone()))
            .child(value)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap(r(2.))
                    .when(secret, |d| {
                        d.child(btn("reveal").icon(if shown { IconName::EyeOff } else { IconName::Eye }).tooltip(if shown { "Masquer" } else { "Afficher" }).on_click(cx.listener(move |this, _, _, cx| {
                            if !this.revealed.remove(&ix) {
                                this.revealed.insert(ix);
                            }
                            cx.notify();
                        })))
                    })
                    .child(btn("copy").icon(IconName::Copy).tooltip("Copier la valeur").on_click(move |_, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                        window.push_notification("Valeur copiée.", cx);
                    }))
                    .child(btn("edit").icon(Icon::empty().path("icons/pencil.svg")).tooltip("Modifier").on_click(cx.listener(move |this, _, window, cx| this.edit(ix, window, cx))))
                    .child(btn("delete").icon(IconName::Delete).tooltip("Supprimer").on_click(cx.listener(move |this, _, window, cx| this.delete(ix, key.clone(), window, cx)))),
            )
    }

    /// Missing and extra keys against `.env.example`.
    fn render_compare(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.selected.as_deref() == Some(EXAMPLE) {
            return None;
        }
        let example = Doc::parse(self.disk(EXAMPLE)?.as_ref().ok()?);
        let (missing, extra) = compare(&self.doc, &example);
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let (yellow, blue) = (theme.yellow, theme.blue);
        let title = |text: String| div().text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(text);
        let chip = |color: gpui::Hsla| h_flex().flex_shrink_0().gap(r(6.)).pl(r(10.)).pr(r(4.)).py(r(2.)).rounded_full().bg(color.opacity(0.12)).font_family(theme.mono_font_family.clone()).text_size(r(12.));
        let mut section = v_flex().flex_shrink_0().gap(r(10.)).pt(r(18.)).child(div().font_weight(FontWeight::SEMIBOLD).child(format!("Comparé à {EXAMPLE}")));
        if missing.is_empty() && extra.is_empty() {
            return Some(section.child(div().text_size(r(12.5)).text_color(muted).child(format!("Mêmes variables que {EXAMPLE}."))));
        }
        if !missing.is_empty() {
            let chips: Vec<_> = missing
                .into_iter()
                .enumerate()
                .map(|(i, (key, value))| {
                    let label = key.clone();
                    chip(yellow).child(div().text_color(fg).child(label)).child(
                        button(SharedString::from(format!("add-missing-{i}"))).ghost().xsmall().label("Ajouter").tooltip(format!("Avec la valeur de {EXAMPLE}")).on_click(cx.listener(move |this, _, _, cx| {
                            this.commit(cx);
                            if this.doc.get(&key).is_none() {
                                this.doc.add(&key, &value);
                            }
                            cx.notify();
                        })),
                    )
                })
                .collect();
            section = section.child(title(format!("MANQUANTES · {}", chips.len()))).child(h_flex().flex_wrap().gap(r(6.)).children(chips));
        }
        if !extra.is_empty() {
            let count = extra.len();
            section = section
                .child(title(format!("EN TROP · {count}")))
                .child(h_flex().flex_wrap().gap(r(6.)).children(extra.into_iter().map(|key| chip(blue).pr(r(10.)).py(r(4.)).text_color(fg).child(key))));
        }
        Some(section)
    }
}

impl Render for EnvView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Another folder opened: its own files, once the edits of the old one are saved or dropped.
        let root = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf));
        let left_behind = root != self.root && self.modified();
        if root != self.root && !left_behind {
            self.reload(root, cx);
        }
        let refresh = button("env-refresh")
            .ghost()
            .small()
            .icon(Icon::empty().path("icons/refresh.svg"))
            .tooltip("Actualiser")
            .on_click(cx.listener(|this, _, window, cx| {
                this.discard_then(window, cx, |this, cx| {
                    let root = this.root.clone();
                    this.reload(root, cx);
                })
            }))
            .into_any_element();
        let header = header(tool("env"), "Les fichiers .env du projet : lisez, modifiez, comparez avec .env.example.", vec![refresh], cx);
        let empty = |title: &str, text: &str| ui::empty_state((tool("env").icon)(), cx.theme().yellow, title, text, cx).into_any_element();
        let body = match () {
            _ if self.root.is_none() => empty("Aucun dossier ouvert", "Ouvrez un dossier pour voir ses fichiers .env."),
            _ if !self.loaded => ui::loading("Lecture des fichiers .env…", cx).into_any_element(),
            _ if self.files.is_empty() => empty("Aucun fichier .env", "Aucun fichier .env ni .env.* à la racine du projet."),
            _ => {
                let files = self.render_files(cx);
                let main = self.render_main(cx);
                h_flex().flex_1().min_h_0().child(files).child(main).into_any_element()
            }
        };
        let notice = left_behind.then(|| {
            let theme = cx.theme();
            let old = self.root.as_deref().and_then(Path::file_name).map_or(String::new(), |n| n.to_string_lossy().into_owned());
            h_flex()
                .flex_shrink_0()
                .px(r(24.))
                .py(r(10.))
                .gap(r(10.))
                .bg(theme.yellow.opacity(0.12))
                .child(div().flex_1().min_w_0().child(format!("Modifications non enregistrées de {old} : enregistrez ou abandonnez.")))
                // Saving writes the old folder's file (`self.root` is still the old one).
                .child(button("env-keep").primary().small().label("Enregistrer").on_click(cx.listener(|this, _, window, cx| this.save(window, cx))))
                .child(button("env-drop").outline().small().label("Abandonner").on_click(cx.listener(|this, _, _, cx| {
                    let selected = this.selected.clone();
                    this.select(selected);
                    cx.notify();
                })))
        });
        v_flex().size_full().on_action(cx.listener(|this, _: &Save, window, cx| this.save(window, cx))).child(header).children(notice).child(body)
    }
}

impl EnvView {
    fn render_main(&self, cx: &mut Context<Self>) -> Div {
        let name = self.selected.clone().unwrap_or_default();
        let modified = self.modified();
        let error = match self.disk(&name) {
            Some(Err(e)) => Some(e.clone()),
            _ => None,
        };
        let rows: Vec<Div> = self.doc.vars().map(|(ix, v)| self.render_row(ix, v, cx)).collect();
        let compare = self.render_compare(cx);
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let toolbar = h_flex()
            .flex_shrink_0()
            .px(r(24.))
            .py(r(12.))
            .gap(r(10.))
            .border_b_1()
            .border_color(border)
            .child(div().flex_1().min_w_0().truncate().font_family(theme.mono_font_family.clone()).font_weight(FontWeight::SEMIBOLD).child(name))
            .when(modified, |d| d.child(div().text_size(r(12.)).text_color(theme.yellow).child("● modifié")))
            .child(
                button("env-save")
                    .outline()
                    .small()
                    .icon(Icon::empty().path("icons/save.svg"))
                    .label("Enregistrer")
                    .tooltip("Écrire le fichier (⌘S)")
                    .disabled(!modified || error.is_some())
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
            );
        let content = if let Some(e) = error {
            ui::error_state("Fichier illisible", &e, cx).into_any_element()
        } else {
            let head = h_flex()
                .flex_shrink_0()
                .px(r(8.))
                .pb(r(6.))
                .gap(r(12.))
                .border_b_1()
                .border_color(border)
                .text_size(r(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(muted)
                .child(div().w(r(240.)).flex_shrink_0().child("CLÉ"))
                .child(div().flex_1().child("VALEUR"));
            let add = h_flex()
                .flex_shrink_0()
                .pt(r(12.))
                .gap(r(8.))
                .child(div().w(r(240.)).flex_shrink_0().child(Input::new(&self.new_key).small()))
                .child(Input::new(&self.new_value).small().flex_1())
                .child(button("env-add").outline().small().icon(IconName::Plus).label("Ajouter").on_click(cx.listener(|this, _, window, cx| this.add(window, cx))));
            v_flex()
                .id("env-vars")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(r(24.))
                .py(r(16.))
                .child(head)
                .when(rows.is_empty(), |d| d.child(div().flex_shrink_0().px(r(8.)).py(r(10.)).text_color(muted).child("Aucune variable dans ce fichier.")))
                .children(rows)
                .child(add)
                .children(compare)
                .into_any_element()
        };
        v_flex().flex_1().min_w_0().h_full().child(toolbar).child(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Base de données\nDATABASE_URL=postgres://localhost/app\nexport API_KEY=\"abc 123\" # clé\n\nGREETING='salut \"toi\"'\nMULTI=\"a\\nb\"\nPORT=3000 # le port\nEMPTY=\nURL=http://x/#anchor\nnot a variable\n  INDENTED = value  \r\n";

    #[test]
    fn unchanged_files_round_trip_exactly() {
        for text in [SAMPLE, "", "A=1", "\n\n", "A=1\r\nB=2\r\n"] {
            assert_eq!(Doc::parse(text).text(), text);
        }
    }

    #[test]
    fn lines_parse_like_dotenv() {
        let doc = Doc::parse(SAMPLE);
        let vars: Vec<(&str, &str)> = doc.vars().map(|(_, v)| (v.key.as_str(), v.value.as_str())).collect();
        assert_eq!(
            vars,
            [
                ("DATABASE_URL", "postgres://localhost/app"),
                ("API_KEY", "abc 123"),
                ("GREETING", "salut \"toi\""),
                ("MULTI", "a\\nb"),
                ("PORT", "3000"),
                ("EMPTY", ""),
                ("URL", "http://x/#anchor"),
                ("INDENTED", "value"),
            ]
        );
        assert_eq!(parse_line("A=\"say \\\"hi\\\"\" # c").map(|v| v.value), Some("say \"hi\"".into()));
        assert_eq!(parse_line("# A=1"), None);
        assert_eq!(parse_line("1A=1"), None);
    }

    #[test]
    fn editing_one_value_keeps_everything_else() {
        let mut doc = Doc::parse(SAMPLE);
        let ix = |doc: &Doc, k: &str| doc.vars().find(|(_, v)| v.key == k).unwrap().0;
        let api = ix(&doc, "API_KEY");
        doc.set(api, "new value".into());
        let port = ix(&doc, "PORT");
        doc.set(port, "8080".into());
        let indented = ix(&doc, "INDENTED");
        doc.set(indented, "v2".into());
        let expected = SAMPLE.replace("export API_KEY=\"abc 123\" # clé", "export API_KEY=\"new value\" # clé").replace("PORT=3000 # le port", "PORT=8080 # le port").replace("  INDENTED = value  \r", "INDENTED=v2\r");
        assert_eq!(doc.text(), expected);
        assert_eq!(Doc::parse(&doc.text()).get("API_KEY").unwrap().value, "new value");

        let mut doc = Doc::parse("A=1\n");
        doc.add("B", "two words");
        doc.add("C", "");
        let a = ix(&doc, "A");
        doc.remove(a);
        assert_eq!(doc.text(), "B=\"two words\"\nC=\n");
        let mut doc = Doc::parse("A=1");
        doc.add("B", "2");
        assert_eq!(doc.text(), "A=1\nB=2");
    }

    #[test]
    fn backslashes_in_quoted_values_round_trip() {
        for value in ["C:\\dir\\", "a\\\\", "say \\\"hi\\\"", "a\\nb", "\\"] {
            let mut doc = Doc::parse("A=\"x\"\n");
            doc.set(0, value.into());
            assert_eq!(Doc::parse(&doc.text()).get("A").unwrap().value, value, "{}", doc.text());
        }
        let mut doc = Doc::parse("A=\"x\"");
        doc.set(0, "C:\\dir\\".into());
        assert_eq!(doc.text(), "A=\"C:\\dir\\\\\"");
        doc.set(0, "a\\nb".into());
        assert_eq!(doc.text(), "A=\"a\\nb\"", "other escapes stay as typed");
    }

    #[test]
    fn secret_keys_are_detected() {
        for key in ["API_KEY", "stripe_secret", "GITHUB_TOKEN", "DB_PASSWORD", "SMTP_PASS", "MYSQL_PWD", "PRIVATE_URL", "GOOGLE_CREDENTIALS", "NEXTAUTH_URL"] {
            assert!(is_secret(key), "{key}");
        }
        for key in ["DATABASE_HOST", "PORT", "NODE_ENV", "PUBLIC_URL"] {
            assert!(!is_secret(key), "{key}");
        }
    }

    #[test]
    fn missing_and_extra_against_the_example() {
        let example = Doc::parse("# exemple\nDATABASE_URL=postgres://localhost/app\nAPI_KEY=\nPORT=3000\n");
        let env = Doc::parse("DATABASE_URL=postgres://prod/app\nDEBUG=1\nDEBUG=2\n");
        let (missing, extra) = compare(&env, &example);
        assert_eq!(missing, [("API_KEY".to_string(), String::new()), ("PORT".to_string(), "3000".to_string())]);
        assert_eq!(extra, ["DEBUG".to_string()]);
    }

    #[test]
    fn only_env_files_at_the_root_are_listed() {
        let root = std::env::temp_dir().join(format!("akolade-env-files-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".env.d")).unwrap();
        for name in [".env", ".env.local", ".env.example", ".envrc", "env", "..env.akolade-tmp"] {
            fs::write(root.join(name), "A=1\n").unwrap();
        }
        let names: Vec<String> = load(&root).into_iter().map(|(n, t)| (n, t.unwrap()).0).collect();
        assert_eq!(names, [".env", ".env.example", ".env.local"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edits_made_elsewhere_block_the_save() {
        let path = std::env::temp_dir().join(format!("akolade-env-changed-{}", std::process::id()));
        fs::write(&path, "A=1\n").unwrap();
        assert!(!changed_on_disk(&path, "A=1\n"));
        fs::write(&path, "A=2\n").unwrap();
        assert!(changed_on_disk(&path, "A=1\n"));
        fs::remove_file(&path).unwrap();
        assert!(changed_on_disk(&path, "A=1\n"));
    }
}
