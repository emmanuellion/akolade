//! Native completion: candidates ranked by an estimated probability of appearing.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use gpui::{App, Context, Task, Window};
use gpui_component::input::{CompletionProvider, DefinitionProvider, HoverProvider, InputState, Rope, RopeExt};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, Hover, LocationLink,
    Position, Range, TextEdit,
};

use futures::future::{self, Either};
use serde_json::Value;

use crate::{
    languages::{Decl, Language, Words},
    lsp::{self, Servers},
    settings::AppSettings,
};

/// Identifiers of `src` with their byte offset. Words inside the language's
/// strings and comments are skipped (they are not code).
fn tokens<'a>(src: &'a str, lang: &Language, is_word: &impl Fn(char) -> bool) -> Vec<(usize, &'a str)> {
    let mut out = vec![];
    let mut it = src.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let rest = &src[i..];
        let skip_to = if is_word(c) {
            let end = rest.find(|c: char| !is_word(c)).map_or(src.len(), |n| i + n);
            out.push((i, &src[i..end]));
            end
        } else if lang.line_comments.iter().any(|m| rest.starts_with(m)) {
            rest.find('\n').map_or(src.len(), |n| i + n)
        } else if let Some((open, close)) = lang.block_comment.filter(|(open, _)| rest.starts_with(open)) {
            rest[open.len()..].find(close).map_or(src.len(), |n| i + open.len() + n + close.len())
        } else if lang.quotes.contains(c) {
            string_end(src, i, c)
        } else {
            continue;
        };
        while it.next_if(|&(j, _)| j < skip_to).is_some() {}
    }
    out
}

/// Offset just after the closing quote (or end of line for unterminated '…' / "…").
fn string_end(src: &str, start: usize, quote: char) -> usize {
    let mut escaped = false;
    for (j, c) in src[start + 1..].char_indices() {
        let at = start + 1 + j;
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            return at + 1;
        } else if c == '\n' && quote != '`' {
            return at;
        }
    }
    src.len()
}

/// Names the file declares: `<declarer> name`, `import a, b`, `x = …`, `x := …`.
fn declarations<'a>(src: &str, toks: &[(usize, &'a str)], lang: &Language, typing: usize) -> HashMap<&'a str, Decl> {
    let mut declared = HashMap::new();
    if !lang.code {
        return declared;
    }
    let declarer = |w: &str| lang.declarers.iter().find(|(d, _)| d.eq_ignore_ascii_case(w)).map(|(_, decl)| *decl);
    let end = |t: (usize, &str)| t.0 + t.1.len();
    let same_line = |a: (usize, &str), b: (usize, &str)| !src[end(a)..b.0].contains('\n');
    for (k, &tok) in toks.iter().enumerate() {
        let (pos, w) = tok;
        match declarer(w) {
            Some(Decl::Module) => {
                // shortcut: names must be on the import's line (multi-line imports are missed).
                let mut prev = tok;
                for &next in &toks[k + 1..] {
                    if !same_line(prev, next) {
                        break;
                    }
                    if next.0 != typing && !matches!(next.1, "from" | "as" | "type" | "import") {
                        declared.entry(next.1).or_insert(Decl::Module);
                    }
                    prev = next;
                }
            }
            Some(decl) => {
                if let Some(&next) = toks[k + 1..].iter().find(|t| t.1 != "mut")
                    && next.0 != typing
                    && same_line(tok, next)
                    && declarer(next.1).is_none()
                {
                    declared.entry(next.1).or_insert(decl);
                }
            }
            None if pos != typing && !src[..pos].ends_with('.') => {
                let after = src[end(tok)..].trim_start_matches([' ', '\t']);
                let assigned = after.starts_with(":=") || (after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=>"));
                if assigned {
                    declared.entry(w).or_insert(Decl::Variable);
                }
            }
            None => {}
        }
    }
    declared
}

fn describe(decl: Decl) -> (&'static str, CompletionItemKind) {
    match decl {
        Decl::Variable => ("variable", CompletionItemKind::VARIABLE),
        Decl::Function => ("fonction", CompletionItemKind::FUNCTION),
        Decl::Class => ("classe", CompletionItemKind::CLASS),
        Decl::Type => ("type", CompletionItemKind::INTERFACE),
        Decl::Module => ("import", CompletionItemKind::MODULE),
    }
}

