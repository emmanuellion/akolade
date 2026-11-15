//! "Base de données": the project's SQLite files, their tables, and a query
//! editor, through the `sqlite3` command line (`-json` output).

use std::{
    fmt,
    io::{BufRead, BufReader},
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use gpui::{
    AnyElement, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, WeakEntity, Window, div, prelude::FluentBuilder, uniform_list,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};

use crate::{
    settings::AppSettings,
    tools::{header, tool},
    ui,
    workspace::{Workspace, button, r},
};

/// Rows kept from a query: more would only slow the table down.
const MAX_ROWS: usize = 1000;
/// Rows shown when a table is clicked.
const PREVIEW_ROWS: usize = 200;
/// Longer values are cut in the table.
const MAX_CELL: usize = 100;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    /// `None`: NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// More rows came: they were left out.
    pub truncated: bool,
}

/// A `sqlite3 -json` row, its columns in order (a join may repeat a name).
struct Row(Vec<(String, serde_json::Value)>);

impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct RowVisitor;
        impl<'de> Visitor<'de> for RowVisitor {
            type Value = Row;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a row object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Row, A::Error> {
                let mut row = vec![];
                while let Some(entry) = map.next_entry()? {
                    row.push(entry);
                }
                Ok(Row(row))
            }
        }
        d.deserialize_map(RowVisitor)
    }
}

/// `sqlite3 -json` prints one row per line (`[{…},` … `{…}]`), a new `[` for
/// each statement that returns rows: the last result is kept, up to `max` rows.
// shortcut: stops reading at `max` rows, so a later statement's result is lost; fine for a console.
pub fn parse_rows(lines: impl Iterator<Item = String>, max: usize) -> Result<Table, String> {
    let mut table = Table::default();
    for line in lines {
        let mut l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Some(rest) = l.strip_prefix('[') {
            (table, l) = (Table::default(), rest);
        }
        let l = l.strip_suffix(',').or_else(|| l.strip_suffix(']')).unwrap_or(l);
        let Row(row) = serde_json::from_str(l).map_err(|e| format!("Réponse de sqlite3 illisible : {e}"))?;
        if table.rows.len() == max {
            table.truncated = true;
            break;
        }
        if table.columns.is_empty() {
            table.columns = row.iter().map(|(k, _)| k.clone()).collect();
        }
        table.rows.push(
            row.into_iter()
                .map(|(_, v)| match v {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s),
                    v => Some(v.to_string()),
                })
                .collect(),
        );
    }
    Ok(table)
}

/// Read-only queries start with SELECT, WITH, PRAGMA or EXPLAIN (comments skipped). The rest
/// asks first; a read-only one that writes anyway is refused by `-readonly`.
pub fn modifies(sql: &str) -> bool {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, after)| after).trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |(_, after)| after).trim_start();
        } else {
            break;
        }
    }
    let word: String = s.chars().take_while(char::is_ascii_alphabetic).collect();
    match word.to_ascii_uppercase().as_str() {
        "SELECT" | "EXPLAIN" => false,
        // A CTE can end in a write: `WITH x AS (…) DELETE FROM …`.
        "WITH" => s.to_ascii_uppercase().split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|w| matches!(w, "DELETE" | "UPDATE" | "INSERT" | "REPLACE")),
        // `PRAGMA x = y` and `PRAGMA x(y)` set a value, but for the pragmas whose argument names what to read.
        "PRAGMA" => {
            let name = s[6..].trim_start().split(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.').next().unwrap_or("");
            let name = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            const READS: [&str; 10] = ["table_info", "table_xinfo", "table_list", "index_info", "index_xinfo", "index_list", "foreign_key_list", "foreign_key_check", "integrity_check", "quick_check"];
            s.contains('=') || (s.contains('(') && !READS.contains(&name.as_str()))
        }
        _ => true,
    }
}

