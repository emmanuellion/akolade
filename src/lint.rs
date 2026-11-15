//! Linters (store plugins): their findings join the language servers' in
//! `Problems`. Most read the text on stdin and run as you type; Clippy checks
//! the whole crate and runs on save.

use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

use serde_json::Value;

use crate::{
    lsp,
    problems::{Problem, Severity},
    settings::AppSettings,
};

pub struct Linter {
    /// Plugin id (its own on/off switch, apart from a formatter of the same program).
    pub id: &'static str,
    pub name: &'static str,
    pub program: &'static str,
    /// `{file}`: the file's path.
    pub args: &'static [&'static str],
    pub languages: &'static [&'static str],
    pub install: Option<&'static str>,
    pub source: &'static str,
    pub about: &'static str,
    /// Checks the whole project from the disk (on save), not the text on stdin.
    pub project: bool,
    /// Columns counted in characters, not UTF-16 units: converted with the text.
    pub char_columns: bool,
    pub parse: fn(&str, &Path, &Path) -> HashMap<PathBuf, Vec<Problem>>,
}

pub static LINTERS: &[Linter] = &[
    Linter {
        id: "eslint",
        name: "ESLint",
        program: "eslint",
        args: &["--format", "json", "--stdin", "--stdin-filename", "{file}"],
        languages: &["JavaScript", "TypeScript", "JSX", "TSX"],
        install: Some("npm install --prefix {dir} eslint"),
        source: "npm",
        about: "Les erreurs et mauvaises pratiques JavaScript et TypeScript, selon la configuration ESLint du projet.",
        project: false,
        char_columns: false, // JS string indices: UTF-16 already
        parse: parse_eslint,
    },
    Linter {
        id: "ruff-check",
        name: "Ruff (linter)",
        program: "ruff",
        args: &["check", "--output-format", "json", "--stdin-filename", "{file}", "-"],
        languages: &["Python"],
        install: Some("brew install ruff"),
        source: "Homebrew",
        about: "Des centaines de règles Python en un éclair : noms inconnus, imports inutiles, pièges courants.",
        project: false,
        char_columns: true,
        parse: parse_ruff,
    },
    Linter {
        id: "shellcheck",
        name: "ShellCheck",
        program: "shellcheck",
        args: &["-f", "json", "-"],
        languages: &["Shell"],
        install: Some("brew install shellcheck"),
        source: "Homebrew",
        about: "Les pièges des scripts shell : variables non citées, commandes qui échouent en silence.",
        project: false,
        // shortcut: unit unclear (tabs may count as 8), kept as is; check against shellcheck's json1 format if columns drift.
        char_columns: false,
        parse: parse_shellcheck,
    },
    Linter {
        id: "hadolint",
        name: "Hadolint",
        program: "hadolint",
        args: &["-f", "json", "-"],
        languages: &["Dockerfile"],
        install: Some("brew install hadolint"),
        source: "Homebrew",
        about: "Les bonnes pratiques des Dockerfile : versions d'images, couches inutiles, apt sans nettoyage.",
        project: false,
        // shortcut: unit unclear, kept as is; the column is nearly always 1 (the instruction's start).
        char_columns: false,
        parse: parse_hadolint,
    },
    Linter {
        id: "clippy",
        name: "Clippy",
        program: "cargo-clippy",
        args: &["clippy", "--message-format=json", "--quiet"],
        languages: &["Rust"],
        install: None,
        source: "fourni avec Rust",
        about: "Le linter officiel de Rust : des centaines de conseils. Vérifie tout le projet à chaque enregistrement.",
        project: true,
        char_columns: true, // rustc: "1-based, character offset"
        parse: parse_clippy,
    },
];

/// The enabled, installed linters of a language, with their program. In a
/// folder not trusted, none that runs the project's code.
pub fn for_language(language: &str, root: &Path, settings: &AppSettings) -> Vec<(&'static Linter, PathBuf)> {
    let trusted = crate::trust::is_trusted(root);
    LINTERS
        .iter()
        .filter(|l| l.languages.contains(&language) && settings.plugin_enabled(l.id) && (trusted || !runs_project_code(l)))
        .filter_map(|l| Some((l, program(l, root, trusted)?)))
        .collect()
}