#[derive(Debug, PartialEq)]
pub struct Candidate {
    pub word: String,
    /// Why it is proposed, shown in the menu ("mot-clé", "variable", …).
    pub detail: &'static str,
    pub kind: CompletionItemKind,
    /// Estimated probability that this word is the one being typed.
    pub probability: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Statement,
    Expression,
    Member,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Class {
    Keyword,
    Declared,
    Global,
    Member,
    File,
}

/// P(class | context): how likely each kind of word is at this position.
fn class_weight(ctx: Ctx, class: Class) -> f64 {
    match (ctx, class) {
        (Ctx::Statement, Class::Keyword) => 0.55,
        (Ctx::Statement, Class::Declared) => 0.25,
        (Ctx::Statement, Class::Global) => 0.12,
        (Ctx::Statement, _) => 0.08,
        (Ctx::Expression, Class::Declared) => 0.45,
        (Ctx::Expression, Class::Global) => 0.25,
        (Ctx::Expression, Class::File) => 0.2,
        (Ctx::Expression, _) => 0.1,
        (Ctx::Member, _) => 0.5,
    }
}

/// Scales `v` so it sums to 1; None if it carries no information.
fn normalized(v: Vec<f64>) -> Option<Vec<f64>> {
    let sum: f64 = v.iter().sum();
    (sum > 0.0).then(|| v.into_iter().map(|x| x / sum).collect())
}

fn shorten<'a>(w: &'static str) -> &'a str {
    w
}

fn words(lists: Words) -> impl Iterator<Item = &'static str> {
    lists.iter().flat_map(|l| l.split_whitespace())
}

/// Byte offset where the word ending at `offset` starts.
fn word_start(src: &str, offset: usize, lang: &Language) -> usize {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || lang.word_chars.contains(c);
    src[..offset].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(offset, |(i, _)| i)
}

/// What kind of word is expected after `before`.
fn context(before: &str) -> Ctx {
    let line = before[before.rfind('\n').map_or(0, |i| i + 1)..].trim_end();
    if before.ends_with('.') || before.ends_with("->") || before.ends_with("::") {
        Ctx::Member
    } else if line.is_empty() || line.ends_with([';', '{', '}']) || line.ends_with("export") {
        Ctx::Statement
    } else {
        Ctx::Expression
    }
}

/// `from x import |`, `import { | } from`, `use a::|`, `#include <|`…
fn import_line(before: &str) -> bool {
    let line = before[before.rfind('\n').map_or(0, |i| i + 1)..].trim_start();
    ["import ", "from ", "use ", "#include", "require"].iter().any(|k| line.starts_with(k))
}

// Weights of the three estimators in the final probability.
const W_BIGRAM: f64 = 0.45;
const W_FREQUENCY: f64 = 0.35;
const W_PRIOR: f64 = 0.20;