/// `"name"`, for a table name in SQL.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Runs `sql` on the database (blocking: run it in the background). Read-only
/// unless `write`.
pub fn query(db: &Path, sql: &str, write: bool, max: usize) -> Result<Table, String> {
    match run_sqlite(db.as_os_str(), sql, write, max) {
        // A WAL database read-only needs its "-shm" file, which a read-only open
        // can't create ("unable to open database file (14)"). Read it as a frozen
        // file instead: creates nothing, and no "-wal" means no pending write to miss.
        Err(e) if !write && e.contains("(14)") && !side_file(db, "-wal").exists() => run_sqlite(immutable_uri(db).as_ref(), sql, false, max),
        other => other,
    }
}

/// `rag.db` → `rag.db-wal`.
fn side_file(db: &Path, suffix: &str) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

/// `file:/a b/r%g.db?immutable=1`, the characters a URI reserves escaped.
fn immutable_uri(db: &Path) -> String {
    const RESERVED: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b'%').add(b'?').add(b'#');
    format!("file:{}?immutable=1", percent_encoding::utf8_percent_encode(&db.to_string_lossy(), RESERVED))
}

fn run_sqlite(db: &std::ffi::OsStr, sql: &str, write: bool, max: usize) -> Result<Table, String> {
    let mut cmd = Command::new("sqlite3");
    // -safe: no .shell, ATTACH or extension from a query; -init: ~/.sqliterc can't change the output.
    cmd.args(["-init", "/dev/null", "-safe", "-json"]);
    if !write {
        cmd.arg("-readonly");
    }
    // The space: SQL starting with "-- comment" is not taken for an option.
    cmd.arg(db).arg(format!(" {sql}")).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("sqlite3 : {e}"))?;
    let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    // As bytes: a text value that is not UTF-8 must not end the reading.
    let table = parse_rows(stdout.split(b'\n').map_while(Result::ok).map(|l| String::from_utf8_lossy(&l).into_owned()), max);
    let truncated = matches!(&table, Ok(t) if t.truncated);
    if truncated {
        let _ = child.kill();
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !truncated && !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("erreur inconnue");
        // "Parse error in 5th command line argument: no such column: x" → "no such column: x".
        return Err(line.split_once("argument: ").map_or(line, |(_, e)| e).trim_start_matches("Error: ").to_string());
    }
    table
}

/// A modifying query: how many rows it changed.
pub fn execute(db: &Path, sql: &str) -> Result<i64, String> {
    // On its own line: a last "-- comment" can't swallow it. No cap: stopping
    // early kills sqlite3, which rolls the change back.
    let t = query(db, &format!("{sql}\n;\nSELECT total_changes() AS n"), true, usize::MAX)?;
    Ok(t.rows.last().and_then(|r| r.first()).cloned().flatten().and_then(|n| n.parse().ok()).unwrap_or(0))
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableInfo {
    pub name: String,
    pub view: bool,
    /// Unknown when counting failed (a broken view).
    pub rows: Option<i64>,
}

pub fn tables(db: &Path) -> Result<Vec<TableInfo>, String> {
    let list = query(db, "SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name", false, usize::MAX)?;
    let mut out: Vec<TableInfo> = list
        .rows
        .into_iter()
        .map(|r| TableInfo { name: r.first().cloned().flatten().unwrap_or_default(), view: r.get(1).cloned().flatten().as_deref() == Some("view"), rows: None })
        .collect();
    if out.is_empty() {
        return Ok(out);
    }
    // All counts in one row; a broken view leaves them all unknown.
    let counts: Vec<String> = out.iter().map(|t| format!("(SELECT count(*) FROM {})", ident(&t.name))).collect();
    if let Ok(c) = query(db, &format!("SELECT {}", counts.join(", ")), false, 1)
        && let Some(row) = c.rows.first()
    {
        for (t, n) in out.iter_mut().zip(row) {
            t.rows = n.as_deref().and_then(|n| n.parse().ok());
        }
    }
    Ok(out)
}

fn is_sqlite(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| ["db", "sqlite", "sqlite3"].contains(&e.to_lowercase().as_str()))
}

