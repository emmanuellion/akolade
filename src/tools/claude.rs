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
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime},
};

use futures::{StreamExt, channel::mpsc};
use gpui::{
    AnyElement, AppContext, ClipboardEntry, Context, Div, ElementId, EntityInputHandler, StyleRefinement, Entity, FontWeight, Hsla, ListAlignment, ListState, PromptLevel, list, InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div, img, prelude::FluentBuilder, px, relative, rgb,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable,
    button::ButtonVariants,
    h_flex,
    input::{Enter, IndentInline, Input, InputEvent, InputState, Paste},
    menu::{DropdownMenu, PopupMenu, PopupMenuItem},
    spinner::Spinner,
    clipboard::Clipboard,
    text::{TextView, TextViewStyle},
    tooltip::Tooltip,
    v_flex,
};
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
/// What each model of `MODELS` is for, in the model menu.
const MODEL_ABOUT: [&str; 4] = ["Celui de vos réglages Claude Code", "Le plus capable, pour les tâches complexes", "Pour le travail de tous les jours", "Le plus rapide et le moins cher"];

/// A choice of the model menu.
#[derive(Clone, Copy)]
pub enum Pick {
    Model(usize),
    Effort(usize),
    Mode(usize),
}

/// What a choice of the model menu does.
pub type OnPick = std::rc::Rc<dyn Fn(Pick, &mut Window, &mut gpui::App)>;

/// The model menu, as in Claude's chat: each model with what it is for, the current one
/// checked; the thinking effort in a submenu, its value shown.
pub fn model_menu(mut menu: PopupMenu, model: usize, effort: usize, pick: OnPick, window: &mut Window, cx: &mut Context<PopupMenu>) -> PopupMenu {
    let on = |p: Pick| {
        let pick = pick.clone();
        move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut gpui::App| pick(p, window, cx)
    };
    menu = menu.min_w(px(300.)).check_side(gpui_component::Side::Right);
    for (i, (m, about)) in MODELS.iter().zip(MODEL_ABOUT).enumerate() {
        // All rows one width: the checks line up on the right.
        let row = move |_: &mut Window, cx: &mut gpui::App| v_flex().w(r(250.)).py(r(4.)).child(div().child(m.0)).child(div().text_size(r(12.)).text_color(cx.theme().muted_foreground).child(about));
        menu = menu.item(PopupMenuItem::element(row).checked(i == model).on_click(on(Pick::Model(i))));
    }
    let efforts: Vec<_> = (0..EFFORTS.len()).map(|i| on(Pick::Effort(i))).collect();
    menu.separator().submenu(format!("Effort\t{}", EFFORTS[effort].1), window, cx, move |mut sub, _, _| {
        sub = sub.min_w(px(170.)).check_side(gpui_component::Side::Right);
        for (i, e) in EFFORTS.iter().enumerate() {
            let click = efforts[i].clone();
            sub = sub.item(PopupMenuItem::new(e.1).checked(i == effort).on_click(click));
        }
        sub
    })
}

/// The permissions menu: each mode with what it lets Claude do, the current one checked.
pub fn mode_menu(mut menu: PopupMenu, mode: usize, pick: OnPick) -> PopupMenu {
    menu = menu.min_w(px(320.)).check_side(gpui_component::Side::Right);
    for (i, m) in MODES.iter().enumerate() {
        let pick = pick.clone();
        let row = move |_: &mut Window, cx: &mut gpui::App| v_flex().w(r(270.)).py(r(4.)).child(div().child(m.0)).child(div().text_size(r(12.)).text_color(cx.theme().muted_foreground).child(m.2));
        menu = menu.item(PopupMenuItem::element(row).checked(i == mode).on_click(move |_, window, cx| pick(Pick::Mode(i), window, cx)));
    }
    menu
}

/// `--effort`: (value, label); "" lets Claude Code choose.
pub const EFFORTS: [(&str, &str); 6] = [("", "Par défaut"), ("low", "Faible"), ("medium", "Moyen"), ("high", "Élevé"), ("xhigh", "Très élevé"), ("max", "Maximum")];
const SUGGESTIONS: [&str; 3] = ["Explique-moi l'architecture de ce projet", "Cherche des bugs dans le fichier ouvert", "Écris des tests pour le fichier ouvert"];

mod history;
mod protocol;
mod surface;

use history::*;
use protocol::*;

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
    /// `opts`: a session's own effort, budget, instructions and arguments, over the settings'.
    fn spawn(program: &Path, root: &Path, model: &str, mode: &str, resume: Option<&str>, settings: &AppSettings, opts: &SessionOptions) -> std::io::Result<(Proc, mpsc::UnboundedReceiver<Event>)> {
        let mut cmd = crate::platform::command(program);
        // `--replay-user-messages`: a message sent while Claude works says when it is taken.
        cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--replay-user-messages"]);
        // Permission requests come to us on stdin/stdout (the cards' buttons).
        cmd.args(["--permission-prompt-tool", "stdio", "--permission-mode", mode]);
        if !model.is_empty() {
            cmd.args(["--model", model]);
        }
        if let Some(id) = resume {
            cmd.args(["--resume", id]);
        }
        let effort = opts.effort.as_deref().unwrap_or(&settings.claude_effort);
        if !effort.is_empty() {
            cmd.args(["--effort", effort]);
        }
        let budget = opts.budget.unwrap_or(settings.claude_budget);
        if budget > 0. {
            cmd.args(["--max-budget-usd", &budget.to_string()]);
        }
        let prompt = [settings.claude_instructions.trim(), opts.system_prompt.trim()].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n");
        if !prompt.is_empty() {
            cmd.args(["--append-system-prompt", &prompt]);
        }
        cmd.args(&opts.extra_args);
        let _child = cmd.current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = crate::platform::own_group(&mut cmd).spawn()?;
        RUNNING.lock().unwrap_or_else(|e| e.into_inner()).push(child.id());
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

/// Every `claude` running, for `kill_all` (Akolade quitting may not drop the views).
static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Akolade quits: each `claude` and what it started stops with it.
pub fn kill_all() {
    for pid in std::mem::take(&mut *RUNNING.lock().unwrap_or_else(|e| e.into_inner())) {
        crate::platform::kill_tree(pid);
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        RUNNING.lock().unwrap_or_else(|e| e.into_inner()).retain(|&pid| pid != child.id());
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
/// `rules`: Claude Code's suggestions, kept as "Toujours autoriser" (in the project's
/// .claude/settings.local.json, as its own "don't ask again").
fn permission_response(request: &str, allow: bool, input: &Value, rules: Option<&Value>) -> Value {
    let response = if allow {
        let mut r = json!({ "behavior": "allow", "updatedInput": input });
        if let Some(rules) = rules {
            r["updatedPermissions"] = rules.clone();
        }
        r
    } else {
        return deny_response(request, "L'utilisateur a refusé depuis Akolade.");
    };
    json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request, "response": response } })
}

/// A refusal with its reason: Claude reads it as the tool's result.
fn deny_response(request: &str, message: &str) -> Value {
    json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request, "response": { "behavior": "deny", "message": message } } })
}