/// Returns the byte offset where the word under the cursor starts, and the
/// candidates for that prefix, sorted by estimated probability. The probability
/// mixes three estimators, each normalized over the candidates:
/// - bigram: how often the word follows the previous token in this file,
/// - frequency: occurrences in the file (nearby ones weigh more) × P(class | context),
/// - prior: P(class | context) × Zipf rank of the word in the language lists,
///   context being start of statement, expression, or member access.
pub fn complete(src: &str, offset: usize, lang: &Language) -> Option<(usize, Vec<Candidate>)> {
    use CompletionItemKind as K;
    if !src.is_char_boundary(offset) {
        return None;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || lang.word_chars.contains(c);
    let start = word_start(src, offset, lang);
    let prefix = &src[start..offset];
    if prefix.is_empty() || prefix.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let before = &src[..start];
    let ctx = context(before);

    // shortcut: re-tokenizes the whole buffer on each keystroke, fine up to a few MB.
    let toks = tokens(src, lang, &is_word);
    let declared = declarations(src, &toks, lang, start);
    let cursor_line = before.matches('\n').count();
    let prev = toks.iter().rev().find(|t| t.0 < start).map(|t| t.1);
    let mut freq: HashMap<&str, f64> = HashMap::new();
    let mut bigram: HashMap<&str, f64> = HashMap::new();
    let (mut line_no, mut last) = (0, 0);
    for (k, &(pos, w)) in toks.iter().enumerate() {
        line_no += src[last..pos].matches('\n').count();
        last = pos;
        if pos == start {
            continue; // the word being typed
        }
        let distance = line_no.abs_diff(cursor_line) as f64;
        *freq.entry(w).or_default() += 0.5 + 1.0 / (1.0 + distance / 20.0);
        if k > 0 && Some(toks[k - 1].1) == prev && toks[k - 1].0 != start {
            *bigram.entry(w).or_default() += 1.0;
        }
    }

    let rank = |lists: Words, w: &str| words(lists).position(|x| x == w);
    // (class, detail, kind, rank in the language lists); None = not relevant here.
    let classify = |w: &str| -> Option<(Class, &'static str, K, usize)> {
        let decl = declared.get(w).copied().map(describe);
        let (keyword, global) = (rank(lang.statements, w), rank(lang.globals, w));
        let in_file = freq.contains_key(w);
        if ctx == Ctx::Member {
            return if let Some(r) = rank(lang.members, w) {
                Some((Class::Member, "membre", K::METHOD, r))
            } else if in_file && keyword.is_none() && global.is_none() {
                let (d, k) = decl.unwrap_or(("fichier", K::FIELD));
                Some((Class::File, d, k, 0))
            } else {
                None
            };
        }
        match (keyword, decl, global) {
            (Some(r), ..) if ctx == Ctx::Statement => Some((Class::Keyword, "mot-clé", K::KEYWORD, r)),
            (_, Some((d, k)), _) => Some((Class::Declared, d, k, 0)),
            (_, _, Some(r)) => Some((Class::Global, "global", K::VALUE, r)),
            (Some(r), ..) => Some((Class::Keyword, "mot-clé", K::KEYWORD, r)),
            _ if in_file => Some((Class::File, "fichier", K::TEXT, 0)),
            _ => None, // known members only make sense after a `.`
        }
    };

    let lower = prefix.to_lowercase();
    // Re-borrowed so they chain with the file's own (non-'static) words.
    let known = words(lang.statements).chain(words(lang.globals)).chain(words(lang.members)).map(shorten);
    let mut seen = HashSet::new();
    let cands: Vec<_> = known
        .chain(freq.keys().copied())
        .filter(|w| w.len() > 1 && *w != prefix && !w.starts_with(|c: char| c.is_ascii_digit()))
        .filter(|w| w.to_lowercase().starts_with(&lower) && seen.insert(*w))
        .filter_map(|w| classify(w).map(|(class, detail, kind, rank)| (w, class, detail, kind, rank)))
        .collect();

    // Prior: class weight, shared inside the class by Zipf rank.
    let zipf = |rank: usize| 1.0 / (rank as f64 + 1.0);
    let mut class_mass: HashMap<Class, f64> = HashMap::new();
    for c in &cands {
        *class_mass.entry(c.1).or_default() += zipf(c.4);
    }
    let prior = cands.iter().map(|c| class_weight(ctx, c.1) * zipf(c.4) / class_mass[&c.1]).collect();
    // Contextual frequency: a keyword used 10 times is still unlikely after `=`.
    let frequency = cands.iter().map(|c| freq.get(c.0).copied().unwrap_or(0.0) * class_weight(ctx, c.1)).collect();
    let following = cands.iter().map(|c| bigram.get(c.0).copied().unwrap_or(0.0)).collect();

    // Mix the estimators that carry information, renormalizing their weights.
    let estimators: Vec<(f64, Vec<f64>)> = [(W_PRIOR, prior), (W_FREQUENCY, frequency), (W_BIGRAM, following)]
        .into_iter()
        .filter_map(|(weight, v)| normalized(v).map(|v| (weight, v)))
        .collect();
    let total: f64 = estimators.iter().map(|e| e.0).sum();
    let scores = cands.iter().enumerate().map(|(i, c)| {
        let p: f64 = estimators.iter().map(|(weight, v)| weight * v[i]).sum::<f64>() / total;
        // Same case as typed is more likely than a case-insensitive match.
        if c.0.starts_with(prefix) { p } else { p * 0.5 }
    });
    let probs = normalized(scores.collect()).unwrap_or_default();

    let mut out: Vec<Candidate> = cands
        .iter()
        .zip(probs)
        .map(|(c, probability)| Candidate { word: c.0.to_string(), detail: c.2, kind: c.3, probability })
        .collect();
    out.sort_by(|a, b| b.probability.total_cmp(&a.probability).then(a.word.len().cmp(&b.word.len())));
    out.truncate(50);
    Some((start, out))
}

/// Longer than a warm server needs; past it, our own list shows alone.
const SERVER_TIMEOUT: Duration = Duration::from_millis(500);

/// A language server's suggestion.
#[derive(Debug, PartialEq)]
struct LspItem {
    label: String,
    insert: String,
    kind: CompletionItemKind,
    /// The server's own range (editor columns), e.g. `?.c` replacing the `.` before the word.
    range: Option<Range>,
}

/// Editor position (character column) of an LSP one (UTF-16 column).
pub fn editor_position(text: &Rope, pos: Position) -> Position {
    let mut units = 0;
    let character = text.slice_line(pos.line as usize).chars().take_while(|c| {
        units += c.len_utf16() as u32;
        units <= pos.character
    });
    Position::new(pos.line, character.count() as u32)
}

fn editor_range(text: &Rope, r: Range) -> Range {
    Range::new(editor_position(text, r.start), editor_position(text, r.end))
}

/// Items of a `textDocument/completion` answer matching `prefix`, alphabetical,
/// `_private` / `__dunder__` names last.
fn lsp_items(answer: Value, prefix: &str, text: &Rope) -> Vec<LspItem> {
    let items = match serde_json::from_value::<CompletionResponse>(answer) {
        Ok(CompletionResponse::Array(items)) => items,
        Ok(CompletionResponse::List(list)) => list.items,
        Err(_) => return vec![],
    };
    let lower = prefix.to_lowercase();
    let mut out: Vec<LspItem> = items
        .into_iter()
        .filter_map(|item| {
            let label = item.label.trim().to_string();
            let filter = item.filter_text.as_deref().unwrap_or(&label).trim().to_lowercase();
            // `.c` for `?.c`: the edit starts before the word.
            if !filter.starts_with(&lower) && !filter.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_').starts_with(&lower) {
                return None;
            }
            let (insert, range) = match item.text_edit {
                Some(CompletionTextEdit::Edit(e)) => (e.new_text, Some(e.range)),
                // Insert, not replace: the rest of the word after the cursor stays.
                Some(CompletionTextEdit::InsertAndReplace(e)) => (e.new_text, Some(e.insert)),
                None => (item.insert_text.unwrap_or_else(|| label.clone()), None),
            };
            let range = range.map(|r| editor_range(text, r));
            Some(LspItem { label, insert: insert.trim().to_string(), kind: item.kind.unwrap_or(CompletionItemKind::TEXT), range })
        })
        .collect();
    out.sort_by_key(|i| (i.label.starts_with('_'), i.label.to_lowercase()));
    out.dedup_by(|a, b| a.label == b.label);
    out
}