enum Outcome {
    Rows(Table),
    Changed(i64),
}

pub struct DatabaseView {
    workspace: WeakEntity<Workspace>,
    root: Option<PathBuf>,
    files: Vec<PathBuf>,
    scanning: bool,
    path: Entity<InputState>,
    editor: Entity<InputState>,
    db: Option<PathBuf>,
    /// The open database's tables; `None` while read.
    tables: Option<Result<Vec<TableInfo>, String>>,
    table: Option<String>,
    result: Option<Result<Outcome, String>>,
    busy: bool,
    _subs: Vec<Subscription>,
}

impl DatabaseView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("Autre fichier : collez son chemin"));
        let editor = cx.new(|cx| InputState::new(window, cx).code_editor("sql").placeholder("SELECT * FROM …"));
        let _subs = vec![
            cx.subscribe_in(&path, window, |this, _, ev, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.open_typed(cx);
                }
            }),
            cx.subscribe_in(&editor, window, |this, _, ev, window, cx| {
                if let InputEvent::PressEnter { secondary: true } = ev {
                    this.run(window, cx);
                }
            }),
        ];
        let mut view = Self { workspace, root, files: vec![], scanning: false, path, editor, db: None, tables: None, table: None, result: None, busy: false, _subs };
        view.scan(cx);
        view
    }

    /// The refresh button: the folder open now (the tab may outlive the one it
    /// was built for). Not in `new`: the workspace is being updated then.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        if let Some(root) = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf)) {
            self.root = Some(root);
        }
        self.scan(cx);
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        let options = AppSettings::get(cx).scan_options();
        self.scanning = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            crate::explorer::scan(&root, &options).into_iter().filter(|e| !e.is_dir && is_sqlite(&e.rel)).map(|e| root.join(e.rel)).collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let files = task.await;
            let _ = this.update(cx, |this, cx| {
                (this.files, this.scanning) = (files, false);
                cx.notify();
            });
        })
        .detach();
    }

    fn open_typed(&mut self, cx: &mut Context<Self>) {
        let typed = self.path.read(cx).value().trim().to_string();
        if typed.is_empty() {
            return;
        }
        let path = match typed.strip_prefix("~/") {
            Some(rest) => std::env::home_dir().unwrap_or_default().join(rest),
            None => self.root.as_deref().map_or_else(|| PathBuf::from(&typed), |root| root.join(&typed)),
        };
        if !path.is_file() {
            self.result = Some(Err(format!("Fichier introuvable : {}", path.display())));
            return cx.notify();
        }
        if !self.files.contains(&path) {
            self.files.push(path.clone());
        }
        self.open_db(path, cx);
    }

    fn open_db(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        (self.db, self.tables, self.table, self.result) = (Some(path.clone()), None, None, None);
        cx.notify();
        self.load_tables(cx);
    }

    fn load_tables(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.db.clone() else { return };
        let task = cx.background_executor().spawn({
            let path = path.clone();
            async move { tables(&path) }
        });
        cx.spawn(async move |this, cx| {
            let list = task.await;
            let _ = this.update(cx, |this, cx| {
                // Another database was chosen meanwhile.
                if this.db.as_ref() == Some(&path) {
                    this.tables = Some(list);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn show_table(&mut self, name: String, cx: &mut Context<Self>) {
        let sql = format!("SELECT * FROM {} LIMIT {}", ident(&name), PREVIEW_ROWS + 1);
        self.table = Some(name);
        self.start(move |db| query(db, &sql, false, PREVIEW_ROWS).map(Outcome::Rows), cx);
    }

    /// "Exécuter" or ⌘⏎: a modifying query asks first.
    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).value().trim().to_string();
        if sql.is_empty() || self.busy {
            return;
        }
        let Some(db) = self.db.clone() else {
            return window.push_notification("Choisissez d'abord une base dans la liste.", cx);
        };
        if !modifies(&sql) {
            return self.start(move |db| query(db, &sql, false, MAX_ROWS).map(Outcome::Rows), cx);
        }
        let name = db.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Cette requête modifie la base {name}. L'exécuter ?"),
            Some("Les changements sont enregistrés tout de suite, sans retour en arrière possible."),
            &["Exécuter", "Annuler"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| this.start(move |db| execute(db, &sql).map(Outcome::Changed), cx));
            }
        })
        .detach();
    }

    fn start(&mut self, work: impl FnOnce(&Path) -> Result<Outcome, String> + Send + 'static, cx: &mut Context<Self>) {
        let Some(db) = self.db.clone() else { return };
        self.busy = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move { work(&db) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                if matches!(result, Ok(Outcome::Changed(_))) {
                    this.load_tables(cx);
                }
                this.result = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    fn render_list(&self, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let (fg, muted, border, primary) = (theme.foreground, theme.muted_foreground, theme.border, theme.primary);
        let empty = match () {
            _ if self.root.is_none() => Some("Ouvrez un dossier, ou collez le chemin d'une base ci-dessous."),
            _ if self.scanning && self.files.is_empty() => Some("Recherche…"),
            _ if self.files.is_empty() => Some("Aucun fichier .db, .sqlite ou .sqlite3 dans le projet."),
            _ => None,
        };
        let item = |id: SharedString, on: bool, indent: f32| {
            h_flex()
                .id(id)
                .flex_shrink_0()
                .h(r(30.))
                .pl(r(8. + indent))
                .pr(r(8.))
                .gap(r(8.))
                .rounded(r(6.))
                .cursor_pointer()
                .text_size(r(13.))
                .when(on, |d| d.bg(primary.opacity(0.14)))
                .when(!on, |d| d.hover(|d| d.bg(fg.opacity(0.05))))
        };
        let mut list = vec![];
        for (i, file) in self.files.iter().enumerate() {
            let on = self.db.as_ref() == Some(file);
            let label = self.root.as_deref().and_then(|root| file.strip_prefix(root).ok()).unwrap_or(file).display().to_string();
            let path = file.clone();
            list.push(
                item(SharedString::from(format!("db-{i}")), on && self.table.is_none(), 0.)
                    .child(Icon::empty().path("icons/database.svg").size(r(14.)).text_color(muted))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_db(path.clone(), cx)))
                    .into_any_element(),
            );
            if !on {
                continue;
            }
            match &self.tables {
                None => list.push(div().flex_shrink_0().pl(r(30.)).py(r(4.)).text_size(r(12.5)).text_color(muted).child("Lecture…").into_any_element()),
                Some(Err(e)) => list.push(div().flex_shrink_0().pl(r(30.)).py(r(4.)).text_size(r(12.5)).text_color(theme.red).child(e.clone()).into_any_element()),
                Some(Ok(t)) if t.is_empty() => list.push(div().flex_shrink_0().pl(r(30.)).py(r(4.)).text_size(r(12.5)).text_color(muted).child("Aucune table.").into_any_element()),
                Some(Ok(t)) => list.extend(t.iter().enumerate().map(|(j, t)| {
                    let name = t.name.clone();
                    item(SharedString::from(format!("table-{j}")), self.table.as_ref() == Some(&t.name), 18.)
                        .child(if t.view { Icon::new(IconName::Eye) } else { Icon::empty().path("icons/table.svg") }.size(r(14.)).text_color(muted))
                        .child(div().flex_1().min_w_0().truncate().child(t.name.clone()))
                        .child(div().flex_shrink_0().text_size(r(11.5)).text_color(muted).child(t.rows.map_or("?".into(), |n| n.to_string())))
                        .on_click(cx.listener(move |this, _, _, cx| this.show_table(name.clone(), cx)))
                        .into_any_element()
                })),
            }
        }
        v_flex()
            .w(r(260.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(border)
            .bg(fg.opacity(0.015))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .h(r(40.))
                    .pl(r(16.))
                    .pr(r(8.))
                    .child(div().flex_1().text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(format!("BASES · {}", self.files.len())))
                    .child(button("rescan").ghost().xsmall().icon(Icon::empty().path("icons/refresh.svg")).tooltip("Actualiser").loading(self.scanning).on_click(cx.listener(|this, _, _, cx| this.rescan(cx)))),
            )
            .child(
                v_flex()
                    .id("databases")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(r(6.))
                    .gap(r(1.))
                    .when_some(empty, |d, text| d.child(div().px(r(10.)).pt(r(4.)).text_size(r(12.5)).text_color(muted).child(text)))
                    .children(list),
            )
            .child(div().flex_shrink_0().p(r(10.)).border_t_1().border_color(border).child(Input::new(&self.path).small()))
    }

    fn render_result(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let center = |title: &str, text: &str| ui::empty_state((tool("database").icon)(), theme.cyan, title, text, cx).into_any_element();
        let table = match &self.result {
            _ if self.busy => return ui::loading("Lecture…", cx).into_any_element(),
            None if self.db.is_none() => return center("Aucune base ouverte", "Choisissez un fichier SQLite à gauche."),
            None => return center("Prête", "Cliquez sur une table, ou écrivez une requête puis « Exécuter » (⌘⏎)."),
            Some(Err(e)) => return ui::error_state("La requête a échoué", e, cx).into_any_element(),
            Some(Ok(Outcome::Changed(n))) => return center(&if *n == 1 { "1 ligne modifiée".into() } else { format!("{n} lignes modifiées") }, "La base a été enregistrée."),
            Some(Ok(Outcome::Rows(t))) if t.columns.is_empty() => return center("Aucune ligne", "La requête n'a rien renvoyé."),
            Some(Ok(Outcome::Rows(t))) => t,
        };
        let widths = column_widths(table);
        let n = table.rows.len();
        let note = if table.truncated { format!("{n} premières lignes affichées, la suite est coupée") } else if n == 1 { "1 ligne".into() } else { format!("{n} lignes") };
        // Only the rows in view are built (the editor re-renders on every cursor blink); the
        // header and the list sit in one horizontal scroll so the columns stay aligned.
        let mut list = uniform_list("rows", n, cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx))).flex_1().min_h_0();
        list.style().restrict_scroll_to_axis = Some(true);
        let mut rows = div().id("rows-x").flex_1().min_h_0().overflow_x_scroll().font_family(theme.mono_font_family.clone()).text_size(theme.mono_font_size);
        rows.style().restrict_scroll_to_axis = Some(true);
        v_flex()
            .flex_1()
            .min_h_0()
            .child(div().flex_shrink_0().px(r(16.)).py(r(8.)).border_b_1().border_color(border).text_size(r(12.)).text_color(muted).child(note))
            .child(
                rows.child(
                    v_flex()
                        .h_full()
                        .min_w_full()
                        .w(r(widths.iter().sum()))
                        .child(
                            h_flex()
                                .flex_shrink_0()
                                .py(r(6.))
                                .border_b_1()
                                .border_color(border)
                                .font_weight(FontWeight::SEMIBOLD)
                                .children(table.columns.iter().zip(&widths).map(|(c, w)| cell(*w).child(c.clone()))),
                        )
                        .child(list),
                ),
            )
            .into_any_element()
    }

    fn render_rows(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<gpui::Div> {
        let Some(Ok(Outcome::Rows(table))) = &self.result else { return vec![] };
        let (muted, border) = (cx.theme().muted_foreground, cx.theme().border);
        let shorten = |s: &str| {
            let s = s.replace('\n', " ");
            if s.chars().count() > MAX_CELL { s.chars().take(MAX_CELL).collect::<String>() + "…" } else { s }
        };
        let widths = column_widths(table);
        table.rows[range.start.min(table.rows.len())..range.end.min(table.rows.len())]
            .iter()
            .map(|row| {
                h_flex().py(r(4.)).border_b_1().border_color(border.opacity(0.5)).children(row.iter().zip(&widths).map(|(v, w)| match v {
                    Some(v) => cell(*w).child(shorten(v)),
                    None => cell(*w).italic().text_color(muted.opacity(0.7)).child("NULL"),
                }))
            })
            .collect()
    }
}

