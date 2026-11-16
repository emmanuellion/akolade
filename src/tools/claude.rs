//! "Claude Code": a conversation with this Mac's `claude` program, run in the
//! project's folder (`claude -p`, JSON lines both ways: your settings, plugins
//! and subscription apply). Answers stream in, each tool it uses shows as a
//! card, permission requests are answered here, and the side panel follows
//! tokens, cost, context, subscription limits and the usage day by day.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, SystemTime},
};

use futures::{StreamExt, channel::mpsc};
use gpui::{
    AnyElement, AppContext, Context, Div, Entity, FontWeight, Hsla, ListAlignment, ListState, PromptLevel, list, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder, px, relative, rgb,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    text::TextView,
    tooltip::Tooltip,
    v_flex,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    lsp,
    settings::{AppSettings, app_dir, read_json},
    tools::{header, in_terminal, tile, tool},
    ui,
    workspace::{Workspace, button, r},
};

/// Claude's own orange: the tool's accent.
pub const ORANGE: u32 = 0xd97757;
pub const MODELS: [(&str, &str); 4] = [("Par défaut", ""), ("Opus", "opus"), ("Sonnet", "sonnet"), ("Haiku", "haiku")];
pub const MODES: [(&str, &str, &str); 4] = [
    ("Demander", "manual", "Chaque commande et chaque modification attend votre accord"),
    ("Modifs auto", "acceptEdits", "Les modifications de fichiers passent seules, les commandes attendent votre accord"),
    ("Plan", "plan", "Claude explore le projet et propose un plan, sans rien modifier"),
    ("Tout autoriser", "bypassPermissions", "Claude agit sans rien demander : commandes et modifications passent toutes seules"),
];
const BYPASS: usize = 3;
/// `--effort`: (value, label); "" lets Claude Code choose.
pub const EFFORTS: [(&str, &str); 6] = [("", "Par défaut"), ("low", "Faible"), ("medium", "Moyen"), ("high", "Élevé"), ("xhigh", "Très élevé"), ("max", "Maximum")];
const SUGGESTIONS: [&str; 3] = ["Explique-moi l'architecture de ce projet", "Cherche des bugs dans le fichier ouvert", "Écris des tests pour le fichier ouvert"];

// ---------------------------------------------------------------- protocol

/// Tokens and their price (estimated at the API's list price).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

impl Usage {
    fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cost += o.cost;
    }

    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// What a line of `claude -p --output-format stream-json` means here.
#[derive(Debug, PartialEq)]
pub enum Event {
    Init { session: String, model: String, subscription: bool },
    TextStart,
    Text(String),
    Thinking,
    /// Tokens the model reads for this request: how full the context is.
    Context(u64),
    ToolUse { id: String, name: String, input: Value },
    ToolResult { id: String, text: String, error: bool },
    /// A permission request (`can_use_tool`), for the card of `tool_id`.
    Ask { request: String, tool_id: String, tool: String, input: Value },
    /// Another control request: answered "not supported" so the CLI never waits.
    Unsupported { request: String },
    /// End of a turn: per model, its usage since the last result and its context size.
    Result { error: bool, text: String, secs: f64, models: Vec<(String, Usage, u64)> },
    /// Subscription windows: name, part used (0–1), reset time (Unix seconds).
    Limits(Vec<(&'static str, f64, i64)>),
    Stderr(String),
    Exit,
}

fn num(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn text_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// A tool result's text: a string, or text blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

pub fn parse(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return vec![] };
    // A sub-agent's own steps: only its summary (the Task card) shows.
    let sub = !v["parent_tool_use_id"].is_null();
    let blocks = |kind: &str| v["message"]["content"].as_array().into_iter().flatten().filter(|b| b["type"] == kind).cloned().collect::<Vec<_>>();
    match v["type"].as_str().unwrap_or_default() {
        "system" if v["subtype"] == "init" => {
            vec![Event::Init { session: text_of(&v["session_id"]), model: text_of(&v["model"]), subscription: v["apiKeySource"] == "none" }]
        }
        "stream_event" if !sub => {
            let e = &v["event"];
            match e["type"].as_str().unwrap_or_default() {
                "message_start" => {
                    let u = &e["message"]["usage"];
                    vec![Event::Context(num(&u["input_tokens"]) + num(&u["cache_read_input_tokens"]) + num(&u["cache_creation_input_tokens"]))]
                }
                "content_block_start" => match e["content_block"]["type"].as_str() {
                    Some("text") => vec![Event::TextStart],
                    Some("thinking") => vec![Event::Thinking],
                    _ => vec![],
                },
                "content_block_delta" if e["delta"]["type"] == "text_delta" => vec![Event::Text(text_of(&e["delta"]["text"]))],
                _ => vec![],
            }
        }
        "assistant" if !sub => blocks("tool_use").into_iter().map(|b| Event::ToolUse { id: text_of(&b["id"]), name: text_of(&b["name"]), input: b["input"].clone() }).collect(),
        "user" if !sub => blocks("tool_result")
            .into_iter()
            .map(|b| Event::ToolResult { id: text_of(&b["tool_use_id"]), text: result_text(&b["content"]), error: b["is_error"] == true })
            .collect(),
        "control_request" => {
            let (request, req) = (text_of(&v["request_id"]), &v["request"]);
            if req["subtype"] == "can_use_tool" {
                vec![Event::Ask { request, tool_id: text_of(&req["tool_use_id"]), tool: text_of(&req["tool_name"]), input: req["input"].clone() }]
            } else {
                vec![Event::Unsupported { request }]
            }
        }
        "result" => {
            // shortcut: without `model_usage_since_last_result` (older CLIs) a turn counts nothing; upgrade by diffing `modelUsage`.
            let models = v["model_usage_since_last_result"].as_object().into_iter().flatten().map(|(name, m)| {
                let usage = Usage {
                    input: num(&m["inputTokens"]),
                    output: num(&m["outputTokens"]),
                    cache_read: num(&m["cacheReadInputTokens"]),
                    cache_write: num(&m["cacheCreationInputTokens"]),
                    cost: m["costUSD"].as_f64().unwrap_or(0.),
                };
                (name.clone(), usage, num(&m["contextWindow"]))
            });
            let error = v["is_error"] == true || v["subtype"] != "success";
            let text = match v["subtype"].as_str() {
                Some("error_max_budget_usd") => "Budget maximum de la session atteint : Claude s'est arrêté (Paramètres › Outils).".into(),
                Some("error_max_turns") => "Claude a atteint son nombre maximum d'étapes.".into(),
                _ => v["result"].as_str().or(v["errors"][0].as_str()).map(str::to_string).unwrap_or_else(|| text_of(&v["subtype"])),
            };
            vec![Event::Result { error, text, secs: num(&v["duration_ms"]) as f64 / 1000., models: models.collect() }]
        }
        "rate_limit_event" => {
            let w = &v["rate_limit_info"]["unifiedWindows"];
            let limits = [("five_hour", "5 heures"), ("seven_day", "7 jours")]
                .into_iter()
                .filter_map(|(key, label)| Some((label, w[key]["utilization"].as_f64()?, w[key]["resetsAt"].as_i64().unwrap_or(0))))
                .collect::<Vec<_>>();
            if limits.is_empty() { vec![] } else { vec![Event::Limits(limits)] }
        }
        _ => vec![],
    }
}

/// The file a tool writes, if it does.
fn edited_file(name: &str, input: &Value) -> Option<PathBuf> {
    matches!(name, "Edit" | "MultiEdit" | "Write" | "NotebookEdit").then(|| input["file_path"].as_str().or(input["notebook_path"].as_str()).map(PathBuf::from)).flatten()
}

fn label(name: &str) -> String {
    match name {
        "Bash" => "Commande".into(),
        "Read" => "Lecture".into(),
        "Edit" | "MultiEdit" => "Modification".into(),
        "Write" => "Écriture".into(),
        "NotebookEdit" => "Notebook".into(),
        "Grep" => "Recherche".into(),
        "Glob" => "Fichiers".into(),
        "WebFetch" => "Page web".into(),
        "WebSearch" => "Recherche web".into(),
        "Task" | "Agent" => "Sous-agent".into(),
        "TodoWrite" => "Tâches".into(),
        "ExitPlanMode" => "Plan".into(),
        "AskUserQuestion" => "Question".into(),
        // MCP tools: `mcp__server__tool`.
        _ => name.rsplit("__").next().unwrap_or(name).to_string(),
    }
}

fn tool_icon(name: &str) -> Icon {
    match name {
        "Bash" => Icon::new(IconName::SquareTerminal),
        "Read" => Icon::new(IconName::File),
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Icon::empty().path("icons/pencil.svg"),
        "Grep" | "Glob" => Icon::new(IconName::Search),
        "WebFetch" | "WebSearch" => Icon::new(IconName::Globe),
        "Task" | "Agent" => Icon::new(IconName::Bot),
        "TodoWrite" | "ExitPlanMode" => Icon::empty().path("icons/list-todo.svg"),
        "AskUserQuestion" => Icon::empty().path("icons/circle-help.svg"),
        _ => Icon::empty().path("icons/wrench.svg"),
    }
}

/// One line saying what a tool works on (paths relative to the project).
pub fn summary(name: &str, input: &Value, root: &Path) -> String {
    let rel = |p: &str| Path::new(p).strip_prefix(root).map_or_else(|_| p.to_string(), |p| p.display().to_string());
    let s = |k: &str| input[k].as_str().unwrap_or_default();
    let line = match name {
        "Bash" => s("command").to_string(),
        "Read" | "Edit" | "MultiEdit" | "Write" => rel(s("file_path")),
        "NotebookEdit" => rel(s("notebook_path")),
        "Grep" | "Glob" => match input["path"].as_str() {
            Some(p) => format!("{} dans {}", s("pattern"), rel(p)),
            None => s("pattern").to_string(),
        },
        "WebFetch" => s("url").to_string(),
        "WebSearch" => s("query").to_string(),
        "Task" | "Agent" => s("description").to_string(),
        "TodoWrite" => {
            let todos = input["todos"].as_array().map_or(&[][..], Vec::as_slice);
            format!("{} terminées sur {}", todos.iter().filter(|t| t["status"] == "completed").count(), todos.len())
        }
        "ExitPlanMode" => "Plan prêt à relire".into(),
        "AskUserQuestion" => text_of(&input["questions"][0]["question"]),
        _ => input.as_object().and_then(|o| o.values().find_map(Value::as_str)).unwrap_or_default().to_string(),
    };
    line.lines().next().unwrap_or_default().to_string()
}

/// What a card shows unfolded: a diff for edits (`-`/`+`), the to-do list, the command, or the output.
fn details(name: &str, input: &Value, result: Option<&str>) -> Vec<(char, String)> {
    let lines = |prefix: char, text: &str| text.lines().map(|l| (prefix, l.to_string())).collect::<Vec<_>>();
    let edit = |e: &Value| [lines('-', e["old_string"].as_str().unwrap_or_default()), lines('+', e["new_string"].as_str().unwrap_or_default())].concat();
    match name {
        "Edit" => edit(input),
        "MultiEdit" => input["edits"].as_array().into_iter().flatten().flat_map(edit).collect(),
        "Write" => lines('+', input["content"].as_str().unwrap_or_default()),
        "TodoWrite" => input["todos"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                let mark = match t["status"].as_str() {
                    Some("completed") => '✓',
                    Some("in_progress") => '›',
                    _ => '·',
                };
                (mark, text_of(&t["content"]))
            })
            .collect(),
        "Bash" if result.is_none() => lines('$', input["command"].as_str().unwrap_or_default()),
        _ => lines(' ', result.unwrap_or_default()),
    }
}

// ---------------------------------------------------------------- usage history

/// Usage per day ("2026-11-15"), kept across sessions in Akolade's folder.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub days: BTreeMap<String, Day>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Day {
    pub usage: Usage,
    pub turns: u32,
    pub sessions: u32,
}