fn kind_label(kind: CompletionItemKind) -> &'static str {
    use CompletionItemKind as K;
    match kind {
        K::METHOD => "méthode",
        K::FUNCTION | K::CONSTRUCTOR => "fonction",
        K::FIELD | K::PROPERTY => "attribut",
        K::VARIABLE => "variable",
        K::CLASS | K::STRUCT => "classe",
        K::INTERFACE | K::TYPE_PARAMETER => "type",
        K::MODULE => "module",
        K::CONSTANT | K::ENUM_MEMBER => "constante",
        K::ENUM => "enum",
        K::KEYWORD => "mot-clé",
        _ => "",
    }
}

/// Server first (alphabetical) on `obj.` and imports: it knows the real members.
/// Elsewhere our probabilistic ranking, then the server's other names.
/// `range`: the word being typed, for items without their own.
fn merge(native: Vec<Candidate>, lsp: Vec<LspItem>, server_first: bool, range: Range) -> Vec<CompletionItem> {
    let edit = |text: String| Some(CompletionTextEdit::Edit(TextEdit::new(range, text)));
    let from_server = |i: LspItem| CompletionItem {
        label: i.label,
        kind: Some(i.kind),
        detail: Some(kind_label(i.kind).to_string()),
        text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(i.range.unwrap_or(range), i.insert))),
        ..Default::default()
    };
    if server_first && !lsp.is_empty() {
        return lsp.into_iter().take(300).map(from_server).collect();
    }
    let known: HashSet<String> = native.iter().map(|c| c.word.clone()).collect();
    let native = native.into_iter().map(|c| CompletionItem {
        label: c.word.clone(),
        kind: Some(c.kind),
        detail: Some(format!("{} · {:.0} %", c.detail, c.probability * 100.0)),
        text_edit: edit(c.word),
        ..Default::default()
    });
    native.chain(lsp.into_iter().filter(|i| !known.contains(&i.label)).map(from_server)).take(80).collect()
}

/// Completion: our own (instant, any language) + the language server's when
/// one is installed (knows types, so `lock.` → the methods of `Lock`).
pub struct NativeCompletion {
    pub lang: &'static Language,
    /// The file and project, for the language server.
    pub path: Option<PathBuf>,
    pub root: PathBuf,
    pub servers: Rc<Servers>,
    /// Opens another file at a 0-based line and UTF-16 column, as the server
    /// gives it (go to definition): converted once that file's text is known.
    pub open: Rc<dyn Fn(PathBuf, u32, u32, &mut Window, &mut App)>,
}

/// A place in a file, from the language server.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub path: PathBuf,
    /// 0-based line and UTF-16 column.
    pub start: (u32, u32),
    pub end: (u32, u32),
}

/// `textDocument/definition` or `references` answers (Location, Location[],
/// LocationLink[]) as places.
pub fn places(answer: &Value) -> Vec<Place> {
    let items = match answer {
        Value::Array(items) => items.clone(),
        Value::Object(_) => vec![answer.clone()],
        _ => vec![],
    };
    let pos = |v: &Value| (v["line"].as_u64().unwrap_or(0) as u32, v["character"].as_u64().unwrap_or(0) as u32);
    items
        .iter()
        .filter_map(|l| {
            // A LocationLink points at the name inside its target.
            let (uri, range) = match l.get("targetUri") {
                Some(uri) => (uri, &l["targetSelectionRange"]),
                None => (&l["uri"], &l["range"]),
            };
            Some(Place { path: crate::problems::path_of_uri(uri.as_str()?)?, start: pos(&range["start"]), end: pos(&range["end"]) })
        })
        .collect()
}

/// One replacement of a WorkspaceEdit: LSP positions (0-based line, UTF-16 column).
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub text: String,
}

