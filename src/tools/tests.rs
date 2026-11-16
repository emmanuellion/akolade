//! "Tests": runs the project's tests (cargo, go, pytest, vitest, jest) in the
//! background and lists them by file or module, failures first.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div,
    prelude::FluentBuilder,
};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, button::ButtonVariants, h_flex, v_flex};
use serde::Deserialize;

use crate::{
    ui::plural,
    tools::{header, tool},
    ui,
    workspace::{Workspace, button, r},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Runner {
    Cargo,
    Go,
    Pytest,
    Vitest,
    Jest,
}

impl Runner {
    pub fn label(self) -> &'static str {
        match self {
            Runner::Cargo => "cargo test",
            Runner::Go => "go test ./...",
            Runner::Pytest => "pytest",
            Runner::Vitest => "vitest",
            Runner::Jest => "jest",
        }
    }
}

/// Failed first: the sort order of a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Failed,
    Passed,
    Ignored,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestCase {
    /// File or module: the list's sections.
    pub group: String,
    pub name: String,
    /// What the runner takes to run this test again.
    pub id: String,
    pub status: Status,
    pub message: String,
    pub file: Option<PathBuf>,
    /// 0-based.
    pub line: u32,
}

#[derive(Default)]
pub struct Report {
    pub tests: Vec<TestCase>,
    pub secs: f32,
    /// The run failed with no failing test (build error, missing program…).
    pub error: Option<String>,
    pub stopped: bool,
}

pub fn detect(root: &Path) -> Option<Runner> {
    let has = |f: &str| root.join(f).exists();
    if has("Cargo.toml") {
        return Some(Runner::Cargo);
    }
    if has("go.mod") {
        return Some(Runner::Go);
    }
    let py_tests = fs::read_dir(root.join("tests")).is_ok_and(|d| d.flatten().any(|e| e.path().extension().is_some_and(|x| x == "py")));
    if py_tests || ["pyproject.toml", "pytest.ini", "setup.cfg"].iter().any(|f| has(f)) {
        return Some(Runner::Pytest);
    }
    let pkg: serde_json::Value = serde_json::from_str(&fs::read_to_string(root.join("package.json")).ok()?).ok()?;
    let dep = |name: &str| ["devDependencies", "dependencies"].iter().any(|k| pkg[k].get(name).is_some());
    if dep("vitest") {
        Some(Runner::Vitest)
    } else if dep("jest") {
        Some(Runner::Jest)
    } else {
        None
    }
}

