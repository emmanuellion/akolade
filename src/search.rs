//! The sidebar's "Recherche" (⌘⇧F) and "TODO" panels: the lines of the
//! project's files that match, grouped by file. TODO only reads code comments.

use std::{
    collections::HashSet,
    fs,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{
    AppContext, Context, Entity, FontWeight, PromptLevel, HighlightStyle, Hsla, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, StyledText, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder, uniform_list,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};

use crate::{
    explorer::{self, ScanOptions},
    icons,
    languages::{self, Language},
    settings::AppSettings,
    workspace::{Workspace, button, r},
};

/// Results stop here: past it, the query is too vague to read anyway.
pub const MAX_HITS: usize = 2000;
/// Bigger files are data (dumps, bundles), not code to search.
const MAX_FILE: u64 = 2 * 1024 * 1024;
/// The notes the TODO panel collects, as whole words in a comment.
pub const TODO_TAGS: [&str; 4] = ["TODO", "FIXME", "HACK", "XXX"];
/// Text kept before a match: the match shows even in a narrow panel.
const CONTEXT_BEFORE: usize = 24;

pub enum Query {
    Text { needle: String, case: bool },
    Todo,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// 0-based.
    pub line: u32,
    /// Chars before the match on its line.
    pub column: u32,
    /// Search: the line around the match. TODO: the note after its tag.
    pub text: String,
    /// Byte ranges of the matches in `text`.
    pub ranges: Vec<Range<usize>>,
    /// TODO only: "TODO", "FIXME"…
    pub tag: Option<&'static str>,
}

pub struct FileHits {
    pub rel: PathBuf,
    pub hits: Vec<Hit>,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Byte ranges of `needle` in `line`.
fn find(line: &str, needle: &str, case: bool) -> Vec<Range<usize>> {
    if case {
        return line.match_indices(needle).map(|(i, m)| i..i + m.len()).collect();
    }
    // Lowercasing keeps byte offsets only if every char keeps its length (almost always):
    // the same total is not enough ("İ" grows, "ẞ" shrinks).
    let same = line.chars().all(|c| c.to_lowercase().map(char::len_utf8).sum::<usize>() == c.len_utf8());
    let (hay, needle) = if same { (line.to_lowercase(), needle.to_lowercase()) } else { (line.to_ascii_lowercase(), needle.to_ascii_lowercase()) };
    hay.match_indices(needle.as_str()).map(|(i, m)| i..i + m.len()).collect()
}

/// Where a comment starts on this line, for the language's comment syntax;
/// a line starting with `*` continues a block comment.
fn comment_start(line: &str, lang: &Language) -> Option<usize> {
    let starts = lang.line_comments.iter().copied().chain(lang.block_comment.map(|(open, _)| open));
    let found = starts.filter_map(|t| line.find(t)).min();
    let indent = line.len() - line.trim_start().len();
    found.or_else(|| (lang.block_comment.is_some() && line.trim_start().starts_with('*')).then_some(indent))
}

/// The first TODO tag inside the line's comment: its range and name.
fn todo_tag(line: &str, lang: &Language) -> Option<(Range<usize>, &'static str)> {
    let from = comment_start(line, lang)?;
    let bytes = line.as_bytes();
    TODO_TAGS
        .iter()
        .filter_map(|tag| {
            line[from..]
                .match_indices(tag)
                .map(|(i, _)| from + i..from + i + tag.len())
                .find(|r| (r.start == 0 || !is_word_byte(bytes[r.start - 1])) && bytes.get(r.end).is_none_or(|b| !is_word_byte(*b)))
                .map(|r| (r, *tag))
        })
        .min_by_key(|(r, _)| r.start)
}

/// The matching lines of one file's text.
pub fn search_text(text: &str, query: &Query, lang: &Language, budget: usize) -> Vec<Hit> {
    let mut out = vec![];
    for (ix, line) in text.lines().enumerate() {
        if out.len() >= budget {
            break;
        }
        match query {
            Query::Text { needle, case } => {
                let ranges = find(line, needle, *case);
                let Some(first) = ranges.first() else { continue };
                let indent = line.len() - line.trim_start().len();
                // From a little before the first match, "…" when cut.
                let mut start = indent.max(line.floor_char_boundary(first.start.saturating_sub(CONTEXT_BEFORE)));
                let prefix = if start > indent { "…" } else { "" };
                if start > indent {
                    // Not in the middle of a word.
                    start = line[start..first.start].find(' ').map_or(start, |i| start + i + 1);
                }
                let end = line.ceil_char_boundary((start + 300).min(line.len()));
                let shift = |r: &Range<usize>| r.start - start + prefix.len()..r.end - start + prefix.len();
                out.push(Hit {
                    line: ix as u32,
                    column: line[..first.start].chars().count() as u32,
                    text: format!("{prefix}{}", line[start..end].trim_end()),
                    ranges: ranges.iter().filter(|r| r.start >= start && r.end <= end).map(shift).collect(),
                    tag: None,
                });
            }
            Query::Todo => {
                let Some((range, tag)) = todo_tag(line, lang) else { continue };
                let note = line[range.end..].trim_start_matches([':', ' ', '\t', '-', '(', ')']).trim_end().trim_end_matches("*/").trim_end_matches("-->").trim_end();
                out.push(Hit { line: ix as u32, column: line[..range.start].chars().count() as u32, text: note.to_string(), ranges: vec![], tag: Some(tag) });
            }
        }
    }
    out
}

/// A result line for a known place (references): the line, the symbol at
/// UTF-16 columns `start..end` highlighted.
pub fn hit_at(line: &str, line_no: u32, start: u32, end: u32) -> Hit {
    let byte = |col: u32| crate::lsp::offset_at(line, (0, col));
    let (s, e) = (byte(start), byte(end).max(byte(start)));
    let indent = (line.len() - line.trim_start().len()).min(s);
    let text = line[indent..].trim_end();
    Hit { line: line_no, column: line[..s].chars().count() as u32, text: text.to_string(), ranges: vec![s - indent..(e - indent).min(text.len())], tag: None }
}

/// Replaces every match of `needle` (as the search finds it) in `text`; the count too.
pub fn replace_all(text: &str, needle: &str, replacement: &str, case: bool) -> (String, usize) {
    if needle.is_empty() {
        return (text.to_string(), 0);
    }
    let (mut out, mut count) = (String::with_capacity(text.len()), 0);
    // Line by line, as the search reads: a match never spans two lines.
    for line in text.split_inclusive('\n') {
        let mut last = 0;
        for r in find(line, needle, case) {
            out.push_str(&line[last..r.start]);
            out.push_str(replacement);
            last = r.end;
            count += 1;
        }
        out.push_str(&line[last..]);
    }
    (out, count)
}

/// Every match in the project's files (the explorer's: hidden and ignored
/// folders left out). TODO reads only files with comments (code, not data).
/// True when cut at `MAX_HITS`. `cancel` set: stops early (a newer search replaced it).
pub fn search(root: &Path, options: &ScanOptions, query: &Query, cancel: &AtomicBool) -> (Vec<FileHits>, bool) {
    if matches!(query, Query::Text { needle, .. } if needle.is_empty()) {
        return (vec![], false);
    }
    let entries = explorer::scan(root, options);
    let mut out = vec![];
    let mut total = 0;
    for e in entries.iter().filter(|e| !e.is_dir) {
        if total >= MAX_HITS {
            return (out, true);
        }
        if cancel.load(Ordering::Relaxed) {
            return (out, false);
        }
        let lang = languages::for_path(&e.rel);
        if matches!(query, Query::Todo) && lang.line_comments.is_empty() && lang.block_comment.is_none() {
            continue;
        }
        let path = root.join(&e.rel);
        if fs::metadata(&path).map_or(true, |m| m.len() > MAX_FILE) {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        // Binary (images, archives…): a NUL byte early on, or not UTF-8.
        if bytes[..bytes.len().min(8000)].contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else { continue };
        let hits = search_text(&text, query, lang, MAX_HITS - total);
        if !hits.is_empty() {
            total += hits.len();
            out.push(FileHits { rel: e.rel.clone(), hits });
        }
    }
    (out, false)
}

fn tag_color(tag: &str, cx: &gpui::App) -> Hsla {
    let theme = cx.theme();
    match tag {
        "FIXME" => theme.red,
        "HACK" => theme.yellow,
        "XXX" => theme.magenta,
        _ => theme.primary,
    }
}

/// A line of the list: a file (index in `files`), or one of its hits.
#[derive(Clone, Copy)]
enum Row {
    File(usize),
    Hit(usize, usize),
}

pub struct SearchView {
    workspace: WeakEntity<Workspace>,
    /// The TODO panel rather than the search one.
    pub todo: bool,
    /// The query field (focused when the panel shows).
    pub input: Entity<InputState>,
    /// What replaces the matches, shown on demand.
    replacement: Entity<InputState>,
    show_replace: bool,
    case: bool,
    files: Rc<Vec<FileHits>>,
    /// Files folded (their hits hidden).
    folded: HashSet<PathBuf>,
    /// TODO: the tag shown, or all.
    tag: Option<&'static str>,
    rows: Rc<Vec<Row>>,
    truncated: bool,
    searching: bool,
    /// A search ran since the folder was set.
    searched: bool,
    has_root: bool,
    /// Results from elsewhere (references), with their title; typing searches again.
    found: Option<String>,
    _search: Option<Task<()>>,
    /// The running search's stop flag: dropping its task doesn't stop the blocking scan.
    cancel: Arc<AtomicBool>,
    _sub: Subscription,
}

impl SearchView {
    pub fn new(workspace: WeakEntity<Workspace>, todo: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Rechercher"));
        let replacement = cx.new(|cx| InputState::new(window, cx).placeholder("Remplacer par"));
        let _sub = cx.subscribe(&input, |this, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                this.refresh(cx);
            }
        });
        Self {
            workspace,
            todo,
            input,
            replacement,
            show_replace: false,
            case: false,
            files: Rc::default(),
            folded: HashSet::new(),
            tag: None,
            rows: Rc::default(),
            truncated: false,
            searching: false,
            searched: false,
            has_root: false,
            found: None,
            _search: None,
            cancel: Arc::default(),
            _sub,
        }
    }

    /// Searches again (files or query changed).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let root = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf));
        self.search_in(root, cx);
    }

    /// Shows results found elsewhere (the references of a symbol) under a title.
    pub fn show_found(&mut self, title: String, files: Vec<FileHits>, cx: &mut Context<Self>) {
        self._search = None;
        self.cancel.store(true, Ordering::Relaxed);
        (self.files, self.found, self.truncated, self.searching, self.searched, self.has_root) = (Rc::new(files), Some(title), false, false, true, true);
        self.rebuild();
        cx.notify();
    }

    /// Searches `root` (for callers inside a workspace update, where `refresh` cannot read it).
    pub fn search_in(&mut self, root: Option<PathBuf>, cx: &mut Context<Self>) {
        self.found = None;
        let query = match self.todo {
            true => Query::Todo,
            false => Query::Text { needle: self.input.read(cx).value().trim().to_string(), case: self.case },
        };
        let options = AppSettings::get(cx).scan_options();
        self.has_root = root.is_some();
        self.searching = root.is_some();
        cx.notify();
        // Typing quickly: only the last query runs (replacing the task cancels the previous one).
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::default();
        let cancel = self.cancel.clone();
        self._search = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(150)).await;
            let (files, truncated) = match root {
                Some(root) => cx.background_executor().spawn(async move { search(&root, &options, &query, &cancel) }).await,
                None => (vec![], false),
            };
            let _ = this.update(cx, |this, cx| {
                (this.files, this.truncated, this.searching, this.searched) = (Rc::new(files), truncated, false, true);
                this.rebuild();
                cx.notify();
            });
        }));
    }

    fn shown(&self, hit: &Hit) -> bool {
        self.tag.is_none_or(|t| hit.tag == Some(t))
    }

    /// The visible lines: folded files and filtered tags left out.
    fn rebuild(&mut self) {
        let mut rows = vec![];
        for (f, file) in self.files.iter().enumerate() {
            let hits: Vec<usize> = (0..file.hits.len()).filter(|&h| self.shown(&file.hits[h])).collect();
            if hits.is_empty() {
                continue;
            }
            rows.push(Row::File(f));
            if !self.folded.contains(&file.rel) {
                rows.extend(hits.into_iter().map(|h| Row::Hit(f, h)));
            }
        }
        self.rows = Rc::new(rows);
    }

    /// Replaces every listed match, after a confirmation (open tabs change like typed, ⌘Z undoes).
    fn replace_everywhere(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let needle = self.input.read(cx).value().trim().to_string();
        let replacement = self.replacement.read(cx).value().to_string();
        let total: usize = self.files.iter().map(|f| f.hits.len()).sum();
        if needle.is_empty() || total == 0 || self.found.is_some() {
            return;
        }
        let files: Vec<PathBuf> = self.files.iter().map(|f| f.rel.clone()).collect();
        let plural = |n: usize| if n > 1 { "s" } else { "" };
        let question = format!("Remplacer {total} occurrence{} de « {needle} » par « {replacement} » dans {} fichier{} ?", plural(total), files.len(), plural(files.len()));
        let answer = window.prompt(PromptLevel::Warning, &question, Some("Les fichiers ouverts changent dans leur onglet (⌘Z annule) ; les autres sont enregistrés directement."), &["Tout remplacer", "Annuler"], cx);
        let case = self.case;
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(ws) = this.workspace.upgrade() {
                    ws.update(cx, |ws, cx| ws.replace_in_files(files, &needle, &replacement, case, window, cx));
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    fn open(&self, rel: &Path, hit: &Hit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace.upgrade() else { return };
        let (line, column, rel) = (hit.line, hit.column, rel.to_path_buf());
        ws.update(cx, |ws, cx| {
            if let Some(root) = ws.root().map(Path::to_path_buf) {
                ws.open_at(root.join(rel), line, column, window, cx);
            }
        });
    }

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<gpui::Stateful<gpui::Div>> {
        let theme = cx.theme();
        let (fg, muted, hover, mark) = (theme.foreground, theme.muted_foreground, theme.foreground.opacity(0.05), theme.primary);
        let (rows, files) = (self.rows.clone(), self.files.clone());
        range
            .filter_map(|ix| rows.get(ix).map(|row| (ix, *row)))
            .map(|(ix, row)| match row {
                Row::File(f) => {
                    let file = &files[f];
                    let rel = file.rel.clone();
                    let name = rel.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let dir = rel.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                    let count = file.hits.iter().filter(|h| self.shown(h)).count();
                    let folded = self.folded.contains(&rel);
                    h_flex()
                        .id(ix)
                        .w_full()
                        .h(r(28.))
                        .px(r(6.))
                        .gap(r(6.))
                        .rounded(r(5.))
                        .cursor_pointer()
                        .hover(|d| d.bg(hover))
                        .child(Icon::new(if folded { IconName::ChevronRight } else { IconName::ChevronDown }).small().text_color(muted))
                        .child(icons::file(&rel, 16., cx))
                        .child(div().min_w_0().truncate().text_size(r(13.)).font_weight(FontWeight::MEDIUM).child(name))
                        .child(div().flex_1().min_w_0().truncate().text_size(r(11.)).text_color(muted).child(dir))
                        .child(div().flex_shrink_0().px(r(6.)).rounded_full().bg(fg.opacity(0.08)).text_size(r(10.5)).text_color(muted).child(count.to_string()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.folded.remove(&rel) {
                                this.folded.insert(rel.clone());
                            }
                            this.rebuild();
                            cx.notify();
                        }))
                }
                Row::Hit(f, h) => {
                    let (rel, hit) = (files[f].rel.clone(), files[f].hits[h].clone());
                    let row = h_flex().id(ix).w_full().h(r(28.)).pl(r(26.)).pr(r(6.)).gap(r(6.)).rounded(r(5.)).cursor_pointer().hover(|d| d.bg(hover)).text_size(r(13.));
                    let row = match hit.tag {
                        Some(tag) => {
                            let color = tag_color(tag, cx);
                            row.child(div().flex_shrink_0().px(r(5.)).rounded(r(4.)).bg(color.opacity(0.16)).text_color(color).text_size(r(9.5)).font_weight(FontWeight::BOLD).child(tag))
                                .child(div().flex_1().min_w_0().truncate().map(|d| if hit.text.is_empty() { d.text_color(muted).child("(sans précision)") } else { d.child(hit.text.clone()) }))
                        }
                        None => {
                            let style = HighlightStyle { background_color: Some(mark.opacity(0.25)), font_weight: Some(FontWeight::SEMIBOLD), ..Default::default() };
                            let text = StyledText::new(SharedString::from(hit.text.clone())).with_highlights(gpui_component::text::char_safe_highlights(&hit.text, hit.ranges.iter().map(|r| (r.clone(), style))));
                            row.child(div().flex_1().min_w_0().truncate().child(text))
                        }
                    };
                    row.child(div().flex_shrink_0().text_size(r(10.5)).text_color(muted).child((hit.line + 1).to_string()))
                        .on_click(cx.listener(move |this, _, window, cx| this.open(&rel, &hit, window, cx)))
                }
            })
            .collect()
    }
}

