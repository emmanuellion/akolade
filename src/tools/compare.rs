//! "Comparateur": the diff of two texts, pasted or read from the project's files.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use gpui::{
    AnyElement, AppContext, Context, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Subscription, Task,
    WeakEntity, Window, div, prelude::FluentBuilder, uniform_list,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, Sizable, WindowExt,
    button::ButtonVariants,
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    popover::Popover,
    v_flex,
};

use crate::{
    git::{DiffLine, LineKind, parse_diff},
    icons, languages,
    settings::AppSettings,
    tools::{header, tool},
    ui::{self, empty_state, error_state},
    workspace::{Workspace, button, r},
};

/// Past this, a side is refused (git would cope, but the editor and the list get slow).
const MAX_LINES: usize = 50_000;
const MAX_BYTES: u64 = 8 << 20;
/// Typing pauses this long before the diff runs again.
const LIVE_DELAY: Duration = Duration::from_millis(150);
/// The picker lists this many matches at most.
const MAX_MATCHES: usize = 200;

/// The diff of two texts, every line kept (no `@@` headers).
// `git diff --no-index` rather than our own LCS: git is already required, its
// Myers diff is fast on big inputs, and `-w` gives "ignore spaces" for free.
pub fn diff(left: &str, right: &str, ignore_space: bool) -> Result<Vec<DiffLine>, String> {
    let count = |s: &str| s.lines().count();
    if count(left).max(count(right)) > MAX_LINES {
        return Err(format!("Trop long pour être comparé : {} lignes au plus de chaque côté.", MAX_LINES));
    }
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let tmp = std::env::temp_dir().join(format!("akolade-compare-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    // A missing last newline would show as a changed last line with the same text.
    let ended = |s: &str| {
        if s.is_empty() || s.ends_with('\n') { s.to_string() } else { format!("{s}\n") }
    };
    let (a, b) = (tmp.join("gauche"), tmp.join("droite"));
    let written = std::fs::create_dir_all(&tmp).and_then(|_| std::fs::write(&a, ended(left))).and_then(|_| std::fs::write(&b, ended(right)));
    let context = format!("-U{}", count(left).max(count(right)) + 1);
    let mut args = vec!["diff", "--no-index", "--no-color", "--no-ext-diff", "--text", context.as_str()];
    if ignore_space {
        args.push("-w");
    }
    let out = written
        .map_err(|e| e.to_string())
        .and_then(|_| Command::new("git").args(&args).arg("--").arg(&a).arg(&b).current_dir(&tmp).stdin(Stdio::null()).output().map_err(|e| format!("git introuvable : {e}")));
    let _ = std::fs::remove_dir_all(&tmp);
    let out = out?;
    // `--no-index` exits 1 when the texts differ.
    if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(parse_diff(&String::from_utf8_lossy(&out.stdout)).into_iter().filter(|l| l.kind != LineKind::Info).collect())
}

/// Lines added, removed, and the blocks of changes they form.
pub fn summary(lines: &[DiffLine]) -> (usize, usize, usize) {
    let count = |k| lines.iter().filter(|l| l.kind == k).count();
    let blocks = lines.iter().enumerate().filter(|(i, l)| l.kind != LineKind::Context && (*i == 0 || lines[i - 1].kind == LineKind::Context)).count();
    (count(LineKind::Added), count(LineKind::Removed), blocks)
}

/// One row of the side-by-side view: the old line on the left, the new on the right.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub left: Option<DiffLine>,
    pub right: Option<DiffLine>,
}

/// Unchanged lines on both sides; in a block of changes, the n-th removed line
/// faces the n-th added one, the longer side facing blanks.
pub fn side_by_side(lines: &[DiffLine]) -> Vec<Row> {
    let mut rows = vec![];
    let (mut removed, mut added) = (vec![], vec![]);
    let flush = |rows: &mut Vec<Row>, removed: &mut Vec<DiffLine>, added: &mut Vec<DiffLine>| {
        let n = removed.len().max(added.len());
        let (mut r, mut a) = (removed.drain(..), added.drain(..));
        rows.extend((0..n).map(|_| Row { left: r.next(), right: a.next() }));
    };
    for l in lines {
        match l.kind {
            LineKind::Removed => removed.push(l.clone()),
            LineKind::Added => added.push(l.clone()),
            _ => {
                flush(&mut rows, &mut removed, &mut added);
                rows.push(Row { left: Some(l.clone()), right: Some(l.clone()) });
            }
        }
    }
    flush(&mut rows, &mut removed, &mut added);
    rows
}

/// A file of the project as text, refused when too big or binary.
fn read_text(path: &Path) -> Result<String, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_BYTES {
        return Err("Ce fichier est trop gros pour être comparé (plus de 8 Mo).".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.contains(&0) {
        return Err("C'est un fichier binaire : seuls les textes se comparent.".into());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

enum Outcome {
    Empty,
    Same,
    Diff { lines: Rc<Vec<DiffLine>>, rows: Rc<Vec<Row>>, added: usize, removed: usize, blocks: usize },
    Failed(String),
}

struct Side {
    editor: Entity<InputState>,
    /// The project file it was filled from.
    file: Option<String>,
}

pub struct CompareView {
    root: Option<PathBuf>,
    sides: [Side; 2],
    side_by_side: bool,
    ignore_space: bool,
    outcome: Outcome,
    /// The project's files (read the first time the picker opens), and the side picking.
    files: Option<Rc<Vec<String>>>,
    picking: Option<usize>,
    query: Entity<InputState>,
    _job: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl CompareView {
    pub fn new(_workspace: WeakEntity<Workspace>, root: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| cx.new(|cx| InputState::new(window, cx).code_editor("text").placeholder(placeholder));
        let (left, right) = (editor("Collez le premier texte…", window, cx), editor("Collez le second texte…", window, cx));
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Chercher un fichier…"));
        let changed = |this: &mut Self, _: &Entity<InputState>, ev: &InputEvent, _: &mut Window, cx: &mut Context<Self>| {
            if matches!(ev, InputEvent::Change) {
                this.schedule(cx);
            }
        };
        let _subs =
            vec![cx.subscribe_in(&left, window, changed), cx.subscribe_in(&right, window, changed), cx.subscribe_in(&query, window, |_, _, _: &InputEvent, _, cx| cx.notify())];
        Self {
            root,
            sides: [Side { editor: left, file: None }, Side { editor: right, file: None }],
            side_by_side: true,
            ignore_space: false,
            outcome: Outcome::Empty,
            files: None,
            picking: None,
            query,
            _job: None,
            _subs,
        }
    }

    /// The diff runs again once typing pauses (a new keystroke cancels the wait).
    fn schedule(&mut self, cx: &mut Context<Self>) {
        self._job = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIVE_DELAY).await;
            let Ok((left, right, ignore)) = this.update(cx, |this, cx| {
                let text = |i: usize| this.sides[i].editor.read(cx).value().to_string();
                (text(0), text(1), this.ignore_space)
            }) else {
                return;
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    if left.is_empty() && right.is_empty() {
                        return Ok(None);
                    }
                    let lines = diff(&left, &right, ignore)?;
                    let rows = side_by_side(&lines);
                    Ok(Some((lines, rows)))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.outcome = match result {
                    Ok(None) => Outcome::Empty,
                    Ok(Some((lines, _))) if lines.iter().all(|l| l.kind == LineKind::Context) => Outcome::Same,
                    Ok(Some((lines, rows))) => {
                        let (added, removed, blocks) = summary(&lines);
                        Outcome::Diff { lines: Rc::new(lines), rows: Rc::new(rows), added, removed, blocks }
                    }
                    Err(e) => Outcome::Failed(e),
                };
                cx.notify();
            });
        }));
    }

    fn open_picker(&mut self, side: usize, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.picking = open.then_some(side);
        if open {
            self.query.update(cx, |q, cx| {
                q.set_value("", window, cx);
                q.focus(window, cx);
            });
            if let (None, Some(root)) = (&self.files, self.root.clone()) {
                let options = AppSettings::get(cx).scan_options();
                let task = cx.background_executor().spawn(async move {
                    crate::explorer::scan(&root, &options).into_iter().filter(|e| !e.is_dir).map(|e| e.rel.to_string_lossy().into_owned()).collect::<Vec<_>>()
                });
                cx.spawn(async move |this, cx| {
                    let files = task.await;
                    let _ = this.update(cx, |this, cx| {
                        this.files = Some(Rc::new(files));
                        cx.notify();
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    fn pick(&mut self, side: usize, rel: String, window: &mut Window, cx: &mut Context<Self>) {
        self.picking = None;
        let Some(path) = self.root.as_ref().map(|r| r.join(&rel)) else {
            return;
        };
        let task = cx.background_executor().spawn(async move { read_text(&path) });
        cx.spawn_in(window, async move |this, cx| {
            let text = task.await;
            let _ = this.update_in(cx, |this, window, cx| match text {
                Ok(text) => {
                    this.sides[side].file = Some(rel);
                    this.fill(side, text, window, cx);
                }
                Err(e) => window.push_notification(Notification::error(e), cx),
            });
        })
        .detach();
        cx.notify();
    }

    /// Puts `text` in a side's editor, colored after its file.
    fn fill(&mut self, side: usize, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.sides[side].file.as_deref().map_or("text", |f| languages::for_path(Path::new(f)).highlight);
        self.sides[side].editor.update(cx, |e, cx| {
            e.set_highlighter(lang, cx);
            e.set_value(text, window, cx);
        });
        self.schedule(cx);
    }

    fn swap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let texts = self.sides.each_ref().map(|s| s.editor.read(cx).value().to_string());
        let [left, right] = &mut self.sides;
        std::mem::swap(&mut left.file, &mut right.file);
        let [a, b] = texts;
        self.fill(0, b, window, cx);
        self.fill(1, a, window, cx);
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for side in 0..2 {
            self.sides[side].file = None;
            self.fill(side, String::new(), window, cx);
        }
    }

    fn render_picker(&self, side: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let q = self.query.read(cx).value().to_lowercase();
        let list: AnyElement = match (&self.root, &self.files) {
            (None, _) => div().p(r(8.)).text_color(muted).child("Ouvrez un dossier pour choisir un de ses fichiers.").into_any_element(),
            (_, None) => div().p(r(8.)).text_color(muted).child("Lecture des fichiers…").into_any_element(),
            (_, Some(files)) => {
                let matches: Vec<&String> = files.iter().filter(|f| f.to_lowercase().contains(&q)).take(MAX_MATCHES).collect();
                if matches.is_empty() {
                    div().p(r(8.)).text_color(muted).child("Aucun fichier ne correspond.").into_any_element()
                } else {
                    let current = self.sides[side].file.as_ref();
                    v_flex()
                        .id("compare-files")
                        .max_h(r(320.))
                        .overflow_y_scroll()
                        .children(matches.into_iter().enumerate().map(|(i, f)| {
                            let selected = current == Some(f);
                            let rel = f.clone();
                            h_flex()
                                .id(i)
                                .flex_shrink_0()
                                .h(r(30.))
                                .px(r(8.))
                                .gap(r(8.))
                                .rounded(r(6.))
                                .cursor_pointer()
                                .map(|d| if selected { d.bg(theme.primary.opacity(0.12)) } else { d.hover(|d| d.bg(ui::hover_bg(cx))) })
                                .child(icons::file(Path::new(f), 16., cx))
                                .child(div().flex_1().min_w_0().truncate().child(f.clone()))
                                .on_click(cx.listener(move |this, _, window, cx| this.pick(side, rel.clone(), window, cx)))
                        }))
                        .into_any_element()
                }
            }
        };
        v_flex().w(r(380.)).gap(r(6.)).text_size(r(13.)).child(Input::new(&self.query).small().prefix(Icon::new(IconName::Search).small().text_color(muted))).child(list)
    }

    fn render_side(&self, side: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let picker = Popover::new(("compare-pick", side))
            .open(self.picking == Some(side))
            .on_open_change(move |open, window, cx| {
                let _ = this.update(cx, |this, cx| this.open_picker(side, *open, window, cx));
            })
            .trigger(button(("pick", side)).ghost().small().icon(IconName::File).label("Choisir un fichier"))
            .child(self.render_picker(side, cx));
        let theme = cx.theme();
        let s = &self.sides[side];
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h(r(40.))
                    .pl(r(16.))
                    .pr(r(8.))
                    .gap(r(8.))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().flex_shrink_0().font_weight(FontWeight::SEMIBOLD).text_size(r(13.)).child(["Gauche", "Droite"][side]))
                    .when_some(s.file.as_ref(), |d, f| {
                        d.child(
                            h_flex()
                                .min_w_0()
                                .gap(r(6.))
                                .child(icons::file(Path::new(f), 16., cx))
                                .child(div().min_w_0().truncate().text_size(r(12.5)).text_color(theme.muted_foreground).child(f.clone())),
                        )
                    })
                    .child(div().flex_1())
                    .child(picker),
            )
            .child(div().flex_1().min_h_0().child(Input::new(&s.editor).h_full().appearance(false).font_family(theme.mono_font_family.clone()).text_size(theme.mono_font_size)))
    }

    /// "Unifié | Côte à côte" and "Ignorer les espaces", and the counts.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let mode = |id: &'static str, label: &'static str, on: bool, value: bool, cx: &mut Context<Self>| {
            h_flex()
                .id(id)
                .h(r(26.))
                .px(r(12.))
                .rounded(r(6.))
                .text_size(r(12.5))
                .map(|d| if on { d.bg(fg.opacity(0.1)).text_color(fg).font_weight(FontWeight::MEDIUM) } else { d.cursor_pointer().text_color(muted).hover(|d| d.text_color(fg)) })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.side_by_side = value;
                    cx.notify();
                }))
        };
        let stats = match &self.outcome {
            Outcome::Diff { added, removed, blocks, .. } => Some(
                h_flex()
                    .gap(r(8.))
                    .text_size(r(12.5))
                    .child(div().font_family(theme.mono_font_family.clone()).text_color(theme.green).child(format!("+{added}")))
                    .child(div().font_family(theme.mono_font_family.clone()).text_color(theme.red).child(format!("−{removed}")))
                    .child(div().text_color(muted).child(format!("· {blocks} bloc{}", if *blocks > 1 { "s" } else { "" }))),
            ),
            _ => None,
        };
        h_flex()
            .flex_shrink_0()
            .h(r(44.))
            .px(r(16.))
            .gap(r(16.))
            .border_y_1()
            .border_color(theme.border)
            .child(h_flex().p(r(2.)).gap(r(2.)).rounded(r(8.)).bg(fg.opacity(0.05)).child(mode("mode-unified", "Unifié", !self.side_by_side, false, cx)).child(mode(
                "mode-split",
                "Côte à côte",
                self.side_by_side,
                true,
                cx,
            )))
            .child(Checkbox::new("ignore-space").label("Ignorer les espaces").checked(self.ignore_space).on_click(cx.listener(|this, checked: &bool, _, cx| {
                this.ignore_space = *checked;
                this.schedule(cx);
                cx.notify();
            })))
            .child(div().flex_1())
            .children(stats)
    }

    fn render_diff(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (lines, rows) = match &self.outcome {
            Outcome::Empty => {
                return empty_state(compare_icon(), theme.primary, "Rien à comparer", "Collez deux textes ou choisissez deux fichiers : leurs différences s'affichent ici.", cx)
                    .into_any_element();
            }
            Outcome::Same => {
                let text = if self.ignore_space { "Les deux côtés sont identiques, aux espaces près." } else { "Les deux côtés sont identiques." };
                return empty_state(Icon::new(IconName::Check), theme.green, "Aucune différence", text, cx).into_any_element();
            }
            Outcome::Failed(e) => return error_state("Comparaison impossible", e, cx).into_any_element(),
            Outcome::Diff { lines, rows, .. } => (lines.clone(), rows.clone()),
        };
        let (font, size) = (theme.mono_font_family.clone(), theme.mono_font_size);
        let colors = Colors { added: theme.green, removed: theme.red, muted: theme.muted_foreground, gutter: theme.foreground.opacity(0.025), border: theme.border };
        let list = if self.side_by_side {
            uniform_list("compare-split", rows.len(), move |range: Range<usize>, _, _| {
                range
                    .map(|ix| {
                        let row = &rows[ix];
                        let changed = row.left.as_ref().is_none_or(|l| l.kind != LineKind::Context) || row.right.as_ref().is_none_or(|l| l.kind != LineKind::Context);
                        h_flex()
                            .w_full()
                            .child(half(row.left.as_ref(), changed, true, &colors))
                            .child(div().w(gpui::px(1.)).h_full().flex_shrink_0().bg(colors.border))
                            .child(half(row.right.as_ref(), changed, false, &colors))
                    })
                    .collect()
            })
        } else {
            uniform_list("compare-unified", lines.len(), move |range: Range<usize>, _, _| {
                range
                    .map(|ix| {
                        let l = &lines[ix];
                        let (bg, side, sign) = colors.of(l.kind);
                        h_flex()
                            .w_full()
                            .bg(bg)
                            .child(h_flex().h_full().flex_shrink_0().bg(side).text_color(colors.muted.opacity(0.7)).child(num(l.old)).child(num(l.new)))
                            .child(div().w(r(22.)).flex_shrink_0().text_center().text_color(colors.muted).child(sign))
                            .child(div().whitespace_nowrap().pr(r(16.)).child(l.text.replace('\t', "    ")))
                    })
                    .collect()
            })
        };
        list.flex_1().min_h_0().font_family(font).text_size(size).line_height(size * 1.65).into_any_element()
    }
}