/// What "Toujours autoriser" keeps, in words ("Commande « git status », dans ce projet");
/// None when Claude Code offers nothing to keep.
fn always_text(rules: &Value) -> Option<String> {
    let parts: Vec<String> = rules
        .as_array()?
        .iter()
        .filter_map(|s| {
            let what = match s["type"].as_str()? {
                "addRules" => s["rules"]
                    .as_array()?
                    .iter()
                    .map(|r| match r["ruleContent"].as_str() {
                        Some(c) => format!("{} « {c} »", label(r["toolName"].as_str().unwrap_or_default())),
                        None => label(r["toolName"].as_str().unwrap_or_default()),
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                "setMode" if s["mode"] == "acceptEdits" => "les modifications de fichiers".into(),
                "addDirectories" => format!("le dossier {}", s["directories"].as_array()?.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
                _ => return None,
            };
            let place = match s["destination"].as_str() {
                Some("session") => "pendant cette conversation",
                Some("userSettings") => "dans tous vos projets",
                _ => "dans ce projet",
            };
            Some(format!("{what}, {place}"))
        })
        .collect();
    (!parts.is_empty()).then(|| format!("Ne plus demander : {}", parts.join(" ; ")))
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
    /// When the process that saved `cost` started (see `latest_totals`).
    cost_start: u64,
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
            2 if num(&v["startTime"]) >= self.cost_start => (self.cost, self.cost_start) = (v["totalCostUSD"].as_f64().or(self.cost), num(&v["startTime"])),
            2 => {}
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

/// A saved conversation as cards (your messages, Claude's answers, its tools and their
/// results) and its numbers for the side panel.
///
/// Each `claude` process saves its totals on exit (`cost-state`): the one started last
/// took over the earlier ones' totals, so it holds the whole conversation. Without one
/// (a crash), the answers' tokens are added up, at an unknown cost.
fn transcript(lines: impl Iterator<Item = String>) -> (Vec<Item>, Stats) {
    let (mut items, mut stats) = (vec![], Stats::default());
    let (mut totals, mut totals_start) = (None::<Value>, 0);
    // An answer's blocks come one per line, with the same id: counted once.
    let (mut answers, mut seen) = (BTreeMap::<String, Usage>::new(), HashSet::new());
    for line in lines.filter(|l| l.contains("\"type\":\"user\"") || l.contains("\"type\":\"assistant\"") || l.contains("\"type\":\"cost-state\"")) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v["type"] == "cost-state" {
            if num(&v["startTime"]) >= totals_start {
                totals_start = num(&v["startTime"]);
                totals = Some(v);
            }
            continue;
        }
        // Sub-agents' steps, and the notes Claude Code adds itself ("No response requested.").
        if v["isSidechain"] == true || v["message"]["model"] == "<synthetic>" {
            continue;
        }
        let blocks = v["message"]["content"].as_array().cloned().unwrap_or_default();
        match v["type"].as_str() {
            Some("user") => {
                if let Some(p) = prompt_of(&v) {
                    let (text, file) = split_file(&p);
                    let images = blocks.iter().filter(|b| b["type"] == "image").map(|_| "Image".to_string()).collect();
                    items.push(Item::User(text, file, images, None));
                    stats.turns += 1;
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
                let m = &v["message"];
                if m["id"].as_str().is_some_and(|id| seen.insert(id.to_string())) {
                    let u = &m["usage"];
                    let usage = Usage {
                        input: num(&u["input_tokens"]),
                        output: num(&u["output_tokens"]),
                        cache_read: num(&u["cache_read_input_tokens"]),
                        cache_write: num(&u["cache_creation_input_tokens"]),
                        cost: 0.,
                    };
                    stats.context = usage.input + usage.cache_read + usage.cache_write;
                    answers.entry(text_of(&m["model"])).or_default().add(&usage);
                }
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => items.push(Item::Text(text_of(&b["text"]))),
                        Some("tool_use") => {
                            let (name, input) = (text_of(&b["name"]), b["input"].clone());
                            *stats.tools.entry(label(&name)).or_default() += 1;
                            if let Some(f) = edited_file(&name, &input).filter(|f| !stats.files.contains(f)) {
                                stats.files.push(f);
                            }
                            items.push(Item::Tool { id: text_of(&b["id"]), name, input, result: None, ask: None });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    stats.models = match &totals {
        Some(t) => {
            stats.secs = num(&t["totalAPIDuration"]) as f64 / 1000.;
            t["modelUsage"].as_object().into_iter().flatten().map(|(name, m)| (name.clone(), model_usage(m))).collect()
        }
        None => answers,
    };
    stats.models.values().for_each(|u| stats.usage.add(u));
    (items, stats)
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

/// Markdown from Claude, at a conversation's scale: gpui-component's headings are page
/// titles (28 px for `#`), its code blocks unframed. Code blocks get a copy button.
fn markdown(id: impl Into<ElementId>, text: String, window: &mut Window, cx: &mut gpui::App) -> TextView {
    let theme = cx.theme();
    let code = StyleRefinement::default().rounded(r(8.)).border_1().border_color(theme.border).px(r(14.)).py(r(11.)).text_size(r(12.5));
    let style = TextViewStyle {
        heading_base_font_size: window.rem_size() * (14.5 / 14.),
        // The same theme as the parse below (`TextView::markdown` takes the app's): no second parse.
        highlight_theme: theme.highlight_theme.clone(),
        is_dark: theme.is_dark(),
        ..TextViewStyle::default()
    }
    .paragraph_gap(gpui::rems(0.75))
    .heading_font_size(|level, base| base * if level == 1 { 1.3 } else if level == 2 { 1.18 } else if level == 3 { 1.08 } else { 1. })
    .code_block(code);
    TextView::markdown(id, text, window, cx).style(style).selectable(true).code_block_actions(|block, _, _| Clipboard::new("copy").value(block.code()))
}

// ---------------------------------------------------------------- attachments

/// Pasted in the box, sent with the next message.
enum Attachment {
    /// A long text: a chip, not hundreds of lines in the box.
    Text(String),
    /// An image as the API takes it: its media type and bytes, and a preview.
    Image { media: &'static str, bytes: Vec<u8>, preview: Arc<gpui::Image> },
}

impl Attachment {
    fn label(&self) -> String {
        match self {
            Attachment::Text(t) => format!("Texte collé, {} lignes", t.lines().count()),
            Attachment::Image { .. } => "Image".into(),
        }
    }
}

/// `shown` moved `n` characters further in `text` (a byte index, on a character).
fn reveal_to(text: &str, shown: usize, n: usize) -> usize {
    let shown = shown.min(text.len());
    text[shown..].char_indices().nth(n).map_or(text.len(), |(i, _)| shown + i)
}

/// Pasted text this long becomes an attachment.
fn is_long(text: &str) -> bool {
    text.lines().count() > 20 || text.len() > 3000
}

/// An image for the API: at most 1568 px on its longest side (Claude scales larger
/// ones down anyway), as PNG, or JPEG when the PNG passes 3.5 MB (the API stops at 5 MB).
fn for_claude(bytes: &[u8]) -> Option<(&'static str, Vec<u8>)> {
    use std::io::Cursor;
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width().max(img.height()) > 1568 { img.resize(1568, 1568, image::imageops::FilterType::Triangle) } else { img };
    let mut png = vec![];
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    if png.len() <= 3_500_000 {
        return Some(("image/png", png));
    }
    let mut jpeg = vec![];
    img.to_rgb8().write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg).ok()?;
    Some(("image/jpeg", jpeg))
}

/// A message's content: the text with the pasted texts after it, images as blocks before.
fn content(text: &str, attachments: &[Attachment]) -> Value {
    use base64::Engine;
    let images: Vec<Value> = attachments
        .iter()
        .filter_map(|a| match a {
            Attachment::Image { media, bytes, .. } => Some(json!({ "type": "image", "source": { "type": "base64", "media_type": media, "data": base64::engine::general_purpose::STANDARD.encode(bytes) } })),
            Attachment::Text(_) => None,
        })
        .collect();
    if images.is_empty() {
        return json!(text);
    }
    let mut blocks = images;
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    Value::Array(blocks)
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
    /// What you asked, the file it came with, its attachments' names, and the project's
    /// files as they were before it (a git snapshot, see `git::snapshot`).
    User(String, Option<String>, Vec<String>, Option<String>),
    Text(String),
    /// `ask`: its permission request and your answer (None: waiting).
    Tool { id: String, name: String, input: Value, result: Option<(String, bool)>, ask: Option<(String, Option<bool>)> },
    Turn { secs: f64, usage: Usage },
    /// The files a turn changed, between two snapshots: each to keep or undo.
    Changes { before: String, after: String, files: Vec<Change> },
    Notice(String, bool),
}

struct Change {
    status: char,
    path: String,
    /// Kept (true) or undone (false); None: not decided yet.
    kept: Option<bool>,
    /// Its diff, read when unfolded.
    diff: Option<Vec<(char, String)>>,
}

/// A `git diff` as the cards' lines: the hunks only, their headers as a gap.
fn diff_lines(diff: &str) -> Vec<(char, String)> {
    diff.lines()
        .skip_while(|l| !l.starts_with("@@"))
        .map(|l| match l.chars().next() {
            Some(c @ ('+' | '-' | ' ')) => (c, l[1..].to_string()),
            _ => (' ', "⋯".to_string()),
        })
        .collect()
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

/// How a session is shown and where it works.
#[derive(Clone)]
pub struct SessionOptions {
    /// The tab's header and statistics panel around the conversation (the Claude Code
    /// tab); without: the conversation and its box only (a multiplexer panel).
    pub chrome: bool,
    /// Follows the folder open in the editor (the tab); else stays in its own.
    pub follow_editor: bool,
    /// Its own model, mode, effort and budget (a multiplexer session): the settings'
    /// apply at its start only, later changes there leave it alone.
    pub own_settings: bool,
    pub effort: Option<String>,
    pub budget: Option<f64>,
    /// Added to the settings' instructions (`--append-system-prompt`).
    pub system_prompt: String,
    /// More arguments for `claude` (`--mcp-config`, `--allowedTools`…).
    pub extra_args: Vec<String>,
    /// Asked first at each permission request (see `Verdict`). With one, "Modifs auto" and
    /// "Tout autoriser" are played by Akolade: `claude` runs in its default mode, where
    /// each write comes to `can_use_tool` (in those two modes it never does).
    pub gate: Option<Gate>,
}

/// A permission request settled before it reaches you.
pub enum Verdict {
    Allow,
    /// Refused with this reason (Claude reads it).
    Deny(String),
    /// Your card, as usual.
    Ask,
}

/// (tool, input, the mode chosen — a `MODES` value) → verdict.
pub type Gate = std::rc::Rc<dyn Fn(&str, &Value, &str, &mut gpui::App) -> Verdict>;

impl SessionOptions {
    /// The Claude Code tab.
    pub fn tab() -> Self {
        Self { chrome: true, follow_editor: true, own_settings: false, effort: None, budget: None, system_prompt: String::new(), extra_args: vec![], gate: None }
    }
}

/// One conversation with `claude`: its process, the stream of events, the cards,
/// the numbers, the permission requests waiting and its folder. It draws its own
/// conversation (cards and box): the Claude Code tab shows one, the multiplexer many.
pub struct ClaudeSession {
    pub(crate) opts: SessionOptions,
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
    /// Pasted images and long texts, for the next message.
    attachments: Vec<Attachment>,
    /// Claude Code's suggestions per waiting permission request (see `always_text`).
    suggested: HashMap<String, Value>,
    /// Sent while Claude works, not taken yet: our uuid, its card.
    queued: Vec<(String, Item)>,
    /// The answer appearing letter by letter: its card, the bytes shown.
    reveal: Option<(usize, usize)>,
    /// The letter-by-letter loop runs (`tick`).
    ticking: bool,
    /// A batched redraw is coming (`throttle`).
    frame_due: bool,
    /// A multiplexer panel's model and mode choices, unfolded.
    _frame: Task<()>,
    _tick: Task<()>,
    /// The mods' band above the box, their panes (id, title, drawing), status lines, last toast.
    band: Option<Value>,
    panes: Vec<(String, String, Option<Value>)>,
    statuses: BTreeMap<String, String>,
    toast: Option<(String, String)>,
    _toast: Task<()>,
    /// Its last write was refused by its gate (a file another session holds): why.
    blocked: Option<String>,
    /// The project's files at the start of the running turn (a git snapshot).
    base: Option<String>,
    /// Messages typed while the first one waits for its snapshot: sent right after it.
    held: Option<Vec<Value>>,
    _snap: Task<()>,
    /// Numbers our requests to the mods' engine.
    ui_seq: u64,
    /// The slash commands Claude Code knows (its own, skills, plugins'): kept from one
    /// session to the next, so `/` suggests them before the first message too.
    commands: Vec<(String, String, String)>,
    _reader: Task<()>,
    _subs: Vec<Subscription>,
}

impl ClaudeSession {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, opts: SessionOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // A multiplexer panel is short: one line to start with.
        let input = cx.new(|cx| InputState::new(window, cx).multi_line(true).auto_grow(if opts.chrome { 2 } else { 1 }, 10).placeholder(if opts.chrome { "Demandez quelque chose à Claude…" } else { "Message à Claude…" }));
        let subs = vec![
            cx.observe_global::<AppSettings>(|this, cx| this.apply_settings(cx)),
            // The `/` suggestions follow what is typed. A first `/`: `claude` starts (no
            // message sent) to say which commands this folder has (`Event::Commands`).
            cx.subscribe_in(&input, window, |this, input, e: &InputEvent, window, cx| {
                if matches!(e, InputEvent::Change) {
                    if input.read(cx).value() == "/" && this.proc.is_none() && this.root.as_deref().is_some_and(crate::trust::is_trusted) {
                        this.start(window, cx);
                    }
                    cx.notify();
                }
            }),
        ];
        let mut view = Self {
            opts,
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
            attachments: vec![],
            suggested: HashMap::new(),
            queued: vec![],
            reveal: None,
            ticking: false,
            frame_due: false,
            _frame: Task::ready(()),
            _tick: Task::ready(()),
            band: None,
            panes: vec![],
            statuses: BTreeMap::new(),
            toast: None,
            _toast: Task::ready(()),
            ui_seq: 0,
            base: None,
            blocked: None,
            held: None,
            _snap: Task::ready(()),
            commands: read_json(&app_dir().join("claude-commands.json")),
            _reader: Task::ready(()),
            _subs: subs,
        };
        view.project = view.root.clone();
        view.apply_settings(cx);
        view
    }

    /// Claude is answering or running a tool.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// The defaults chosen in the settings; `claude` restarts on the same conversation to apply them.
    fn apply_settings(&mut self, cx: &mut Context<Self>) {
        let s = AppSettings::get(cx);
        let now = ClaudeSettings::of(s);
        if self.seen.as_ref() == Some(&now) || self.opts.own_settings && self.seen.is_some() {
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

    /// ⏎ sends; ⌘⏎ (and ⇧⏎, bound in main.rs) goes to the line, as the box does by itself.
    /// While an accent or a Japanese word is being composed, ⏎ is the input method's.
    fn enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if action.secondary || self.input.update(cx, |i, cx| i.marked_text_range(window, cx)).is_some() {
            return;
        }
        cx.stop_propagation();
        // A `/` half typed: ⏎ completes it, as in the terminal; a whole command is sent.
        let typed = self.input.read(cx).value().trim_start_matches('/').to_string();
        let found = self.suggestions(cx);
        if let Some(first) = found.first().filter(|_| !found.iter().any(|c| c.0 == typed)).map(|c| c.0.clone()) {
            return self.complete(&first, window, cx);
        }
        self.send_input(window, cx);
    }

    /// The commands matching a `/` being typed (the box holds `/` and a word, nothing else).
    fn suggestions(&self, cx: &gpui::App) -> Vec<&(String, String, String)> {
        let text = self.input.read(cx).value();
        let Some(word) = text.strip_prefix('/').filter(|w| !w.contains(char::is_whitespace)) else { return vec![] };
        let word = word.to_lowercase();
        // In the order of `Event::Commands`: yours, the plugins', Claude Code's.
        self.commands.iter().filter(|c| c.0.to_lowercase().contains(&word)).collect()
    }

    fn complete(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let text = format!("/{name} ");
        self.input.update(cx, |i, cx| {
            i.set_value(text, window, cx);
            i.focus(window, cx);
        });
    }

    /// ⇥ on a `/` being typed: the first command it matches.
    fn tab(&mut self, _: &IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.suggestions(cx).first().map(|c| c.0.clone()) else { return };
        cx.stop_propagation();
        self.complete(&name, window, cx);
    }

    /// ⌘V in the box: an image or a long text becomes an attachment, a short text goes in as usual.
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return };
        let image = item.entries().iter().find_map(|e| match e {
            ClipboardEntry::Image(i) => Some(i.bytes().to_vec()),
            _ => None,
        });
        if let Some(bytes) = image {
            cx.stop_propagation();
            // Decoding and scaling a Retina screenshot takes a moment: off the UI thread.
            let task = cx.background_executor().spawn(async move { for_claude(&bytes) });
            cx.spawn(async move |this, cx| {
                let image = task.await;
                let _ = this.update(cx, |this, cx| {
                    match image {
                        Some((media, bytes)) => {
                            let format = if media == "image/png" { gpui::ImageFormat::Png } else { gpui::ImageFormat::Jpeg };
                            let preview = Arc::new(gpui::Image::from_bytes(format, bytes.clone()));
                            this.attachments.push(Attachment::Image { media, bytes, preview });
                        }
                        None => this.items.push(Item::Notice("Cette image ne peut pas être envoyée à Claude (format non reconnu).".into(), true)),
                    }
                    cx.notify();
                });
            })
            .detach();
        } else if let Some(text) = item.text().filter(|t| is_long(t)) {
            cx.stop_propagation();
            self.attachments.push(Attachment::Text(text));
            cx.notify();
        }
    }

    fn send_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        // Typed while Claude asks: the answer, in your own words.
        if let Some(ix) = self.pending_question().filter(|_| !text.is_empty()) {
            self.input.update(cx, |i, cx| i.set_value("", window, cx));
            return self.reply(ix, Some(text), cx);
        }
        // While Claude works it waits in line ("Mettre en file", see `send`).
        if !text.is_empty() || !self.attachments.is_empty() {
            self.input.update(cx, |i, cx| i.set_value("", window, cx));
            self.send(text, window, cx);
        }
    }

    /// While Claude works the message waits in line, as in the terminal: Claude takes
    /// it at its next step (with a command's result) or once it has answered.
    fn send(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(_), Some(root)) = (self.program.as_ref(), self.root.clone()) else { return };
        self.blocked = None;
        // In -p mode Claude Code asks nothing: it runs the project's hooks (.claude/settings.json) at once.
        if self.proc.is_none() && !crate::trust::is_trusted(&root) {
            return self.ask_trust(text, root, window, cx);
        }
        if self.proc.is_none() && !self.start(window, cx) {
            return cx.notify();
        }
        let file = self.with_file.then(|| self.last_file(cx)).flatten().map(|p| self.rel(&p));
        let attachments = std::mem::take(&mut self.attachments);
        // The pasted texts after yours; the open file last (`split_file` finds it there).
        let pasted = attachments.iter().filter_map(|a| match a {
            Attachment::Text(t) => Some(t.as_str()),
            Attachment::Image { .. } => None,
        });
        let mut prompt = std::iter::once(text.as_str()).chain(pasted).filter(|t| !t.is_empty()).collect::<Vec<_>>().join("\n\n");
        if let Some(f) = &file {
            prompt = format!("{prompt}\n\n(Fichier ouvert dans l'éditeur : {f})");
        }
        let uuid = crate::tools::devtools::uuid_v4();
        let message = json!({ "type": "user", "uuid": uuid, "message": { "role": "user", "content": content(&prompt, &attachments) } });
        let short: String = text.chars().take(80).collect();
        let card = Item::User(text, file, attachments.iter().map(Attachment::label).collect(), None);
        self.to_bottom = true;
        if self.busy {
            // The first message still waits for its snapshot: this one goes after it.
            if let Some(held) = &mut self.held {
                held.push(message);
                self.queued.push((uuid, card));
            } else if self.proc.as_mut().is_some_and(|p| p.send(message)) {
                self.queued.push((uuid, card));
            } else {
                self.items.push(card);
                self.lost();
            }
            return cx.notify();
        }
        self.items.push(card);
        (self.busy, self.thinking, self.held) = (true, true, Some(vec![]));
        // The files as they are before Claude reads it (to undo its turn): then it goes.
        let (ix, root) = (self.items.len() - 1, self.root.clone());
        let snap = cx.background_executor().spawn(async move { root.and_then(|r| crate::git::snapshot(&r).ok()) });
        self._snap = cx.spawn(async move |this, cx| {
            let tree = snap.await;
            let _ = this.update(cx, |this, cx| {
                if !this.busy {
                    // Stopped before it left: said, as for the messages in line (`end_turn`).
                    this.items.push(Item::Notice(format!("Non envoyé (arrêté avant l'envoi) : « {short} »"), true));
                    return cx.notify();
                }
                if let Some(Item::User(.., before)) = this.items.get_mut(ix) {
                    before.clone_from(&tree);
                }
                this.base = tree;
                // Then the messages typed meanwhile, in order.
                let held = this.held.take().unwrap_or_default();
                if !std::iter::once(message).chain(held).all(|m| this.proc.as_mut().is_some_and(|p| p.send(m))) {
                    this.lost();
                }
                cx.notify();
            });
        });
        cx.notify();
    }

    /// `claude` stopped answering: the turn ends (messages in line said as not sent).
    fn lost(&mut self) {
        self.proc = None;
        self.end_turn();
        self.items.push(Item::Notice("Claude Code ne répond plus : renvoyez votre message pour le relancer.".into(), true));
    }

    /// At the end of a turn: the files it changed, listed to keep or undo.
    fn review(&mut self, cx: &mut Context<Self>) {
        let (Some(before), Some(root)) = (self.base.clone(), self.root.clone()) else { return };
        let session = self.session.clone();
        let task = cx.background_executor().spawn(async move {
            let after = crate::git::snapshot(&root).ok()?;
            let files = crate::git::changed(&root, &before, &after).ok()?;
            Some((before, after, files))
        });
        cx.spawn(async move |this, cx| {
            let Some((before, after, files)) = task.await else { return };
            let _ = this.update(cx, |this, cx| {
                // Messages in line go on from here (a new message takes its own snapshot).
                if this.base.as_ref() == Some(&before) {
                    this.base = Some(after.clone());
                }
                if !files.is_empty() && this.session == session {
                    // "Toujours garder" (settings): decided at once, nothing left to click.
                    let kept = AppSettings::get(cx).claude_auto_keep.then_some(true);
                    let files = files.into_iter().map(|(status, path)| Change { status, path, kept, diff: None }).collect();
                    this.items.push(Item::Changes { before, after, files });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Undoes one file of a turn's changes, or all those not kept (`path` None).
    fn undo(&mut self, ix: usize, path: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(Item::Changes { before, files, .. }), Some(root)) = (self.items.get(ix), self.root.clone()) else { return };
        let picked: Vec<(char, String)> = files.iter().filter(|f| f.kept.is_none() && path.as_ref().is_none_or(|p| *p == f.path)).map(|f| (f.status, f.path.clone())).collect();
        let (before, session) = (before.clone(), self.session.clone());
        let task = cx.background_executor().spawn(async move { crate::git::restore(&root, &before, &picked).map(|_| picked) });
        cx.spawn_in(window, async move |this, cx| {
            let done = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match done {
                    Ok(picked) => {
                        // Card `ix` of this conversation (another one may be shown by now).
                        if let Some(Item::Changes { files, .. }) = this.items.get_mut(ix).filter(|_| this.session == session) {
                            for f in files.iter_mut().filter(|f| picked.iter().any(|p| p.1 == f.path)) {
                                f.kept = Some(false);
                            }
                        }
                        this.reload(window, cx);
                    }
                    Err(e) => this.items.push(Item::Notice(format!("Impossible d'annuler : {e}"), true)),
                }
                (this.relayout, this.to_bottom) = (true, false);
                cx.notify();
            });
        })
        .detach();
    }

    /// Back to the files as they were before message `ix` (Claude's changes since, and yours).
    fn rewind(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(Item::User(_, _, _, Some(before))), Some(root)) = (self.items.get(ix), self.root.clone()) else { return };
        let (before, session) = (before.clone(), self.session.clone());
        let answer = window.prompt(
            PromptLevel::Warning,
            "Revenir avant ce message ?",
            Some("Les fichiers du projet reprennent leur contenu d'avant ce message : les modifications faites depuis (par Claude et par vous) sont perdues. La conversation, elle, continue."),
            &["Revenir", "Annuler"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let done = cx
                .background_executor()
                .spawn(async move {
                    let now = crate::git::snapshot(&root)?;
                    let files = crate::git::changed(&root, &before, &now)?;
                    crate::git::restore(&root, &before, &files).map(|_| files.len())
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let text = match done {
                    Ok(n) => {
                        // The changes listed after it are undone too (if this conversation is still shown).
                        let from = if this.session == session { ix } else { this.items.len() };
                        for item in &mut this.items[from..] {
                            if let Item::Changes { files, .. } = item {
                                files.iter_mut().filter(|f| f.kept.is_none()).for_each(|f| f.kept = Some(false));
                            }
                        }
                        this.reload(window, cx);
                        Item::Notice(format!("Fichiers remis comme avant ce message ({n} {}).", if n > 1 { "fichiers" } else { "fichier" }), false)
                    }
                    Err(e) => Item::Notice(format!("Impossible de revenir avant ce message : {e}"), true),
                };
                this.items.push(text);
                (this.relayout, this.to_bottom) = (true, true);
                cx.notify();
            });
        })
        .detach();
    }

    fn reload(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| ws.reload_from_disk(window, cx));
        }
    }

    /// Marks one file of a turn's changes as kept, or all those not decided (`path` None).
    fn keep(&mut self, ix: usize, path: Option<String>, cx: &mut Context<Self>) {
        if let Some(Item::Changes { files, .. }) = self.items.get_mut(ix) {
            for f in files.iter_mut().filter(|f| f.kept.is_none() && path.as_ref().is_none_or(|p| *p == f.path)) {
                f.kept = Some(true);
            }
            self.relayout = true;
            cx.notify();
        }
    }

    /// Shows (or hides) one changed file's diff.
    fn toggle_diff(&mut self, ix: usize, path: &str, cx: &mut Context<Self>) {
        let root = self.root.clone().unwrap_or_default();
        if let Some(Item::Changes { before, after, files }) = self.items.get_mut(ix)
            && let Some(f) = files.iter_mut().find(|f| f.path == path)
        {
            f.diff = match f.diff {
                Some(_) => None,
                None => Some(diff_lines(&crate::git::tree_diff(&root, before, after, path))),
            };
            self.relayout = true;
            cx.notify();
        }
    }

    /// Starts `claude` (in a trusted folder): false if it could not, said in a card.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (Some(program), Some(root)) = (self.program.clone(), self.root.clone()) else { return false };
        let settings = AppSettings::get(cx).clone();
        // With a gate, Akolade plays the modes that would skip it (see `SessionOptions::gate`).
        let mode = match MODES[self.mode].1 {
            "acceptEdits" | "bypassPermissions" if self.opts.gate.is_some() => "default",
            m => m,
        };
        match Proc::spawn(&program, &root, MODELS[self.model].1, mode, self.session.as_deref(), &settings, &self.opts) {
            Ok((mut proc, mut rx)) => {
                // The mods draw here as on Claude Code's desktop app (see `surface`).
                proc.send(surface::attach());
                // Its slash commands, with their descriptions (for `/`).
                proc.send(json!({ "type": "control_request", "request_id": "akolade-init", "request": { "subtype": "initialize" } }));
                self.proc = Some(proc);
                self.redraw_mods();
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
                return false;
            }
        }
        true
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
        // A multiplexer panel draws streamed text in batches (see `throttle`), without the letter-by-letter reveal.
        let streamed = !self.opts.chrome && matches!(e, Event::Text(_) | Event::TextStart);
        match e {
            Event::Init { session, model, subscription } => {
                if self.session.is_none() {
                    self.history.record(&day(0), &Usage::default(), 0, 1);
                }
                (self.session, self.model_name, self.subscription) = (Some(session), model, subscription);
            }
            Event::TextStart => {
                (self.busy, self.thinking) = (true, false);
                self.items.push(Item::Text(String::new()));
                if self.opts.chrome {
                    self.reveal = Some((self.items.len() - 1, 0));
                }
            }
            Event::Text(t) => {
                self.thinking = false;
                match self.items.last_mut() {
                    Some(Item::Text(s)) => s.push_str(&t),
                    _ => {
                        self.items.push(Item::Text(t));
                        if self.opts.chrome {
                            self.reveal = Some((self.items.len() - 1, 0));
                        }
                    }
                }
                if self.opts.chrome {
                    self.tick(cx);
                }
            }
            // Claude at work, also on a turn a mod started (its band's "Review").
            Event::Thinking => (self.busy, self.thinking) = (true, true),
            Event::Context(n) => (self.stats.context, self.busy) = (n, true),
            Event::Taken(uuid) => {
                if let Some(i) = self.queued.iter().position(|(u, _)| *u == uuid) {
                    let (_, card) = self.queued.remove(i);
                    self.items.push(card);
                    (self.busy, self.thinking, self.to_bottom) = (true, true, true);
                }
            }
            Event::Drawn { request, tree } => {
                if request.starts_with("ui-band-") {
                    self.band = Some(tree);
                } else if let Some(pane) = request.strip_prefix("ui-pane-").and_then(|r| r.rsplit_once('-')).map(|(id, _)| id.to_string()) {
                    if let Some(p) = self.panes.iter_mut().find(|p| p.0 == pane) {
                        p.2 = Some(tree);
                    }
                }
            }
            Event::Redraw => self.redraw_mods(),
            Event::Commands(c) => {
                // Kept on disk: `/` suggests them before `claude` starts, after a restart too.
                if let Ok(text) = serde_json::to_string(&c) {
                    let _ = fs::create_dir_all(app_dir());
                    let _ = crate::workspace::write_file(&app_dir().join("claude-commands.json"), &text);
                }
                self.commands = c;
            }
            Event::Panes(panes) => {
                let old = std::mem::take(&mut self.panes);
                self.panes = panes.into_iter().map(|(id, title)| {
                    let tree = old.iter().find(|p| p.0 == id).and_then(|p| p.2.clone());
                    (id, title, tree)
                }).collect();
                self.redraw_mods();
            }
            Event::Toast(plugin, text, ms) => {
                self.toast = Some((plugin, text));
                // At most 8 s: a toast is a passing word.
                let wait = Duration::from_millis(ms.clamp(1500, 8000));
                self._toast = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(wait).await;
                    let _ = this.update(cx, |this, cx| {
                        this.toast = None;
                        cx.notify();
                    });
                });
            }
            Event::Status(plugin, text) => match text {
                Some(t) => _ = self.statuses.insert(plugin, t),
                None => _ = self.statuses.remove(&plugin),
            },
            Event::ToolUse { id, name, input } => {
                (self.busy, self.thinking) = (true, true);
                *self.stats.tools.entry(label(&name)).or_default() += 1;
                if let Some(f) = edited_file(&name, &input).filter(|f| !self.stats.files.contains(f)) {
                    self.stats.files.push(f);
                }
                if !self.items.iter().any(|i| matches!(i, Item::Tool { id: i, .. } if *i == id)) {
                    self.items.push(Item::Tool { id, name, input, result: None, ask: None });
                }
            }
            Event::ToolResult { id, text, error } => {
                if !error {
                    self.blocked = None;
                }
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
            Event::Ask { request, tool_id, tool, input, rules } => {
                let gate = self.opts.gate.clone().filter(|_| !matches!(tool.as_str(), "AskUserQuestion" | "ExitPlanMode"));
                let verdict = gate.map_or(Verdict::Ask, |g| g(&tool, &input, MODES[self.mode].1, cx));
                let settled = match verdict {
                    Verdict::Ask => None,
                    Verdict::Allow => Some((permission_response(&request, true, &input, None), None)),
                    Verdict::Deny(why) => Some((deny_response(&request, &why), Some(why))),
                };
                if let Some((msg, refused)) = settled {
                    if let Some(p) = &mut self.proc {
                        p.send(msg);
                    }
                    // Its card says it was refused (the reason comes back as its result).
                    if let Some(why) = refused {
                        if let Some(Item::Tool { ask, .. }) = self.items.iter_mut().find(|i| matches!(i, Item::Tool { id, .. } if *id == tool_id && !tool_id.is_empty())) {
                            *ask = Some((request, Some(false)));
                        }
                        self.blocked = Some(why);
                    }
                    return cx.notify();
                }
                if always_text(&rules).is_some() {
                    self.suggested.insert(request.clone(), rules);
                }
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
                // Messages still in line: Claude answers them next.
                let more = !self.queued.is_empty();
                (self.busy, self.thinking) = (more, more);
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
                self.review(cx);
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
        if streamed { self.throttle(cx) } else { cx.notify() }
    }

    /// One redraw for what streamed in the last tenth of a second: N panels streaming at
    /// once redraw at most ten times a second each, not at every word.
    fn throttle(&mut self, cx: &mut Context<Self>) {
        if self.frame_due {
            return;
        }
        self.frame_due = true;
        self._frame = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(100)).await;
            let _ = this.update(cx, |this, cx| {
                this.frame_due = false;
                cx.notify();
            });
        });
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
        (self.busy, self.thinking, self.held) = (false, false, None);
        // Claude stopped before taking them: said, not silently dropped.
        for (_, card) in std::mem::take(&mut self.queued) {
            if let Item::User(text, ..) = card {
                self.items.push(Item::Notice(format!("Non envoyé (Claude s'est arrêté avant) : « {} »", text.chars().take(80).collect::<String>()), true));
            }
        }
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

    /// `always`: allowed, and Claude Code keeps its suggested rule so as not to ask again.
    fn answer(&mut self, ix: usize, allow: bool, always: bool, cx: &mut Context<Self>) {
        // (The message itself: `permission_response`.)
        let Some(Item::Tool { name, input, ask: Some((request, answer @ None)), .. }) = self.items.get_mut(ix) else { return };
        // A plan approved is a plan to apply: as in Claude Code, out of plan mode (from the next message).
        if allow && name == "ExitPlanMode" && MODES[self.mode].1 == "plan" {
            (self.mode, self.restart) = (0, true);
        }
        let rules = self.suggested.remove(request.as_str()).filter(|_| allow && always);
        // "Accept edits" kept: Claude Code is in that mode now, the selector says so.
        let mode = rules.as_ref().and_then(|r| r.as_array()?.iter().find(|s| s["type"] == "setMode")?["mode"].as_str().map(str::to_string));
        if let Some(m) = mode.and_then(|m| MODES.iter().position(|(_, v, _)| *v == m)) {
            self.mode = m;
        }
        let msg = permission_response(request, allow, input, rules.as_ref());
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
        self.answer(ix, true, false, cx);
    }

    /// The answer appears letter by letter, at least ~180 a second and faster when
    /// Claude is ahead, so the text flows instead of landing in blocks.
    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        self._tick = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                let more = this
                    .update(cx, |this, cx| {
                        let text = this.reveal.and_then(|(ix, shown)| match this.items.get(ix) {
                            Some(Item::Text(t)) => Some((ix, shown, t)),
                            _ => None,
                        });
                        let Some((ix, shown, t)) = text else {
                            (this.reveal, this.ticking) = (None, false);
                            return false;
                        };
                        let backlog = t[shown.min(t.len())..].chars().count();
                        // Caught up (more may come: the next text starts the loop again),
                        // or a card came after it (a tool): the rest shows at once.
                        if backlog == 0 || ix + 2 < this.items.len() {
                            if backlog > 0 {
                                this.reveal = None;
                            }
                            this.ticking = false;
                            cx.notify();
                            return false;
                        }
                        this.reveal = Some((ix, reveal_to(t, shown, (backlog / 6).max(3))));
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
        });
    }

    /// Asks the mods' engine for the band and every open pane, drawn afresh.
    fn redraw_mods(&mut self) {
        let Some(p) = &mut self.proc else { return };
        self.ui_seq += 1;
        p.send(surface::render(&format!("ui-band-{}", self.ui_seq), "AbovePrompt", "above-prompt", surface::BAND_COLUMNS));
        for (id, ..) in &self.panes {
            p.send(surface::render(&format!("ui-pane-{id}-{}", self.ui_seq), "Pane", id, surface::PANE_COLUMNS));
        }
    }

    /// A mod's button pressed: its handler runs in Claude Code, which then asks us to redraw.
    fn ui_press(&mut self, press: &Value, key: Option<&str>, cx: &mut Context<Self>) {
        self.ui_seq += 1;
        let id = format!("ui-press-{}", self.ui_seq);
        if let Some(p) = &mut self.proc {
            p.send(surface::press(&id, press, key));
        }
        cx.notify();
    }

    fn close_pane(&mut self, pane: &str, cx: &mut Context<Self>) {
        self.ui_seq += 1;
        let id = format!("ui-close-{}", self.ui_seq);
        if let Some(p) = &mut self.proc {
            p.send(surface::close(&id, pane));
        }
        self.panes.retain(|p| p.0 != pane);
        cx.notify();
    }

    // shortcut: a stopped turn's tokens are not counted (the CLI reports them at its end); upgrade by summing each message's `usage`.
    fn stop(&mut self, cx: &mut Context<Self>) {
        self.proc = None;
        self._reader = Task::ready(());
        if self.busy {
            self.items.push(Item::Notice("Arrêté. La conversation reprend au prochain message.".into(), false));
            self.review(cx);
        }
        self.end_turn();
        cx.notify();
    }

    /// The next message starts a new conversation.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.proc = None;
        self._reader = Task::ready(());
        (self.items, self.session, self.stats, self.model_name, self.restart, self.base) = (vec![], None, Stats::default(), String::new(), false, None);
        self._snap = Task::ready(()); // a message still waiting for its snapshot goes with this conversation
        (self.queued, self.reveal, self.band, self.panes, self.toast) = (vec![], None, None, vec![], None);
        (self.ticking, self._tick) = (false, Task::ready(()));
        self.statuses.clear();
        self.toggled.clear();
        self.picked.clear();
        self.suggested.clear();
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
    pub(crate) fn resume(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(path) = self.root.as_deref().and_then(sessions_dir).map(|d| d.join(format!("{id}.jsonl"))) else { return };
        self.reset(cx);
        (self.session, self.show_past) = (Some(id.clone()), false);
        // Line by line: the file can weigh hundreds of MB.
        let task = cx.background_executor().spawn(async move { fs::File::open(&path).map(|f| transcript(BufReader::new(f).lines().map_while(Result::ok))).unwrap_or_default() });
        self._load = cx.spawn(async move |this, cx| {
            let (items, stats) = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.session.as_deref() == Some(id.as_str()) && this.items.is_empty() {
                    (this.items, this.stats) = (items, stats);
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

    /// Its own thinking effort, from the next message (`claude` restarts on the same conversation).
    fn choose_effort(&mut self, i: usize, cx: &mut Context<Self>) {
        let value = EFFORTS[i.min(EFFORTS.len() - 1)].0.to_string();
        if !self.busy && self.opts.effort.as_ref() != Some(&value) {
            (self.opts.effort, self.proc) = (Some(value), None);
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

    /// A file of a card in the editor. A session in its own folder (the multiplexer's)
    /// may name a file of another project than the editor's: open that project, or the file alone.
    fn open(&self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.workspace.upgrade() else { return };
        let editor_root = ws.read(cx).root().map(Path::to_path_buf);
        let elsewhere = !self.opts.follow_editor && editor_root.as_ref().is_none_or(|r| !path.starts_with(r));
        let Some(project) = self.root.clone().filter(|_| elsewhere) else {
            return ws.update(cx, |ws, cx| ws.open_path(path, window, cx));
        };
        let detail = format!("{} n'est pas dans le projet ouvert dans l'éditeur. Ouvrir « {} » (ses onglets et son explorateur remplacent ceux du projet actuel) ou seulement ce fichier ?", path.display(), project.display());
        let answer = window.prompt(PromptLevel::Info, "Fichier d'un autre projet", Some(&detail), &["Ouvrir le projet", "Seulement le fichier", "Annuler"], cx);
        let ws = ws.downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let choice = answer.await.unwrap_or(2);
            let _ = ws.update_in(cx, |ws, window, cx| match choice {
                0 => {
                    ws.set_root(Some(project), window, cx);
                    ws.open_path(path, window, cx);
                }
                1 => ws.open_path(path, window, cx),
                _ => {}
            });
        })
        .detach();
    }

    // ------------------------------------------------------------ render

    /// `danger`: a choice shown in red once picked.
    fn segmented(&self, id: &'static str, labels: Vec<(&'static str, Option<&'static str>)>, on: usize, danger: Option<usize>, cx: &mut Context<Self>, pick: fn(&mut Self, usize, &mut Window, &mut Context<Self>)) -> Div {
        let theme = cx.theme();
        let (fg, muted, primary_fg, red) = (theme.foreground, theme.muted_foreground, theme.primary_foreground, theme.red);
        let primary = if danger == Some(on) { red } else { theme.primary };
        let busy = self.busy;
        // A multiplexer panel is narrow: tighter choices.
        let (pad, size) = if self.opts.chrome { (9., 12.) } else { (5., 11.5) };
        h_flex().flex_shrink_0().p(r(2.)).gap(r(2.)).rounded(r(8.)).bg(fg.opacity(0.05)).children(labels.into_iter().enumerate().map(|(i, (text, tip))| {
            div()
                .id(SharedString::from(format!("{id}-{i}")))
                .px(r(pad))
                .py(r(4.))
                .rounded(r(6.))
                .text_size(r(size))
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
            // In a multiplexer panel, still: an animation redraws the whole window at each frame.
            None if self.busy && self.opts.chrome => Spinner::new().xsmall().color(muted).into_any_element(),
            None if self.busy => Icon::new(IconName::Ellipsis).xsmall().text_color(muted).into_any_element(),
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
                div().border_t_1().border_color(border).px(r(14.)).py(r(8.)).text_size(r(14.)).child(markdown(("claude-plan", ix), text_of(&input["plan"]), window, cx)).into_any_element()
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
        let always = ask.as_ref().filter(|_| pending && !question && !plan).and_then(|(request, _)| always_text(self.suggested.get(request)?));
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
                .child(button(SharedString::from(format!("deny-{ix}"))).ghost().small().label(no).on_click(cx.listener(move |this, _, _, cx| this.answer(ix, false, false, cx))))
                .when_some(always, |d, why| {
                    d.child(button(SharedString::from(format!("always-{ix}"))).outline().small().label("Toujours autoriser").tooltip(why).on_click(cx.listener(move |this, _, _, cx| this.answer(ix, true, true, cx))))
                })
                .child(button(SharedString::from(format!("allow-{ix}"))).primary().small().icon(IconName::Check).label(yes).disabled(!ready).on_click(cx.listener(move |this, _, _, cx| {
                    if question { this.reply(ix, None, cx) } else { this.answer(ix, true, false, cx) }
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

    /// The files a turn changed: each one's diff a click away, to keep or undo.
    fn render_changes(&self, ix: usize, files: &[Change], cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (fg, muted, border, green, red, yellow, mono) = (theme.foreground, theme.muted_foreground, theme.border, theme.green, theme.red, theme.yellow, theme.mono_font_family.clone());
        let hover = ui::hover_bg(cx);
        let open = files.iter().filter(|f| f.kept.is_none()).count();
        let n = files.len();
        let head = h_flex()
            .gap(r(10.))
            .px(r(12.))
            .py(r(8.))
            .child(tile(rgb(ORANGE).into(), 24., Icon::empty().path("icons/pencil.svg").size(r(13.)).text_color(rgb(ORANGE))))
            .child(div().flex_1().text_size(r(13.)).font_weight(FontWeight::SEMIBOLD).child(format!("{n} {} modifié{} pendant ce tour", if n > 1 { "fichiers" } else { "fichier" }, if n > 1 { "s" } else { "" })))
            .when(open > 1, |d| {
                d.child(button(SharedString::from(format!("keep-all-{ix}"))).ghost().xsmall().label("Tout garder").on_click(cx.listener(move |this, _, _, cx| this.keep(ix, None, cx))))
                    .child(button(SharedString::from(format!("undo-all-{ix}"))).ghost().xsmall().label("Tout annuler").on_click(cx.listener(move |this, _, window, cx| this.undo(ix, None, window, cx))))
            });
        let rows = files.iter().enumerate().map(|(i, f)| {
            let (path, path2, path3, path4) = (f.path.clone(), f.path.clone(), f.path.clone(), f.path.clone());
            let (mark, color) = match f.status {
                'A' => ("Créé", green),
                'D' => ("Supprimé", red),
                _ => ("Modifié", yellow),
            };
            let row = h_flex()
                .id(SharedString::from(format!("change-{ix}-{i}")))
                .gap(r(8.))
                .px(r(12.))
                .py(r(5.))
                .border_t_1()
                .border_color(border)
                .cursor_pointer()
                .hover(|s| s.bg(hover))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_diff(ix, &path, cx)))
                .child(div().w(r(64.)).flex_shrink_0().text_size(r(11.5)).text_color(color).child(mark))
                .child(div().flex_1().min_w_0().truncate().font_family(mono.clone()).text_size(r(12.)).text_color(if f.kept == Some(false) { muted } else { fg }).when(f.kept == Some(false), |d| d.line_through()).child(f.path.clone()))
                .when(f.status != 'D' && f.kept != Some(false), |d| {
                    d.child(button(SharedString::from(format!("change-open-{ix}-{i}"))).ghost().xsmall().icon(IconName::ExternalLink).tooltip("Ouvrir dans l'éditeur").on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if let Some(root) = this.root.clone() {
                            this.open(root.join(&path2), window, cx);
                        }
                    })))
                })
                .map(|d| match f.kept {
                    Some(true) => d.child(div().text_size(r(11.5)).text_color(green).child("Gardé")),
                    Some(false) => d.child(div().text_size(r(11.5)).text_color(muted).child("Annulé")),
                    None => d
                        .child(button(SharedString::from(format!("change-keep-{ix}-{i}"))).ghost().xsmall().icon(IconName::Check).tooltip("Garder").on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.keep(ix, Some(path3.clone()), cx);
                        })))
                        .child(button(SharedString::from(format!("change-undo-{ix}-{i}"))).ghost().xsmall().icon(IconName::Undo2).tooltip("Annuler : le fichier redevient comme avant ce tour").on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.undo(ix, Some(path4.clone()), window, cx);
                        }))),
                });
            v_flex().child(row).children(f.diff.clone().map(|lines| self.render_lines(lines, cx)))
        }).collect::<Vec<_>>();
        v_flex().flex_shrink_0().rounded(r(10.)).border_1().border_color(border).bg(fg.opacity(0.015)).overflow_hidden().child(head).children(rows)
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
            Item::User(text, file, attached, before) => v_flex()
                .flex_shrink_0()
                .items_end()
                .gap(r(4.))
                .group(SharedString::from(format!("user-{ix}")))
                .when(!text.is_empty(), |d| d.child(div().max_w(relative(0.85)).px(r(14.)).py(r(9.)).rounded(r(14.)).rounded_br(r(4.)).bg(primary.opacity(0.14)).text_size(r(14.)).line_height(relative(1.5)).child(text.clone())))
                .when(!attached.is_empty(), |d| d.child(h_flex().gap(r(4.)).text_size(r(11.5)).text_color(muted).child(Icon::empty().path("icons/paperclip.svg").xsmall()).child(attached.join(", "))))
                .when_some(file.clone(), |d, f| d.child(h_flex().gap(r(4.)).text_size(r(11.5)).text_color(muted).child(Icon::new(IconName::File).xsmall()).child(f)))
                // On hover: the project's files back as they were before this message.
                .when(before.is_some() && !self.busy, |d| {
                    d.child(
                        div().invisible().group_hover(SharedString::from(format!("user-{ix}")), |s| s.visible()).child(
                            button(SharedString::from(format!("rewind-{ix}")))
                                .ghost()
                                .xsmall()
                                .icon(Icon::empty().path("icons/history.svg"))
                                .label("Revenir avant ce message")
                                .tooltip("Remet les fichiers du projet comme ils étaient avant ce message")
                                .on_click(cx.listener(move |this, _, window, cx| this.rewind(ix, window, cx))),
                        ),
                    )
                })
                .into_any_element(),
            Item::Changes { files, .. } => self.render_changes(ix, files, cx).into_any_element(),
            Item::Text(text) => {
                let shown = match self.reveal {
                    Some((i, n)) if i == ix => text[..n.min(text.len())].to_string(),
                    _ => text.clone(),
                };
                div().flex_shrink_0().text_size(r(14.5)).line_height(relative(1.6)).child(markdown(("claude-md", ix), shown, window, cx)).into_any_element()
            }
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

    fn render_composer(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
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
        // While Claude works: stop it, or send a message it takes at its next step.
        let stop = self.busy.then(|| button("claude-stop").small().ghost().icon(Icon::new(IconName::Close)).label("Arrêter").on_click(cx.listener(|this, _, _, cx| this.stop(cx))));
        let send_label = if self.busy { "Mettre en file" } else { "Envoyer" };
        // A multiplexer panel: the arrow alone, as in Claude's chat (the words in its tooltip).
        let send = button("claude-send").primary().small().icon(IconName::ArrowUp).map(|b| if self.opts.chrome { b.label(send_label) } else { b.tooltip(send_label) }).on_click(cx.listener(|this, _, window, cx| this.send_input(window, cx)));
        let queued: Vec<Div> = self
            .queued
            .iter()
            .filter_map(|(_, card)| match card {
                Item::User(text, _, attached, _) => Some(if text.is_empty() { attached.join(", ") } else { text.clone() }),
                _ => None,
            })
            .map(|text| {
                h_flex()
                    .gap(r(6.))
                    .text_size(r(12.5))
                    .text_color(muted)
                    .child(Icon::empty().path("icons/history.svg").xsmall())
                    .child(div().flex_shrink_0().child("En attente :"))
                    .child(div().flex_1().min_w_0().truncate().text_color(fg).child(text))
            })
            .collect();
        let found: Vec<(String, String, String)> = self.suggestions(cx).into_iter().cloned().collect();
        let slash = (!found.is_empty()).then(|| {
            let hover = ui::hover_bg(cx);
            // A few rows high, scrolled: the box keeps its buttons on screen.
            v_flex().id("slash-list").max_h(r(220.)).overflow_y_scroll().pb(r(6.)).border_b_1().border_color(border).children(found.into_iter().enumerate().map(|(i, (name, hint, about))| {
                let pick = name.clone();
                h_flex()
                    .id(SharedString::from(format!("slash-{i}")))
                    .flex_shrink_0() // full height: the list scrolls instead of squeezing them
                    .gap(r(8.))
                    .px(r(6.))
                    .py(r(3.))
                    .rounded(r(6.))
                    .cursor_pointer()
                    .hover(|d| d.bg(hover))
                    .text_size(r(12.5))
                    .child(div().flex_shrink_0().font_weight(FontWeight::SEMIBOLD).child(format!("/{name}")))
                    .when(!hint.is_empty(), |d| d.child(div().flex_shrink_0().text_color(muted).child(hint)))
                    .child(div().flex_1().min_w_0().truncate().text_color(muted).child(about))
                    .when(i == 0, |d| d.child(div().flex_shrink_0().text_size(r(11.)).text_color(muted).child("⇥")))
                    .on_click(cx.listener(move |this, _, window, cx| this.complete(&pick, window, cx)))
            }))
        });
        let band = self.band.as_ref().map(|tree| div().text_size(r(12.5)).pb(r(6.)).border_b_1().border_color(border).child(self.draw(tree, "band", window, cx)));
        let theme = cx.theme();
        let notes = (self.toast.is_some() || !self.statuses.is_empty()).then(|| {
            v_flex()
                .gap(r(2.))
                .px(r(6.))
                .text_size(r(11.5))
                .text_color(theme.muted_foreground)
                .children(self.toast.as_ref().map(|(plugin, text)| h_flex().gap(r(6.)).child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.primary).child(plugin.clone())).child(div().min_w_0().truncate().child(text.clone()))))
                .children(self.statuses.iter().map(|(plugin, text)| h_flex().gap(r(6.)).child(div().font_weight(FontWeight::SEMIBOLD).child(plugin.clone())).child(div().min_w_0().truncate().child(text.clone()))))
        });
        h_flex().flex_shrink_0().justify_center().px(r(if self.opts.chrome { 24. } else { 8. })).pb(r(if self.opts.chrome { 16. } else { 8. })).pt(r(6.)).child(
            v_flex()
                .w_full()
                .max_w(r(800.))
                .p(r(10.))
                .gap(r(8.))
                .rounded(r(14.))
                .border_1()
                .border_color(border)
                .bg(bg)
                .shadow_sm()
                .key_context("ClaudeComposer")
                .capture_action(cx.listener(Self::paste))
                .capture_action(cx.listener(Self::enter))
                .capture_action(cx.listener(Self::tab))
                // The mods' band in the Claude Code tab only: a multiplexer panel is too short for it.
                .when(self.opts.chrome, |d| d.children(band))
                .when(self.opts.chrome, |d| d.child(self.render_folder("composer-folder", cx)))
                .children(queued)
                .when(!self.attachments.is_empty(), |d| d.child(self.render_attachments(cx)))
                .children(slash)
                .child(Input::new(&self.input).appearance(false).text_size(r(14.)))
                .map(|d| {
                    if !self.opts.chrome {
                        // A multiplexer panel: model, effort and permissions in one menu, opening upwards.
                        let effort = EFFORTS.iter().position(|e| e.0 == self.opts.effort.as_deref().unwrap_or(&AppSettings::get(cx).claude_effort)).unwrap_or(0);
                        let (model, mode) = (self.model, self.mode);
                        let me = cx.entity().downgrade();
                        let pick: OnPick = std::rc::Rc::new(move |p, window, cx| {
                            let _ = me.update(cx, |this, cx| match p {
                                Pick::Model(i) => this.choose(Some(i), None, cx),
                                Pick::Mode(i) => this.choose_mode(i, window, cx),
                                Pick::Effort(i) => this.choose_effort(i, cx),
                            });
                        });
                        // As in Claude's chat: the model, its effort muted; then the permissions, each its menu.
                        let muted = cx.theme().muted_foreground;
                        let danger = if mode == BYPASS { cx.theme().red } else { muted };
                        let model_label = h_flex().gap(r(6.)).child(MODELS[model].0).when(effort > 0, |d| d.child(div().text_color(muted).child(EFFORTS[effort].1)));
                        let pick_mode = pick.clone();
                        return d.child(
                            h_flex()
                                .gap(r(2.))
                                .child(button("claude-model").ghost().xsmall().disabled(self.busy).child(model_label).dropdown_menu_with_anchor(gpui::Corner::BottomLeft, move |menu, window, cx| model_menu(menu, model, effort, pick.clone(), window, cx)))
                                .child(button("claude-mode").ghost().xsmall().disabled(self.busy).child(div().text_color(danger).child(MODES[mode].0)).dropdown_menu_with_anchor(gpui::Corner::BottomLeft, move |menu, _, _| mode_menu(menu, mode, pick_mode.clone())))
                                .child(div().flex_1())
                                .children(stop)
                                .child(div().flex_shrink_0().child(send)),
                        );
                    }
                    // Two rows, not one that wraps (gpui measures a wrapped row one line high and
                    // it spills out of the box): the model and the keys, then the mode and sending.
                    d.child(
                        v_flex()
                            .gap(r(8.))
                            .child(
                                h_flex()
                                    .gap(r(8.))
                                    .child(models)
                                    .child(div().min_w_0().children(chip))
                                    .child(div().flex_1())
                                    .child(div().min_w_0().truncate().text_size(r(11.5)).text_color(muted).child(if cfg!(target_os = "macos") { "⏎ envoyer, ⌘⏎ à la ligne" } else { "Entrée : envoyer, Ctrl+Entrée : à la ligne" })),
                            )
                            .child(h_flex().gap(r(8.)).child(modes).child(div().flex_1()).children(stop).child(div().flex_shrink_0().child(send))),
                    )
                })
                .children(notes),
        )
    }

    /// The pasted images and texts waiting for the next message, each with its ✕.
    fn render_attachments(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border, fg) = (theme.muted_foreground, theme.border, theme.foreground);
        h_flex().flex_wrap().gap(r(8.)).children(self.attachments.iter().enumerate().map(|(i, a)| {
            let remove = div()
                .id(SharedString::from(format!("unattach-{i}")))
                .cursor_pointer()
                .text_color(muted)
                .hover(|d| d.text_color(fg))
                .tooltip(|window, cx| Tooltip::new("Retirer").build(window, cx))
                .child(Icon::new(IconName::Close).xsmall())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if i < this.attachments.len() {
                        this.attachments.remove(i);
                    }
                    cx.notify();
                }));
            let body = match a {
                Attachment::Image { preview, .. } => div().child(img(preview.clone()).h(r(44.)).max_w(r(120.)).rounded(r(4.))),
                Attachment::Text(_) => h_flex().gap(r(5.)).child(Icon::new(IconName::File).xsmall().text_color(muted)).child(a.label()),
            };
            h_flex().gap(r(6.)).p(r(4.)).pl(r(6.)).rounded(r(8.)).border_1().border_color(border).text_size(r(12.)).child(body).child(remove)
        }))
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> Div {
        // A multiplexer panel: one quiet line, the bar above already says where and how it works.
        if !self.opts.chrome {
            return v_flex().size_full().justify_center().p(r(16.)).child(div().w_full().text_center().text_size(r(12.5)).text_color(cx.theme().muted_foreground).child("Prête : écrivez-lui ci-dessous ou depuis les Échanges."));
        }
        let theme = cx.theme();
        let (muted, border, hover) = (theme.muted_foreground, theme.border, ui::hover_bg(cx));
        ui::empty_state(Icon::empty().path("icons/claude-code.svg"), rgb(ORANGE).into(), "Que voulez-vous faire ?", "Claude Code lit et modifie les fichiers de ce dossier, avec vos réglages, vos plugins et votre abonnement :", cx)
        .child(self.render_folder("empty-folder", cx).flex_shrink_0().max_w(r(560.)).mb(r(4.)))
        .child(
            // Full height (as the list below): the area scrolls rather than squeezing them.
            v_flex().flex_shrink_0().mt(r(10.)).gap(r(6.)).children(SUGGESTIONS.iter().enumerate().map(|(i, s)| {
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
            d.child(v_flex().flex_shrink_0().mt(r(18.)).w(r(420.)).gap(r(8.)).child(ui::section_title("Reprendre une conversation", cx)).child(self.render_past(3, cx)))
        })
    }

    /// One row of the conversation's list: a card, or (the last one) what Claude is doing.
    fn render_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        // An answer starts after your message: Claude's name above it, as the speaker.
        let starts_answer = ix > 0 && matches!(self.items.get(ix - 1), Some(Item::User(..))) && matches!(self.items.get(ix), Some(Item::Text(_) | Item::Tool { .. }));
        let content: AnyElement = match self.items.get(ix) {
            Some(item) if starts_answer => {
                let muted = cx.theme().muted_foreground;
                v_flex()
                    .gap(r(8.))
                    .child(h_flex().gap(r(6.)).text_size(r(12.5)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(Icon::empty().path("icons/claude-code.svg").size(r(14.)).text_color(rgb(ORANGE))).child("Claude"))
                    .child(self.render_item(ix, item, window, cx))
                    .into_any_element()
            }
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
                    .when(shown && self.opts.chrome, |d| d.child(Spinner::new().small().color(rgb(ORANGE).into())))
                    .when(shown && !self.opts.chrome, |d| d.child(Icon::new(IconName::Ellipsis).small().text_color(rgb(ORANGE))))
                    .when(shown, |d| d.child(if working { "Claude travaille…" } else { "Claude réfléchit…" }))
                    .into_any_element()
            }
        };
        // Block layout, centered by margins: cheaper to lay out than a flex row.
        div().block().px(r(if self.opts.chrome { 24. } else { 14. })).pt(r(if ix == 0 { 20. } else { 0. })).pb(r(14.)).child(div().block().w_full().max_w(r(800.)).mx_auto().child(content)).into_any_element()
    }

    fn render_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        self.sync_log();
        if self.show_past {
            let list = v_flex()
                .w_full()
                .max_w(r(800.))
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
        v_flex().flex_1().min_w_0().h_full().child(div().flex_1().min_h_0().child(log)).child(self.render_composer(window, cx))
    }

    fn render_stats(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let border = cx.theme().border;
        // The plugins' panes first: what a mod opened beside the conversation.
        let panes: Vec<Div> = self.panes.iter().map(|(id, title, tree)| {
                let pane = id.clone();
                v_flex()
                    .gap(r(6.))
                    .pb(r(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        h_flex()
                            .child(div().flex_1().min_w_0().truncate().child(ui::section_title(title, cx)))
                            .child(button(SharedString::from(format!("pane-close-{id}"))).ghost().xsmall().icon(IconName::Close).tooltip("Fermer").on_click(cx.listener(move |this, _, _, cx| this.close_pane(&pane, cx)))),
                    )
                    .child(div().text_size(r(12.5)).children(tree.as_ref().map(|t| self.draw(t, &format!("pane-{id}"), window, cx))))
            }).collect();
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
            .children(panes)
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

impl Render for ClaudeSession {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Another folder opened: a new conversation, there (unless Claude works in a folder of its own).
        let project = if self.opts.follow_editor { self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf)) } else { self.project.clone() };
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
        } else if !self.opts.chrome {
            return v_flex().size_full().child(self.render_conversation(window, cx));
        } else {
            h_flex().flex_1().min_h_0().child(self.render_conversation(window, cx)).child(self.render_stats(window, cx)).into_any_element()
        };
        v_flex().size_full().when(self.opts.chrome, |d| d.child(header(t, subtitle, actions, cx))).child(body)
    }
}