/// A `textDocument/rename` answer (WorkspaceEdit: `changes` or `documentChanges`) as edits per file.
pub fn workspace_edits(answer: &Value) -> Vec<(PathBuf, Vec<Edit>)> {
    let pos = |v: &Value| (v["line"].as_u64().unwrap_or(0) as u32, v["character"].as_u64().unwrap_or(0) as u32);
    let edits = |list: &Value| -> Vec<Edit> {
        list.as_array().map_or(vec![], |l| l.iter().map(|e| Edit { start: pos(&e["range"]["start"]), end: pos(&e["range"]["end"]), text: e["newText"].as_str().unwrap_or_default().to_string() }).collect())
    };
    let mut out: Vec<(PathBuf, Vec<Edit>)> = vec![];
    if let Some(changes) = answer["changes"].as_object() {
        out.extend(changes.iter().filter_map(|(uri, list)| Some((crate::problems::path_of_uri(uri)?, edits(list)))));
    }
    // File creations and renames ("kind") are left out: only text edits.
    for change in answer["documentChanges"].as_array().into_iter().flatten().filter(|c| c.get("kind").is_none()) {
        if let Some(path) = change["textDocument"]["uri"].as_str().and_then(crate::problems::path_of_uri) {
            out.push((path, edits(&change["edits"])));
        }
    }
    out.retain(|(_, e)| !e.is_empty());
    out
}

/// The text with the edits applied (they never overlap). Built forward, by start
/// then array order: edits at the same start land in their order (LSP).
pub fn apply_edits(text: &str, edits: &[Edit]) -> String {
    let mut ranges: Vec<(usize, usize, usize, &str)> =
        edits.iter().enumerate().map(|(k, e)| (lsp::offset_at(text, e.start), k, lsp::offset_at(text, e.end), e.text.as_str())).collect();
    ranges.sort_by_key(|r| (r.0, r.1));
    let (mut out, mut last) = (String::with_capacity(text.len()), 0);
    for (start, _, end, new) in ranges {
        out.push_str(&text[last..start.max(last)]);
        out.push_str(new);
        last = last.max(end);
    }
    out.push_str(&text[last..]);
    out
}

impl NativeCompletion {
    /// The language server for a request at `offset`, with the text sent to it first.
    fn server_at(&self, text: &Rope, offset: usize, cx: &App) -> Option<(Arc<lsp::Client>, PathBuf, (usize, usize))> {
        let settings = cx.try_global::<AppSettings>().cloned().unwrap_or_default();
        let server = self.lang.server.as_ref().filter(|s| settings.server_enabled(s.program))?;
        let path = self.path.clone()?;
        let client = self.servers.client(self.lang, &self.root)?;
        let src = text.to_string();
        client.sync(&path, server.language_id, &src);
        Some((client, path, lsp::position(&src, offset)))
    }
}

impl DefinitionProvider for NativeCompletion {
    /// ⌘ + pointer on a name: where it is defined (underlined, ⌘-click goes there).
    fn definitions(&self, text: &Rope, offset: usize, _: &mut Window, cx: &mut App) -> Task<Result<Vec<LocationLink>>> {
        if text.word_at(offset).is_empty() {
            return Task::ready(Ok(vec![]));
        }
        let Some((client, path, (line, character))) = self.server_at(text, offset, cx) else { return Task::ready(Ok(vec![])) };
        let answer = client.definition(&path, line, character);
        let timeout = cx.background_executor().timer(HOVER_TIMEOUT);
        let text = text.clone();
        cx.spawn(async move |_| {
            let answer = match future::select(answer, Box::pin(timeout)).await {
                Either::Left((Ok(answer), _)) => answer,
                _ => Value::Null,
            };
            Ok(places(&answer)
                .into_iter()
                .map(|p| {
                    let mut range = Range::new(Position::new(p.start.0, p.start.1), Position::new(p.end.0, p.end.1));
                    // This file: the editor moves there itself, in its own columns. Other
                    // files keep UTF-16 columns, converted by `open`.
                    if p.path == path {
                        range = editor_range(&text, range);
                    }
                    serde_json::from_value(serde_json::json!({ "targetUri": lsp::uri(&p.path), "targetRange": range, "targetSelectionRange": range }))
                })
                .filter_map(Result::ok)
                .collect())
        })
    }

    fn open_location(&self, location: &LocationLink, window: &mut Window, cx: &mut App) -> bool {
        let Some(path) = crate::problems::path_of_uri(location.target_uri.as_str()) else { return false };
        if self.path.as_ref() == Some(&path) {
            return false; // the editor moves within itself
        }
        let start = location.target_selection_range.start;
        (self.open)(path, start.line, start.character, window, cx);
        true
    }
}

impl CompletionProvider for NativeCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<Result<CompletionResponse>> {
        let settings = cx.try_global::<AppSettings>().cloned().unwrap_or_default();
        if !settings.completion {
            return Task::ready(Ok(CompletionResponse::Array(vec![])));
        }
        let src: Arc<str> = text.to_string().into();
        let start = word_start(&src, offset, self.lang);
        let range = Range::new(text.offset_to_position(start), text.offset_to_position(offset));
        let prefix = src[start..offset].to_string();
        let server_first = context(&src[..start]) == Ctx::Member || import_line(&src[..start]);
        // Re-reads the whole file (~15 ms at 2 MB): off the UI thread, typing never waits.
        let lang = self.lang;
        let native = cx.background_executor().spawn({
            let src = src.clone();
            async move { complete(&src, offset, lang).map(|(_, c)| c).unwrap_or_default() }
        });