/// The Git tab's diff colors.
#[derive(Clone, Copy)]
struct Colors {
    added: Hsla,
    removed: Hsla,
    muted: Hsla,
    gutter: Hsla,
    border: Hsla,
}

impl Colors {
    /// Row background, number gutter background, sign.
    fn of(&self, kind: LineKind) -> (Hsla, Hsla, &'static str) {
        match kind {
            LineKind::Added => (self.added.opacity(0.1), self.added.opacity(0.18), "+"),
            LineKind::Removed => (self.removed.opacity(0.1), self.removed.opacity(0.18), "−"),
            _ => (gpui::transparent_black(), self.gutter, ""),
        }
    }
}

fn num(n: Option<u32>) -> gpui::Div {
    div().w(r(42.)).flex_shrink_0().text_right().pr(r(8.)).child(n.map(|n| n.to_string()).unwrap_or_default())
}

/// One side of a side-by-side row; a blank facing changes is shaded.
fn half(line: Option<&DiffLine>, changed: bool, left: bool, c: &Colors) -> gpui::Div {
    let cell = h_flex().flex_1().min_w_0().h_full().overflow_hidden();
    let Some(l) = line else {
        return cell.bg(c.muted.opacity(if changed { 0.06 } else { 0. }));
    };
    let (bg, side, sign) = c.of(l.kind);
    cell.bg(bg)
        .child(h_flex().h_full().flex_shrink_0().bg(side).text_color(c.muted.opacity(0.7)).child(num(if left { l.old } else { l.new })))
        .child(div().w(r(22.)).flex_shrink_0().text_center().text_color(c.muted).child(sign))
        .child(div().min_w_0().whitespace_nowrap().pr(r(12.)).child(l.text.replace('\t', "    ")))
}