/// What a session is doing, for the multiplexer's bars.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Working,
    /// A permission request or a question waits for you.
    Waiting,
    /// Its last write was refused: a file another session of its project holds.
    Blocked,
    /// Its last word is an error (`claude` stopped, budget reached…).
    Error,
}

impl ClaudeSession {
    pub fn state(&self) -> SessionState {
        if self.items.iter().any(|i| matches!(i, Item::Tool { ask: Some((_, None)), .. })) {
            SessionState::Waiting
        } else if self.blocked.is_some() {
            SessionState::Blocked
        } else if self.busy {
            SessionState::Working
        } else if matches!(self.items.last(), Some(Item::Notice(_, true))) {
            SessionState::Error
        } else {
            SessionState::Idle
        }
    }

    /// New instructions (its project's sessions changed): `claude` restarts on the same
    /// conversation to read them, at once if idle, else once its turn ends.
    pub fn set_system_prompt(&mut self, prompt: String) {
        if self.opts.system_prompt == prompt {
            return;
        }
        self.opts.system_prompt = prompt;
        if self.proc.is_some() {
            if self.busy {
                self.restart = true;
            } else {
                self.proc = None;
                self._reader = Task::ready(());
            }
        }
    }

    /// Why its last write was refused.
    pub fn blocked(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    pub fn cost(&self) -> f64 {
        self.stats.usage.cost
    }

    /// The subscription's windows, as the last answer gave them (they are the account's, not the session's).
    pub fn limits(&self) -> &[(&'static str, f64, i64)] {
        &self.limits
    }

    /// Its conversation's id, to resume it.
    /// (model, permissions, effort): indexes in `MODELS`, `MODES` and `EFFORTS`, as chosen in its menu.
    pub fn choices(&self, cx: &gpui::App) -> (usize, usize, usize) {
        let effort = self.opts.effort.clone().unwrap_or_else(|| AppSettings::get(cx).claude_effort.clone());
        (self.model, self.mode, EFFORTS.iter().position(|e| e.0 == effort).unwrap_or(0))
    }

    pub fn conversation(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// A message to Claude, as if typed in its box.
    pub fn submit(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.send(text, window, cx);
    }

    /// Stops the turn (the conversation goes on at the next message).
    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
    }

    /// Its model and mode (indexes of `MODELS` and `MODES`), without the open file: a
    /// multiplexer session is not about what the editor shows.
    pub fn configure(&mut self, model: usize, mode: usize, cx: &mut Context<Self>) {
        (self.model, self.mode, self.with_file) = (model.min(MODELS.len() - 1), mode.min(MODES.len() - 1), false);
        // Chosen in the multiplexer's form: no need to confirm "Tout autoriser" again.
        self.bypass_ok |= self.mode == BYPASS;
        cx.notify();
    }
}

/// The Claude Code tab: one session, with its header and its statistics.
pub struct ClaudeView {
    session: Entity<ClaudeSession>,
}

impl ClaudeView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self { session: cx.new(|cx| ClaudeSession::new(workspace, root, SessionOptions::tab(), window, cx)) }
    }