/// ESLint executes the project's config (eslint.config.js) and plugins; Clippy
/// compiles it (build.rs, procedural macros).
fn runs_project_code(l: &Linter) -> bool {
    matches!(l.id, "eslint" | "clippy")
}

/// The project's own copy first (its ESLint plugins and config), then ours or
/// PATH's. A project's own program only in a trusted folder: anything can be
/// shipped as node_modules/.bin/ruff.
fn program(l: &Linter, root: &Path, trusted: bool) -> Option<PathBuf> {
    if l.project {
        // `cargo clippy` needs both cargo and its clippy component.
        return lsp::find_program(l.program).and_then(|_| lsp::find_program("cargo"));
    }
    let local = root.join("node_modules/.bin").join(l.program);
    if trusted && local.is_file() { Some(local) } else { lsp::find_program(l.program) }
}

/// Runs a linter (blocking: in the background). Problems per file; a linter
/// that fails (no config, crash) says nothing rather than something wrong.
pub fn run(l: &Linter, program: &Path, file: &Path, root: &Path, text: &str) -> HashMap<PathBuf, Vec<Problem>> {
    let file_arg = file.to_string_lossy();
    let args: Vec<String> = l.args.iter().map(|a| a.replace("{file}", &file_arg)).collect();
    let mut cmd = Command::new(program);
    cmd.args(&args).current_dir(root).stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.stdin(if l.project { Stdio::null() } else { Stdio::piped() });
    let Ok(mut child) = cmd.spawn() else { return HashMap::new() };
    if let Some(mut stdin) = child.stdin.take() {
        let text = text.to_string();
        // Written from a thread: a big file would fill the pipes both ways.
        std::thread::spawn(move || stdin.write_all(text.as_bytes()));
    }
    let Ok(out) = child.wait_with_output() else { return HashMap::new() };
    let mut found = (l.parse)(&String::from_utf8_lossy(&out.stdout), file, root);
    if l.char_columns {
        for (path, list) in &mut found {
            // A project linter read the disk: so do we.
            let disk = if l.project { std::fs::read_to_string(path).unwrap_or_default() } else { String::new() };
            utf16_columns(if l.project { &disk } else { text }, list);
        }
    }
    found
}

/// Character columns → UTF-16 ones (what `Problem` holds): one more per
/// character outside the BMP (emoji) before the column.
fn utf16_columns(text: &str, list: &mut [Problem]) {
    let lines: Vec<&str> = text.lines().collect();
    let convert = |(line, col): (u32, u32)| {
        let wide = lines.get(line as usize).map_or(0, |l| l.chars().take(col as usize).filter(|c| c.len_utf16() == 2).count());
        (line, col + wide as u32)
    };
    for p in list {
        (p.start, p.end) = (convert(p.start), convert(p.end));
    }
}