impl History {
    fn path() -> PathBuf {
        app_dir().join("claude-usage.json")
    }

    fn load() -> Self {
        read_json(&Self::path())
    }

    fn save(&self) {
        let _ = fs::create_dir_all(app_dir());
        // Through a temporary file: a crash mid-write must not wipe the history.
        if let Ok(text) = serde_json::to_string(self) {
            let _ = crate::workspace::write_file(&Self::path(), &text);
        }
    }

    pub fn record(&mut self, day: &str, usage: &Usage, turns: u32, sessions: u32) {
        let d = self.days.entry(day.to_string()).or_default();
        d.usage.add(usage);
        d.turns += turns;
        d.sessions += sessions;
    }

    /// Everything from `first` (a day) on.
    pub fn since(&self, first: &str) -> Day {
        let mut total = Day::default();
        for d in self.days.range(first.to_string()..).map(|(_, d)| d) {
            total.usage.add(&d.usage);
            (total.turns, total.sessions) = (total.turns + d.turns, total.sessions + d.sessions);
        }
        total
    }
}

fn day(days_ago: i64) -> String {
    (chrono::Local::now() - chrono::Duration::days(days_ago)).format("%Y-%m-%d").to_string()
}

// ---------------------------------------------------------------- formats

/// 950, 12,4 k, 1,2 M
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1} k", n as f64 / 1e3).replace('.', ","),
        _ => format!("{:.1} M", n as f64 / 1e6).replace('.', ","),
    }
}

/// 0 $, 0,0044 $, 0,153 $, 12,40 $
pub fn usd(x: f64) -> String {
    let s = match x {
        0. => "0".to_string(),
        x if x < 0.01 => format!("{x:.4}"),
        x if x < 1. => format!("{x:.3}"),
        _ => format!("{x:.2}"),
    };
    format!("{} $", s.replace('.', ","))
}

fn secs(s: f64) -> String {
    if s < 60. { format!("{s:.1} s").replace('.', ",") } else { format!("{} min {:02} s", s as u64 / 60, s as u64 % 60) }
}

fn resets_in(at: i64) -> String {
    let left = (at - chrono::Utc::now().timestamp()).max(0);
    match left {
        0..3_600 => format!("dans {} min", left / 60),
        3_600..86_400 => format!("dans {} h {:02}", left / 3_600, left % 3_600 / 60),
        _ => format!("dans {} j {} h", left / 86_400, left % 86_400 / 3_600),
    }
}

// ---------------------------------------------------------------- process

/// The running `claude`: dropped, it stops with everything it started.
struct Proc {
    child: Option<Child>,
    stdin: ChildStdin,
}