/// Each column as wide as its header and first values, within bounds.
fn column_widths(table: &Table) -> Vec<f32> {
    (0..table.columns.len())
        .map(|c| {
            let chars = table.rows.iter().take(50).filter_map(|row| row.get(c)?.as_ref()).map(|v| v.chars().count().min(MAX_CELL)).chain([table.columns[c].chars().count(), 4]).max().unwrap_or(4);
            (chars as f32 * 7.5 + 20.).clamp(60., 320.)
        })
        .collect()
}

fn cell(w: f32) -> gpui::Div {
    div().w(r(w)).flex_shrink_0().px(r(8.)).truncate()
}

impl Render for DatabaseView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list = self.render_list(cx);
        let result = self.render_result(cx);
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let subtitle = match &self.db {
            Some(db) => db.display().to_string(),
            None => "Les bases SQLite du projet : tables, lignes et requêtes.".into(),
        };
        let editor = v_flex()
            .flex_shrink_0()
            .h(r(170.))
            .border_b_1()
            .border_color(border)
            .child(div().flex_1().min_h_0().child(Input::new(&self.editor).h_full().appearance(false).font_family(theme.mono_font_family.clone()).text_size(theme.mono_font_size)))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px(r(16.))
                    .py(r(8.))
                    .gap(r(10.))
                    .child(div().flex_1().text_size(r(11.5)).text_color(muted).child("⌘⏎ pour exécuter. Lecture seule, sauf les requêtes qui modifient (confirmation demandée)."))
                    .child(
                        button("execute")
                            .primary()
                            .small()
                            .icon(Icon::empty().path("icons/play.svg"))
                            .label("Exécuter")
                            .loading(self.busy)
                            .disabled(self.busy || self.db.is_none())
                            .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
                    ),
            );
        v_flex()
            .size_full()
            .child(header(tool("database"), subtitle, vec![], cx))
            .child(h_flex().flex_1().min_h_0().child(list).child(v_flex().flex_1().min_w_0().h_full().child(editor).child(result)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> impl Iterator<Item = String> + '_ {
        s.lines().map(String::from)
    }

    #[test]
    fn json_rows_keep_column_order_duplicates_and_nulls() {
        let out = "[{\"zeta\":1,\"alpha\":\"Ada \\\"q\\\"\\nnl\",\"id\":null,\"id\":2.5},\n{\"zeta\":2,\"alpha\":\"x,]\",\"id\":3,\"id\":null}]\n";
        let t = parse_rows(lines(out), 10).unwrap();
        assert_eq!(t.columns, ["zeta", "alpha", "id", "id"]);
        assert_eq!(t.rows[0], [Some("1".into()), Some("Ada \"q\"\nnl".into()), None, Some("2.5".into())]);
        assert_eq!(t.rows[1][1].as_deref(), Some("x,]"));
        assert!(!t.truncated);
        // Several statements: the last result wins; capped with a note.
        let t = parse_rows(lines("[{\"a\":1}]\n[{\"b\":1},\n{\"b\":2},\n{\"b\":3}]"), 2).unwrap();
        assert_eq!((t.columns.as_slice(), t.rows.len(), t.truncated), (["b".to_string()].as_slice(), 2, true));
        assert_eq!(parse_rows(lines(""), 10).unwrap(), Table::default());
        assert!(parse_rows(lines("oops"), 10).is_err());
    }

    #[test]
    fn modifying_queries_are_told_apart() {
        for read in ["select 1", "  WITH x AS (SELECT 1) SELECT * FROM x", "-- hi\n/* there */ pragma table_info(a)", "Explain query plan select 1", "with updated AS (SELECT 1) SELECT * FROM updated", "PRAGMA journal_mode", "PRAGMA main.index_list(a)"] {
            assert!(!modifies(read), "{read}");
        }
        for write in ["DELETE FROM a", "-- select\nupdate a set x = 1", "/* select */ insert into a values (1)", "drop table a", "selectx", "", "WITH x AS (SELECT 1) DELETE FROM a", "with x as (select 1) update a set y = 1", "PRAGMA journal_mode = WAL", "PRAGMA user_version(5)", "pragma main.user_version (5)"] {
            assert!(modifies(write), "{write}");
        }
    }

    #[test]
    fn reads_and_changes_a_real_database() {
        let dir = std::env::temp_dir().join(format!("akolade-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("app.sqlite");
        let setup = "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, note TEXT); INSERT INTO users(name, note) VALUES ('Ada', NULL), ('Bob \"B\"', 'x'); CREATE VIEW named AS SELECT name FROM users; CREATE TABLE \"we\"\"ird\"(v);";
        assert!(Command::new("sqlite3").arg(&db).arg(setup).status().unwrap().success());

        let list = tables(&db).unwrap();
        assert_eq!(
            list,
            [
                TableInfo { name: "named".into(), view: true, rows: Some(2) },
                TableInfo { name: "users".into(), view: false, rows: Some(2) },
                TableInfo { name: "we\"ird".into(), view: false, rows: Some(0) },
            ]
        );
        let t = query(&db, "-- the users\nSELECT * FROM users ORDER BY id", false, 1).unwrap();
        assert_eq!((t.columns.len(), t.rows[0].clone(), t.truncated), (3, vec![Some("1".into()), Some("Ada".into()), None], true));
        assert!(query(&db, "SELECT nope FROM users", false, 10).unwrap_err().contains("no such column: nope"));
        assert!(query(&db, "DELETE FROM users", false, 10).unwrap_err().contains("readonly"), "browsing never writes");
        assert!(query(&db, ".shell echo hi", false, 10).is_err(), "never a dot-command");
        assert_eq!(execute(&db, "UPDATE users SET note = 'y' -- all").unwrap(), 2);
        assert_eq!(query(&db, "SELECT count(*) AS n FROM users WHERE note = 'y'", false, 10).unwrap().rows, [[Some("2".to_string())]]);
        // A write returning more rows than MAX_ROWS still goes through, its count right.
        assert_eq!(execute(&db, &format!("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n LIMIT {}) INSERT INTO users(name) SELECT 'u' FROM n", MAX_ROWS + 500)).unwrap(), (MAX_ROWS + 500) as i64);
        assert_eq!(execute(&db, "DELETE FROM users WHERE name = 'u' RETURNING *").unwrap(), (MAX_ROWS + 500) as i64);
        assert_eq!(query(&db, "SELECT count(*) FROM users", false, 10).unwrap().rows, [[Some("2".to_string())]]);
        // A value that is not UTF-8 neither ends the reading nor hides the rows after it.
        let t = query(&db, "SELECT CAST(x'ff' AS TEXT) AS v UNION ALL SELECT 'ok'", false, 10).unwrap();
        assert_eq!(t.rows, [[Some("\u{fffd}".to_string())], [Some("ok".to_string())]]);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(is_sqlite(Path::new("a/b.DB")) && !is_sqlite(Path::new("db")));
    }

    #[test]
    fn reads_a_wal_database_without_its_side_files_and_creates_none() {
        let dir = std::env::temp_dir().join(format!("akolade-wal-{}", std::process::id())).join("a b#c");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("r%g.db");
        assert!(Command::new("sqlite3").arg(&db).arg("PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES (1);").stdout(Stdio::null()).status().unwrap().success());
        // As a copied or freshly checkpointed database: no "-shm" nor "-wal".
        for s in ["-shm", "-wal"] {
            let _ = std::fs::remove_file(side_file(&db, s));
        }
        assert_eq!(tables(&db).unwrap(), [TableInfo { name: "t".into(), view: false, rows: Some(1) }]);
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["r%g.db"], "reading left nothing next to it");
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }
}