        let server = self.lang.server.as_ref().zip(self.path.as_ref()).filter(|(s, _)| settings.server_enabled(s.program) && (!prefix.is_empty() || server_first));
        let Some((client, (server, path))) = server.and_then(|s| Some((self.servers.client(self.lang, &self.root)?, s))) else {
            return cx.spawn(async move |_, _| Ok(CompletionResponse::Array(merge(native.await, vec![], false, range))));
        };
        client.sync(path, server.language_id, &src);
        let (line, character) = lsp::position(&src, offset);
        let answer = client.completion(path, line, character);
        let timeout = cx.background_executor().timer(SERVER_TIMEOUT);
        let rope = text.clone();
        cx.spawn(async move |_, _| {
            // A slow server (still indexing…) must not block typing: fall back to ours.
            let answer = match future::select(answer, Box::pin(timeout)).await {
                Either::Left((Ok(answer), _)) => answer,
                _ => Value::Null,
            };
            Ok(CompletionResponse::Array(merge(native.await, lsp_items(answer, &prefix, &rope), server_first, range)))
        })
    }

    // Always ask: an empty answer is what closes the menu.
    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut Context<InputState>) -> bool {
        !new_text.is_empty()
    }
}

/// The pointer must rest this long: sweeping across the text asks the server nothing.
const HOVER_DELAY: Duration = Duration::from_millis(250);
const HOVER_TIMEOUT: Duration = Duration::from_secs(3);

/// A server's hover answer, unless it has nothing to say.
fn hover_of(answer: Value) -> Option<Hover> {
    let c = &answer["contents"];
    let empty = c.is_null() || c == "" || c["value"] == "" || c.as_array().is_some_and(Vec::is_empty);
    if empty { None } else { serde_json::from_value(answer).ok() }
}