impl Proc {
    /// `settings`: effort, budget and instructions from the "Outils" page.
    fn spawn(program: &Path, root: &Path, model: &str, mode: &str, resume: Option<&str>, settings: &AppSettings) -> std::io::Result<(Proc, mpsc::UnboundedReceiver<Event>)> {
        let mut cmd = crate::platform::command(program);
        cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]);
        // Permission requests come to us on stdin/stdout (the cards' buttons).
        cmd.args(["--permission-prompt-tool", "stdio", "--permission-mode", mode]);
        if !model.is_empty() {
            cmd.args(["--model", model]);
        }
        if let Some(id) = resume {
            cmd.args(["--resume", id]);
        }
        if !settings.claude_effort.is_empty() {
            cmd.args(["--effort", &settings.claude_effort]);
        }
        if settings.claude_budget > 0. {
            cmd.args(["--max-budget-usd", &settings.claude_budget.to_string()]);
        }
        if !settings.claude_instructions.trim().is_empty() {
            cmd.args(["--append-system-prompt", settings.claude_instructions.trim()]);
        }
        let _child = cmd.current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = crate::platform::own_group(&mut cmd).spawn()?;
        let (stdin, stdout, stderr) = (child.stdin.take().expect("piped"), child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));
        let (tx, rx) = mpsc::unbounded();
        let errors = tx.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = errors.unbounded_send(Event::Stderr(line));
            }
        });
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                for e in parse(&line) {
                    let _ = tx.unbounded_send(e);
                }
            }
            let _ = tx.unbounded_send(Event::Exit);
        });
        Ok((Proc { child: Some(child), stdin }, rx))
    }

    fn send(&mut self, v: Value) -> bool {
        writeln!(self.stdin, "{v}").and_then(|_| self.stdin.flush()).is_ok()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        // Its commands too (a dev server started by Bash…). A moment to save the
        // conversation (its cost, for the list), then for good.
        crate::platform::kill_tree(child.id());
        thread::spawn(move || {
            for _ in 0..20 {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

/// The answer to a permission request (`can_use_tool`), as `claude` waits for it
/// on its stdin: allowed with the tool's input (a question's carries `answers`), or refused.
fn permission_response(request: &str, allow: bool, input: &Value) -> Value {
    let response = if allow {
        json!({ "behavior": "allow", "updatedInput": input })
    } else {
        json!({ "behavior": "deny", "message": "L'utilisateur a refusé depuis Akolade." })
    };
    json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request, "response": response } })
}

/// `claude` in PATH, or where its installer puts it (apps opened from the Dock get a short PATH).
fn find_claude() -> Option<PathBuf> {
    lsp::find_program("claude").or_else(|| {
        let home = crate::platform::home();
        [".local/bin/claude", ".claude/local/claude"].into_iter().map(|p| home.join(p)).find(|p| p.is_file())
    })
}

// ---------------------------------------------------------------- past conversations

/// A conversation Claude Code saved for this project.
#[derive(Clone, Debug, PartialEq)]
pub struct Past {
    pub id: String,
    pub title: String,
    pub modified: SystemTime,
    /// Known once `claude` ended normally.
    pub cost: Option<f64>,
}

/// Claude Code's folder name for a project: every character but ASCII letters and digits becomes `-`.
pub fn encode(root: &Path) -> String {
    root.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

// shortcut: very long paths (Claude Code shortens their folder name) are not found; upgrade by matching the `cwd` in the files.
fn sessions_dir(root: &Path) -> Option<PathBuf> {
    let config = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| Some(crate::platform::home().join(".claude")))?;
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    [root, canonical.as_path()].into_iter().map(|p| config.join("projects").join(encode(p))).find(|d| d.is_dir())
}

/// Our prompts end with the open file: given back apart, for its chip.
fn split_file(text: &str) -> (String, Option<String>) {
    match text.rsplit_once("\n\n(Fichier ouvert dans l'éditeur : ") {
        Some((t, f)) if f.ends_with(')') => (t.to_string(), Some(f[..f.len() - 1].to_string())),
        _ => (text.to_string(), None),
    }
}

/// What you typed in a saved `user` line (not a tool result, a command or a note of Claude Code's).
fn prompt_of(v: &Value) -> Option<String> {
    // Claude Code's own notes start with these tags; a question about `<div>` is yours.
    const NOTES: [&str; 6] = ["<command-", "<local-command-", "<system-reminder>", "<bash-", "<task-notification>", "<user-memory-input>"];
    if v["type"] != "user" || v["isMeta"] == true || v["isSidechain"] == true {
        return None;
    }
    let c = &v["message"]["content"];
    let text = c.as_str().map(str::to_string).or_else(|| c.as_array()?.iter().find(|b| b["type"] == "text").map(|b| text_of(&b["text"])))?;
    let text = text.trim();
    (!text.is_empty() && !NOTES.iter().any(|n| text.starts_with(n))).then(|| text.to_string())
}

/// What a conversation file says of itself, read up to `len` bytes: files only
/// grow, so a later look reads just what was added.
#[derive(Clone, Debug, Default)]
struct Scan {
    len: u64,
    custom: Option<String>,
    ai: Option<String>,
    first: Option<String>,
    cost: Option<f64>,
}

impl Scan {
    fn line(&mut self, line: &str) {
        // Only the few lines that matter are parsed (images make the others big).
        let kind = match line {
            l if l.contains("\"type\":\"custom-title\"") => 0,
            l if l.contains("\"type\":\"ai-title\"") => 1,
            l if l.contains("\"type\":\"cost-state\"") => 2,
            l if self.first.is_none() && l.contains("\"type\":\"user\"") => 3,
            _ => return,
        };
        let Ok(v) = serde_json::from_str::<Value>(line) else { return };
        match kind {
            0 => self.custom = v["customTitle"].as_str().map(str::to_string).or(self.custom.take()),
            1 => self.ai = v["aiTitle"].as_str().map(str::to_string).or(self.ai.take()),
            2 => self.cost = v["totalCostUSD"].as_f64().or(self.cost),
            _ => self.first = prompt_of(&v).map(|p| split_file(&p).0),
        }
    }

    /// The lines added since the last look; a line still being written waits for the next one.
    fn read(&mut self, path: &Path) -> std::io::Result<()> {
        use std::io::{Seek, SeekFrom};
        let mut file = fs::File::open(path)?;
        file.seek(SeekFrom::Start(self.len))?;
        let (mut reader, mut buf) = (BufReader::new(file), Vec::new());
        loop {
            buf.clear();
            let n = reader.read_until(b'\n', &mut buf)?;
            if n == 0 || buf.last() != Some(&b'\n') {
                return Ok(());
            }
            self.len += n as u64;
            self.line(&String::from_utf8_lossy(&buf));
        }
    }

    /// Its title (renamed, else Claude's, else the first prompt) and cost.
    fn past(&self, id: String, modified: SystemTime) -> Option<Past> {
        let title = self.custom.clone().or(self.ai.clone()).or(self.first.clone())?;
        Some(Past { id, title: title.lines().next().unwrap_or_default().chars().take(120).collect(), modified, cost: self.cost })
    }
}

#[cfg(test)]
pub fn read_past(id: String, text: &str, modified: SystemTime) -> Option<Past> {
    let mut scan = Scan::default();
    text.lines().for_each(|l| scan.line(l));
    scan.past(id, modified)
}

/// This project's conversations, the latest first. Read in the background;
/// remembered between looks (one file can weigh hundreds of MB).
pub fn list_past(root: &Path) -> Vec<Past> {
    static SCANS: Mutex<BTreeMap<PathBuf, Scan>> = Mutex::new(BTreeMap::new());
    let Some(dir) = sessions_dir(root) else { return vec![] };
    // Claude Code's ids: letters, digits and dashes. Anything else is not one of its files
    // (and could not be typed safely in `claude --resume <id>`).
    let id_like = |p: &Path| p.file_stem().is_some_and(|s| !s.is_empty() && s.to_string_lossy().chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    let mut files: Vec<(SystemTime, u64, PathBuf)> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl") && id_like(p))
        .filter_map(|p| {
            let meta = fs::metadata(&p).ok()?;
            Some((meta.modified().ok()?, meta.len(), p))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    // shortcut: the 50 latest only; older ones stay reachable with `claude --resume` in the terminal.
    files
        .into_iter()
        .take(50)
        .filter_map(|(modified, len, path)| {
            let cached = SCANS.lock().unwrap_or_else(|e| e.into_inner()).get(&path).cloned();
            // Shorter than read before: rewritten, read again from the start.
            let mut scan = cached.filter(|s| s.len <= len).unwrap_or_default();
            if scan.len < len {
                scan.read(&path).ok()?;
                SCANS.lock().unwrap_or_else(|e| e.into_inner()).insert(path.clone(), scan.clone());
            }
            scan.past(path.file_stem()?.to_string_lossy().into(), modified)
        })
        .collect()
}

/// A saved conversation as cards: your messages, Claude's answers, its tools and their results.
fn transcript(lines: impl Iterator<Item = String>) -> Vec<Item> {
    let mut items = vec![];
    for line in lines.filter(|l| l.contains("\"type\":\"user\"") || l.contains("\"type\":\"assistant\"")) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        // Sub-agents' steps, and the notes Claude Code adds itself ("No response requested.").
        if v["isSidechain"] == true || v["message"]["model"] == "<synthetic>" {
            continue;
        }
        let blocks = v["message"]["content"].as_array().cloned().unwrap_or_default();
        match v["type"].as_str() {
            Some("user") => {
                if let Some(p) = prompt_of(&v) {
                    let (text, file) = split_file(&p);
                    items.push(Item::User(text, file));
                }
                for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                    let id = text_of(&b["tool_use_id"]);
                    for item in &mut items {
                        if let Item::Tool { id: i, result, .. } = item
                            && *i == id
                        {
                            *result = Some((result_text(&b["content"]), b["is_error"] == true));
                        }
                    }
                }
            }
            Some("assistant") => {
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => items.push(Item::Text(text_of(&b["text"]))),
                        Some("tool_use") => items.push(Item::Tool { id: text_of(&b["id"]), name: text_of(&b["name"]), input: b["input"].clone(), result: None, ask: None }),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    items
}

/// "aujourd'hui à 14:32", "hier à 09:05", "12 nov. à 18:40", "3 mars 2025"
fn when(t: SystemTime) -> String {
    use chrono::{DateTime, Datelike, Local};
    const MONTHS: [&str; 12] = ["janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.", "déc."];
    let (t, now): (DateTime<Local>, DateTime<Local>) = (t.into(), Local::now());
    let hour = t.format("%H:%M");
    match (now.date_naive() - t.date_naive()).num_days() {
        0 => format!("aujourd'hui à {hour}"),
        1 => format!("hier à {hour}"),
        _ if t.year() == now.year() => format!("{} {} à {hour}", t.day(), MONTHS[t.month0() as usize]),
        _ => format!("{} {} {}", t.day(), MONTHS[t.month0() as usize], t.year()),
    }
}

// ---------------------------------------------------------------- view

/// The settings `claude` is started with.
#[derive(Clone, Debug, PartialEq)]
struct ClaudeSettings {
    model: String,
    effort: String,
    mode: String,
    with_file: bool,
    budget: f64,
    instructions: String,
}

impl ClaudeSettings {
    fn of(s: &AppSettings) -> Self {
        Self {
            model: s.claude_model.clone(),
            effort: s.claude_effort.clone(),
            mode: s.claude_mode.clone(),
            with_file: s.claude_with_file,
            budget: s.claude_budget,
            instructions: s.claude_instructions.clone(),
        }
    }
}

enum Item {
    /// What you asked, and the file it came with.
    User(String, Option<String>),
    Text(String),
    /// `ask`: its permission request and your answer (None: waiting).
    Tool { id: String, name: String, input: Value, result: Option<(String, bool)>, ask: Option<(String, Option<bool>)> },
    Turn { secs: f64, usage: Usage },
    Notice(String, bool),
}

#[derive(Default)]
struct Stats {
    usage: Usage,
    turns: u32,
    secs: f64,
    context: u64,
    window: u64,
    models: BTreeMap<String, Usage>,
    tools: BTreeMap<String, u32>,
    files: Vec<PathBuf>,
}

pub struct ClaudeView {
    workspace: WeakEntity<Workspace>,
    root: Option<PathBuf>,
    program: Option<PathBuf>,
    input: Entity<InputState>,
    items: Vec<Item>,
    proc: Option<Proc>,
    /// To go on with the same conversation after a restart (model or mode changed, stopped).
    session: Option<String>,
    model: usize,
    mode: usize,
    model_name: String,
    subscription: bool,
    busy: bool,
    thinking: bool,
    with_file: bool,
    stats: Stats,
    limits: Vec<(&'static str, f64, i64)>,
    history: History,
    stderr: Vec<String>,
    /// The folder open in the editor; `root` is Claude's own (the same unless `custom`).
    project: Option<PathBuf>,
    /// A folder picked for Claude: it stays when the editor opens another.
    custom: bool,
    /// The mode changed during a turn: `claude` restarts once it ends.
    restart: bool,
    /// Cards folded or unfolded against their default.
    toggled: HashSet<usize>,
    /// Options chosen in a question card: (card, question) → options.
    picked: HashMap<(usize, usize), BTreeSet<usize>>,
    /// "Tout autoriser" confirmed once.
    bypass_ok: bool,
    show_past: bool,
    /// This project's conversations (None: being read).
    past: Option<Vec<Past>>,
    /// The folder the list was asked for.
    _past_for: Option<PathBuf>,
    _past: Task<()>,
    _load: Task<()>,
    /// The "Outils" settings last applied.
    seen: Option<ClaudeSettings>,
    /// The conversation's cards, laid out only when on screen: a long one (thousands
    /// of cards) costs no more than a short one. Follows the end while there.
    log: ListState,
    /// A card other than the last two changed height (a result, a fold, an answer).
    relayout: bool,
    /// Back to the end at the next frame (a message sent, a conversation opened).
    to_bottom: bool,
    _reader: Task<()>,
    _subs: Vec<Subscription>,
}

impl ClaudeView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).multi_line(true).auto_grow(2, 10).placeholder("Demandez quelque chose à Claude… (⌘⏎ pour envoyer)"));
        let subs = vec![
            cx.subscribe_in(&input, window, |this, _, ev, window, cx| {
                if let InputEvent::PressEnter { secondary: true } = ev {
                    this.send_input(window, cx);
                }
            }),
            cx.observe_global::<AppSettings>(|this, cx| this.apply_settings(cx)),
        ];
        let mut view = Self {
            workspace,
            root,
            program: find_claude(),
            input,
            items: vec![],
            proc: None,
            session: None,
            model: 0,
            mode: 0,
            model_name: String::new(),
            subscription: false,
            busy: false,
            thinking: false,
            with_file: true,
            stats: Stats::default(),
            limits: vec![],
            history: History::load(),
            stderr: vec![],
            restart: false,
            project: None,
            custom: false,
            toggled: HashSet::new(),
            picked: HashMap::new(),
            bypass_ok: false,
            show_past: false,
            past: None,
            _past_for: None,
            _past: Task::ready(()),
            _load: Task::ready(()),
            seen: None,
            log: ListState::new(0, ListAlignment::Bottom, px(800.)),
            relayout: false,
            to_bottom: false,
            _reader: Task::ready(()),
            _subs: subs,
        };
        view.project = view.root.clone();
        view.apply_settings(cx);
        view
    }

    /// The defaults chosen in the settings; `claude` restarts on the same conversation to apply them.
    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let s = AppSettings::get(cx);
        let now = ClaudeSettings::of(s);
        if self.seen.as_ref() == Some(&now) {
            return;
        }
        self.model = MODELS.iter().position(|m| m.1 == s.claude_model).unwrap_or(0);
        self.mode = MODES.iter().position(|m| m.1 == s.claude_mode).unwrap_or(0);
        // Chosen in the settings: no need to confirm it again here.
        self.bypass_ok |= self.mode == BYPASS;
        self.with_file = s.claude_with_file;
        if self.seen.is_some() {
            if self.busy {
                self.restart = true;
            } else {
                self.proc = None;
                self._reader = Task::ready(());
            }
        }
        self.seen = Some(now);
        cx.notify();
    }

    fn last_file(&self, cx: &Context<Self>) -> Option<PathBuf> {
        self.workspace.upgrade().and_then(|w| w.read(cx).last_file_path())
    }

    /// Where Claude works: the project's folder, `~` for the home folder.
    fn folder(&self) -> String {
        let Some(root) = &self.root else { return String::new() };
        let home = Some(crate::platform::home());
        match home.as_deref().and_then(|h| root.strip_prefix(h).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => root.display().to_string(),
        }
    }

    /// The folder, its last two parts when long ("…/perso/akolade").
    fn short_folder(&self) -> String {
        let full = self.folder();
        let parts: Vec<&str> = full.rsplitn(3, '/').collect();
        if full.chars().count() <= 48 || parts.len() < 3 { full } else { format!("…/{}/{}", parts[1], parts[0]) }
    }

    /// The working folder: a click picks another one; the Finder, and back to the project's when another was picked.
    fn render_folder(&self, id: &'static str, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, fg, mono, hover) = (theme.muted_foreground, theme.foreground, theme.mono_font_family.clone(), ui::hover_bg(cx));
        let root = self.root.clone();
        let busy = self.busy;
        let chip = h_flex()
            .id(id)
            .min_w_0()
            .gap(r(6.))
            .px(r(6.))
            .py(r(3.))
            .rounded(r(6.))
            .when(!busy, |d| d.cursor_pointer().hover(|d| d.bg(hover)))
            .tooltip({
                let tip = format!("{}\n\nClaude lit, modifie et lance ses commandes dans ce dossier. Cliquez pour en choisir un autre.", self.folder());
                move |window, cx| Tooltip::new(tip.clone()).build(window, cx)
            })
            .text_size(r(12.))
            .child(Icon::new(IconName::FolderOpen).xsmall().text_color(rgb(ORANGE)))
            .child(div().flex_shrink_0().text_color(muted).child(if self.custom { "Dossier choisi" } else { "Dossier de travail" }))
            .child(div().min_w_0().truncate().font_family(mono).text_color(fg).child(self.short_folder()))
            .child(Icon::new(IconName::ChevronDown).xsmall().text_color(muted))
            .on_click(cx.listener(|this, _, window, cx| this.pick_folder(window, cx)));
        let back = self.project.clone().filter(|_| self.custom).map(|p| {
            let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
            button(SharedString::from(format!("{id}-back"))).ghost().xsmall().label(format!("Revenir à {name}")).tooltip("Le dossier ouvert dans l'éditeur").disabled(busy).on_click(cx.listener(move |this, _, _, cx| this.set_folder(p.clone(), cx)))
        });
        h_flex()
            .min_w_0()
            .gap(r(4.))
            .child(chip)
            .child(button(SharedString::from(format!("{id}-finder"))).ghost().xsmall().icon(IconName::ExternalLink).tooltip(format!("Ouvrir dans {}", crate::platform::FILE_MANAGER)).on_click(move |_, _, _| {
                if let Some(root) = &root {
                    crate::platform::open(root);
                }
            }))
            .children(back)
    }

    /// Another folder for Claude, whatever the editor shows.
    fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Travailler ici".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = rx.await else { return };
            let Some(path) = paths.pop() else { return };
            let _ = this.update(cx, |this, cx| this.set_folder(path, cx));
        })
        .detach();
    }

    /// A new conversation in `path` (back to following the editor when it is the project's).
    fn set_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.busy || self.root.as_ref() == Some(&path) && self.custom == (self.project.as_ref() != Some(&path)) {
            return;
        }
        self.custom = self.project.as_ref() != Some(&path);
        (self.root, self.past, self._past_for) = (Some(path), None, None);
        self.reset(cx);
    }

    fn rel(&self, p: &Path) -> String {
        self.root.as_deref().and_then(|root| p.strip_prefix(root).ok()).unwrap_or(p).display().to_string()
    }

    /// The card of a question waiting for an answer.
    fn pending_question(&self) -> Option<usize> {
        self.items.iter().position(|i| matches!(i, Item::Tool { name, ask: Some((_, None)), .. } if name == "AskUserQuestion"))
    }

    fn send_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        // Typed while Claude asks: the answer, in your own words.
        if let Some(ix) = self.pending_question().filter(|_| !text.is_empty()) {
            self.input.update(cx, |i, cx| i.set_value("", window, cx));
            return self.reply(ix, Some(text), cx);
        }
        if !text.is_empty() && !self.busy {
            self.input.update(cx, |i, cx| i.set_value("", window, cx));
            self.send(text, window, cx);
        }
    }

    fn send(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(program), Some(root)) = (self.program.clone(), self.root.clone()) else { return };
        if self.busy {
            return;
        }
        // In -p mode Claude Code asks nothing: it runs the project's hooks (.claude/settings.json) at once.
        if self.proc.is_none() && !crate::trust::is_trusted(&root) {
            return self.ask_trust(text, root, window, cx);
        }
        if self.proc.is_none() {
            let settings = AppSettings::get(cx).clone();
            match Proc::spawn(&program, &root, MODELS[self.model].1, MODES[self.mode].1, self.session.as_deref(), &settings) {
                Ok((proc, mut rx)) => {
                    self.proc = Some(proc);
                    self.stderr.clear();
                    self._reader = cx.spawn_in(window, async move |this, cx| {
                        while let Some(e) = rx.next().await {
                            if this.update_in(cx, |this, window, cx| this.apply(e, window, cx)).is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(e) => {
                    self.items.push(Item::Notice(format!("Impossible de lancer Claude Code : {e}"), true));
                    return cx.notify();
                }
            }
        }
        let file = self.with_file.then(|| self.last_file(cx)).flatten().map(|p| self.rel(&p));
        let prompt = match &file {
            Some(f) => format!("{text}\n\n(Fichier ouvert dans l'éditeur : {f})"),
            None => text.clone(),
        };
        let sent = self.proc.as_mut().is_some_and(|p| p.send(json!({ "type": "user", "message": { "role": "user", "content": prompt } })));
        self.items.push(Item::User(text, file));
        if sent {
            (self.busy, self.thinking) = (true, true);
        } else {
            self.proc = None;
            self.items.push(Item::Notice("Claude Code ne répond plus : renvoyez votre message pour le relancer.".into(), true));
        }
        self.to_bottom = true;
        cx.notify();
    }

    /// Before Claude's first start in a folder not trusted yet: yes is remembered
    /// (the same trust as the editor's), no gives the message back to the box.
    fn ask_trust(&mut self, text: String, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let detail = format!(
            "Claude Code va travailler dans « {} ». Il charge les réglages du projet (.claude/ : hooks, serveurs MCP, commandes) et peut exécuter les commandes qu'ils définissent.\n\nN'acceptez que si vous faites confiance à ce dossier.",
            self.folder()
        );
        let answer = window.prompt(PromptLevel::Warning, "Faire confiance à ce dossier ?", Some(&detail), &["Faire confiance", "Annuler"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let yes = answer.await == Ok(0);
            let _ = this.update_in(cx, |this, window, cx| {
                // Another folder meanwhile: that one was not asked about.
                if this.root.as_ref() != Some(&root) {
                    return;
                }
                if yes {
                    AppSettings::update(cx, |s| s.trusted_folders.push(root));
                    this.send(text, window, cx);
                } else if this.input.read(cx).value().is_empty() {
                    this.input.update(cx, |i, cx| i.set_value(text, window, cx));
                }
            });
        })
        .detach();
    }

    fn apply(&mut self, e: Event, window: &mut Window, cx: &mut Context<Self>) {
        // Cards before the last two that change height (see `sync_log`).
        self.relayout |= matches!(e, Event::ToolResult { .. } | Event::Ask { .. } | Event::Exit);
        match e {
            Event::Init { session, model, subscription } => {
                if self.session.is_none() {
                    self.history.record(&day(0), &Usage::default(), 0, 1);
                }
                (self.session, self.model_name, self.subscription) = (Some(session), model, subscription);
            }
            Event::TextStart => {
                self.thinking = false;
                self.items.push(Item::Text(String::new()));
            }
            Event::Text(t) => {
                self.thinking = false;
                match self.items.last_mut() {
                    Some(Item::Text(s)) => s.push_str(&t),
                    _ => self.items.push(Item::Text(t)),
                }
            }
            Event::Thinking => self.thinking = true,
            Event::Context(n) => self.stats.context = n,
            Event::ToolUse { id, name, input } => {
                self.thinking = true;
                *self.stats.tools.entry(label(&name)).or_default() += 1;
                if let Some(f) = edited_file(&name, &input).filter(|f| !self.stats.files.contains(f)) {
                    self.stats.files.push(f);
                }
                if !self.items.iter().any(|i| matches!(i, Item::Tool { id: i, .. } if *i == id)) {
                    self.items.push(Item::Tool { id, name, input, result: None, ask: None });
                }
            }
            Event::ToolResult { id, text, error } => {
                let mut edited = false;
                for item in &mut self.items {
                    if let Item::Tool { id: i, name, input, result, .. } = item
                        && *i == id
                    {
                        edited = !error && edited_file(name, input).is_some();
                        *result = Some((text.clone(), error));
                    }
                }
                // Its edits show in the open tabs right away.
                if edited && let Some(ws) = self.workspace.upgrade() {
                    ws.update(cx, |ws, cx| ws.reload_from_disk(window, cx));
                }
            }
            Event::Ask { request, tool_id, tool, input } => {
                self.thinking = false;
                // On its tool's card; a card of its own if that one is not there yet.
                let card = self.items.iter_mut().find_map(|i| match i {
                    Item::Tool { id, ask, .. } if *id == tool_id && !tool_id.is_empty() => Some(ask),
                    _ => None,
                });
                match card {
                    Some(ask) => *ask = Some((request, None)),
                    None => self.items.push(Item::Tool { id: tool_id, name: tool, input, result: None, ask: Some((request, None)) }),
                }
            }
            Event::Unsupported { request } => {
                if let Some(p) = &mut self.proc {
                    p.send(json!({ "type": "control_response", "response": { "subtype": "error", "request_id": request, "error": "Non pris en charge par Akolade" } }));
                }
            }
            Event::Result { error, text, secs, models } => {
                let mut turn = Usage::default();
                for (name, u, window) in models {
                    turn.add(&u);
                    self.stats.models.entry(name).or_default().add(&u);
                    self.stats.window = self.stats.window.max(window);
                }
                self.stats.usage.add(&turn);
                (self.stats.turns, self.stats.secs) = (self.stats.turns + 1, self.stats.secs + secs);
                self.history.record(&day(0), &turn, 1, 0);
                self.history.save();
                (self.busy, self.thinking) = (false, false);
                if std::mem::take(&mut self.restart) {
                    self.proc = None;
                }
                if error {
                    let text = if text.contains("/login") { "Vous n'êtes pas connecté à Claude Code : ouvrez-le dans le terminal (bouton en haut à droite) et tapez /login.".into() } else { text };
                    self.items.push(Item::Notice(text, true));
                }
                if turn.tokens() > 0 {
                    self.items.push(Item::Turn { secs, usage: turn });
                }
            }
            Event::Limits(l) => self.limits = l,
            Event::Stderr(line) => {
                self.stderr.push(line);
                if self.stderr.len() > 12 {
                    self.stderr.remove(0);
                }
            }
            Event::Exit => {
                self.proc = None;
                self._reader = Task::ready(());
                let why = if self.stderr.is_empty() { "il s'est arrêté sans message.".to_string() } else { self.stderr.join("\n") };
                if self.busy || self.session.is_none() {
                    self.items.push(Item::Notice(format!("Claude Code s'est arrêté : {why}"), true));
                }
                self.end_turn();
            }
        }
        cx.notify();
    }

    /// Tells the list what changed since the last frame: cards added, the last
    /// ones (streamed text, the status line) measured again, all of them when
    /// one higher up changed. The scroll position stays where it was.
    fn sync_log(&mut self) {
        let n = self.items.len() + 1; // the cards, then the status line
        let old = self.log.item_count();
        if n < old || std::mem::take(&mut self.to_bottom) {
            self.relayout = false;
            return self.log.reset(n);
        }
        if std::mem::take(&mut self.relayout) {
            let top = self.log.logical_scroll_top();
            self.log.splice(0..old, n);
            // At the end it stays there by itself; elsewhere, back where it was.
            if top.item_ix < old {
                self.log.scroll_to(top);
            }
            return;
        }
        let from = old.saturating_sub(2);
        self.log.splice(from..old, n - from);
    }

    /// Nothing runs any more: pending tools and questions are closed.
    fn end_turn(&mut self) {
        (self.busy, self.thinking) = (false, false);
        self.relayout = true;
        for item in &mut self.items {
            match item {
                Item::Tool { result, ask, .. } => {
                    if let Some((_, a @ None)) = ask {
                        *a = Some(false);
                    }
                    if result.is_none() {
                        *result = Some(("Interrompu.".into(), true));
                    }
                }
                _ => {}
            }
        }
    }

    fn answer(&mut self, ix: usize, allow: bool, cx: &mut Context<Self>) {
        // (The message itself: `permission_response`.)
        let Some(Item::Tool { name, input, ask: Some((request, answer @ None)), .. }) = self.items.get_mut(ix) else { return };
        // A plan approved is a plan to apply: as in Claude Code, out of plan mode (from the next message).
        if allow && name == "ExitPlanMode" && MODES[self.mode].1 == "plan" {
            (self.mode, self.restart) = (0, true);
        }
        let msg = permission_response(request, allow, input);
        *answer = Some(allow);
        self.relayout = true;
        if let Some(p) = &mut self.proc {
            p.send(msg);
        }
        self.thinking = allow;
        cx.notify();
    }

    /// An option clicked: a single choice for a lone question is the answer right away.
    fn pick(&mut self, ix: usize, q: usize, option: usize, cx: &mut Context<Self>) {
        let Some(Item::Tool { input, ask: Some((_, None)), .. }) = self.items.get(ix) else { return };
        let questions = input["questions"].as_array().map_or(0, Vec::len);
        let multi = input["questions"][q]["multiSelect"] == true;
        self.relayout = true;
        let chosen = self.picked.entry((ix, q)).or_default();
        if multi {
            if !chosen.remove(&option) {
                chosen.insert(option);
            }
        } else {
            *chosen = BTreeSet::from([option]);
        }
        if questions == 1 && !multi {
            return self.reply(ix, None, cx);
        }
        cx.notify();
    }

    /// The answers go back with the tool's input (`answers`: question → labels), as Claude Code's own dialog does.
    fn reply(&mut self, ix: usize, free: Option<String>, cx: &mut Context<Self>) {
        let Some(Item::Tool { input, ask: Some((_, None)), .. }) = self.items.get_mut(ix) else { return };
        let mut answers = serde_json::Map::new();
        for (q, question) in input["questions"].as_array().cloned().unwrap_or_default().iter().enumerate() {
            let labels: Vec<String> = self.picked.get(&(ix, q)).into_iter().flatten().map(|&o| text_of(&question["options"][o]["label"])).collect();
            let answer = if labels.is_empty() { free.clone().unwrap_or_default() } else { labels.join(", ") };
            answers.insert(text_of(&question["question"]), answer.into());
        }
        input["answers"] = Value::Object(answers);
        self.answer(ix, true, cx);
    }

    // shortcut: a stopped turn's tokens are not counted (the CLI reports them at its end); upgrade by summing each message's `usage`.
    fn stop(&mut self, cx: &mut Context<Self>) {
        self.proc = None;
        self._reader = Task::ready(());
        if self.busy {
            self.items.push(Item::Notice("Arrêté. La conversation reprend au prochain message.".into(), false));
        }
        self.end_turn();
        cx.notify();
    }

    /// The next message starts a new conversation.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.proc = None;
        self._reader = Task::ready(());
        (self.items, self.session, self.stats, self.model_name, self.restart) = (vec![], None, Stats::default(), String::new(), false);
        self.toggled.clear();
        self.picked.clear();
        (self.past, self._past_for) = (None, None);
        self.end_turn();
        cx.notify();
    }

    fn load_past(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        let task = cx.background_executor().spawn(async move { list_past(&root) });
        self._past = cx.spawn(async move |this, cx| {
            let past = task.await;
            let _ = this.update(cx, |this, cx| {
                this.past = Some(past);
                cx.notify();
            });
        });
    }

    fn toggle_past(&mut self, cx: &mut Context<Self>) {
        self.show_past = !self.show_past;
        if self.show_past {
            self.load_past(cx);
        }
        cx.notify();
    }

    /// Back in a past conversation: shown as it was, the next message goes on with it.
    fn resume(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(path) = self.root.as_deref().and_then(sessions_dir).map(|d| d.join(format!("{id}.jsonl"))) else { return };
        self.reset(cx);
        (self.session, self.show_past) = (Some(id.clone()), false);
        // Line by line: the file can weigh hundreds of MB.
        let task = cx.background_executor().spawn(async move { fs::File::open(&path).map(|f| transcript(BufReader::new(f).lines().map_while(Result::ok))).unwrap_or_default() });
        self._load = cx.spawn(async move |this, cx| {
            let items = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.session.as_deref() == Some(id.as_str()) && this.items.is_empty() {
                    this.items = items;
                    this.items.push(Item::Notice("Conversation reprise : votre prochain message la continue.".into(), false));
                    this.to_bottom = true;
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    /// "Tout autoriser" is asked once: Claude will then act without asking.
    fn choose_mode(&mut self, mode: usize, window: &mut Window, cx: &mut Context<Self>) {
        if mode != BYPASS || self.bypass_ok || self.busy {
            return self.choose(None, Some(mode), cx);
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Tout autoriser ?",
            Some("Claude exécutera ses commandes et modifiera les fichiers du projet sans vous demander.\n\nÀ réserver aux projets que vous pouvez restaurer (avec Git, par exemple)."),
            &["Tout autoriser", "Annuler"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update(cx, |this, cx| {
                    this.bypass_ok = true;
                    this.choose(None, Some(mode), cx);
                });
            }
        })
        .detach();
    }

    /// The model or mode applies from the next message: `claude` restarts on the same conversation.
    fn choose(&mut self, model: Option<usize>, mode: Option<usize>, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (model, mode) = (model.unwrap_or(self.model), mode.unwrap_or(self.mode));
        if (model, mode) != (self.model, self.mode) {
            (self.model, self.mode, self.proc) = (model, mode, None);
            self._reader = Task::ready(());
        }
        cx.notify();
    }

    fn in_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // One process per conversation: ours stops, the terminal's takes over.
        self.stop(cx);
        let mut command = match &self.session {
            Some(id) => format!("claude --resume {id}"),
            None => "claude".into(),
        };
        // The terminal opens in the project: Claude's conversations are kept per folder.
        if let Some(dir) = self.root.as_ref().filter(|_| self.custom) {
            command = format!("cd '{}' && {command}", dir.display().to_string().replace('\'', "'\\''"));
        }
        in_terminal(&self.workspace, &command, window, cx);
    }

    fn open(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| ws.open_path(path, window, cx));
        }
    }

    // ------------------------------------------------------------ render

    /// `danger`: a choice shown in red once picked.
    fn segmented(&self, id: &'static str, labels: Vec<(&'static str, Option<&'static str>)>, on: usize, danger: Option<usize>, cx: &mut Context<Self>, pick: fn(&mut Self, usize, &mut Window, &mut Context<Self>)) -> Div {
        let theme = cx.theme();
        let (fg, muted, primary_fg, red) = (theme.foreground, theme.muted_foreground, theme.primary_foreground, theme.red);
        let primary = if danger == Some(on) { red } else { theme.primary };
        let busy = self.busy;
        h_flex().flex_shrink_0().p(r(2.)).gap(r(2.)).rounded(r(8.)).bg(fg.opacity(0.05)).children(labels.into_iter().enumerate().map(|(i, (text, tip))| {
            div()
                .id(SharedString::from(format!("{id}-{i}")))
                .px(r(9.))
                .py(r(4.))
                .rounded(r(6.))
                .text_size(r(12.))
                .font_weight(FontWeight::MEDIUM)
                .map(|d| if i == on { d.bg(primary).text_color(primary_fg) } else { d.text_color(muted) })
                .when(!busy && i != on, |d| d.cursor_pointer().hover(|d| d.text_color(fg)))
                .when_some(tip, |d, tip| d.tooltip(move |window, cx| Tooltip::new(tip).build(window, cx)))
                .child(text)
                .on_click(cx.listener(move |this, _, window, cx| pick(this, i, window, cx)))
        }))
    }

    fn render_tool(&self, ix: usize, item: &Item, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let Item::Tool { name, input, result, ask, .. } = item else { return div() };
        let theme = cx.theme();
        let (fg, muted, border, mono, hover) = (theme.foreground, theme.muted_foreground, theme.border, theme.mono_font_family.clone(), ui::hover_bg(cx));
        let (yellow, green, red) = (theme.yellow, theme.green, theme.red);
        let root = self.root.clone().unwrap_or_default();
        let answer = ask.as_ref().map(|a| a.1);
        let (pending, refused, plan, question) = (answer == Some(None), answer == Some(Some(false)), name == "ExitPlanMode", name == "AskUserQuestion");
        let open = pending || matches!(name.as_str(), "Edit" | "MultiEdit" | "Write" | "TodoWrite" | "ExitPlanMode" | "AskUserQuestion") != self.toggled.contains(&ix);
        let file = edited_file(name, input).or_else(|| (name == "Read").then(|| input["file_path"].as_str().map(PathBuf::from)).flatten());
        let note = |text: &'static str, color: Hsla| div().flex_shrink_0().text_size(r(11.5)).text_color(color).child(text).into_any_element();
        let status: AnyElement = match result {
            _ if pending => note(if question { "En attente de votre réponse" } else { "En attente de votre accord" }, yellow),
            _ if refused => note("Refusé", red),
            None if self.busy => Spinner::new().xsmall().color(muted).into_any_element(),
            None => div().into_any_element(),
            Some((_, false)) => Icon::new(IconName::Check).xsmall().text_color(green).into_any_element(),
            Some((_, true)) => Icon::new(IconName::Close).xsmall().text_color(red).into_any_element(),
        };
        let accent = if pending { yellow } else if refused || result.as_ref().is_some_and(|r| r.1) { red } else { rgb(ORANGE).into() };
        let row = h_flex()
            .id(SharedString::from(format!("tool-{ix}")))
            .gap(r(10.))
            .px(r(10.))
            .py(r(7.))
            .cursor_pointer()
            .hover(|d| d.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.toggled.remove(&ix) {
                    this.toggled.insert(ix);
                }
                this.relayout = true;
                cx.notify();
            }))
            .child(tile(accent, 24., tool_icon(name).size(r(13.)).text_color(accent)))
            .child(div().flex_shrink_0().text_size(r(13.)).font_weight(FontWeight::SEMIBOLD).child(label(name)))
            .child(div().flex_1().min_w_0().truncate().font_family(mono).text_size(r(12.)).text_color(muted).child(summary(name, input, &root)))
            .when_some(file, |d, f| {
                d.child(button(SharedString::from(format!("tool-open-{ix}"))).ghost().xsmall().icon(IconName::ExternalLink).tooltip("Ouvrir dans l'éditeur").on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.open(f.clone(), window, cx);
                })))
            })
            .child(status);
        let body: Option<AnyElement> = open.then(|| {
            if question {
                self.render_questions(ix, input, pending, cx).into_any_element()
            } else if plan {
                div().border_t_1().border_color(border).px(r(14.)).py(r(8.)).text_size(r(14.)).child(TextView::markdown(("claude-plan", ix), text_of(&input["plan"]), window, cx).selectable(true)).into_any_element()
            } else {
                let lines = details(name, input, result.as_ref().map(|r| r.0.as_str()));
                let lines = if lines.is_empty() && pending { vec![(' ', summary(name, input, &root))] } else { lines };
                self.render_lines(lines, cx).into_any_element()
            }
        });
        let (yes, no, why) = match () {
            _ if question => ("Répondre", "Ignorer", "Ou répondez librement dans la zone de saisie"),
            _ if plan => ("Approuver", "Continuer à planifier", "Votre accord sur ce plan ?"),
            _ => ("Autoriser", "Refuser", "Claude demande votre autorisation"),
        };
        // A question is answered once each has a choice (or in the box below).
        let ready = !question || (0..input["questions"].as_array().map_or(0, Vec::len)).all(|q| self.picked.get(&(ix, q)).is_some_and(|c| !c.is_empty()));
        let footer = pending.then(|| {
            h_flex()
                .gap(r(8.))
                .px(r(12.))
                .py(r(9.))
                .border_t_1()
                .border_color(yellow.opacity(0.3))
                .child(if question { Icon::empty().path("icons/circle-help.svg").small().text_color(yellow) } else { Icon::new(IconName::TriangleAlert).small().text_color(yellow) })
                .child(div().flex_1().min_w_0().text_size(r(12.5)).text_color(if question { muted } else { fg }).child(why))
                .child(button(SharedString::from(format!("deny-{ix}"))).ghost().small().label(no).on_click(cx.listener(move |this, _, _, cx| this.answer(ix, false, cx))))
                .child(button(SharedString::from(format!("allow-{ix}"))).primary().small().icon(IconName::Check).label(yes).disabled(!ready).on_click(cx.listener(move |this, _, _, cx| {
                    if question { this.reply(ix, None, cx) } else { this.answer(ix, true, cx) }
                })))
        });
        v_flex()
            .flex_shrink_0()
            .rounded(r(10.))
            .border_1()
            .border_color(if pending { yellow.opacity(0.6) } else { border })
            .bg(if pending { yellow.opacity(0.04) } else { fg.opacity(0.015) })
            .overflow_hidden()
            .child(row)
            .children(body)
            .children(footer)
    }

    /// Claude's questions: each with its options (one, or several when allowed); once answered, the answers.
    fn render_questions(&self, ix: usize, input: &Value, pending: bool, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (fg, muted, border, primary, green, hover) = (theme.foreground, theme.muted_foreground, theme.border, theme.primary, theme.green, ui::hover_bg(cx));
        let questions = input["questions"].as_array().cloned().unwrap_or_default();
        v_flex().border_t_1().border_color(border).px(r(14.)).py(r(12.)).gap(r(14.)).children(questions.iter().enumerate().map(|(q, question)| {
            let multi = question["multiSelect"] == true;
            let given = input["answers"][text_of(&question["question"])].as_str().map(str::to_string);
            let head = v_flex()
                .gap(r(3.))
                .child(div().text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(text_of(&question["header"]).to_uppercase()))
                .child(div().text_size(r(14.)).font_weight(FontWeight::SEMIBOLD).child(text_of(&question["question"])))
                .when(multi && pending, |d| d.child(div().text_size(r(11.5)).text_color(muted).child("Plusieurs choix possibles")));
            let body: AnyElement = match given {
                Some(a) if !pending => h_flex().gap(r(6.)).text_size(r(13.)).text_color(green).child(Icon::new(IconName::Check).xsmall()).child(if a.is_empty() { "(sans réponse)".to_string() } else { a }).into_any_element(),
                _ => v_flex()
                    .gap(r(6.))
                    .children(question["options"].as_array().cloned().unwrap_or_default().iter().enumerate().map(|(o, option)| {
                        let on = self.picked.get(&(ix, q)).is_some_and(|c| c.contains(&o));
                        let mark = div()
                            .size(r(16.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .border_1()
                            .map(|d| if multi { d.rounded(r(4.)) } else { d.rounded_full() })
                            .map(|d| if on { d.border_color(primary).bg(primary) } else { d.border_color(muted.opacity(0.6)) })
                            .when(on, |d| d.child(Icon::new(IconName::Check).size(r(11.)).text_color(theme.primary_foreground)));
                        h_flex()
                            .id(SharedString::from(format!("option-{ix}-{q}-{o}")))
                            .gap(r(10.))
                            .px(r(12.))
                            .py(r(8.))
                            .rounded(r(8.))
                            .border_1()
                            .map(|d| if on { d.border_color(primary.opacity(0.7)).bg(primary.opacity(0.08)) } else { d.border_color(border) })
                            .when(pending, |d| d.cursor_pointer().hover(|d| d.bg(hover)).on_click(cx.listener(move |this, _, _, cx| this.pick(ix, q, o, cx))))
                            .child(mark)
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_size(r(13.5)).text_color(fg).child(text_of(&option["label"])))
                                    .when(option["description"].as_str().is_some_and(|d| !d.is_empty()), |d| d.child(div().text_size(r(12.)).text_color(muted).child(text_of(&option["description"])))),
                            )
                    }))
                    .into_any_element(),
            };
            v_flex().gap(r(8.)).child(head).child(body)
        }))
    }

    /// This project's conversations: the latest first, a click goes back in.
    fn render_past(&self, limit: usize, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border, hover) = (theme.muted_foreground, theme.border, ui::hover_bg(cx));
        let Some(past) = &self.past else { return div().child(ui::loading("Lecture des conversations…", cx)) };
        if past.is_empty() {
            return div().text_size(r(13.)).text_color(muted).child("Aucune conversation enregistrée pour ce projet.");
        }
        v_flex().gap(r(4.)).children(past.iter().take(limit).map(|p| {
            let current = self.session.as_deref() == Some(p.id.as_str());
            let meta = match p.cost {
                Some(c) if c > 0. => format!("{} · {}", when(p.modified), usd(c)),
                _ => when(p.modified),
            };
            let id = p.id.clone();
            h_flex()
                .id(SharedString::from(format!("past-{}", p.id)))
                .gap(r(10.))
                .px(r(12.))
                .py(r(8.))
                .rounded(r(8.))
                .border_1()
                .border_color(if current { theme.primary.opacity(0.5) } else { border })
                .cursor_pointer()
                .hover(|d| d.bg(hover))
                .child(Icon::empty().path("icons/history.svg").small().text_color(muted))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().text_size(r(13.5)).font_weight(FontWeight::MEDIUM).child(p.title.clone()))
                        .child(div().text_size(r(11.5)).text_color(muted).child(if current { format!("{meta} · en cours") } else { meta })),
                )
                .child(Icon::new(IconName::ArrowRight).xsmall().text_color(muted))
                .on_click(cx.listener(move |this, _, _, cx| this.resume(id.clone(), cx)))
        }))
    }

    /// Mono lines, `-`/`+` tinted as a diff; long outputs cut.
    fn render_lines(&self, lines: Vec<(char, String)>, cx: &mut Context<Self>) -> Div {
        const MAX: usize = 40;
        let theme = cx.theme();
        let (muted, border, mono, red, green) = (theme.muted_foreground, theme.border, theme.mono_font_family.clone(), theme.red, theme.green);
        let more = lines.len().saturating_sub(MAX);
        let empty = lines.iter().all(|l| l.1.trim().is_empty());
        v_flex()
            .border_t_1()
            .border_color(border)
            .py(r(6.))
            .font_family(mono)
            .text_size(r(12.))
            .when(empty, |d| d.child(div().px(r(12.)).text_color(muted).child("(rien en sortie)")))
            .when(!empty, |d| {
                d.children(lines.into_iter().take(MAX).map(|(mark, text)| {
                    let (bg, color) = match mark {
                        '-' => (red.opacity(0.10), Some(red)),
                        '+' => (green.opacity(0.10), Some(green)),
                        _ => (gpui::transparent_black(), None),
                    };
                    h_flex()
                        .px(r(12.))
                        .bg(bg)
                        .child(div().w(r(16.)).flex_shrink_0().text_color(color.unwrap_or(muted)).child(if mark == ' ' { String::new() } else { mark.to_string() }))
                        .child(div().flex_1().min_w_0().truncate().child(if text.is_empty() { " ".to_string() } else { text }))
                }))
            })
            .when(more > 0, |d| d.child(div().px(r(12.)).pt(r(4.)).text_color(muted).child(format!("… {more} lignes de plus"))))
    }

    fn render_item(&self, ix: usize, item: &Item, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, primary, red) = (theme.muted_foreground, theme.primary, theme.red);
        match item {
            Item::User(text, file) => v_flex()
                .flex_shrink_0()
                .items_end()
                .gap(r(4.))
                .child(div().max_w(relative(0.85)).px(r(14.)).py(r(9.)).rounded(r(14.)).bg(primary.opacity(0.14)).text_size(r(14.)).child(text.clone()))
                .when_some(file.clone(), |d, f| d.child(h_flex().gap(r(4.)).text_size(r(11.5)).text_color(muted).child(Icon::new(IconName::File).xsmall()).child(f)))
                .into_any_element(),
            Item::Text(text) => div().flex_shrink_0().text_size(r(14.)).child(TextView::markdown(("claude-md", ix), text.clone(), window, cx).selectable(true)).into_any_element(),
            Item::Tool { .. } => self.render_tool(ix, item, window, cx).into_any_element(),
            Item::Turn { secs: s, usage } => h_flex()
                .flex_shrink_0()
                .gap(r(8.))
                .text_size(r(11.5))
                .text_color(muted)
                .child(div().h(px(1.)).w(r(24.)).bg(theme.border))
                .child(format!("{} · {} tokens · {}", secs(*s), tokens(usage.tokens()), usd(usage.cost)))
                .into_any_element(),
            Item::Notice(text, error) => div()
                .flex_shrink_0()
                .px(r(12.))
                .py(r(8.))
                .rounded(r(8.))
                .text_size(r(13.))
                .map(|d| if *error { d.bg(red.opacity(0.08)).text_color(red) } else { d.text_color(muted) })
                .child(text.clone())
                .into_any_element(),
        }
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border, bg, fg) = (theme.muted_foreground, theme.border, theme.background, theme.foreground);
        let file = self.last_file(cx).map(|p| self.rel(&p));
        let models = self.segmented("model", MODELS.iter().map(|m| (m.0, None)).collect(), self.model, None, cx, |this, i, _, cx| this.choose(Some(i), None, cx));
        let modes = self.segmented("mode", MODES.iter().map(|m| (m.0, Some(m.2))).collect(), self.mode, Some(BYPASS), cx, |this, i, window, cx| this.choose_mode(i, window, cx));
        let theme = cx.theme();
        let chip = file.map(|f| {
            let on = self.with_file;
            h_flex()
                .id("with-file")
                .flex_shrink_0()
                .max_w(r(220.))
                .gap(r(5.))
                .px(r(8.))
                .py(r(4.))
                .rounded(r(8.))
                .cursor_pointer()
                .text_size(r(12.))
                .border_1()
                .map(|d| if on { d.border_color(theme.primary.opacity(0.5)).text_color(fg) } else { d.border_color(border).text_color(muted).line_through() })
                .tooltip(move |window, cx| Tooltip::new(if on { "Claude saura quel fichier est ouvert (cliquez pour l'ignorer)" } else { "Fichier ignoré (cliquez pour l'inclure)" }).build(window, cx))
                .child(Icon::new(IconName::File).xsmall())
                .child(div().min_w_0().truncate().child(f))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.with_file = !this.with_file;
                    cx.notify();
                }))
        });
        let send = if self.busy {
            button("claude-stop").small().icon(Icon::new(IconName::Close)).label("Arrêter").on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
        } else {
            button("claude-send").primary().small().icon(IconName::ArrowUp).label("Envoyer").on_click(cx.listener(|this, _, window, cx| this.send_input(window, cx)))
        };
        h_flex().flex_shrink_0().justify_center().px(r(24.)).pb(r(16.)).pt(r(6.)).child(
            v_flex()
                .w_full()
                .max_w(r(860.))
                .p(r(10.))
                .gap(r(8.))
                .rounded(r(14.))
                .border_1()
                .border_color(border)
                .bg(bg)
                .shadow_sm()
                .child(self.render_folder("composer-folder", cx))
                .child(Input::new(&self.input).appearance(false).text_size(r(14.)))
                .child(
                    // The choices wrap when narrow; sending stays at the right.
                    h_flex()
                        .items_end()
                        .gap(r(8.))
                        .child(h_flex().flex_1().min_w_0().flex_wrap().gap(r(8.)).child(models).child(modes).children(chip))
                        .child(div().flex_shrink_0().pb(r(6.)).text_size(r(11.5)).text_color(muted).child("⌘⏎"))
                        .child(div().flex_shrink_0().child(send)),
                ),
        )
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border, hover) = (theme.muted_foreground, theme.border, ui::hover_bg(cx));
        ui::empty_state(Icon::empty().path("icons/claude-code.svg"), rgb(ORANGE).into(), "Que voulez-vous faire ?", "Claude Code lit et modifie les fichiers de ce dossier, avec vos réglages, vos plugins et votre abonnement :", cx)
        .child(self.render_folder("empty-folder", cx).max_w(r(560.)).mb(r(4.)))
        .child(
            v_flex().mt(r(10.)).gap(r(6.)).children(SUGGESTIONS.iter().enumerate().map(|(i, s)| {
                h_flex()
                    .id(SharedString::from(format!("suggest-{i}")))
                    .gap(r(8.))
                    .px(r(12.))
                    .py(r(7.))
                    .rounded(r(8.))
                    .border_1()
                    .border_color(border)
                    .cursor_pointer()
                    .hover(|d| d.bg(hover))
                    .text_size(r(13.))
                    .child(Icon::new(IconName::ArrowRight).xsmall().text_color(muted))
                    .child(*s)
                    .on_click(cx.listener(move |this, _, window, cx| this.send(s.to_string(), window, cx)))
            })),
        )
        .when(self.past.as_ref().is_some_and(|p| !p.is_empty()), |d| {
            d.child(v_flex().mt(r(18.)).w(r(420.)).gap(r(8.)).child(ui::section_title("Reprendre une conversation", cx)).child(self.render_past(3, cx)))
        })
    }

    /// One row of the conversation's list: a card, or (the last one) what Claude is doing.
    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let content: AnyElement = match self.items.get(ix) {
            Some(item) => self.render_item(ix, item, window, cx),
            None => {
                let muted = cx.theme().muted_foreground;
                let waiting = self.items.iter().any(|i| matches!(i, Item::Tool { ask: Some((_, None)), .. }));
                let shown = self.busy && !waiting && (self.thinking || !matches!(self.items.last(), Some(Item::Text(_))));
                let working = matches!(self.items.last(), Some(Item::Tool { result: None, .. }));
                h_flex()
                    .gap(r(8.))
                    .text_size(r(13.))
                    .text_color(muted)
                    .when(shown, |d| d.child(Spinner::new().small().color(rgb(ORANGE).into())).child(if working { "Claude travaille…" } else { "Claude réfléchit…" }))
                    .into_any_element()
            }
        };
        // Block layout, centered by margins: cheaper to lay out than a flex row.
        div().block().px(r(24.)).pt(r(if ix == 0 { 20. } else { 0. })).pb(r(14.)).child(div().block().w_full().max_w(r(860.)).mx_auto().child(content)).into_any_element()
    }

    fn render_conversation(&mut self, cx: &mut Context<Self>) -> Div {
        self.sync_log();
        if self.show_past {
            let list = v_flex()
                .w_full()
                .max_w(r(860.))
                .gap(r(12.))
                .child(
                    h_flex()
                        .gap(r(10.))
                        .child(div().flex_1().text_size(r(16.)).font_weight(FontWeight::SEMIBOLD).child("Conversations de ce projet"))
                        .child(button("past-back").ghost().small().icon(IconName::ArrowLeft).label("Retour").on_click(cx.listener(|this, _, _, cx| this.toggle_past(cx)))),
                )
                .child(self.render_past(usize::MAX, cx));
            return v_flex().flex_1().min_w_0().h_full().child(div().id("claude-past").size_full().overflow_y_scroll().child(h_flex().justify_center().px(r(24.)).py(r(20.)).child(list)));
        }
        let log: AnyElement = if self.items.is_empty() {
            // Scrolls when the recent conversations make it taller than the tab.
            div().id("claude-empty").size_full().overflow_y_scroll().child(self.render_empty(cx).h_auto().min_h_full()).into_any_element()
        } else {
            list(self.log.clone(), cx.processor(|this, ix: usize, window, cx| this.render_row(ix, window, cx))).size_full().into_any_element()
        };
        v_flex().flex_1().min_w_0().h_full().child(div().flex_1().min_h_0().child(log)).child(self.render_composer(cx))
    }

    fn render_stats(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let theme = cx.theme();
        let (fg, muted, border, hover) = (theme.foreground, theme.muted_foreground, theme.border, ui::hover_bg(cx));
        let (green, yellow, red) = (theme.green, theme.yellow, theme.red);
        let s = &self.stats;
        let bar = |part: f64, color: Hsla| div().h(r(6.)).w_full().rounded_full().bg(fg.opacity(0.08)).child(div().h_full().rounded_full().w(relative(part.clamp(0., 1.) as f32)).bg(color));
        let level = |part: f64| if part < 0.6 { green } else if part < 0.85 { yellow } else { red };
        let tile_stat = |value: String, name: &'static str| {
            v_flex().flex_1().min_w_0().p(r(10.)).rounded(r(8.)).bg(fg.opacity(0.04)).child(div().text_size(r(15.)).font_weight(FontWeight::SEMIBOLD).child(value)).child(div().text_size(r(11.5)).text_color(muted).child(name))
        };
        let section = |title: &str| ui::section_title(title, cx).mt(r(6.));

        let mut panel = v_flex()
            .id("claude-stats")
            .w(r(300.))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(border)
            .p(r(18.))
            .gap(r(10.))
            .child(section("Cette session"))
            .child(
                v_flex()
                    .child(div().text_size(r(26.)).font_weight(FontWeight::SEMIBOLD).child(usd(s.usage.cost)))
                    .child(div().text_size(r(11.5)).text_color(muted).child(if self.subscription { "au tarif de l'API · inclus dans votre abonnement" } else { "estimé au tarif de l'API" })),
            )
            .child(h_flex().gap(r(8.)).child(tile_stat(tokens(s.usage.input), "Entrée")).child(tile_stat(tokens(s.usage.output), "Sortie")))
            .child(h_flex().gap(r(8.)).child(tile_stat(tokens(s.usage.cache_read), "Cache lu")).child(tile_stat(tokens(s.usage.cache_write), "Cache écrit")))
            .child(h_flex().gap(r(8.)).child(tile_stat(s.turns.to_string(), "Échanges")).child(tile_stat(secs(s.secs), "Temps de réponse")));
        if s.context > 0 {
            let part = if s.window > 0 { s.context as f64 / s.window as f64 } else { 0. };
            let of = if s.window > 0 { format!("{} / {} · {:.0} %", tokens(s.context), tokens(s.window), part * 100.) } else { tokens(s.context) };
            panel = panel.child(
                v_flex()
                    .gap(r(6.))
                    .mt(r(4.))
                    .child(h_flex().text_size(r(12.5)).child(div().flex_1().child("Contexte")).child(div().text_color(muted).child(of)))
                    .child(bar(part, level(part))),
            );
        }
        if s.models.len() > 1 {
            panel = panel.child(section("Par modèle")).children(s.models.iter().map(|(m, u)| {
                h_flex().gap(r(8.)).text_size(r(12.5)).child(div().flex_1().min_w_0().truncate().child(m.clone())).child(div().text_color(muted).child(format!("{} · {}", tokens(u.tokens()), usd(u.cost))))
            }));
        }
        if !self.limits.is_empty() {
            panel = panel.child(section("Limites de l'abonnement")).children(self.limits.iter().map(|&(name, part, at)| {
                v_flex()
                    .gap(r(5.))
                    .child(h_flex().text_size(r(12.5)).child(div().flex_1().child(name)).child(div().text_color(level(part)).child(format!("{:.0} %", part * 100.))))
                    .child(bar(part, level(part)))
                    .when(at > 0, |d| d.child(div().text_size(r(11.)).text_color(muted).child(format!("Réinitialisée {}", resets_in(at)))))
            }));
        }
        if !s.tools.is_empty() {
            let mut tools: Vec<_> = s.tools.iter().collect();
            tools.sort_by(|a, b| b.1.cmp(a.1));
            panel = panel.child(section("Outils utilisés")).child(h_flex().flex_wrap().gap(r(6.)).children(tools.into_iter().map(|(name, n)| {
                h_flex().gap(r(5.)).px(r(8.)).py(r(3.)).rounded(r(6.)).bg(fg.opacity(0.05)).text_size(r(12.)).child(name.clone()).child(div().text_color(muted).child(n.to_string()))
            })));
        }
        if !s.files.is_empty() {
            panel = panel.child(section("Fichiers modifiés")).children(s.files.iter().enumerate().map(|(i, f)| {
                let path = f.clone();
                h_flex()
                    .id(SharedString::from(format!("edited-{i}")))
                    .gap(r(6.))
                    .px(r(6.))
                    .py(r(4.))
                    .rounded(r(6.))
                    .cursor_pointer()
                    .hover(|d| d.bg(hover))
                    .text_size(r(12.5))
                    .child(Icon::empty().path("icons/pencil.svg").xsmall().text_color(muted))
                    .child(div().min_w_0().truncate().child(self.rel(f)))
                    .on_click(cx.listener(move |this, _, window, cx| this.open(path.clone(), window, cx)))
            }));
        }

        // Day by day, from Akolade's own record.
        let (today, week, all) = (self.history.since(&day(0)), self.history.since(&day(6)), self.history.since(""));
        let row = |name: &'static str, d: &Day| {
            h_flex().text_size(r(12.5)).child(div().flex_1().child(name)).child(div().text_color(muted).child(format!("{} · {}", tokens(d.usage.tokens()), usd(d.usage.cost))))
        };
        let days: Vec<(String, f64)> = (0..14).rev().map(|ago| day(ago)).map(|d| (d.clone(), self.history.days.get(&d).map_or(0., |d| d.usage.cost))).collect();
        let top = days.iter().map(|d| d.1).fold(0., f64::max);
        let orange: Hsla = rgb(ORANGE).into();
        panel
            .child(section("Historique"))
            .child(row("Aujourd'hui", &today))
            .child(row("7 derniers jours", &week))
            .child(row("Depuis le début", &all))
            .child(div().text_size(r(11.5)).text_color(muted).child(format!("{} sessions · {} échanges au total", all.sessions, all.turns)))
            .child(
                h_flex().h(r(56.)).mt(r(4.)).items_end().gap(r(3.)).children(days.into_iter().map(|(d, cost)| {
                    let part = if top > 0. { (cost / top) as f32 } else { 0. };
                    div()
                        .id(SharedString::from(format!("day-{d}")))
                        .flex_1()
                        .h(relative(part.max(0.04)))
                        .rounded_t(r(3.))
                        .bg(if cost > 0. { orange } else { fg.opacity(0.08) })
                        .tooltip(move |window, cx| Tooltip::new(format!("{} : {}", &d[5..].replace('-', "/"), usd(cost))).build(window, cx))
                }))
            )
            .child(h_flex().text_size(r(11.)).text_color(muted).child(div().flex_1().child("il y a 14 jours")).child("aujourd'hui"))
    }
}