/// One run per (linter, file) at a time: asked while one runs, the newest text
/// waits and runs right after (older waiting ones are dropped). No pile-up, and
/// runs end in the order they were asked, so a stale result never wins.
/// Also the linters turned off: their running ones publish nothing more.
#[derive(Default)]
pub struct Gate(Mutex<HashMap<(&'static str, PathBuf), Option<String>>>, Mutex<HashSet<&'static str>>);

impl Gate {
    /// The linter was turned off: its runs publish nothing (call before
    /// `clear_origin`), and the text waiting for it is dropped.
    pub fn stop(&self, id: &'static str) {
        self.1.lock().unwrap().insert(id);
        self.0.lock().unwrap().iter_mut().filter(|((l, _), _)| *l == id).for_each(|(_, waiting)| *waiting = None);
    }

    /// The linter was turned on again.
    pub fn resume(&self, id: &'static str) {
        self.1.lock().unwrap().remove(id);
    }

    /// True: run now. False: one is running, `text` runs after it.
    fn begin(&self, id: &'static str, key: &Path, text: &str) -> bool {
        let mut runs = self.0.lock().unwrap();
        match runs.get_mut(&(id, key.to_path_buf())) {
            Some(waiting) => {
                *waiting = Some(text.to_string());
                false
            }
            None => {
                runs.insert((id, key.to_path_buf()), None);
                true
            }
        }
    }

    /// The run ended: the text asked meanwhile, if any (still running then).
    fn finish(&self, id: &'static str, key: &Path) -> Option<String> {
        let mut runs = self.0.lock().unwrap();
        let next = runs.get_mut(&(id, key.to_path_buf()))?.take();
        if next.is_none() {
            runs.remove(&(id, key.to_path_buf()));
        }
        next
    }
}

/// `run` through the gate (blocking: in the background), `publish` after each
/// run. Returns at once if one already runs: it reruns with this text after.
/// Keyed by file, or by project for project linters (Clippy).
pub fn run_gated(gate: &Gate, l: &'static Linter, program: &Path, file: &Path, root: &Path, text: String, publish: impl Fn(HashMap<PathBuf, Vec<Problem>>)) {
    let key = if l.project { root } else { file };
    if !gate.begin(l.id, key, &text) {
        return;
    }
    let mut text = text;
    loop {
        let found = run(l, program, file, root, &text);
        // Checked under the lock held while publishing: a `stop` can't slip in between.
        let stopped = gate.1.lock().unwrap();
        if !stopped.contains(l.id) {
            publish(found);
        }
        drop(stopped);
        match gate.finish(l.id, key) {
            Some(next) => text = next,
            None => break,
        }
    }
}

fn one_file(file: &Path, list: Vec<Problem>) -> HashMap<PathBuf, Vec<Problem>> {
    HashMap::from([(file.to_path_buf(), list)])
}

/// 1-based line and column → LSP's 0-based.
fn at(line: &Value, column: &Value) -> (u32, u32) {
    let n = |v: &Value| (v.as_u64().unwrap_or(1).max(1) - 1) as u32;
    (n(line), n(column))
}

fn problem(start: (u32, u32), end: (u32, u32), severity: Severity, message: &str, code: String, source: &str) -> Problem {
    let message = if code.is_empty() { message.trim().to_string() } else { format!("{} ({code})", message.trim()) };
    // An empty range still underlines something.
    let end = if end <= start { (start.0, start.1 + 1) } else { end };
    Problem { start, end, severity, message, source: source.into() }
}

fn level(name: &str) -> Option<Severity> {
    match name {
        "error" => Some(Severity::Error),
        "warning" => Some(Severity::Warning),
        "info" => Some(Severity::Info),
        _ => None, // "style": noise
    }
}

pub fn parse_eslint(out: &str, file: &Path, _: &Path) -> HashMap<PathBuf, Vec<Problem>> {
    let Ok(files) = serde_json::from_str::<Vec<Value>>(out) else { return HashMap::new() };
    let list = files
        .iter()
        .flat_map(|f| f["messages"].as_array().cloned().unwrap_or_default())
        .map(|m| {
            let severity = if m["severity"].as_u64() == Some(2) { Severity::Error } else { Severity::Warning };
            let start = at(&m["line"], &m["column"]);
            let end = if m["endLine"].is_null() { start } else { at(&m["endLine"], &m["endColumn"]) };
            problem(start, end, severity, m["message"].as_str().unwrap_or_default(), m["ruleId"].as_str().unwrap_or_default().into(), "eslint")
        })
        .collect();
    one_file(file, list)
}

pub fn parse_ruff(out: &str, file: &Path, _: &Path) -> HashMap<PathBuf, Vec<Problem>> {
    let Ok(items) = serde_json::from_str::<Vec<Value>>(out) else { return HashMap::new() };
    let list = items
        .iter()
        .map(|d| {
            let code = d["code"].as_str().unwrap_or_default();
            // Syntax errors and undefined names break the program; the rest is advice.
            let severity = if code.is_empty() || code.starts_with("E9") || code.starts_with("F8") { Severity::Error } else { Severity::Warning };
            let start = at(&d["location"]["row"], &d["location"]["column"]);
            let end = at(&d["end_location"]["row"], &d["end_location"]["column"]);
            problem(start, end, severity, d["message"].as_str().unwrap_or_default(), code.into(), "ruff")
        })
        .collect();
    one_file(file, list)
}

pub fn parse_shellcheck(out: &str, file: &Path, _: &Path) -> HashMap<PathBuf, Vec<Problem>> {
    let Ok(items) = serde_json::from_str::<Vec<Value>>(out) else { return HashMap::new() };
    let list = items
        .iter()
        .filter_map(|d| {
            let severity = level(d["level"].as_str()?)?;
            let code = d["code"].as_u64().map(|c| format!("SC{c}")).unwrap_or_default();
            Some(problem(at(&d["line"], &d["column"]), at(&d["endLine"], &d["endColumn"]), severity, d["message"].as_str()?, code, "shellcheck"))
        })
        .collect();
    one_file(file, list)
}

pub fn parse_hadolint(out: &str, file: &Path, _: &Path) -> HashMap<PathBuf, Vec<Problem>> {
    let Ok(items) = serde_json::from_str::<Vec<Value>>(out) else { return HashMap::new() };
    let list = items
        .iter()
        .filter_map(|d| {
            let severity = level(d["level"].as_str()?)?;
            let start = at(&d["line"], &d["column"]);
            // Hadolint gives no end: the whole line.
            Some(problem(start, (start.0, u32::MAX / 2), severity, d["message"].as_str()?, d["code"].as_str().unwrap_or_default().into(), "hadolint"))
        })
        .collect();
    one_file(file, list)
}

/// `cargo clippy --message-format=json`: one JSON object per line, for every
/// file of the crate. Paths are relative to the Cargo workspace root: the
/// package's folder or one above it (the open folder may be a member crate).
pub fn parse_clippy(out: &str, _: &Path, root: &Path) -> HashMap<PathBuf, Vec<Problem>> {
    let mut files: HashMap<PathBuf, Vec<Problem>> = HashMap::new();
    for line in out.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        if line["reason"] != "compiler-message" {
            continue;
        }
        let msg = &line["message"];
        let Some(severity) = level(msg["level"].as_str().unwrap_or_default()) else { continue };
        let Some(span) = msg["spans"].as_array().and_then(|s| s.iter().find(|s| s["is_primary"] == true)) else { continue };
        let name = span["file_name"].as_str().unwrap_or_default();
        let package = line["manifest_path"].as_str().map(Path::new).and_then(Path::parent).unwrap_or(root);
        let path = package.ancestors().map(|a| a.join(name)).find(|p| p.is_file()).unwrap_or_else(|| root.join(name));
        let code = msg["code"]["code"].as_str().unwrap_or_default().trim_start_matches("clippy::").to_string();
        let p = problem(at(&span["line_start"], &span["column_start"]), at(&span["line_end"], &span["column_end"]), severity, msg["message"].as_str().unwrap_or_default(), code, "clippy");
        files.entry(path).or_default().push(p);
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(map: HashMap<PathBuf, Vec<Problem>>) -> Vec<Problem> {
        map.into_values().next().unwrap_or_default()
    }

    #[test]
    fn character_columns_become_utf16() {
        let p = |start, end| Problem { start, end, severity: Severity::Error, message: String::new(), source: String::new() };
        // Ruff: `x` at character 9 (0-based) after "s = '😀'"; the emoji is 2 UTF-16 units.
        let mut list = vec![p((0, 9), (0, 10)), p((1, 0), (1, 1))];
        utf16_columns("s = '😀'; x\ny", &mut list);
        assert_eq!((list[0].start, list[0].end, list[1].start), ((0, 10), (0, 11), (1, 0)));
    }

    #[test]
    fn the_gate_runs_one_at_a_time_and_then_the_newest() {
        let gate = Gate::default();
        let f = Path::new("/p/a.py");
        assert!(gate.begin("ruff", f, "v1"));
        assert!(!gate.begin("ruff", f, "v2") && !gate.begin("ruff", f, "v3"), "one runs already");
        assert!(gate.begin("eslint", f, "v1"), "another linter: its own run");
        assert_eq!(gate.finish("ruff", f).as_deref(), Some("v3"), "the newest, v2 dropped");
        assert!(!gate.begin("ruff", f, "v4"), "v3 is running");
        assert_eq!(gate.finish("ruff", f).as_deref(), Some("v4"));
        assert_eq!(gate.finish("ruff", f), None);
        assert!(gate.begin("ruff", f, "v5"), "idle again");
    }

    #[test]
    fn eslint_json_gives_errors_and_warnings() {
        let out = r#"[{"filePath":"/p/a.js","messages":[
            {"ruleId":"no-unused-vars","severity":2,"message":"'x' is assigned a value but never used.","line":1,"column":7,"endLine":1,"endColumn":8},
            {"ruleId":"semi","severity":1,"message":"Missing semicolon.","line":2,"column":10}],"errorCount":1}]"#;
        let list = one(parse_eslint(out, Path::new("/p/a.js"), Path::new("/p")));
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].start, list[0].end, list[0].severity), ((0, 6), (0, 7), Severity::Error));
        assert_eq!(list[0].message, "'x' is assigned a value but never used. (no-unused-vars)");
        assert_eq!((list[1].severity, list[1].start, list[1].end), (Severity::Warning, (1, 9), (1, 10)), "no end: one character");
        assert!(parse_eslint("Oops! Something went wrong", Path::new("a"), Path::new("/")).is_empty(), "no config: nothing");
    }

    #[test]
    fn ruff_json_marks_undefined_names_as_errors() {
        let out = r#"[{"cell":null,"code":"F821","end_location":{"column":21,"row":3},"filename":"/p/a.py","fix":null,"location":{"column":7,"row":3},"message":"Undefined name `undefined_name`","noqa_row":3,"url":"https://docs.astral.sh/ruff/rules/undefined-name"},
                     {"code":"F401","end_location":{"column":10,"row":1},"location":{"column":8,"row":1},"message":"`os` imported but unused"}]"#;
        let list = one(parse_ruff(out, Path::new("/p/a.py"), Path::new("/p")));
        assert_eq!((list[0].severity, list[0].start, list[0].end), (Severity::Error, (2, 6), (2, 20)));
        assert_eq!((list[1].severity, list[1].message.as_str()), (Severity::Warning, "`os` imported but unused (F401)"));
    }

    #[test]
    fn shellcheck_and_hadolint_skip_style_notes() {
        let sc = r#"[{"file":"-","line":2,"endLine":2,"column":6,"endColumn":8,"level":"warning","code":2034,"message":"x appears unused.","fix":null},
                    {"file":"-","line":3,"endLine":3,"column":1,"endColumn":2,"level":"style","code":2006,"message":"Use $(...)"}]"#;
        let list = one(parse_shellcheck(sc, Path::new("/p/a.sh"), Path::new("/p")));
        assert_eq!((list.len(), list[0].message.as_str(), list[0].start), (1, "x appears unused. (SC2034)", (1, 5)));
        let hl = r#"[{"code":"DL3006","column":1,"file":"-","level":"warning","line":1,"message":"Always tag the version of an image explicitly"},
                    {"code":"DL3059","column":1,"file":"-","level":"info","line":4,"message":"Multiple consecutive RUN instructions."}]"#;
        let list = one(parse_hadolint(hl, Path::new("/p/Dockerfile"), Path::new("/p")));
        assert_eq!((list.len(), list[0].severity, list[1].severity, list[0].start), (2, Severity::Warning, Severity::Info, (0, 0)));
    }

    #[test]
    fn clippy_messages_are_spread_over_the_crate_files() {
        let out = r#"{"reason":"compiler-artifact","package_id":"x"}
{"reason":"compiler-message","message":{"code":{"code":"clippy::needless_return"},"level":"warning","message":"unneeded `return` statement","spans":[{"file_name":"src/main.rs","is_primary":true,"line_start":3,"line_end":3,"column_start":5,"column_end":13}]}}
{"reason":"compiler-message","message":{"code":{"code":"E0425"},"level":"error","message":"cannot find value `y` in this scope","spans":[{"file_name":"src/lib.rs","is_primary":false,"line_start":1,"line_end":1,"column_start":1,"column_end":2},{"file_name":"src/lib.rs","is_primary":true,"line_start":9,"line_end":9,"column_start":13,"column_end":14}]}}
{"reason":"compiler-message","message":{"level":"note","message":"a note","spans":[]}}
{"reason":"build-finished","success":false}"#;
        let files = parse_clippy(out, Path::new("/p/src/main.rs"), Path::new("/p"));
        assert_eq!(files.len(), 2);
        let main = &files[Path::new("/p/src/main.rs")];
        assert_eq!((main[0].message.as_str(), main[0].start, main[0].severity), ("unneeded `return` statement (needless_return)", (2, 4), Severity::Warning));
        assert_eq!(files[Path::new("/p/src/lib.rs")][0].start, (8, 12), "the primary span");
    }

    #[test]
    fn clippy_paths_start_at_the_cargo_workspace() {
        // ws/ is the Cargo workspace, ws/app/ the member crate open as the folder.
        let ws = std::env::temp_dir().join(format!("akolade-clippy-ws-{}", std::process::id()));
        std::fs::create_dir_all(ws.join("app/src")).unwrap();
        std::fs::write(ws.join("app/src/lib.rs"), "").unwrap();
        let out = format!(
            r#"{{"reason":"compiler-message","manifest_path":"{}","message":{{"level":"warning","message":"m","spans":[{{"file_name":"app/src/lib.rs","is_primary":true,"line_start":1,"line_end":1,"column_start":1,"column_end":2}}]}}}}"#,
            ws.join("app/Cargo.toml").display()
        );
        let files = parse_clippy(&out, &ws.join("app/src/lib.rs"), &ws.join("app"));
        assert_eq!(files.keys().collect::<Vec<_>>(), [&ws.join("app/src/lib.rs")]);
        let no_manifest = out.replace(r#""manifest_path""#, r#""x""#);
        assert!(parse_clippy(&no_manifest, Path::new(""), &ws.join("app")).contains_key(&ws.join("app/src/lib.rs")), "from the open folder up");
        std::fs::remove_dir_all(ws).unwrap();
    }

    #[test]
    fn a_stopped_linter_publishes_nothing_and_drops_its_waiting_text() {
        let gate = Gate::default();
        let f = Path::new("/p/a.py");
        assert!(gate.begin("ruff", f, "v1"));
        assert!(!gate.begin("ruff", f, "v2"));
        gate.stop("ruff");
        assert_eq!(gate.finish("ruff", f), None, "v2 dropped");
        // A run through the gate: `run` fails (no such program), `publish` must not be called.
        let l = LINTERS.iter().find(|l| l.id == "ruff-check").unwrap();
        gate.stop(l.id);
        let published = std::cell::Cell::new(false);
        run_gated(&gate, l, Path::new("/nonexistent/ruff"), f, Path::new("/"), String::new(), |_| published.set(true));
        assert!(!published.get());
        gate.resume(l.id);
        run_gated(&gate, l, Path::new("/nonexistent/ruff"), f, Path::new("/"), String::new(), |_| published.set(true));
        assert!(published.get(), "on again");
    }

    /// End to end with the real Clippy on a tiny crate (skipped without it).
    #[test]
    #[ignore = "compiles a crate: a few seconds"]
    fn clippy_runs_on_a_crate() {
        let l = LINTERS.iter().find(|l| l.id == "clippy").unwrap();
        let Some(cargo) = lsp::find_program("cargo") else { return };
        let dir = std::env::temp_dir().join(format!("akolade-clippy-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn f() -> i32 {\n    return 1;\n}\nfn main() { f(); }\n").unwrap();
        let files = run(l, &cargo, &dir.join("src/main.rs"), &dir, "");
        let main = &files[&dir.join("src/main.rs")];
        assert!(main.iter().any(|p| p.message.contains("needless_return")), "{main:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