impl HoverProvider for NativeCompletion {
    /// What the plugin knows of the word under the pointer (e.g. `append`):
    /// signature with parameter types, then documentation.
    fn hover(&self, text: &Rope, offset: usize, _: &mut Window, cx: &mut App) -> Task<Result<Option<Hover>>> {
        let settings = cx.try_global::<AppSettings>().cloned().unwrap_or_default();
        let server = self.lang.server.as_ref().zip(self.path.clone()).filter(|(s, _)| settings.server_enabled(s.program) && !text.word_at(offset).is_empty());
        let Some((client, (server, path))) = server.and_then(|s| Some((self.servers.client(self.lang, &self.root)?, s))) else {
            return Task::ready(Ok(None));
        };
        let (text, delay, timeout) = (text.clone(), cx.background_executor().timer(HOVER_DELAY), cx.background_executor().timer(HOVER_DELAY + HOVER_TIMEOUT));
        // Dropped (pointer moved on) before the delay ends: nothing is sent.
        cx.spawn(async move |_| {
            delay.await;
            let src = text.to_string();
            client.sync(&path, server.language_id, &src);
            let (line, character) = lsp::position(&src, offset);
            let answer = match future::select(client.hover(&path, line, character), Box::pin(timeout)).await {
                Either::Left((Ok(answer), _)) => answer,
                _ => Value::Null,
            };
            Ok(hover_of(answer).map(|mut h| {
                h.range = h.range.map(|r| editor_range(&text, r));
                h
            }))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::languages::{PLAIN, for_path};

    fn words(src: &str, ext: &str) -> Vec<(String, &'static str)> {
        let lang = for_path(Path::new(ext));
        complete(src, src.len(), lang).map_or(vec![], |(_, c)| c.into_iter().map(|c| (c.word, c.detail)).collect())
    }

    fn first(src: &str, ext: &str) -> (String, &'static str) {
        words(src, ext).into_iter().next().unwrap_or_else(|| panic!("no completion for {src:?}"))
    }

    #[test]
    fn definition_and_reference_answers_become_places() {
        let loc = serde_json::json!({ "uri": "file:///p/a.py", "range": { "start": { "line": 3, "character": 4 }, "end": { "line": 3, "character": 7 } } });
        assert_eq!(places(&loc), vec![Place { path: "/p/a.py".into(), start: (3, 4), end: (3, 7) }]);
        let link = serde_json::json!([{ "targetUri": "file:///p/b.ts", "targetRange": { "start": { "line": 0, "character": 0 }, "end": { "line": 9, "character": 1 } },
            "targetSelectionRange": { "start": { "line": 1, "character": 9 }, "end": { "line": 1, "character": 12 } } }]);
        assert_eq!(places(&link)[0].start, (1, 9), "the name, not the whole body");
        assert!(places(&Value::Null).is_empty());
    }

    #[test]
    fn rename_edits_apply_across_lines_and_wide_characters() {
        let answer = serde_json::json!({ "changes": { "file:///p/a.py": [
            { "range": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 7 } }, "newText": "total" },
            { "range": { "start": { "line": 1, "character": 8 }, "end": { "line": 1, "character": 11 } }, "newText": "total" }
        ]}, "documentChanges": [
            { "textDocument": { "uri": "file:///p/b.py", "version": 1 }, "edits": [ { "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 6 } }, "newText": "total" } ] },
            { "kind": "rename", "oldUri": "file:///x", "newUri": "file:///y" }
        ]});
        let files = workspace_edits(&answer);
        assert_eq!(files.len(), 2);
        assert_eq!(apply_edits("def sum(a):\n    x = sum(a)\n", &files[0].1), "def total(a):\n    x = total(a)\n");
        // "😀" counts 2 UTF-16 units: the column after it is 3, not 2.
        let emoji = [Edit { start: (0, 3), end: (0, 6), text: "total".into() }];
        assert_eq!(apply_edits("😀 sum", &emoji), "😀 total");
    }

    #[test]
    fn empty_hover_answers_show_nothing() {
        assert!(hover_of(Value::Null).is_none());
        assert!(hover_of(serde_json::json!({ "contents": "" })).is_none());
        assert!(hover_of(serde_json::json!({ "contents": [] })).is_none());
        assert!(hover_of(serde_json::json!({ "contents": { "kind": "markdown", "value": "" } })).is_none());
        let h = hover_of(serde_json::json!({ "contents": { "kind": "markdown", "value": "```python\ndef append(object: int) -> None\n```" } }));
        assert!(h.is_some());
    }

    #[test]
    fn statement_start_puts_keywords_first() {
        // `code` only appears in a string, `Courses` is a declared class.
        let src = "const t = 'some code';\nclass Courses {}\nconsole.log(t);\nco";
        let w = words(src, "a.js");
        assert_eq!(w[0], ("const".into(), "mot-clé"));
        assert!(!w.iter().any(|(w, _)| w == "code"), "words in strings are not code: {w:?}");
        assert!(w.contains(&("Courses".into(), "classe")));
        assert!(w.contains(&("console".into(), "global")));
    }

    #[test]
    fn expression_puts_file_declarations_first() {
        let src = "const myValue = 1;\nfunction myFunc() {}\n// myComment\nlet x = my";
        assert_eq!(words(src, "a.ts"), vec![("myFunc".into(), "fonction"), ("myValue".into(), "variable")]);
    }

    #[test]
    fn imports_are_declarations() {
        let src = "import { readFile, writeFile as wf } from 'fs';\nconst x = rea";
        assert_eq!(first(src, "a.js"), ("readFile".into(), "import"));
    }

    #[test]
    fn member_access_proposes_members() {
        let w = words("const items = [];\nitems.fi", "a.js");
        assert!(w.contains(&("filter".into(), "membre")));
        assert!(!w.iter().any(|(w, _)| w == "function" || w == "for"), "{w:?}");
        // Rust paths and PHP/C++ arrows are member accesses too.
        assert_eq!(first("let v = Vec::new();\nv.iter().unw", "a.rs").0, "unwrap");
    }

    #[test]
    fn file_usage_beats_static_rank() {
        // `filter` ranks higher than `forEach` in general, but this file uses forEach.
        let src = "items.forEach(a);\nitems.forEach(b);\nitems.f";
        assert_eq!(first(src, "a.js").0, "forEach");
    }

    #[test]
    fn bigram_follows_previous_token() {
        // After `console.`, this file always writes `error`, never `log`.
        let src = "console.error(a);\nconsole.error(b);\nconsole.e";
        let lang = for_path(Path::new("a.js"));
        let (_, c) = complete(src, src.len(), lang).unwrap();
        assert_eq!(c[0].word, "error");
        let total: f64 = c.iter().map(|c| c.probability).sum();
        assert!((total - 1.0).abs() < 1e-9, "probabilities sum to 1: {total}");
    }

    #[test]
    fn python_declarations_and_hash_comments() {
        let src = "import numpy as np\n# total_comment\ndef total_price(items):\n    total_count = 0\n    return tot";
        let w = words(src, "a.py");
        assert!(w.contains(&("total_price".into(), "fonction")), "{w:?}");
        assert!(w.contains(&("total_count".into(), "variable")), "{w:?}");
        assert!(!w.iter().any(|(w, _)| w == "total_comment"), "comments skipped: {w:?}");
        assert_eq!(first("import numpy as np\nx = n", "a.py").0, "np");
        assert_eq!(first("de", "a.py").0, "def");
    }

    #[test]
    fn rust_lifetimes_are_not_strings() {
        // A `'a` read as an unterminated string would swallow `buffer_len`.
        let src = "fn f<'a>(s: &'a str) { let buffer_len = 1; }\nbuf";
        assert_eq!(first(src, "a.rs"), ("buffer_len".into(), "variable"));
        assert_eq!(first("let mut counter = 0;\ncou", "a.rs"), ("counter".into(), "variable"));
    }

    #[test]
    fn go_short_declarations_and_sql_comments() {
        assert_eq!(first("func main() {\n\tuserName := \"x\"\n\tfmt.Println(use", "a.go"), ("userName".into(), "variable"));
        let sql = "-- secret_note\nCREATE TABLE users (id INTEGER);\nSELECT * FROM us";
        let w = words(sql, "a.sql");
        assert_eq!(w[0], ("users".into(), "type"));
        assert!(!w.iter().any(|(w, _)| w == "secret_note"));
        assert_eq!(first("SEL", "a.sql").0, "SELECT");
    }

    #[test]
    fn server_items_are_filtered_and_alphabetical() {
        let answer = serde_json::json!({ "isIncomplete": false, "items": [
            { "label": "release", "kind": 2 },
            { "label": "__enter__", "kind": 2 },
            { "label": "acquire", "kind": 2, "insertText": "acquire" },
            { "label": "_at_fork_reinit", "kind": 2 },
            { "label": "locked", "kind": 2 },
        ]});
        let empty = Rope::from_str("");
        let labels: Vec<String> = lsp_items(answer.clone(), "", &empty).into_iter().map(|i| i.label).collect();
        assert_eq!(labels, ["acquire", "locked", "release", "__enter__", "_at_fork_reinit"]);
        assert_eq!(lsp_items(answer, "lo", &empty).len(), 1);
        assert!(lsp_items(serde_json::Value::Null, "", &empty).is_empty());
    }

    #[test]
    fn server_edit_ranges_are_kept_in_editor_columns() {
        // `😀.b.` then `?.c` replacing the last `.`: UTF-16 columns 4..5, characters 3..4.
        let text = Rope::from_str("😀.b.");
        let answer = serde_json::json!([
            { "label": "c", "filterText": ".c", "textEdit": { "range": { "start": { "line": 0, "character": 4 }, "end": { "line": 0, "character": 5 } }, "newText": "?.c" } },
            { "label": "d" },
        ]);
        let ours = Range::new(Position::new(0, 4), Position::new(0, 4));
        let items = merge(vec![], lsp_items(answer.clone(), "", &text), true, ours);
        let edit = |i: &CompletionItem| match &i.text_edit {
            Some(CompletionTextEdit::Edit(e)) => (e.range, e.new_text.clone()),
            _ => unreachable!(),
        };
        assert_eq!(edit(&items[0]), (Range::new(Position::new(0, 3), Position::new(0, 4)), "?.c".into()), "the server's range");
        assert_eq!(edit(&items[1]).0, ours, "no edit of its own: ours");
        // Typed `c`: still offered (`.c` filters it); a native `d` is not listed twice.
        assert_eq!(lsp_items(answer.clone(), "c", &text).len(), 1);
        let native = vec![Candidate { word: "d".into(), detail: "fichier", kind: CompletionItemKind::TEXT, probability: 1.0 }];
        assert_eq!(merge(native, lsp_items(answer, "", &text), false, ours).len(), 2);
    }

    #[test]
    fn same_start_edits_apply_in_their_order() {
        let edits = [Edit { start: (0, 1), end: (0, 1), text: "A".into() }, Edit { start: (0, 1), end: (0, 3), text: "B".into() }];
        assert_eq!(apply_edits("abcd", &edits), "aABd");
        let edits = [Edit { start: (0, 1), end: (0, 3), text: "B".into() }, Edit { start: (0, 1), end: (0, 1), text: "A".into() }];
        assert_eq!(apply_edits("abcd", &edits), "aBAd", "replace first, then the insert");
    }

    #[test]
    fn imports_and_members_ask_the_server_first() {
        assert!(import_line("x = 1\nfrom os import pa"));
        assert!(import_line("import { use"));
        assert!(!import_line("value = imp"));
        assert!(context("lock.") == Ctx::Member && context("a->") == Ctx::Member);
    }

    #[test]
    fn css_words_include_dashes() {
        let css = for_path(Path::new("a.css"));
        let (start, c) = complete("a { background-c", 16, css).unwrap();
        assert_eq!(start, 4);
        assert_eq!(c[0].word, "background-color");
    }

    #[test]
    fn no_completion_without_prefix() {
        assert!(complete("foo ", 4, &PLAIN).is_none());
        assert!(complete("12", 2, &PLAIN).is_none());
    }

    #[test]
    fn tokens_skip_strings_and_comments() {
        let js = for_path(Path::new("a.js"));
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let t: Vec<_> = tokens("a \"b \\\" c\" /* d */ e // f\ng `h` 'i", js, &is_word).into_iter().map(|t| t.1).collect();
        assert_eq!(t, ["a", "e", "g"]);
    }
}
