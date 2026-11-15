//! "Statistiques": the project's size at a glance: lines of code, comments
//! and blanks per language, biggest files, and from git the most changed
//! files and the contributors. Computed off the UI thread when first shown.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use gpui::{
    Context, Div, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div, relative,
    rgb,
};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, h_flex, v_flex};

use crate::{
    explorer::{self, ScanOptions},
    git, icons,
    languages::{self, Language},
    settings::AppSettings,
    tools::{header, tool},
    ui,
    workspace::{Workspace, button, r},
};

/// Bigger files are data (dumps, bundles), not code: as in the search.
const MAX_FILE: u64 = 2 * 1024 * 1024;
/// Commits read for "most changed": enough to show where the work goes.
const MAX_COMMITS: usize = 2000;
const TOP: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Lines {
    pub code: usize,
    pub comment: usize,
    pub blank: usize,
}

impl Lines {
    fn total(&self) -> usize {
        self.code + self.comment + self.blank
    }

    fn add(&mut self, o: Lines) {
        (self.code, self.comment, self.blank) = (self.code + o.code, self.comment + o.comment, self.blank + o.blank);
    }
}

/// Heuristic, line by line: blank when only spaces; comment when it starts with
/// a line comment, or inside a block comment that a line starts (a block opened
/// after code on the same line is ignored: `"src/**/*.rs"` must not open one);
/// else code. Comment markers inside strings are not lexed.
pub fn classify(text: &str, lang: &Language) -> Lines {
    let mut out = Lines::default();
    let mut close: Option<&str> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(end) = close {
            out.comment += 1;
            if t.contains(end) {
                close = None;
            }
        } else if t.is_empty() {
            out.blank += 1;
        } else if let Some((open, end)) = lang.block_comment.filter(|(open, _)| t.starts_with(open)) {
            // Checked before line comments: Lua's `--[[` starts with `--`.
            out.comment += 1;
            if !t[open.len()..].contains(end) {
                close = Some(end);
            }
        } else if lang.line_comments.iter().any(|c| t.starts_with(c)) {
            out.comment += 1;
        } else {
            out.code += 1;
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileStat {
    pub rel: PathBuf,
    pub lang: &'static str,
    pub color: u32,
    pub lines: Lines,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LangStat {
    pub name: &'static str,
    pub color: u32,
    pub files: usize,
    pub lines: Lines,
}

/// Per language, most code lines first.
pub fn by_language(files: &[FileStat]) -> Vec<LangStat> {
    let mut map: HashMap<&str, LangStat> = HashMap::new();
    for f in files {
        let l = map.entry(f.lang).or_insert(LangStat { name: f.lang, color: f.color, files: 0, lines: Lines::default() });
        l.files += 1;
        l.lines.add(f.lines);
    }
    let mut out: Vec<LangStat> = map.into_values().collect();
    out.sort_by(|a, b| b.lines.code.cmp(&a.lines.code).then(b.lines.total().cmp(&a.lines.total())).then(a.name.cmp(b.name)));
    out
}

/// `git log --name-only --format=`: one path per line per commit (blank lines
/// between commits) → paths by number of commits, most first.
pub fn parse_changes(log: &str) -> Vec<(String, usize)> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for path in log.lines().map(str::trim).filter(|l| !l.is_empty()) {
        *counts.entry(path).or_default() += 1;
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().map(|(p, n)| (p.to_string(), n)).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// `git shortlog -sn`: "   42\tAda Lovelace" → (name, commits).
pub fn parse_shortlog(out: &str) -> Vec<(String, usize)> {
    out.lines().filter_map(|l| l.trim().split_once('\t')).filter_map(|(n, name)| Some((name.trim().to_string(), n.trim().parse().ok()?))).collect()
}

pub struct GitStats {
    pub changed: Vec<(PathBuf, usize)>,
    /// Name and commits, most first.
    pub contributors: Vec<(String, usize)>,
}

pub struct Report {
    pub files: usize,
    pub bytes: u64,
    pub lines: Lines,
    pub languages: Vec<LangStat>,
    pub biggest: Vec<(PathBuf, usize)>,
    /// Binaries and huge files: counted in files and size, not read.
    pub skipped: usize,
    /// Lock files, left out of the line counts.
    pub generated: usize,
    /// The scan stopped at `explorer::MAX_ENTRIES`.
    pub truncated: bool,
    pub git: Option<GitStats>,
}

fn git_stats(root: &Path) -> Option<GitStats> {
    git::toplevel(root)?;
    // `--relative` and `.`: paths and commits of this folder, even inside a bigger repository.
    let max = format!("--max-count={MAX_COMMITS}");
    let log = git::git(root, &["-c", "core.quotepath=off", "log", &max, "--no-merges", "--relative", "--name-only", "--format=", "--", "."]).ok()?;
    let changed = parse_changes(&log).into_iter().map(|(p, n)| (PathBuf::from(p), n)).filter(|(p, _)| root.join(p).is_file()).take(TOP).collect();
    let contributors = git::git(root, &["shortlog", "-sn", "--no-merges", "HEAD", "--", "."]).map(|o| parse_shortlog(&o)).unwrap_or_default();
    Some(GitStats { changed, contributors })
}

/// Reads the project's files (the explorer's: hidden and ignored folders left out).
fn compute(root: &Path, options: &ScanOptions) -> Report {
    let entries = explorer::scan(root, options);
    let truncated = entries.len() >= explorer::MAX_ENTRIES;
    let (mut files, mut bytes, mut skipped, mut generated, mut stats) = (0, 0, 0, 0, vec![]);
    for e in entries.iter().filter(|e| !e.is_dir) {
        let path = root.join(&e.rel);
        let Ok(meta) = fs::metadata(&path) else { continue };
        files += 1;
        bytes += meta.len();
        // Lock files (Cargo.lock, package-lock.json…) are generated: thousands of lines
        // that would count as the project's "code".
        if crate::icons::kind(&e.rel) == "lock" {
            generated += 1;
            continue;
        }
        // Binary (images, archives…): a NUL byte early on, or not UTF-8. As in the search.
        let text = (meta.len() <= MAX_FILE).then(|| fs::read(&path).ok()).flatten().filter(|b| !b[..b.len().min(8000)].contains(&0)).and_then(|b| String::from_utf8(b).ok());
        let Some(text) = text else {
            skipped += 1;
            continue;
        };
        let lang = languages::for_path(&e.rel);
        stats.push(FileStat { rel: e.rel.clone(), lang: lang.name, color: lang.color, lines: classify(&text, lang) });
    }
    let mut lines = Lines::default();
    stats.iter().for_each(|f| lines.add(f.lines));
    let languages = by_language(&stats);
    stats.sort_by(|a, b| b.lines.total().cmp(&a.lines.total()));
    let biggest = stats.into_iter().take(TOP).map(|f| (f.rel, f.lines.total())).collect();
    Report { files, bytes, lines, languages, biggest, skipped, generated, truncated, git: git_stats(root) }
}

/// "12 345": French thousands.
fn num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
}

fn size_fr(bytes: u64) -> String {
    let units = ["o", "Ko", "Mo", "Go", "To"];
    let (mut v, mut u) = (bytes as f64, 0);
    while v >= 1024. && u < units.len() - 1 {
        (v, u) = (v / 1024., u + 1);
    }
    if u == 0 { format!("{bytes} o") } else { format!("{v:.1} {}", units[u]).replace('.', ",") }
}

fn plural(n: usize, word: &str) -> String {
    format!("{} {word}{}", num(n), if n > 1 { "s" } else { "" })
}

pub struct StatsView {
    workspace: WeakEntity<Workspace>,
    root: Option<PathBuf>,
    report: Option<Report>,
    loading: bool,
    _scan: Task<()>,
}

impl StatsView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>) -> Self {
        Self { workspace, root, report: None, loading: false, _scan: Task::ready(()) }
    }

    fn refresh(&mut self, options: ScanOptions, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        self.loading = true;
        let task = cx.background_executor().spawn(async move { compute(&root, &options) });
        self._scan = cx.spawn(async move |this, cx| {
            let report = task.await;
            let _ = this.update(cx, |this, cx| {
                (this.report, this.loading) = (Some(report), false);
                cx.notify();
            });
        });
    }

    fn open(&self, rel: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(root), Some(ws)) = (self.root.as_ref(), self.workspace.upgrade()) else { return };
        let path = root.join(rel);
        ws.update(cx, |ws, cx| ws.open_path(path, window, cx));
    }

    fn card(&self, title: &str, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .min_w(r(320.))
            .p(r(12.))
            .gap(r(2.))
            .rounded(r(10.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.foreground.opacity(0.02))
            .child(div().px(r(8.)).pb(r(6.)).child(ui::section_title(title, cx)))
    }

    fn file_rows(&self, id: &str, files: &[(PathBuf, usize)], unit: &str, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let (muted, hover) = (cx.theme().muted_foreground, ui::hover_bg(cx));
        files
            .iter()
            .enumerate()
            .map(|(i, (rel, n))| {
                let rel2 = rel.clone();
                h_flex()
                    .id(SharedString::from(format!("{id}-{i}")))
                    .h(r(30.))
                    .px(r(8.))
                    .gap(r(8.))
                    .rounded(r(6.))
                    .cursor_pointer()
                    .hover(|s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.open(&rel2, window, cx)))
                    .child(icons::file(rel, 16., cx))
                    .child(div().flex_1().min_w_0().truncate().text_size(r(13.)).child(rel.display().to_string()))
                    .child(div().flex_shrink_0().text_size(r(12.)).text_color(muted).child(plural(*n, unit)))
                    .into_any_element()
            })
            .collect()
    }

    fn render_report(&self, rep: &Report, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let tile = |label: &str, value: String| {
            v_flex()
                .flex_1()
                .min_w(r(130.))
                .px(r(16.))
                .py(r(12.))
                .gap(r(2.))
                .rounded(r(10.))
                .border_1()
                .border_color(border)
                .bg(theme.foreground.opacity(0.02))
                .child(div().text_size(r(22.)).font_weight(FontWeight::SEMIBOLD).child(value))
                .child(div().text_size(r(12.)).text_color(muted).child(label.to_string()))
        };
        let known = rep.languages.iter().filter(|l| l.name != languages::PLAIN.name).count();
        let tiles = h_flex()
            .flex_shrink_0()
            .flex_wrap()
            .gap(r(10.))
            .child(tile("Fichiers", num(rep.files)))
            .child(tile("Lignes de code", num(rep.lines.code)))
            .child(tile("Commentaires", num(rep.lines.comment)))
            .child(tile("Lignes vides", num(rep.lines.blank)))
            .child(tile("Taille", size_fr(rep.bytes)))
            .child(tile("Langages", num(known)));

        let total_code = rep.lines.code.max(1) as f32;
        let bar = h_flex()
            .h(r(10.))
            .w_full()
            .rounded_full()
            .overflow_hidden()
            .bg(theme.foreground.opacity(0.06))
            .children(rep.languages.iter().filter(|l| l.lines.code > 0).map(|l| div().h_full().w(relative(l.lines.code as f32 / total_code)).bg(rgb(l.color))));
        let cell = |text: String| div().w(r(90.)).flex_shrink_0().text_right().child(text);
        let mut langs = self.card("Langages", cx).child(div().px(r(8.)).pb(r(10.)).child(bar)).child(
            h_flex().px(r(8.)).h(r(24.)).gap(r(8.)).text_size(r(12.)).text_color(muted).child(div().flex_1().child("Langage")).child(cell("Fichiers".into())).child(cell("Lignes de code".into())).child(cell("Part".into())),
        );
        for l in &rep.languages {
            langs = langs.child(
                h_flex()
                    .h(r(30.))
                    .px(r(8.))
                    .gap(r(8.))
                    .text_size(r(13.))
                    .child(h_flex().flex_1().min_w_0().gap(r(8.)).child(div().size(r(9.)).flex_shrink_0().rounded_full().bg(rgb(l.color))).child(div().min_w_0().truncate().child(l.name)))
                    .child(cell(num(l.files)).text_color(muted))
                    .child(cell(num(l.lines.code)))
                    .child(cell(format!("{:.1} %", l.lines.code as f32 * 100. / total_code).replace('.', ",")).text_color(muted)),
            );
        }

        let biggest = self.card("Plus gros fichiers", cx).children(self.file_rows("big", &rep.biggest, "ligne", cx));
        let mut cards = h_flex().flex_shrink_0().flex_wrap().items_start().gap(r(10.)).child(biggest);
        if let Some(g) = &rep.git {
            let changed = if g.changed.is_empty() {
                vec![div().px(r(8.)).text_size(r(13.)).text_color(muted).child("Aucun commit pour l'instant.").into_any_element()]
            } else {
                self.file_rows("changed", &g.changed, "commit", cx)
            };
            let title = format!("Contributeurs · {}", g.contributors.len());
            let people = g.contributors.iter().take(TOP).map(|(name, n)| {
                h_flex().h(r(30.)).px(r(8.)).gap(r(8.)).text_size(r(13.)).child(Icon::new(IconName::User).small().text_color(muted)).child(div().flex_1().min_w_0().truncate().child(name.clone())).child(div().flex_shrink_0().text_size(r(12.)).text_color(muted).child(plural(*n, "commit")))
            });
            cards = cards.child(self.card("Plus souvent modifiés", cx).children(changed)).child(self.card(&title, cx).children(people));
        }

        let mut notes = vec![];
        if rep.truncated {
            notes.push(format!("Projet très grand : seuls les {} premiers éléments sont comptés.", num(explorer::MAX_ENTRIES)));
        }
        if rep.skipped > 0 {
            notes.push(format!("{} non lus (binaires ou de plus de 2 Mo).", plural(rep.skipped, "fichier")));
        }
        if rep.generated > 0 {
            notes.push(format!("{} de verrouillage (Cargo.lock, package-lock.json…) hors du compte des lignes.", plural(rep.generated, "fichier")));
        }
        v_flex()
            .id("stats")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p(r(20.))
            .gap(r(12.))
            .child(tiles)
            .children(notes.into_iter().map(|n| div().flex_shrink_0().text_size(r(12.)).text_color(muted).child(n)))
            .child(langs.flex_shrink_0())
            .child(cards)
            .into_any_element()
    }
}