impl Render for SearchView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query_empty = self.input.read(cx).value().trim().is_empty();
        let total: usize = self.files.iter().map(|f| f.hits.iter().filter(|h| self.shown(h)).count()).sum();
        let nfiles = self.files.iter().filter(|f| f.hits.iter().any(|h| self.shown(h))).count();
        let plural = |n: usize| if n > 1 { "s" } else { "" };
        let summary = match () {
            _ if self.found.is_some() => format!("{} · {total} résultat{}", self.found.clone().unwrap_or_default(), plural(total)),
            _ if self.searching => "Recherche…".to_string(),
            _ if !self.todo && query_empty => String::new(),
            _ if total == 0 => String::new(),
            _ => format!("{}{total} résultat{} · {nfiles} fichier{}", if self.truncated { "+ de " } else { "" }, plural(total), plural(nfiles)),
        };
        let empty: Option<String> = match () {
            _ if self.found.is_some() => None,
            _ if !self.has_root => Some("Ouvrez un dossier pour chercher dans ses fichiers.".into()),
            _ if self.searching || !self.rows.is_empty() || !self.searched => None,
            _ if self.todo => Some("Aucun TODO, FIXME, HACK ni XXX dans les commentaires du code.".into()),
            _ if query_empty => Some("Tapez un mot, un nom de fonction… dans tous les fichiers du projet.".into()),
            _ => Some(format!("Rien ne correspond à « {} ».", self.input.read(cx).value().trim())),
        };
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let controls = if self.todo {
            // Filter by tag, with each one's count.
            let count = |t: Option<&str>| self.files.iter().flat_map(|f| &f.hits).filter(|h| t.is_none_or(|t| h.tag == Some(t))).count();
            let chips: Vec<_> = std::iter::once(None)
                .chain(TODO_TAGS.iter().copied().map(Some))
                // "Tout" counts them all: no TODO, no chip.
                .filter(|t| count(*t) > 0)
                .map(|t| {
                    let on = self.tag == t;
                    let color = t.map_or(fg, |t| tag_color(t, cx));
                    div()
                        .id(SharedString::from(format!("tag-{}", t.unwrap_or("all"))))
                        .px(r(8.))
                        .py(r(2.))
                        .rounded_full()
                        .cursor_pointer()
                        .text_size(r(11.5))
                        .border_1()
                        .map(|d| if on { d.bg(color.opacity(0.16)).border_color(color.opacity(0.4)).text_color(color) } else { d.border_color(fg.opacity(0.1)).text_color(muted).hover(|d| d.text_color(fg)) })
                        .child(format!("{} {}", t.unwrap_or("Tout"), count(t)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.tag = t;
                            this.rebuild();
                            cx.notify();
                        }))
                })
                .collect();
            h_flex().flex_wrap().gap(r(4.)).children(chips).into_any_element()
        } else {
            h_flex()
                .w_full()
                .gap(r(4.))
                .child(Input::new(&self.input).small().cleanable(true).flex_1().prefix(Icon::new(IconName::Search).small().text_color(muted)))
                .child(
                    button("case")
                        .ghost()
                        .small()
                        .icon(IconName::CaseSensitive)
                        .selected(self.case)
                        .tooltip("Respecter les majuscules")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.case = !this.case;
                            this.refresh(cx);
                        })),
                )
                .child(
                    button("toggle-replace")
                        .ghost()
                        .small()
                        .icon(IconName::Replace)
                        .selected(self.show_replace)
                        .tooltip("Remplacer")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_replace = !this.show_replace;
                            cx.notify();
                        })),
                )
                .into_any_element()
        };
        let can_replace = !self.todo && self.found.is_none() && !self.rows.is_empty();
        let replace_row = (self.show_replace && !self.todo).then(|| {
            h_flex()
                .flex_shrink_0()
                .px(r(10.))
                .pb(r(8.))
                .gap(r(4.))
                .child(Input::new(&self.replacement).small().cleanable(true).flex_1().prefix(Icon::new(IconName::Replace).small().text_color(muted)))
                .child(
                    button("replace-all")
                        .small()
                        .outline()
                        .label("Tout")
                        .tooltip("Remplacer tous les résultats")
                        .disabled(!can_replace)
                        .on_click(cx.listener(|this, _, window, cx| this.replace_everywhere(window, cx))),
                )
        });
        v_flex()
            .size_full()
            .child(v_flex().flex_shrink_0().px(r(10.)).pb(r(8.)).child(controls))
            .children(replace_row)
            .when(!summary.is_empty(), |d| d.child(div().flex_shrink_0().px(r(14.)).pb(r(4.)).text_size(r(11.5)).text_color(muted).child(summary)))
            .child(match empty {
                Some(text) => div().px(r(14.)).pt(r(8.)).text_size(r(12.5)).text_color(muted).child(text).into_any_element(),
                None => uniform_list(if self.todo { "todo-list" } else { "search-list" }, self.rows.len(), cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)))
                    .flex_1()
                    .min_h_0()
                    .px(r(6.))
                    .pb(r(8.))
                    .into_any_element(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang(file: &str) -> &'static Language {
        languages::for_path(Path::new(file))
    }

    fn text(needle: &str, case: bool) -> Query {
        Query::Text { needle: needle.into(), case }
    }

    #[test]
    fn highlights_never_cut_a_char() {
        // A run ending inside a character aborts the app (gpui), as "\u{a0}" did.
        use gpui_component::text::char_safe_highlights as safe;
        assert_eq!(safe("a\u{a0}b", [(0..2, ()), (2..3, ()), (3..9, ())]), vec![(0..3, ()), (3..4, ())]);
        assert_eq!(safe("é", [(0..1, ())]), vec![(0..2, ())]);
        assert!(safe("ab", [(5..9, ())]).is_empty());
    }

    #[test]
    fn finds_every_match_with_its_line_and_column() {
        let hits = search_text("fn main() {\n    let Main = main();\n}", &text("main", false), lang("a.rs"), 100);
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[1].line, hits[1].column, hits[1].text.as_str()), (1, 8, "let Main = main();"));
        assert_eq!(hits[1].ranges, vec![4..8, 11..15], "ranges in the text without its indentation");
        assert_eq!(search_text("let Main = main();", &text("Main", true), lang("a.rs"), 100)[0].ranges, vec![4..8]);
        assert_eq!(search_text("Élan élan", &text("élan", false), lang("a.txt"), 100)[0].ranges, vec![0..5, 6..11]);
    }

    #[test]
    fn long_lines_show_from_a_little_before_the_match() {
        let line = format!("{} the needle here", "word ".repeat(1000));
        let hit = &search_text(&line, &text("needle", true), lang("a.txt"), 100)[0];
        assert!(hit.text.starts_with('…') && hit.text.len() < 60, "{}", hit.text);
        assert_eq!(&hit.text[hit.ranges[0].clone()], "needle");
    }

    #[test]
    fn replace_all_follows_the_search_rules() {
        assert_eq!(replace_all("Total total\nTOTAL", "total", "somme", false), ("somme somme\nsomme".to_string(), 3));
        assert_eq!(replace_all("Total total\n", "total", "somme", true), ("Total somme\n".to_string(), 1));
        assert_eq!(replace_all("Élan élan", "élan", "x", false), ("x x".to_string(), 2));
        assert_eq!(replace_all("abc", "", "x", false), ("abc".to_string(), 0));
        assert_eq!(replace_all("İ x ẞ", "x", "y", false), ("İ y ẞ".to_string(), 1), "lengths that cancel out");
    }

    #[test]
    fn reference_lines_highlight_the_symbol() {
        let hit = hit_at("    total = add(a, b)  ", 4, 12, 15);
        assert_eq!((hit.text.as_str(), hit.column, hit.line), ("total = add(a, b)", 12, 4));
        assert_eq!(&hit.text[hit.ranges[0].clone()], "add");
    }

    #[test]
    fn todo_reads_only_comments_and_keeps_the_note() {
        let src = "// TODO: gérer zéro\nlet todo = \"TODO in a string\"; # x\nx = 1 # FIXME arrondi\n/* HACK - vite */\n * XXX suite\n// TODOS\n// todo minuscule";
        let hits = search_text(src, &Query::Todo, lang("a.js"), 100);
        let got: Vec<(u32, Option<&str>, &str)> = hits.iter().map(|h| (h.line, h.tag, h.text.as_str())).collect();
        assert_eq!(got, vec![(0, Some("TODO"), "gérer zéro"), (3, Some("HACK"), "vite"), (4, Some("XXX"), "suite")]);
        let py = search_text("x = 1  # FIXME arrondi\ns = 'TODO'", &Query::Todo, lang("a.py"), 100);
        assert_eq!((py.len(), py[0].tag, py[0].text.as_str(), py[0].column), (1, Some("FIXME"), "arrondi", 9));
    }

    #[test]
    fn todo_skips_data_files_and_search_skips_binaries() {
        let dir = std::env::temp_dir().join(format!("akolade-search-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.py"), "# TODO: un\nhello\n").unwrap();
        fs::write(dir.join("cache.json"), "{\"text\": \"// TODO pas un commentaire\"}").unwrap();
        fs::write(dir.join("notes.txt"), "TODO acheter du pain").unwrap();
        fs::write(dir.join("b.bin"), b"hello\0\x01").unwrap();
        let (todo, _) = search(&dir, &ScanOptions::default(), &Query::Todo, &AtomicBool::new(false));
        assert_eq!(todo.iter().map(|f| f.rel.to_string_lossy().into_owned()).collect::<Vec<_>>(), ["a.py"]);
        let (files, truncated) = search(&dir, &ScanOptions::default(), &text("hello", false), &AtomicBool::new(false));
        assert_eq!((files.len(), files[0].hits.len(), truncated), (1, 1, false));
        let (files, _) = search(&dir, &ScanOptions::default(), &text("hello", false), &AtomicBool::new(true));
        assert!(files.is_empty(), "cancelled: stops before reading");
        assert_eq!(search_text(&"a\n".repeat(10), &text("a", false), lang("a.txt"), 3).len(), 3);
        fs::remove_dir_all(dir).unwrap();
    }
}