impl Render for ClaudeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Another folder opened: a new conversation, there (unless Claude works in a folder of its own).
        let project = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf));
        if project != self.project {
            self.project = project.clone();
            if self.custom {
                self.custom = self.project.is_some() && self.project != self.root;
            } else if project != self.root && self.busy {
                // Claude is working there: it goes on, in that folder.
                self.custom = self.root.is_some();
            } else if project != self.root {
                (self.root, self.past) = (project, None);
                self.reset(cx);
            }
        }
        // The list under the suggestions, read once per folder.
        if self.past.is_none() && self.root.is_some() && matches!(self._past_for.as_deref(), p if p != self.root.as_deref()) {
            self._past_for = self.root.clone();
            self.load_past(cx);
        }
        // Installed meanwhile (the button below): found without restarting.
        if self.program.is_none() {
            self.program = find_claude();
        }
        let t = tool("claude");
        let subtitle = match (&self.root, self.model_name.is_empty()) {
            (Some(_), false) => format!("{} · {} · dans {}", self.model_name, MODES[self.mode].0.to_lowercase(), self.short_folder()),
            (Some(_), true) => format!("Travaille dans {}", self.short_folder()),
            _ => "Claude Code dans l'éditeur : conversation, outils, coût et limites.".into(),
        };
        let actions = vec![
            button("claude-past").map(|b| if self.show_past { b.primary() } else { b.ghost() }).small().icon(Icon::empty().path("icons/history.svg")).label("Conversations").tooltip("Reprendre une conversation passée").disabled(self.program.is_none() || self.root.is_none()).on_click(cx.listener(|this, _, _, cx| this.toggle_past(cx))).into_any_element(),
            button("claude-terminal").ghost().small().icon(IconName::SquareTerminal).tooltip("Continuer cette conversation dans le terminal").disabled(self.program.is_none()).on_click(cx.listener(|this, _, window, cx| this.in_terminal(window, cx))).into_any_element(),
            button("claude-new").outline().small().icon(IconName::Plus).label("Nouvelle session").disabled(self.items.is_empty()).on_click(cx.listener(|this, _, _, cx| this.reset(cx))).into_any_element(),
        ];
        let body: AnyElement = if self.program.is_none() {
            ui::empty_state(Icon::empty().path("icons/claude-code.svg"), rgb(ORANGE).into(), "Claude Code n'est pas installé", "Installez-le depuis claude.com/code et connectez-vous une fois dans le terminal : cet outil le trouvera tout seul.", cx)
                .child(button("claude-install").primary().mt(r(8.)).label("Installer dans le terminal").on_click(cx.listener(|this, _, window, cx| in_terminal(&this.workspace, "curl -fsSL https://claude.ai/install.sh | bash", window, cx))))
                .into_any_element()
        } else if self.root.is_none() {
            ui::empty_state(Icon::new(IconName::FolderOpen), rgb(ORANGE).into(), "Où Claude doit-il travailler ?", "Ouvrez un projet dans l'éditeur, ou choisissez un dossier rien que pour Claude.", cx)
                .child(button("claude-pick-folder").primary().mt(r(8.)).icon(IconName::FolderOpen).label("Choisir un dossier").on_click(cx.listener(|this, _, window, cx| this.pick_folder(window, cx))))
                .into_any_element()
        } else {
            h_flex().flex_1().min_h_0().child(self.render_conversation(cx)).child(self.render_stats(cx)).into_any_element()
        };
        v_flex().size_full().child(header(t, subtitle, actions, cx)).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_turn_of_the_cli() {
        // Lines as `claude` 2.1 prints them (trimmed).
        let init = r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-haiku-5-5","apiKeySource":"none"}"#;
        assert_eq!(parse(init), vec![Event::Init { session: "s1".into(), model: "claude-haiku-5-5".into(), subscription: true }]);
        let start = r#"{"type":"stream_event","event":{"type":"message_start","message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":10,"cache_read_input_tokens":30}}},"parent_tool_use_id":null}"#;
        assert_eq!(parse(start), vec![Event::Context(42)]);
        let delta = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"OK"}},"parent_tool_use_id":null}"#;
        assert_eq!(parse(delta), vec![Event::Text("OK".into())]);
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"parent_tool_use_id":null}"#;
        assert_eq!(parse(tool), vec![Event::ToolUse { id: "t1".into(), name: "Bash".into(), input: json!({ "command": "ls" }) }]);
        let result = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"a\nb"}],"is_error":true}]},"parent_tool_use_id":null}"#;
        assert_eq!(parse(result), vec![Event::ToolResult { id: "t1".into(), text: "a\nb".into(), error: true }]);
        let ask = r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"rm x"},"tool_use_id":"t1"}}"#;
        assert_eq!(parse(ask), vec![Event::Ask { request: "r1".into(), tool_id: "t1".into(), tool: "Bash".into(), input: json!({ "command": "rm x" }) }]);
        assert_eq!(parse(r#"{"type":"control_request","request_id":"r2","request":{"subtype":"hook_callback"}}"#), vec![Event::Unsupported { request: "r2".into() }]);
        // A sub-agent's steps are not the conversation's.
        assert!(parse(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{}}]},"parent_tool_use_id":"t9"}"#).is_empty());
        let end = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":3932,"result":"OK","total_cost_usd":0.0048,
            "model_usage_since_last_result":{"claude-haiku-5-5":{"inputTokens":2,"outputTokens":36,"cacheReadInputTokens":32423,"cacheCreationInputTokens":114,"costUSD":0.000365,"contextWindow":1000000}}}"#;
        let usage = Usage { input: 2, output: 36, cache_read: 32423, cache_write: 114, cost: 0.000365 };
        assert_eq!(parse(end), vec![Event::Result { error: false, text: "OK".into(), secs: 3.932, models: vec![("claude-haiku-5-5".into(), usage, 1_000_000)] }]);
        let limits = r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.15,"resetsAt":100},"seven_day":{"utilization":0.44,"resetsAt":200}}}}"#;
        assert_eq!(parse(limits), vec![Event::Limits(vec![("5 heures", 0.15, 100), ("7 jours", 0.44, 200)])]);
        let budget = r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,"duration_ms":10,"errors":["Reached maximum budget ($0.0001)"]}"#;
        assert!(matches!(&parse(budget)[..], [Event::Result { error: true, text, .. }] if text.starts_with("Budget maximum")));
        let other = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["boom"]}"#;
        assert!(matches!(&parse(other)[..], [Event::Result { text, .. }] if text == "boom"));
        assert!(parse("pas du json").is_empty());
    }

    #[test]
    fn reads_saved_conversations() {
        assert_eq!(encode(Path::new("/private/tmp/a.b/-c d")), "-private-tmp-a-b--c-d");
        let file = [
            r#"{"type":"queue-operation","operation":"enqueue"}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"[Image: original 10x10]"}}"#,
            r#"{"type":"user","message":{"role":"user","content":"Corrige maximum\n\n(Fichier ouvert dans l'éditeur : src/calc.py)"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"/p/calc.py"}}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"content":[{"type":"text","text":"sous-agent"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"C'est corrigé."}]}}"#,
            r#"{"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}}"#,
            r#"{"type":"ai-title","aiTitle":"Correction de maximum"}"#,
            r#"{"type":"cost-state","totalCostUSD":0.25}"#,
        ]
        .join("\n");
        let t = SystemTime::UNIX_EPOCH;
        assert_eq!(read_past("s1".into(), &file, t), Some(Past { id: "s1".into(), title: "Correction de maximum".into(), modified: t, cost: Some(0.25) }));
        // Without a title: the first prompt, without the file we add.
        assert_eq!(read_past("s2".into(), &file.replace("ai-title", "x"), t).map(|p| p.title), Some("Corrige maximum".into()));
        assert_eq!(read_past("s3".into(), r#"{"type":"queue-operation"}"#, t), None);
        let items = transcript(file.lines().map(str::to_string));
        assert_eq!(items.len(), 3);
        assert!(matches!(&items[0], Item::User(text, Some(f)) if text == "Corrige maximum" && f == "src/calc.py"));
        assert!(matches!(&items[1], Item::Tool { name, result: Some((out, false)), .. } if name == "Edit" && out == "ok"));
        assert!(matches!(&items[2], Item::Text(text) if text == "C'est corrigé."));
    }

    #[test]
    fn answers_permission_requests_as_the_cli_expects() {
        let input = json!({ "command": "ls" });
        assert_eq!(
            permission_response("r1", true, &input),
            json!({ "type": "control_response", "response": { "subtype": "success", "request_id": "r1", "response": { "behavior": "allow", "updatedInput": { "command": "ls" } } } })
        );
        let no = permission_response("r2", false, &input);
        assert_eq!((no["response"]["request_id"].as_str(), no["response"]["response"]["behavior"].as_str()), (Some("r2"), Some("deny")));
        assert!(no["response"]["response"]["message"].is_string(), "a refusal says why");
    }

    #[test]
    fn notes_are_not_prompts_but_tags_typed_by_you_are() {
        let user = |content: &str| json!({ "type": "user", "message": { "content": content } });
        assert_eq!(prompt_of(&user("<div> ne se centre pas")), Some("<div> ne se centre pas".into()));
        assert_eq!(prompt_of(&user("<command-name>/clear</command-name>")), None);
        assert_eq!(prompt_of(&json!({ "type": "assistant", "message": { "content": "x" } })), None);
    }

    #[test]
    fn history_sums_days() {
        let mut h = History::default();
        let u = Usage { input: 10, output: 5, cache_read: 0, cache_write: 0, cost: 0.5 };
        h.record("2026-11-01", &u, 1, 1);
        h.record("2026-11-15", &u, 2, 0);
        h.record("2026-11-15", &u, 1, 0);
        let week = h.since("2026-11-09");
        assert_eq!((week.usage.tokens(), week.usage.cost, week.turns, week.sessions), (30, 1.0, 3, 0));
        assert_eq!(h.since("").turns, 4);
    }

    #[test]
    fn shows_tools_and_numbers() {
        let root = Path::new("/p");
        assert_eq!(summary("Edit", &json!({ "file_path": "/p/src/a.rs" }), root), "src/a.rs");
        assert_eq!(summary("Bash", &json!({ "command": "cargo test\n--all" }), root), "cargo test");
        assert_eq!(summary("Grep", &json!({ "pattern": "fn", "path": "/p/src" }), root), "fn dans src");
        assert_eq!(label("mcp__github__create_issue"), "create_issue");
        assert_eq!(details("Edit", &json!({ "old_string": "a", "new_string": "b\nc" }), None), vec![('-', "a".into()), ('+', "b".into()), ('+', "c".into())]);
        assert_eq!(edited_file("Write", &json!({ "file_path": "/p/x" })), Some(PathBuf::from("/p/x")));
        assert_eq!(edited_file("Read", &json!({ "file_path": "/p/x" })), None);
        assert_eq!((tokens(950), tokens(12_400), tokens(1_250_000)), ("950".into(), "12,4 k".into(), "1,2 M".into()));
        assert_eq!((usd(0.), usd(0.0044), usd(0.153), usd(12.4)), ("0 $".into(), "0,0044 $".into(), "0,153 $".into(), "12,40 $".into()));
    }
}
