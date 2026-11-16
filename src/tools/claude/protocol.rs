//! What `claude -p --output-format stream-json` says, read as `Event`s, and how
//! each tool it uses is described on its card (label, icon, one line, details).

use std::path::{Path, PathBuf};

use gpui_component::{Icon, IconName};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub(super) fn add(&mut self, o: &Usage) {
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
    /// A permission request (`can_use_tool`), for the card of `tool_id`; `rules`: what
    /// Claude Code offers to remember so as not to ask again (`permission_suggestions`).
    Ask { request: String, tool_id: String, tool: String, input: Value, rules: Value },
    /// Another control request: answered "not supported" so the CLI never waits.
    Unsupported { request: String },
    /// End of a turn: per model, its usage since the last result and its context size.
    Result { error: bool, text: String, secs: f64, models: Vec<(String, Usage, u64)> },
    /// Subscription windows: name, part used (0–1), reset time (Unix seconds).
    Limits(Vec<(&'static str, f64, i64)>),
    /// A message of ours, taken by Claude (`--replay-user-messages`): the uuid we gave it.
    Taken(String),
    /// A mod's drawing came back: our request id, its tree.
    Drawn { request: String, tree: Value },
    /// The mods' drawings may be stale: ask again.
    Redraw,
    /// The slash commands (name, argument hint, description), from `initialize`.
    Commands(Vec<(String, String, String)>),
    /// The plugins' panes changed: (id, title) in open order.
    Panes(Vec<(String, String)>),
    /// A mod's toast (plugin, text, how long), or its pinned status line (None: cleared).
    Toast(String, String, u64),
    Status(String, Option<String>),
    Stderr(String),
    Exit,
}

pub(super) fn num(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

pub(super) fn text_of(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// A tool result's text: a string, or text blocks.
pub(super) fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// One model's tokens and cost, as the CLI writes them (a result, a saved `cost-state`).
pub(super) fn model_usage(m: &Value) -> Usage {
    Usage {
        input: num(&m["inputTokens"]),
        output: num(&m["outputTokens"]),
        cache_read: num(&m["cacheReadInputTokens"]),
        cache_write: num(&m["cacheCreationInputTokens"]),
        cost: m["costUSD"].as_f64().unwrap_or(0.),
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
        "user" if v["isReplay"] == true => vec![Event::Taken(text_of(&v["uuid"]))],
        "user" if !sub => blocks("tool_result")
            .into_iter()
            .map(|b| Event::ToolResult { id: text_of(&b["tool_use_id"]), text: result_text(&b["content"]), error: b["is_error"] == true })
            .collect(),
        "control_request" => {
            let (request, req) = (text_of(&v["request_id"]), &v["request"]);
            if req["subtype"] == "can_use_tool" {
                vec![Event::Ask { request, tool_id: text_of(&req["tool_use_id"]), tool: text_of(&req["tool_name"]), input: req["input"].clone(), rules: req["permission_suggestions"].clone() }]
            } else {
                vec![Event::Unsupported { request }]
            }
        }
        "result" => {
            // shortcut: without `model_usage_since_last_result` (older CLIs) a turn counts nothing; upgrade by diffing `modelUsage`.
            let models = v["model_usage_since_last_result"].as_object().into_iter().flatten().map(|(name, m)| (name.clone(), model_usage(m), num(&m["contextWindow"])));
            let error = v["is_error"] == true || v["subtype"] != "success";
            let text = match v["subtype"].as_str() {
                Some("error_max_budget_usd") => "Budget maximum de la session atteint : Claude s'est arrêté (Paramètres › Outils).".into(),
                Some("error_max_turns") => "Claude a atteint son nombre maximum d'étapes.".into(),
                _ => v["result"].as_str().or(v["errors"][0].as_str()).map(str::to_string).unwrap_or_else(|| text_of(&v["subtype"])),
            };
            vec![Event::Result { error, text, secs: num(&v["duration_ms"]) as f64 / 1000., models: models.collect() }]
        }
        "control_response" if v["response"]["request_id"] == "akolade-init" => {
            let mut commands: Vec<&Value> = v["response"]["response"]["commands"].as_array().into_iter().flatten().collect();
            // Your skills and commands, then the plugins' (`plugin:name`, "(plugin) …"), then Claude Code's own; each by name.
            let group = |c: &Value| {
                let (name, about) = (c["name"].as_str().unwrap_or_default(), c["description"].as_str().unwrap_or_default());
                let plugin = name.split_once(':').is_some_and(|(p, _)| about.starts_with(&format!("({p})")));
                if c["builtin"] == true { 2 } else if plugin { 1 } else { 0 }
            };
            commands.sort_by_cached_key(|c| (group(c), text_of(&c["name"]).to_lowercase()));
            vec![Event::Commands(commands.into_iter().map(|c| (text_of(&c["name"]), text_of(&c["argumentHint"]), text_of(&c["description"]))).collect())]
        }
        "control_response" if v["response"]["request_id"].as_str().is_some_and(|id| id.starts_with("ui-")) => match v["response"]["response"]["tree"].clone() {
            Value::Null => vec![],
            tree => vec![Event::Drawn { request: text_of(&v["response"]["request_id"]), tree }],
        },
        "system" if v["subtype"] == "ui_invalidate" => vec![Event::Redraw],
        "system" if v["subtype"] == "ui_panes" => {
            let panes = v["panes"].as_array().into_iter().flatten().map(|p| (text_of(&p["id"]), p["title"].as_str().unwrap_or(p["id"].as_str().unwrap_or_default()).to_string()));
            vec![Event::Panes(panes.collect())]
        }
        "system" if v["subtype"] == "ui_toast" => vec![Event::Toast(text_of(&v["plugin"]), text_of(&v["text"]), num(&v["timeout_ms"]))],
        "system" if v["subtype"] == "ui_status" => vec![Event::Status(text_of(&v["plugin"]), v["text"].as_str().map(str::to_string))],
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
pub(super) fn edited_file(name: &str, input: &Value) -> Option<PathBuf> {
    matches!(name, "Edit" | "MultiEdit" | "Write" | "NotebookEdit").then(|| input["file_path"].as_str().or(input["notebook_path"].as_str()).map(PathBuf::from)).flatten()
}

pub(super) fn label(name: &str) -> String {
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

pub(super) fn tool_icon(name: &str) -> Icon {
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
pub(super) fn details(name: &str, input: &Value, result: Option<&str>) -> Vec<(char, String)> {
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