fn regex_escape(s: &str) -> String {
    s.chars().fold(String::new(), |mut out, c| {
        if r"\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

/// The command line; `failed` not empty: only those tests again.
pub fn command(runner: Runner, root: &Path, out: &Path, failed: &[TestCase]) -> Vec<String> {
    let out = out.display();
    let unique = |of: &dyn Fn(&TestCase) -> String| {
        let mut list: Vec<String> = failed.iter().map(of).collect();
        list.dedup();
        list
    };
    let mut argv: Vec<String> = match runner {
        Runner::Cargo => ["cargo", "test", "--no-fail-fast"].map(String::from).into(),
        Runner::Go => ["go", "test", "-json"].map(String::from).into(),
        Runner::Pytest => {
            let venv = root.join(".venv/bin/pytest");
            let mut argv: Vec<String> = if venv.is_file() {
                vec![venv.display().to_string()]
            } else if crate::lsp::find_program("pytest").is_some() {
                vec!["pytest".into()]
            } else {
                ["python3", "-m", "pytest"].map(String::from).into()
            };
            // xunit1: the only JUnit flavour where pytest writes file and line.
            argv.extend(["-q".into(), "-o".into(), "junit_family=xunit1".into(), format!("--junitxml={out}")]);
            argv
        }
        Runner::Vitest => vec!["npx".into(), "--no".into(), "vitest".into(), "run".into(), "--reporter=json".into(), format!("--outputFile={out}")],
        Runner::Jest => vec!["npx".into(), "--no".into(), "jest".into(), "--json".into(), format!("--outputFile={out}")],
    };
    if failed.is_empty() {
        if runner == Runner::Go {
            argv.push("./...".into());
        }
        return argv;
    }
    match runner {
        Runner::Cargo => argv.extend(["--".into(), "--exact".into()].into_iter().chain(unique(&|t| t.id.clone()))),
        Runner::Go => {
            // Subtests run again with their parent.
            let names = unique(&|t| t.id.split('/').next().unwrap_or_default().to_string());
            argv.extend(["-run".into(), format!("^({})$", names.join("|"))]);
            argv.extend(unique(&|t| t.group.clone()));
        }
        Runner::Pytest => argv.extend(unique(&|t| t.id.clone())),
        Runner::Vitest | Runner::Jest => {
            argv.extend(unique(&|t| t.group.clone()));
            let names = unique(&|t| regex_escape(&t.id));
            argv.extend(["-t".into(), format!("({})", names.join("|"))]);
        }
    }
    argv
}

/// Where a Rust panic happened: "thread 'x' panicked at src/lib.rs:10:9:".
fn panic_location(msg: &str) -> Option<(String, u32)> {
    let at = msg.lines().find_map(|l| l.split_once(" panicked at ").map(|(_, a)| a))?;
    let mut parts = at.trim_end_matches(':').rsplitn(3, ':');
    let (_column, line, file) = (parts.next()?, parts.next()?.parse().ok()?, parts.next()?);
    Some((file.to_string(), line))
}

/// libtest's text: `test a::b ... ok|FAILED|ignored`, then a `---- a::b stdout ----`
/// section per failure.
pub fn parse_cargo(out: &str, root: &Path) -> Vec<TestCase> {
    let mut tests = vec![];
    let mut messages: Vec<(String, String)> = vec![];
    let mut section: Option<(String, String)> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("---- ").and_then(|l| l.strip_suffix(" ----")) {
            messages.extend(section.take());
            let name = rest.rsplit_once(' ').map_or(rest, |(name, _stdout)| name);
            section = Some((name.to_string(), String::new()));
            continue;
        }
        if line == "failures:" || line.starts_with("test result:") {
            messages.extend(section.take());
        }
        if let Some((_, msg)) = &mut section {
            if !line.starts_with("note: run with `RUST_BACKTRACE") {
                msg.push_str(line);
                msg.push('\n');
            }
            continue;
        }
        let Some((name, result)) = line.strip_prefix("test ").and_then(|l| l.rsplit_once(" ... ")) else { continue };
        let status = match result {
            "ok" => Status::Passed,
            "FAILED" => Status::Failed,
            r if r.starts_with("ignored") => Status::Ignored,
            _ => continue,
        };
        let name = name.strip_suffix(" - should panic").unwrap_or(name);
        let mut test = TestCase { group: String::new(), name: name.into(), id: name.into(), status, message: String::new(), file: None, line: 0 };
        // Doc tests: "src/lib.rs - add (line 3)".
        if let Some((file, rest)) = name.split_once(" - ") {
            let line = rest.rsplit_once("(line ").and_then(|(_, l)| l.trim_end_matches(')').parse::<u32>().ok());
            (test.group, test.name, test.file, test.line) = (file.into(), rest.into(), Some(root.join(file)), line.unwrap_or(1).saturating_sub(1));
        } else if let Some((module, short)) = name.rsplit_once("::") {
            (test.group, test.name) = (module.into(), short.into());
        }
        tests.push(test);
    }
    for (name, msg) in messages {
        let Some(test) = tests.iter_mut().find(|t| t.id == name) else { continue };
        test.message = msg.trim().to_string();
        if test.file.is_none()
            && let Some((file, line)) = panic_location(&msg)
        {
            (test.file, test.line) = (Some(root.join(file)), line.saturating_sub(1));
        }
    }
    tests
}

#[derive(Deserialize)]
struct GoEvent {
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Package", default)]
    package: String,
    #[serde(rename = "Test")]
    test: Option<String>,
    #[serde(rename = "Output", default)]
    output: String,
}

/// The module path in go.mod, to find a package's folder.
fn go_module(root: &Path) -> String {
    let text = fs::read_to_string(root.join("go.mod")).unwrap_or_default();
    text.lines().find_map(|l| l.strip_prefix("module ")).map(|m| m.trim().trim_matches('"').to_string()).unwrap_or_default()
}

/// `go test -json`: one event per line; a test's output comes before its result.
pub fn parse_go(out: &str, root: &Path, module: &str) -> Vec<TestCase> {
    let mut outputs: HashMap<(String, String), String> = HashMap::new();
    let mut tests = vec![];
    for e in out.lines().filter_map(|l| serde_json::from_str::<GoEvent>(l).ok()) {
        let Some(test) = e.test else { continue };
        let key = (e.package.clone(), test.clone());
        let status = match e.action.as_str() {
            "output" => {
                outputs.entry(key).or_default().push_str(&e.output);
                continue;
            }
            "pass" => Status::Passed,
            "fail" => Status::Failed,
            "skip" => Status::Ignored,
            _ => continue,
        };
        let output = outputs.remove(&key).unwrap_or_default();
        let message: Vec<&str> = output.lines().filter(|l| !l.trim_start().starts_with("=== ") && !l.trim_start().starts_with("--- ")).collect();
        let message = if status == Status::Failed { message.join("\n").trim().to_string() } else { String::new() };
        // "    math_test.go:9: got 3, want 4": relative to the package's folder.
        let at = message.lines().find_map(|l| {
            let (file, rest) = l.trim().split_once(".go:")?;
            Some((format!("{file}.go"), rest.split(':').next()?.parse::<u32>().ok()?))
        });
        let dir = root.join(e.package.strip_prefix(module).unwrap_or_default().trim_start_matches('/'));
        tests.push(TestCase {
            group: e.package,
            name: test.clone(),
            id: test,
            status,
            message,
            file: at.as_ref().map(|(f, _)| dir.join(f)),
            line: at.map_or(0, |(_, l)| l.saturating_sub(1)),
        });
    }
    tests
}

fn unescape(s: &str) -> String {
    let (mut out, mut rest) = (String::new(), s);
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let entity = rest.find(';').map(|end| (&rest[1..end], end));
        let c = entity.and_then(|(e, _)| match e {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => e.strip_prefix("#x").map_or_else(|| e.strip_prefix('#').and_then(|n| n.parse().ok()), |h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32),
        });
        match (c, entity) {
            (Some(c), Some((_, end))) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out + rest
}

fn attr(attrs: &str, key: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some((k, value)) = rest.split_once('=') {
        let value = value.trim_start();
        let quote = value.chars().next()?;
        let (v, after) = value[1..].split_once(quote)?;
        if k.trim() == key {
            return Some(unescape(v));
        }
        rest = after;
    }
    None
}

/// pytest's JUnit XML: `testcase` (classname, name, file, line) and its `failure`,
/// `error` or `skipped`.
/// shortcut: no CDATA nor comments (pytest's ElementTree writes none), use a real
/// XML parser if other JUnit producers are read.
pub fn parse_junit(xml: &str, root: &Path) -> Vec<TestCase> {
    let mut tests = vec![];
    let mut current: Option<TestCase> = None;
    // Inside `<failure>` or `<error>`: its text (the traceback).
    let mut body: Option<String> = None;
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        if let Some(b) = &mut body {
            b.push_str(&unescape(&rest[..i]));
        }
        rest = &rest[i + 1..];
        let end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..end];
        rest = &rest[(end + 1).min(rest.len())..];
        let closed = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, attrs) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        let get = |key| attr(attrs, key).unwrap_or_default();
        match name {
            "testcase" => {
                let (class, test) = (get("classname"), get("name"));
                let file = attr(attrs, "file").unwrap_or_else(|| format!("{}.py", class.replace('.', "/")));
                // "tests.test_math.TestSub" → node id "tests/test_math.py::TestSub::test_x".
                let module = file.trim_end_matches(".py").replace('/', ".");
                let shown = match class.strip_prefix(&module).and_then(|c| c.strip_prefix('.')) {
                    Some(c) => format!("{c}::{test}"),
                    None => test,
                };
                current = Some(TestCase {
                    id: format!("{file}::{shown}"),
                    name: shown,
                    file: Some(root.join(&file)),
                    group: file,
                    status: Status::Passed,
                    message: String::new(),
                    line: get("line").parse().unwrap_or(0),
                });
                if closed {
                    tests.extend(current.take());
                }
            }
            "/testcase" => tests.extend(current.take()),
            "failure" | "error" => {
                if let Some(c) = &mut current {
                    (c.status, c.message) = (Status::Failed, get("message"));
                    if !closed {
                        body = Some(String::new());
                    }
                }
            }
            "/failure" | "/error" => {
                if let (Some(c), Some(b)) = (&mut current, body.take())
                    && !b.trim().is_empty()
                {
                    c.message = b.trim().to_string();
                }
            }
            "skipped" => {
                if let Some(c) = &mut current {
                    (c.status, c.message) = (Status::Ignored, get("message"));
                }
            }
            _ => {}
        }
    }
    tests
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct JestReport {
    test_results: Vec<JestFile>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct JestFile {
    name: String,
    status: String,
    message: String,
    assertion_results: Vec<JestTest>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct JestTest {
    full_name: String,
    title: String,
    ancestor_titles: Vec<String>,
    status: String,
    failure_messages: Vec<String>,
    location: Option<JestLocation>,
}

#[derive(Deserialize)]
struct JestLocation {
    line: u32,
}

/// The JSON report of jest (`--json`) or vitest (`--reporter=json`): same shape.
pub fn parse_jest(json: &str, root: &Path) -> Vec<TestCase> {
    let report: JestReport = serde_json::from_str(json).unwrap_or_default();
    let mut tests = vec![];
    for f in report.test_results {
        let path = PathBuf::from(&f.name);
        let group = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        // A file that could not run (syntax error…) has no tests but a message.
        if f.assertion_results.is_empty() && f.status == "failed" {
            let message = f.message.trim().to_string();
            tests.push(TestCase { group: group.clone(), name: "Le fichier n'a pas pu être lancé".into(), id: String::new(), status: Status::Failed, message, file: Some(path.clone()), line: 0 });
        }
        for t in f.assertion_results {
            let status = match t.status.as_str() {
                "passed" => Status::Passed,
                "failed" => Status::Failed,
                _ => Status::Ignored,
            };
            let mut titles = t.ancestor_titles;
            titles.push(t.title);
            tests.push(TestCase {
                group: group.clone(),
                name: titles.join(" › "),
                id: if t.full_name.is_empty() { titles.join(" ") } else { t.full_name },
                status,
                message: t.failure_messages.join("\n\n").trim().to_string(),
                file: Some(path.clone()),
                line: t.location.map_or(0, |l| l.line.saturating_sub(1)),
            });
        }
    }
    tests
}

/// SIGTERM to the run's whole process group (cargo and its test binaries, npx and node…).
fn kill_group(pid: u32) {
    if pid != 0 {
        crate::platform::kill_tree(pid);
    }
}

/// Runs the tests (blocking: in the background). `pid` gets the child's process group.
pub fn execute(runner: Runner, root: &Path, failed: &[TestCase], pid: &AtomicU32) -> Report {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let out = std::env::temp_dir().join(format!("akolade-tests-{}-{nanos}", std::process::id()));
    let argv = command(runner, root, &out, failed);
    let start = Instant::now();
    let mut cmd = crate::platform::command(&argv[0]);
    cmd.args(&argv[1..]).current_dir(root).env("NO_COLOR", "1").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = crate::platform::own_group(&mut cmd).spawn();
    let output = child.and_then(|child| {
        pid.store(child.id(), Ordering::Relaxed);
        child.wait_with_output()
    });
    let output = match output {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Report { error: Some(format!("{} est introuvable : installez-le ou ajoutez-le au PATH.", argv[0])), ..Default::default() },
        Err(e) => return Report { error: Some(format!("Impossible de lancer {} : {e}", argv[0])), ..Default::default() },
    };
    let file = fs::read_to_string(&out).unwrap_or_default();
    let _ = fs::remove_file(&out);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut tests = match runner {
        Runner::Cargo => parse_cargo(&stdout, root),
        Runner::Go => parse_go(&stdout, root, &go_module(root)),
        Runner::Pytest => parse_junit(&file, root),
        Runner::Vitest | Runner::Jest => parse_jest(&file, root),
    };
    for t in &mut tests {
        t.file = t.file.take().filter(|f| f.is_file());
    }
    let failing: HashSet<String> = tests.iter().filter(|t| t.status == Status::Failed).map(|t| t.group.clone()).collect();
    tests.sort_by_key(|t| (!failing.contains(&t.group), t.group.clone(), t.status, t.name.clone()));
    let error = (!output.status.success() && failing.is_empty()).then(|| {
        let all = format!("{}\n{stdout}", String::from_utf8_lossy(&output.stderr));
        let lines: Vec<&str> = all.trim().lines().collect();
        lines[lines.len().saturating_sub(40)..].join("\n")
    });
    Report { tests, secs: start.elapsed().as_secs_f32(), error, stopped: false }
}

fn duration_fr(secs: f32) -> String {
    if secs < 60. { format!("{secs:.1} s").replace('.', ",") } else { format!("{} min {:02} s", secs as u32 / 60, secs as u32 % 60) }
}

pub struct TestsView {
    workspace: WeakEntity<Workspace>,
    root: Option<PathBuf>,
    runner: Option<Runner>,
    /// While a run goes on: its process group (0 until started).
    running: Option<Arc<AtomicU32>>,
    stopping: bool,
    report: Option<Report>,
    /// Failures showing their message (indexes in the report).
    unfolded: HashSet<usize>,
    _run: Task<()>,
}

impl TestsView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>) -> Self {
        let runner = root.as_deref().and_then(detect);
        Self { workspace, root, runner, running: None, stopping: false, report: None, unfolded: HashSet::new(), _run: Task::ready(()) }
    }

    fn run(&mut self, only_failed: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(runner), Some(root)) = (self.runner, self.root.clone()) else { return };
        if self.running.is_some() {
            return;
        }
        // The workspace's answer: one question per folder, for its scripts and its tests alike.
        if !self.workspace.upgrade().is_some_and(|w| w.read(cx).trusted()) {
            let answer = window.prompt(
                PromptLevel::Warning,
                "Lancer les tests de ce projet ?",
                Some(&format!("Les tests exécutent le code de ce projet ({}).\n\nN'acceptez que si vous faites confiance à ce projet.", runner.label())),
                &["Lancer", "Annuler"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                if answer.await == Ok(0) {
                    let _ = this.update_in(cx, |this, window, cx| {
                        // The yes was for this project only, not one opened meanwhile.
                        if this.root.as_ref() != Some(&root) {
                            return;
                        }
                        let _ = this.workspace.update(cx, |w, cx| {
                            // This view may not have seen a folder change yet: the workspace's root decides.
                            if w.root() == Some(root.as_path()) {
                                w.trust(cx);
                            }
                        });
                        this.run(only_failed, window, cx);
                    });
                }
            })
            .detach();
            return;
        }
        let failed: Vec<TestCase> = if only_failed { self.report.iter().flat_map(|r| &r.tests).filter(|t| t.status == Status::Failed).cloned().collect() } else { vec![] };
        let pid = Arc::new(AtomicU32::new(0));
        (self.running, self.stopping) = (Some(pid.clone()), false);
        let task = cx.background_executor().spawn(async move { execute(runner, &root, &failed, &pid) });
        self._run = cx.spawn(async move |this, cx| {
            let mut report = task.await;
            let _ = this.update(cx, |this, cx| {
                report.stopped = this.stopping;
                (this.running, this.report) = (None, Some(report));
                this.unfolded.clear();
                cx.notify();
            });
        });
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(pid) = &self.running {
            self.stopping = true;
            let pid = pid.load(Ordering::Relaxed);
            cx.background_executor().spawn(async move { kill_group(pid) }).detach();
            cx.notify();
        }
    }

    fn open(&self, t: &TestCase, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(file), Some(ws)) = (t.file.clone(), self.workspace.upgrade()) else { return };
        ws.update(cx, |ws, cx| ws.open_at(file, t.line, 0, window, cx));
    }

    fn render_test(&self, i: usize, t: &TestCase, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let (muted, hover, red, mono) = (theme.muted_foreground, ui::hover_bg(cx), theme.red, theme.mono_font_family.clone());
        let (icon, color) = match t.status {
            Status::Passed => (IconName::CircleCheck, theme.green),
            Status::Failed => (IconName::CircleX, red),
            Status::Ignored => (IconName::Minus, muted),
        };
        let failed = t.status == Status::Failed;
        let unfolded = self.unfolded.contains(&i);
        let (t1, t2) = (t.clone(), t.clone());
        v_flex()
            .flex_shrink_0()
            .child(
                h_flex()
                    .id(SharedString::from(format!("test-{i}")))
                    .gap(r(8.))
                    .px(r(8.))
                    .py(r(4.))
                    .rounded(r(6.))
                    .when(failed || t.file.is_some(), |d| d.cursor_pointer().hover(|s| s.bg(hover)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if !failed {
                            return this.open(&t1, window, cx);
                        }
                        if !this.unfolded.remove(&i) {
                            this.unfolded.insert(i);
                        }
                        cx.notify();
                    }))
                    .child(Icon::new(if unfolded { IconName::ChevronDown } else { IconName::ChevronRight }).small().text_color(if failed { muted } else { gpui::transparent_black() }))
                    .child(Icon::new(icon).small().text_color(color))
                    .child(div().flex_1().min_w_0().truncate().child(t.name.clone()))
                    .when(failed && t.file.is_some(), |d| {
                        d.child(button(SharedString::from(format!("open-{i}"))).ghost().xsmall().icon(IconName::ExternalLink).tooltip("Ouvrir le test").on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open(&t2, window, cx);
                        })))
                    }),
            )
            .when(failed && unfolded, |d| {
                let message = if t.message.is_empty() { "Pas de message.".to_string() } else { t.message.clone() };
                d.child(div().ml(r(44.)).mr(r(8.)).my(r(4.)).p(r(10.)).rounded(r(6.)).bg(red.opacity(0.06)).font_family(mono).text_size(r(12.)).child(message))
            })
    }

    fn render_report(&self, report: &Report, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let (muted, border, green, red, mono) = (theme.muted_foreground, theme.border, theme.green, theme.red, theme.mono_font_family.clone());
        let count = |s| report.tests.iter().filter(|t| t.status == s).count();
        let summary = h_flex()
            .flex_shrink_0()
            .px(r(24.))
            .py(r(10.))
            .gap(r(18.))
            .border_b_1()
            .border_color(border)
            .text_size(r(13.))
            .child(div().text_color(green).child(format!("✓ {}", plural(count(Status::Passed), "réussi"))))
            .child(div().text_color(red).child(format!("✗ {}", plural(count(Status::Failed), "échoué"))))
            .child(div().text_color(muted).child(plural(count(Status::Ignored), "ignoré")))
            .child(div().flex_1())
            .child(div().text_color(muted).child(duration_fr(report.secs)));
        // shortcut: every row is drawn (no virtual list, rows unfold to any height); fine for a few thousand tests.
        let mut list = v_flex().id("tests").flex_1().overflow_y_scroll().p(r(16.)).gap(r(2.));
        if report.stopped {
            list = list.child(div().flex_shrink_0().pb(r(8.)).text_color(muted).child("Tests arrêtés : les résultats sont incomplets."));
        } else if let Some(e) = &report.error {
            list = list.child(
                v_flex()
                    .flex_shrink_0()
                    .mb(r(8.))
                    .p(r(12.))
                    .gap(r(6.))
                    .rounded(r(8.))
                    .bg(red.opacity(0.06))
                    .child(div().font_weight(FontWeight::SEMIBOLD).text_color(red).child("Les tests n'ont pas pu aller au bout"))
                    .child(div().font_family(mono.clone()).text_size(r(12.)).child(e.clone())),
            );
        } else if report.tests.is_empty() {
            list = list.child(ui::empty_state((tool("tests").icon)(), green, "Aucun test trouvé", "Le lanceur n'a trouvé aucun test dans ce projet.", cx));
        }
        let mut group = None;
        for (i, t) in report.tests.iter().enumerate() {
            if group != Some(&t.group) {
                group = Some(&t.group);
                let title = if t.group.is_empty() { "(racine)".to_string() } else { t.group.clone() };
                list = list.child(div().flex_shrink_0().pt(r(if i == 0 { 0. } else { 12. })).pb(r(4.)).px(r(8.)).truncate().text_size(r(12.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(title));
            }
            list = list.child(self.render_test(i, t, cx));
        }
        v_flex().flex_1().min_h_0().child(summary).child(list).into_any_element()
    }
}

impl Drop for TestsView {
    // A closed tab leaves no test running.
    fn drop(&mut self) {
        if let Some(pid) = &self.running {
            kill_group(pid.load(Ordering::Relaxed));
        }
    }
}

impl Render for TestsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Another folder opened: its own runner and results.
        let root = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf));
        if root != self.root {
            // shortcut: a run whose process has not started yet (pid 0) is not killed, only forgotten; track the Child if it matters.
            self.stop(cx);
            let fresh = Self::new(self.workspace.clone(), root);
            *self = fresh;
        }
        let running = self.running.is_some();
        let failed = self.report.iter().flat_map(|r| &r.tests).any(|t| t.status == Status::Failed);
        let mut actions = vec![];
        if running {
            actions.push(button("stop").outline().small().label(if self.stopping { "Arrêt…" } else { "Arrêter" }).on_click(cx.listener(|this, _, _, cx| this.stop(cx))).into_any_element());
        } else if self.runner.is_some() {
            if failed {
                actions.push(button("rerun").outline().small().label("Relancer les échecs").on_click(cx.listener(|this, _, window, cx| this.run(true, window, cx))).into_any_element());
            }
            let run = button("run").primary().small().icon(Icon::empty().path("icons/play.svg")).label("Lancer tous les tests");
            actions.push(run.on_click(cx.listener(|this, _, window, cx| this.run(false, window, cx))).into_any_element());
        }
        let subtitle = self.runner.map_or("Les tests du projet, d'un coup d'œil.".to_string(), |r| format!("Lanceur : {}", r.label()));
        let header = header(tool("tests"), subtitle, actions, cx);
        let empty = |title: &str, text: &str| ui::empty_state((tool("tests").icon)(), cx.theme().green, title, text, cx).into_any_element();
        let body = if self.runner.is_none() {
            empty("Aucun test reconnu", "Ouvrez un projet Cargo, Go, pytest, Jest ou Vitest : ses tests se lancent d'ici.")
        } else if running {
            ui::loading("Tests en cours…", cx).into_any_element()
        } else if let Some(report) = &self.report {
            self.render_report(report, cx)
        } else {
            empty("Prêt", "Lancez les tests pour voir ce qui passe et ce qui casse.")
        };
        v_flex().size_full().child(header).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/p")
    }

    #[test]
    fn cargo_lines_and_failure_sections() {
        let out = r#"
running 4 tests
test tools::docker::tests::statuses_read_in_french ... ok
test tools::docker::tests::parse ... FAILED
test slow ... ignored, needs network
test math::tests::boom - should panic ... ok

failures:

---- tools::docker::tests::parse stdout ----

thread 'tools::docker::tests::parse' panicked at src/tools/docker.rs:285:9:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    tools::docker::tests::parse

test result: FAILED. 2 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s

   Doc-tests demo

running 1 test
test src/lib.rs - add (line 3) ... ok
"#;
        let t = parse_cargo(out, &root());
        assert_eq!(t.len(), 5);
        assert_eq!((t[0].group.as_str(), t[0].name.as_str(), t[0].status), ("tools::docker::tests", "statuses_read_in_french", Status::Passed));
        let f = &t[1];
        assert_eq!((f.status, f.id.as_str()), (Status::Failed, "tools::docker::tests::parse"));
        assert!(f.message.starts_with("thread 'tools::docker::tests::parse' panicked") && f.message.ends_with("right: 2"), "{}", f.message);
        assert_eq!((f.file.clone(), f.line), (Some(root().join("src/tools/docker.rs")), 284));
        assert_eq!((t[2].group.as_str(), t[2].status), ("", Status::Ignored));
        assert_eq!((t[3].id.as_str(), t[3].name.as_str()), ("math::tests::boom", "boom"));
        assert_eq!((t[4].group.as_str(), t[4].name.as_str(), t[4].line), ("src/lib.rs", "add (line 3)", 2));
    }

    #[test]
    fn go_json_events() {
        let out = r#"{"Time":"2026-01-01T10:00:00Z","Action":"start","Package":"example.com/calc/math"}
{"Time":"2026-01-01T10:00:00Z","Action":"run","Package":"example.com/calc/math","Test":"TestAdd"}
{"Time":"2026-01-01T10:00:00Z","Action":"output","Package":"example.com/calc/math","Test":"TestAdd","Output":"=== RUN   TestAdd\n"}
{"Time":"2026-01-01T10:00:00Z","Action":"output","Package":"example.com/calc/math","Test":"TestAdd","Output":"    math_test.go:9: got 3, want 4\n"}
{"Time":"2026-01-01T10:00:00Z","Action":"output","Package":"example.com/calc/math","Test":"TestAdd","Output":"--- FAIL: TestAdd (0.00s)\n"}
{"Time":"2026-01-01T10:00:00Z","Action":"fail","Package":"example.com/calc/math","Test":"TestAdd","Elapsed":0}
{"Time":"2026-01-01T10:00:00Z","Action":"run","Package":"example.com/calc/math","Test":"TestSub"}
{"Time":"2026-01-01T10:00:00Z","Action":"output","Package":"example.com/calc/math","Test":"TestSub","Output":"--- PASS: TestSub (0.00s)\n"}
{"Time":"2026-01-01T10:00:00Z","Action":"pass","Package":"example.com/calc/math","Test":"TestSub","Elapsed":0}
{"Time":"2026-01-01T10:00:00Z","Action":"skip","Package":"example.com/calc/math","Test":"TestSlow","Elapsed":0}
{"Time":"2026-01-01T10:00:00Z","Action":"output","Package":"example.com/calc/math","Output":"FAIL\n"}
{"Time":"2026-01-01T10:00:00Z","Action":"fail","Package":"example.com/calc/math","Elapsed":0.01}"#;
        let t = parse_go(out, &root(), "example.com/calc");
        assert_eq!(t.len(), 3);
        assert_eq!((t[0].name.as_str(), t[0].status, t[0].message.as_str()), ("TestAdd", Status::Failed, "math_test.go:9: got 3, want 4"));
        assert_eq!((t[0].file.clone(), t[0].line), (Some(root().join("math/math_test.go")), 8));
        assert_eq!((t[1].status, t[1].message.as_str(), t[2].status), (Status::Passed, "", Status::Ignored));
    }

    #[test]
    fn pytest_junit_xml() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><testsuites><testsuite name="pytest" errors="0" failures="1" skipped="1" tests="3" time="0.031"><testcase classname="tests.test_math" file="tests/test_math.py" line="2" name="test_add" time="0.001" /><testcase classname="tests.test_math.TestSub" file="tests/test_math.py" line="7" name="test_sub[2-1]" time="0.001"><failure message="assert 1 == 2&#10; +  where 1 = sub(2, 1)">self = &lt;tests.test_math.TestSub object&gt;