fn compare_icon() -> Icon {
    Icon::empty().path("icons/diff.svg")
}

impl Render for CompareView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let swap = button("compare-swap")
            .ghost()
            .small()
            .icon(Icon::empty().path("icons/arrow-left-right.svg"))
            .label("Inverser")
            .on_click(cx.listener(|this, _, window, cx| this.swap(window, cx)));
        let clear = button("compare-clear").ghost().small().icon(IconName::Delete).label("Effacer").on_click(cx.listener(|this, _, window, cx| this.clear(window, cx)));
        v_flex()
            .size_full()
            .child(header(tool("compare"), "Les différences entre deux textes ou deux fichiers, ligne par ligne.", vec![swap.into_any_element(), clear.into_any_element()], cx))
            .child(h_flex().flex_shrink_0().h(r(240.)).child(self.render_side(0, cx)).child(div().w(gpui::px(1.)).h_full().bg(border)).child(self.render_side(1, cx)))
            .child(self.render_toolbar(cx))
            .child(v_flex().flex_1().min_h_0().child(self.render_diff(cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(lines: &[DiffLine]) -> String {
        lines
            .iter()
            .map(|l| match l.kind {
                LineKind::Added => '+',
                LineKind::Removed => '-',
                _ => ' ',
            })
            .collect()
    }

    #[test]
    fn diff_keeps_every_line_with_both_numbers() {
        let lines = diff("a\nb\nc\nd", "a\nB\nc\nd\ne\n", false).unwrap();
        assert_eq!(kinds(&lines), " -+  +", "{lines:?}");
        assert_eq!((lines[1].old, lines[2].new, lines[5].new), (Some(2), Some(2), Some(5)));
        assert_eq!(summary(&lines), (2, 1, 2));
        assert!(diff("same\n", "same", false).unwrap().iter().all(|l| l.kind == LineKind::Context), "a missing last newline is not a change");
        assert_eq!(kinds(&diff("", "x\ny", false).unwrap()), "++");
    }

    #[test]
    fn ignoring_spaces_hides_space_only_changes() {
        let (a, b) = ("fn main() {\n    run();\n}\n", "fn main()  {\n\trun();\n}\n");
        assert_eq!(summary(&diff(a, b, false).unwrap()), (2, 2, 1));
        assert!(diff(a, b, true).unwrap().iter().all(|l| l.kind == LineKind::Context));
    }

    #[test]
    fn side_by_side_pairs_removed_with_added() {
        let line = |kind, old, new, text: &str| DiffLine { kind, old, new, text: text.into() };
        use LineKind::*;
        let lines = vec![
            line(Context, Some(1), Some(1), "a"),
            line(Removed, Some(2), None, "b"),
            line(Removed, Some(3), None, "c"),
            line(Added, None, Some(2), "B"),
            line(Context, Some(4), Some(3), "d"),
            line(Added, None, Some(4), "e"),
        ];
        let rows = side_by_side(&lines);
        let texts: Vec<(Option<&str>, Option<&str>)> = rows.iter().map(|r| (r.left.as_ref().map(|l| l.text.as_str()), r.right.as_ref().map(|l| l.text.as_str()))).collect();
        assert_eq!(texts, [(Some("a"), Some("a")), (Some("b"), Some("B")), (Some("c"), None), (Some("d"), Some("d")), (None, Some("e"))]);
        assert_eq!(summary(&lines), (2, 2, 2));
    }

    #[test]
    fn huge_sides_are_refused() {
        let big = "x\n".repeat(MAX_LINES + 1);
        assert!(diff(&big, "", false).unwrap_err().contains("Trop long"));
    }
}