impl Render for StatsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = tool("stats");
        // Another folder opened: its own numbers.
        let root = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf));
        if root != self.root {
            *self = Self::new(self.workspace.clone(), root);
        }
        // First shown: computed now, not polled.
        if self.root.is_some() && self.report.is_none() && !self.loading {
            self.refresh(AppSettings::get(cx).scan_options(), cx);
        }
        let mut actions = vec![];
        if self.root.is_some() {
            let refresh = button("refresh").outline().small().icon(Icon::empty().path("icons/refresh.svg")).label("Actualiser").loading(self.loading);
            actions.push(refresh.on_click(cx.listener(|this, _, _, cx| this.refresh(AppSettings::get(cx).scan_options(), cx))).into_any_element());
        }
        let subtitle = match &self.report {
            Some(rep) => format!("{} · {}", plural(rep.lines.total(), "ligne"), plural(rep.files, "fichier")),
            None => "Le projet en chiffres : langages, lignes, fichiers.".into(),
        };
        let header = header(t, subtitle, actions, cx);
        let body = match &self.report {
            _ if self.root.is_none() => ui::empty_state((t.icon)(), (t.accent)(cx.theme()), "Aucun dossier ouvert", "Ouvrez un dossier pour voir ses statistiques.", cx).into_any_element(),
            Some(rep) if rep.files == 0 => ui::empty_state((t.icon)(), (t.accent)(cx.theme()), "Aucun fichier", "Ce dossier est vide (ou tout y est masqué par les options de l'explorateur).", cx).into_any_element(),
            Some(rep) => self.render_report(rep, cx),
            None => ui::loading("Lecture des fichiers…", cx).into_any_element(),
        };
        v_flex().size_full().child(header).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang(file: &str) -> &'static Language {
        languages::for_path(Path::new(file))
    }

    fn lines(code: usize, comment: usize, blank: usize) -> Lines {
        Lines { code, comment, blank }
    }

    #[test]
    fn rust_lines_with_block_comments() {
        let text = "//! Doc\nuse std::fs;\n\n/* one\n * two\n */\nfn main() {\n    /* inline */\n    let glob = \"src/**/*.rs\"; // trailing\n}\n";
        assert_eq!(classify(text, lang("a.rs")), lines(4, 5, 1));
    }

    #[test]
    fn python_html_lua_and_plain_text() {
        assert_eq!(classify("# a\nimport os\n\n   \nprint(1)  # b\n", lang("a.py")), lines(2, 1, 2));
        assert_eq!(classify("<!-- a\n b -->\n<p>x</p>\n", lang("a.html")), lines(1, 2, 0));
        assert_eq!(classify("--[[ a\nb ]]\n-- c\nlocal x = 1\n", lang("a.lua")), lines(1, 3, 0));
        assert_eq!(classify("# Title\n\nsome text\n", lang("LICENSE")), lines(2, 0, 1));
    }

    #[test]
    fn languages_add_up_most_code_first() {
        let f = |rel: &str, code| {
            let l = lang(rel);
            FileStat { rel: rel.into(), lang: l.name, color: l.color, lines: lines(code, 1, 2) }
        };
        let langs = by_language(&[f("a.rs", 10), f("b.py", 30), f("c.rs", 25)]);
        assert_eq!(langs.iter().map(|l| (l.name, l.files, l.lines)).collect::<Vec<_>>(), vec![("Rust", 2, lines(35, 2, 4)), ("Python", 1, lines(30, 1, 2))]);
    }

    #[test]
    fn git_log_and_shortlog() {
        let log = "src/main.rs\nREADME.md\n\nsrc/main.rs\n\nsrc/ui.rs\nsrc/main.rs\n\nREADME.md\n";
        assert_eq!(parse_changes(log), vec![("src/main.rs".into(), 3), ("README.md".into(), 2), ("src/ui.rs".into(), 1)]);
        let short = "   120\tAda Lovelace\n     7\tGrace Hopper\n";
        assert_eq!(parse_shortlog(short), vec![("Ada Lovelace".into(), 120), ("Grace Hopper".into(), 7)]);
    }

    #[test]
    fn french_numbers_and_sizes() {
        assert_eq!((num(7), num(1234), num(1234567)), ("7".into(), "1\u{202f}234".into(), "1\u{202f}234\u{202f}567".into()));
        assert_eq!((size_fr(900), size_fr(1536), size_fr(3 * 1024 * 1024)), ("900 o".into(), "1,5 Ko".into(), "3,0 Mo".into()));
    }
}