    /// Claude is answering or running a tool.
    pub fn is_busy(&self, cx: &gpui::App) -> bool {
        self.session.read(cx).is_busy()
    }
}

impl Render for ClaudeView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.session.clone())
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
        assert_eq!(parse(ask), vec![Event::Ask { request: "r1".into(), tool_id: "t1".into(), tool: "Bash".into(), input: json!({ "command": "rm x" }), rules: Value::Null }]);
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
        let (items, stats) = transcript(file.lines().map(str::to_string));
        assert_eq!(items.len(), 3);
        assert_eq!((stats.turns, stats.tools.get("Modification"), stats.files.len()), (1, Some(&1), 1));
        assert!(matches!(&items[0], Item::User(text, Some(f), ..) if text == "Corrige maximum" && f == "src/calc.py"));
        assert!(matches!(&items[1], Item::Tool { name, result: Some((out, false)), .. } if name == "Edit" && out == "ok"));
        assert!(matches!(&items[2], Item::Text(text) if text == "C'est corrigé."));
    }

    #[test]
    fn mods_and_queued_messages_are_read() {
        // Lines as Claude Code 2.1.325 writes them (cut down).
        assert_eq!(parse(r#"{"type":"user","isReplay":true,"uuid":"u1","message":{"role":"user","content":"BANANE"}}"#), vec![Event::Taken("u1".into())]);
        let drawn = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ui-band-3","response":{"tree":{"type":"Box","children":["x"]},"rewritten":true}}}"#;
        assert!(matches!(&parse(drawn)[..], [Event::Drawn { request, tree }] if request == "ui-band-3" && tree["type"] == "Box"));
        // Our other requests' answers (a press, `ui_attach`) and the CLI's own are not drawings.
        assert!(parse(r#"{"type":"control_response","response":{"request_id":"ui-attach","response":{"surfaces":["desktop"]}}}"#).is_empty());
        assert!(parse(r#"{"type":"control_response","response":{"request_id":"a1","response":{"tree":{}}}}"#).is_empty());
        assert_eq!(parse(r#"{"type":"system","subtype":"ui_invalidate","event":"ui.render"}"#), vec![Event::Redraw]);
        assert_eq!(parse(r#"{"type":"system","subtype":"ui_toast","plugin":"ponytail","text":"Copié","timeout_ms":4000}"#), vec![Event::Toast("ponytail".into(), "Copié".into(), 4000)]);
        assert_eq!(parse(r#"{"type":"system","subtype":"ui_status","plugin":"p","text":null}"#), vec![Event::Status("p".into(), None)]);
        assert_eq!(parse(r#"{"type":"system","subtype":"ui_panes","panes":[{"id":"board","title":"Tableau"},{"id":"x"}]}"#), vec![Event::Panes(vec![("board".into(), "Tableau".into()), ("x".into(), "x".into())])]);
        let init = r#"{"type":"control_response","response":{"request_id":"akolade-init","response":{"commands":[
            {"name":"compact","description":"Résumer","argumentHint":"","builtin":true},
            {"name":"ponytail:ponytail-help","description":"(ponytail) Aide","argumentHint":""},
            {"name":"zeta","description":"Mon skill","argumentHint":""},
            {"name":"Alpha","description":"Le mien aussi","argumentHint":"<x>"},
            {"name":"anthropic-skills:docs","description":"Docs (claude.ai sync)","argumentHint":""}]}}}"#;
        let Event::Commands(c) = parse(&init.replace('\n', "")).remove(0) else { panic!() };
        let names: Vec<&str> = c.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(names, ["Alpha", "anthropic-skills:docs", "zeta", "ponytail:ponytail-help", "compact"]);
        assert_eq!(c[0], ("Alpha".into(), "<x>".into(), "Le mien aussi".into()));
        // Letter by letter, never inside a character.
        assert_eq!(reveal_to("héhé", 0, 2), 3);
        assert_eq!(reveal_to("héhé", 3, 10), 6);
    }

    #[test]
    fn pasted_images_and_texts_go_with_the_message() {
        // A wide screenshot: scaled to 1568 px, sent as PNG.
        let mut wide = vec![];
        image::DynamicImage::new_rgb8(3000, 100).write_to(&mut std::io::Cursor::new(&mut wide), image::ImageFormat::Png).unwrap();
        let (media, bytes) = for_claude(&wide).unwrap();
        assert_eq!((media, image::load_from_memory(&bytes).unwrap().width()), ("image/png", 1568));
        assert_eq!(for_claude(b"<svg/>"), None);
        assert!(is_long(&"l\n".repeat(21)) && !is_long("court"));
        // Images first, then the text; without an image, the text as before.
        let image = Attachment::Image { media, bytes, preview: Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, vec![])) };
        let c = content("Que vois-tu ?", &[image]);
        assert_eq!((c[0]["type"].as_str(), c[0]["source"]["media_type"].as_str(), c[1]["text"].as_str()), (Some("image"), Some("image/png"), Some("Que vois-tu ?")));
        assert_eq!(content("salut", &[Attachment::Text("x".into())]), json!("salut"));
    }

    #[test]
    fn a_resumed_conversation_gets_its_numbers_back() {
        let answer = r#"{"type":"assistant","message":{"id":"m1","model":"claude-haiku-5-5","usage":{"input_tokens":2,"output_tokens":30,"cache_read_input_tokens":100,"cache_creation_input_tokens":8},"content":[{"type":"text","text":"a"}]}}"#;
        let more = answer.replace(r#"{"type":"text","text":"a"}"#, r#"{"type":"tool_use","id":"t","name":"Bash","input":{}}"#);
        let lines = |extra: &[&str]| [r#"{"type":"user","message":{"content":"salut"}}"#, answer, &more].iter().chain(extra).map(|l| l.to_string()).collect::<Vec<_>>();
        // No saved totals: the answer's tokens once (its two lines are one answer), its context.
        let (_, s) = transcript(lines(&[]).into_iter());
        assert_eq!((s.usage.output, s.usage.cost, s.context, s.turns), (30, 0., 110, 1));
        assert_eq!(s.tools.get("Commande"), Some(&1));
        // Saved totals: those of the process started last, whatever their order in the file.
        let totals = |start: u64, cost: f64| format!(r#"{{"type":"cost-state","startTime":{start},"totalAPIDuration":4500,"modelUsage":{{"claude-opus-5-5":{{"inputTokens":5,"outputTokens":70,"costUSD":{cost}}}}}}}"#);
        let (newer, older) = (totals(20, 2.5), totals(10, 1.));
        let (_, s) = transcript(lines(&[&newer, &older]).into_iter());
        assert_eq!((s.usage.output, s.usage.cost, s.secs, s.models.len()), (70, 2.5, 4.5, 1));
    }

    #[test]
    fn answers_permission_requests_as_the_cli_expects() {
        let input = json!({ "command": "ls" });
        assert_eq!(
            permission_response("r1", true, &input, None),
            json!({ "type": "control_response", "response": { "subtype": "success", "request_id": "r1", "response": { "behavior": "allow", "updatedInput": { "command": "ls" } } } })
        );
        let no = permission_response("r2", false, &input, None);
        assert_eq!((no["response"]["request_id"].as_str(), no["response"]["response"]["behavior"].as_str()), (Some("r2"), Some("deny")));
        assert!(no["response"]["response"]["message"].is_string(), "a refusal says why");
        // "Toujours autoriser": Claude Code's own suggestion goes back, and says what it keeps.
        let rules = json!([{ "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": "git status *" }], "behavior": "allow", "destination": "localSettings" }]);
        assert_eq!(permission_response("r3", true, &input, Some(&rules))["response"]["response"]["updatedPermissions"], rules);
        assert_eq!(always_text(&rules).as_deref(), Some("Ne plus demander : Commande « git status * », dans ce projet"));
        assert_eq!(always_text(&json!([{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }])).as_deref(), Some("Ne plus demander : les modifications de fichiers, pendant cette conversation"));
        assert_eq!(always_text(&Value::Null), None);
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