&gt;       assert sub(2, 1) == 2
E       assert 1 == 2

tests/test_math.py:9: AssertionError</failure></testcase><testcase classname="tests.test_math" file="tests/test_math.py" line="11" name="test_slow" time="0.000"><skipped type="pytest.skip" message="trop lent">tests/test_math.py:12: trop lent</skipped></testcase></testsuite></testsuites>"#;
        let t = parse_junit(xml, &root());
        assert_eq!(t.len(), 3);
        assert_eq!((t[0].id.as_str(), t[0].status, t[0].line), ("tests/test_math.py::test_add", Status::Passed, 2));
        let f = &t[1];
        assert_eq!((f.id.as_str(), f.name.as_str(), f.group.as_str()), ("tests/test_math.py::TestSub::test_sub[2-1]", "TestSub::test_sub[2-1]", "tests/test_math.py"));
        assert_eq!(f.status, Status::Failed);
        assert!(f.message.starts_with("self = <tests.test_math.TestSub object>\n\n>       assert") && f.message.ends_with("AssertionError"), "{}", f.message);
        assert_eq!((t[2].status, t[2].message.as_str()), (Status::Ignored, "trop lent"));
        assert_eq!(unescape("a &amp;&lt; &#x41;&#66; & b"), "a &< AB & b");
    }

    #[test]
    fn jest_and_vitest_json() {
        let json = r#"{"numFailedTests":1,"numPassedTests":1,"success":false,"testResults":[
{"name":"/p/src/sum.test.ts","status":"failed","message":"","startTime":1,"endTime":2,"assertionResults":[
 {"ancestorTitles":["sum"],"fullName":"sum adds","status":"passed","title":"adds","duration":2,"failureMessages":[],"location":{"line":4,"column":3}},
 {"ancestorTitles":["sum"],"fullName":"sum handles (negatives)","status":"failed","title":"handles (negatives)","duration":3,"failureMessages":["AssertionError: expected -1 to be 1\n    at src/sum.test.ts:9:22"],"location":null},
 {"ancestorTitles":[],"fullName":"later","status":"todo","title":"later","failureMessages":[]}]},
{"name":"/p/src/broken.test.ts","status":"failed","message":"SyntaxError: Unexpected token","assertionResults":[]}]}"#;
        let t = parse_jest(json, &root());
        assert_eq!(t.len(), 4);
        assert_eq!((t[0].group.as_str(), t[0].name.as_str(), t[0].line), ("src/sum.test.ts", "sum › adds", 3));
        assert_eq!((t[1].status, t[1].message.as_str()), (Status::Failed, "AssertionError: expected -1 to be 1\n    at src/sum.test.ts:9:22"));
        assert_eq!((t[2].status, t[3].status, t[3].message.as_str()), (Status::Ignored, Status::Failed, "SyntaxError: Unexpected token"));
        let argv = command(Runner::Vitest, &root(), Path::new("/t/o"), &t[1..2]);
        assert_eq!(argv[argv.len() - 3..], ["src/sum.test.ts", "-t", r"(sum handles \(negatives\))"]);
    }

    #[test]
    fn reruns_only_the_failures() {
        let case = |group: &str, id: &str| TestCase { group: group.into(), name: id.into(), id: id.into(), status: Status::Failed, message: String::new(), file: None, line: 0 };
        let out = Path::new("/t/o");
        assert_eq!(command(Runner::Cargo, &root(), out, &[case("m", "m::a"), case("m", "m::b")]).join(" "), "cargo test --no-fail-fast -- --exact m::a m::b");
        assert_eq!(command(Runner::Go, &root(), out, &[case("ex/p", "TestA/sub"), case("ex/p", "TestB")]).join(" "), "go test -json -run ^(TestA|TestB)$ ex/p");
        assert_eq!(command(Runner::Go, &root(), out, &[]).join(" "), "go test -json ./...");
        assert!(command(Runner::Pytest, &root(), out, &[case("t.py", "t.py::test_x")]).join(" ").ends_with("-q -o junit_family=xunit1 --junitxml=/t/o t.py::test_x"));
    }

    #[test]
    fn detects_the_runner() {
        let dir = std::env::temp_dir().join(format!("akolade-tests-detect-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(detect(&dir), None);
        fs::write(dir.join("package.json"), r#"{"devDependencies":{"vitest":"^2"}}"#).unwrap();
        assert_eq!(detect(&dir), Some(Runner::Vitest));
        fs::write(dir.join("go.mod"), "module example.com/calc\n\ngo 1.22\n").unwrap();
        assert_eq!((detect(&dir), go_module(&dir).as_str()), (Some(Runner::Go), "example.com/calc"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn runs_cargo_test_on_a_tiny_crate_then_only_its_failure() {
        let dir = std::env::temp_dir().join(format!("akolade-tests-e2e-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        fs::write(dir.join("src/lib.rs"), "#[test] fn ok() {}\n#[test] fn bad() { assert_eq!(1, 2); }\n").unwrap();
        let report = execute(Runner::Cargo, &dir, &[], &AtomicU32::new(0));
        let summary: Vec<_> = report.tests.iter().map(|t| (t.name.as_str(), t.status, t.line)).collect();
        assert_eq!(summary, [("bad", Status::Failed, 1), ("ok", Status::Passed, 0)], "{:?}", report.error);
        assert_eq!(report.tests[0].file, Some(dir.join("src/lib.rs")));
        let again = execute(Runner::Cargo, &dir, &report.tests[..1], &AtomicU32::new(0));
        assert_eq!(again.tests.len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stop_kills_the_whole_process_group() {
        // A shell and its own child, like cargo and a test binary.
        let child = crate::platform::own_group(crate::platform::command("sh").args(["-c", "sleep 30; true"]).stdout(Stdio::piped())).spawn().unwrap();
        // Once the shell has started its child, as when a run is stopped (a slow CI machine
        // may not have forked `sleep` yet right after the spawn).
        std::thread::sleep(std::time::Duration::from_millis(300));
        let start = Instant::now();
        kill_group(child.id());
        let _ = child.wait_with_output();
        assert!(start.elapsed().as_secs() < 5);
    }
}
