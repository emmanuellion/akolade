//! Minimal Language Server Protocol client: enough for semantic completion
//! (`obj.` → the object's real methods) in any language that has a server.

use std::{
    cell::RefCell,
    collections::HashMap,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use futures::channel::oneshot;
use serde_json::{Value, json};

use crate::{
    languages::Language,
    problems::{self, Problems},
};

/// A language server and how to get it.
#[derive(Clone, Copy)]
pub struct Server {
    pub program: &'static str,
    pub args: &'static [&'static str],
    /// LSP `languageId` of the documents.
    pub language_id: &'static str,
    /// Shell command typed in the terminal by the "install" button.
    /// `{dir}` = Akolade's own server folder (npm packages go there, not global).
    pub install: Option<&'static str>,
    /// `initializationOptions`, computed from the server's installed path.
    pub init: Option<fn(&Path) -> Value>,
}

/// typescript-language-server must be told where TypeScript is: the
/// `typescript` package installed next to it, in the same node_modules.
pub fn tsserver_options(program: &Path) -> Value {
    match next_to(program, "typescript/lib/tsserver.js").filter(|p| p.is_file()) {
        Some(path) => json!({ "tsserver": { "path": path } }),
        None => Value::Null,
    }
}

/// `rel` in the node_modules holding the (resolved) program.
fn next_to(program: &Path, rel: &str) -> Option<PathBuf> {
    let real = program.canonicalize().unwrap_or_else(|_| program.to_path_buf());
    real.ancestors().find(|d| d.ends_with("node_modules")).map(|nm| nm.join(rel))
}

/// Answer to `workspace/configuration`: our settings, `null` (the server's
/// defaults) for the rest. basedpyright's default "recommended" mode flags every
/// value of unknown type ("Argument type is Any") in ordinary code: "standard"
/// instead, as pyright. A pyrightconfig.json or [tool.basedpyright] still wins.
fn configuration(items: &Value) -> Value {
    let items = items.as_array().map(Vec::as_slice).unwrap_or_default();
    let mode = json!({ "typeCheckingMode": "standard" });
    Value::Array(
        items
            .iter()
            .map(|item| match item["section"].as_str() {
                Some("basedpyright.analysis") => mode.clone(),
                Some("basedpyright") => json!({ "analysis": mode }),
                _ => Value::Null,
            })
            .collect(),
    )
}

/// Vue and Astro servers need TypeScript's library folder (installed next to them).
pub fn tsdk_options(program: &Path) -> Value {
    match next_to(program, "typescript/lib").filter(|p| p.is_dir()) {
        Some(tsdk) => json!({ "typescript": { "tsdk": tsdk }, "vue": { "hybridMode": false } }),
        None => Value::Null,
    }
}

/// Where Akolade installs language servers.
pub fn servers_dir() -> PathBuf {
    crate::settings::app_dir().join("servers")
}

/// `program` in our servers folder, then in PATH.
pub fn find_program(program: &str) -> Option<PathBuf> {
    let ours = servers_dir().join("node_modules/.bin");
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::iter::once(ours).chain(std::env::split_paths(&path)).find_map(|dir| crate::platform::executable_in(&dir, program))
}

/// `file://` URI, percent-encoded.
pub fn uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn write_message(out: &mut impl Write, msg: &Value) -> io::Result<()> {
    let body = msg.to_string();
    write!(out, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    out.flush()
}

fn read_message(input: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; len?];
    input.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// A document as the server last saw it.
struct Doc {
    version: i32,
    text: String,
}

pub struct Client {
    outgoing: mpsc::Sender<Value>,
    pending: Pending,
    next_id: AtomicU64,
    docs: Mutex<HashMap<String, Doc>>,
    /// False once the server's output closed (it exited or crashed).
    alive: Arc<AtomicBool>,
    /// The server accepts edits as ranges (`textDocumentSync` kind 2).
    incremental: Arc<AtomicBool>,
    /// Set once stopped: a late `publishDiagnostics` must not undo `clear_origin`.
    /// A lock, not an atomic: the reader checks and writes under it.
    stopped: Arc<Mutex<bool>>,
    child: Mutex<Child>,
}

/// Byte range that changed between two texts: `old[start..old_end]` became
/// `new[start..new_end]`. On char boundaries.
pub fn changed_range(old: &str, new: &str) -> (usize, usize, usize) {
    let mut start = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(start) || !new.is_char_boundary(start) {
        start -= 1;
    }
    let max_suffix = (old.len() - start).min(new.len() - start);
    let mut suffix = old.bytes().rev().zip(new.bytes().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    (start, old.len() - suffix, new.len() - suffix)
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        // Reaped, or each restart leaves a zombie. Prompt after SIGKILL.
        let _ = child.wait();
    }
}

impl Client {
    /// Spawns the server. Messages sent before the `initialize` handshake is
    /// done are queued, so the client is usable right away.
    /// Its diagnostics land in `problems`, under `origin` (its program).
    pub fn start(program: &Path, args: &[&str], options: Value, root: &Path, problems: Arc<Problems>, origin: &'static str) -> io::Result<Arc<Self>> {
        let mut child = crate::platform::command(program)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (outgoing, queue) = mpsc::channel::<Value>();
        let (ready, wait_ready) = mpsc::channel::<()>();
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let incremental = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(Mutex::new(false));

        let root_uri = uri(root);
        let init = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "initializationOptions": options,
                "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
                "capabilities": {
                    "textDocument": {
                        "completion": { "completionItem": { "snippetSupport": false } },
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "synchronization": { "dynamicRegistration": false },
                        "publishDiagnostics": { "relatedInformation": false },
                        "definition": { "linkSupport": true },
                        "references": {},
                        "rename": { "prepareSupport": false },
                    },
                    "workspace": { "configuration": true, "workspaceFolders": true },
                },
            },
        });
        thread::spawn(move || {
            if write_message(&mut stdin, &init).is_err() || wait_ready.recv().is_err() {
                return;
            }
            let _ = write_message(&mut stdin, &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
            for msg in queue {
                if write_message(&mut stdin, &msg).is_err() {
                    break;
                }
            }
        });

        let (waiting, reply, running, ranges, halted) = (pending.clone(), outgoing.clone(), alive.clone(), incremental.clone(), stopped.clone());
        thread::spawn(move || {
            let mut input = BufReader::new(stdout);
            while let Some(msg) = read_message(&mut input) {
                match (msg.get("id").cloned(), msg.get("method").and_then(Value::as_str)) {
                    // The server asks us something: answer "no particular setting".
                    (Some(id), Some(method)) => {
                        let result = match method {
                            "workspace/configuration" => configuration(&msg["params"]["items"]),
                            _ => Value::Null,
                        };
                        let _ = reply.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                    }
                    (Some(id), None) if id == 0 => {
                        let sync = &msg["result"]["capabilities"]["textDocumentSync"];
                        ranges.store(sync.as_u64().or_else(|| sync["change"].as_u64()) == Some(2), Ordering::Relaxed);
                        let _ = ready.send(());
                    }
                    (Some(id), None) => {
                        let sender = id.as_u64().and_then(|id| waiting.lock().unwrap().remove(&id));
                        if let Some(sender) = sender {
                            let _ = sender.send(msg.get("result").cloned().unwrap_or(Value::Null));
                        }
                    }
                    (None, Some("textDocument/publishDiagnostics")) => {
                        let halted = halted.lock().unwrap();
                        if let Some((path, list)) = problems::from_lsp(&msg["params"], origin).filter(|_| !*halted) {
                            problems.set(path, origin, list);
                        }
                    }
                    _ => {} // other notifications (logs, progress…): not used
                }
            }
            // The server is gone: release everyone waiting for an answer.
            running.store(false, Ordering::Relaxed);
            waiting.lock().unwrap().clear();
        });

        Ok(Arc::new(Self {
            outgoing,
            pending,
            next_id: AtomicU64::new(1),
            docs: Mutex::default(),
            alive,
            incremental,
            stopped,
            child: Mutex::new(child),
        }))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// No more diagnostics from this server, even if a task still holds it.
    fn halt(&self) {
        *self.stopped.lock().unwrap() = true;
    }

    /// The file's tab closed: the server reads it from the disk again (didClose).
    pub fn close(&self, path: &Path) {
        let uri = uri(path);
        if self.docs.lock().unwrap().remove(&uri).is_some() {
            self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }));
        }
    }

    /// The text the server has for this file (None: not opened, it reads the disk).
    pub fn synced_text(&self, path: &Path) -> Option<String> {
        self.docs.lock().unwrap().get(&uri(path)).map(|d| d.text.clone())
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self.outgoing.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// The answer; resolves at once as cancelled if the server is gone.
    fn request(&self, method: &str, params: Value) -> oneshot::Receiver<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if !self.is_alive() {
            return rx; // `tx` dropped: cancelled
        }
        self.pending.lock().unwrap().insert(id, tx);
        if self.outgoing.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).is_err() {
            self.pending.lock().unwrap().remove(&id);
        }
        rx
    }

    /// Tells the server the document's text: `didOpen` the first time, then
    /// `didChange` with only the edited range (or the whole text if the server
    /// wants that), nothing if unchanged.
    pub fn sync(&self, path: &Path, language_id: &str, text: &str) {
        let uri = uri(path);
        let mut docs = self.docs.lock().unwrap();
        let Some(doc) = docs.get_mut(&uri) else {
            self.notify("textDocument/didOpen", json!({ "textDocument": { "uri": uri, "languageId": language_id, "version": 1, "text": text } }));
            docs.insert(uri, Doc { version: 1, text: text.to_string() });
            return;
        };
        if doc.text == text {
            return;
        }
        let change = if self.incremental.load(Ordering::Relaxed) {
            let (start, old_end, new_end) = changed_range(&doc.text, text);
            let (sl, sc) = position(&doc.text, start);
            let (el, ec) = position(&doc.text, old_end);
            json!({ "range": { "start": { "line": sl, "character": sc }, "end": { "line": el, "character": ec } }, "text": &text[start..new_end] })
        } else {
            json!({ "text": text })
        };
        doc.version += 1;
        doc.text = text.to_string();
        self.notify("textDocument/didChange", json!({ "textDocument": { "uri": uri, "version": doc.version }, "contentChanges": [change] }));
    }

    /// `line` and `character` as LSP wants them (UTF-16 columns).
    pub fn completion(&self, path: &Path, line: usize, character: usize) -> oneshot::Receiver<Value> {
        self.request(
            "textDocument/completion",
            json!({ "textDocument": { "uri": uri(path) }, "position": { "line": line, "character": character } }),
        )
    }

    /// Signature, types and documentation of the symbol at this position.
    pub fn hover(&self, path: &Path, line: usize, character: usize) -> oneshot::Receiver<Value> {
        self.request("textDocument/hover", json!({ "textDocument": { "uri": uri(path) }, "position": { "line": line, "character": character } }))
    }

    /// Where the symbol at this position is defined (Location, Location[] or LocationLink[]).
    pub fn definition(&self, path: &Path, line: usize, character: usize) -> oneshot::Receiver<Value> {
        self.request("textDocument/definition", json!({ "textDocument": { "uri": uri(path) }, "position": { "line": line, "character": character } }))
    }

    /// Every use of the symbol at this position, its declaration included.
    pub fn references(&self, path: &Path, line: usize, character: usize) -> oneshot::Receiver<Value> {
        self.request(
            "textDocument/references",
            json!({ "textDocument": { "uri": uri(path) }, "position": { "line": line, "character": character }, "context": { "includeDeclaration": true } }),
        )
    }

    /// The edits renaming the symbol at this position everywhere (a WorkspaceEdit).
    pub fn rename(&self, path: &Path, line: usize, character: usize, new_name: &str) -> oneshot::Receiver<Value> {
        self.request("textDocument/rename", json!({ "textDocument": { "uri": uri(path) }, "position": { "line": line, "character": character }, "newName": new_name }))
    }
}

/// UTF-16 position of a byte offset, as LSP counts columns.
pub fn position(src: &str, offset: usize) -> (usize, usize) {
    let line_start = src[..offset].rfind('\n').map_or(0, |i| i + 1);
    (src[..line_start].matches('\n').count(), src[line_start..offset].encode_utf16().count())
}

/// Byte offset of an LSP position (UTF-16 column) in `text`, clamped to its line.
pub fn offset_at(text: &str, (line, col): (u32, u32)) -> usize {
    let start = text.split_inclusive('\n').take(line as usize).map(str::len).sum::<usize>().min(text.len());
    let line_text = text[start..].split('\n').next().unwrap_or_default();
    let mut units = 0;
    for (i, c) in line_text.char_indices() {
        if units >= col as usize {
            return start + i;
        }
        units += c.len_utf16();
    }
    start + line_text.len()
}

/// A crashed server is started again after this delay, not on every keystroke.
const RESTART_DELAY: Duration = Duration::from_secs(10);

/// One server per (program, project), started on first use: JavaScript and
/// TypeScript files share one typescript-language-server, C and C++ one clangd.
#[derive(Default)]
pub struct Servers {
    /// What the servers report (and the linters too).
    pub problems: Arc<Problems>,
    running: RefCell<HashMap<(&'static str, PathBuf), Arc<Client>>>,
    crashed: RefCell<HashMap<(&'static str, PathBuf), Instant>>,
    /// Missing servers are looked up again at most every few seconds
    /// (the user may be installing one right now).
    missing: RefCell<HashMap<&'static str, Instant>>,
}

impl Servers {
    pub fn client(&self, lang: &'static Language, root: &Path) -> Option<Arc<Client>> {
        let server = lang.server.as_ref()?;
        let key = (server.program, root.to_path_buf());
        let existing = self.running.borrow().get(&key).cloned();
        match existing {
            Some(client) if client.is_alive() => return Some(client),
            Some(_) => {
                self.running.borrow_mut().remove(&key);
                self.crashed.borrow_mut().insert(key.clone(), Instant::now());
                // shortcut: clears this program's problems in every project (origins aren't per root), fine while one project is open.
                self.problems.clear_origin(server.program);
            }
            None => {}
        }
        if self.crashed.borrow().get(&key).is_some_and(|t| t.elapsed() < RESTART_DELAY) {
            return None; // our own completion meanwhile
        }
        // These run the project's own code (ruby-lsp its Gemfile, graphql-lsp a JS
        // config): only in a trusted folder.
        if matches!(server.program, "ruby-lsp" | "graphql-lsp") && !crate::trust::is_trusted(root) {
            return None;
        }
        let program = self.find(server)?;
        let options = server.init.map_or(Value::Null, |init| init(&program));
        let client = Client::start(&program, server.args, options, root, self.problems.clone(), server.program).ok()?;
        self.running.borrow_mut().insert(key, client.clone());
        Some(client)
    }

    pub fn is_running(&self, program: &str) -> bool {
        self.running.borrow().iter().any(|((p, _), client)| *p == program && client.is_alive())
    }

    /// Stops a program's servers (all projects).
    pub fn stop(&self, program: &str) {
        self.running.borrow_mut().retain(|(p, _), client| *p != program || {
            client.halt();
            false
        });
        self.problems.clear_origin(program);
    }

    pub fn stop_all(&self) {
        for ((program, _), client) in self.running.borrow_mut().drain() {
            client.halt();
            self.problems.clear_origin(program);
        }
    }

    /// A tab closed: every server that had the file forgets its text.
    pub fn close(&self, path: &Path) {
        self.running.borrow().values().for_each(|client| client.close(path));
    }

    /// Is the server of this language installed?
    pub fn available(&self, lang: &'static Language) -> bool {
        lang.server.as_ref().is_some_and(|s| self.find(s).is_some())
    }

    fn find(&self, server: &Server) -> Option<PathBuf> {
        let recent = self.missing.borrow().get(server.program).is_some_and(|t| t.elapsed() < Duration::from_secs(3));
        if recent {
            return None;
        }
        let found = find_program(server.program);
        if found.is_none() {
            self.missing.borrow_mut().insert(server.program, Instant::now());
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_and_positions() {
        assert_eq!(uri(Path::new("/a b/é.py")), "file:///a%20b/%C3%A9.py");
        // "é" is 1 UTF-16 unit, "😀" is 2.
        assert_eq!(position("ab\né😀x", "ab\né😀".len()), (1, 3));
    }

    #[test]
    fn changed_range_finds_the_edit() {
        assert_eq!(changed_range("hello world", "hello brave world"), (6, 6, 12));
        assert_eq!(changed_range("abc", "ac"), (1, 2, 1));
        assert_eq!(changed_range("aaa", "aaaa"), (3, 3, 4), "prefix and suffix never overlap");
        assert_eq!(changed_range("café", "cafè"), (3, 5, 5), "on char boundaries");
        assert_eq!(changed_range("same", "same"), (4, 4, 4));
    }

    fn fake_language(program: &'static str) -> &'static Language {
        Box::leak(Box::new(Language {
            server: Some(Server { program, args: &[], language_id: "x", install: None, init: None }),
            ..crate::languages::PLAIN
        }))
    }

    #[test]
    fn languages_sharing_a_program_share_one_server() {
        // `cat` echoes our `initialize`, which is enough to look like a server.
        let (a, b) = (fake_language("cat"), fake_language("cat"));
        let servers = Servers::default();
        let root = std::env::temp_dir();
        assert!(Arc::ptr_eq(&servers.client(a, &root).unwrap(), &servers.client(b, &root).unwrap()));
    }

    #[test]
    fn a_dead_server_answers_at_once_and_is_not_restarted_on_each_keystroke() {
        let lang = fake_language("false"); // exits immediately
        let servers = Servers::default();
        let root = std::env::temp_dir();
        let client = servers.client(lang, &root).unwrap();
        let t = Instant::now();
        while client.is_alive() && t.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!client.is_alive());
        // No 1.5 s wait for an answer that will never come.
        assert!(futures::executor::block_on(client.completion(&root.join("a"), 0, 0)).is_err());
        assert!(servers.client(lang, &root).is_none(), "not restarted right away");
    }

    /// Answers `initialize`, then echoes back all we send (`cat`).
    fn echo_server(problems: Arc<Problems>) -> Arc<Client> {
        let script = r#"printf 'Content-Length: 20\r\n\r\n{"id":0,"result":{}}'; exec cat"#;
        Client::start(Path::new("/bin/sh"), &["-c", script], Value::Null, &std::env::temp_dir(), problems, "echo").unwrap()
    }

    /// Waits until the server has echoed everything sent before.
    fn barrier(client: &Client) {
        let (tx, rx) = oneshot::channel();
        client.pending.lock().unwrap().insert(999, tx);
        client.outgoing.send(json!({ "jsonrpc": "2.0", "id": 999, "result": 1 })).unwrap();
        futures::executor::block_on(rx).unwrap();
    }

    #[test]
    fn a_stopped_server_posts_no_more_diagnostics() {
        let problems: Arc<Problems> = Arc::default();
        let client = echo_server(problems.clone());
        let publish = |line: u32| {
            let range = json!({ "start": { "line": line, "character": 0 }, "end": { "line": line, "character": 1 } });
            client.notify("textDocument/publishDiagnostics", json!({ "uri": "file:///a.py", "diagnostics": [{ "range": range, "message": "m" }] }));
        };
        publish(1);
        barrier(&client);
        assert_eq!(problems.of(Path::new("/a.py"))[0].start.0, 1);
        client.halt();
        publish(2);
        barrier(&client);
        assert_eq!(problems.of(Path::new("/a.py"))[0].start.0, 1, "ignored once stopped");
    }

    #[test]
    fn closed_files_are_forgotten() {
        let client = echo_server(Arc::default());
        let file = Path::new("/p/a.py");
        client.sync(file, "python", "x = 1");
        assert_eq!(client.synced_text(file).as_deref(), Some("x = 1"));
        client.close(file);
        assert_eq!(client.synced_text(file), None);
        client.sync(file, "python", "x = 2"); // opened again: didOpen, whole text
        assert_eq!(client.synced_text(file).as_deref(), Some("x = 2"));
    }

    #[test]
    fn offsets_of_utf16_positions() {
        let text = "ab\né😀x\n";
        assert_eq!(offset_at(text, (1, 3)), "ab\né😀".len(), "😀 is 2 UTF-16 units");
        assert_eq!(offset_at(text, (1, 99)), "ab\né😀x".len(), "clamped to the line");
        let (line, col) = position(text, "ab\né😀".len());
        assert_eq!(offset_at(text, (line as u32, col as u32)), "ab\né😀".len(), "round trip");
    }

    #[test]
    fn messages_round_trip() {
        let mut buf = vec![];
        write_message(&mut buf, &json!({ "id": 1, "result": "é" })).unwrap();
        let body = r#"{"id":1,"result":"é"}"#;
        assert!(buf.starts_with(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes()));
        assert_eq!(read_message(&mut &buf[..]).unwrap()["result"], "é");
    }

    /// Asks a real server for completions at the end of `src`, until `expected` are
    /// all there (servers answer early while still loading the project, for a few
    /// seconds at most). None if the server is not installed.
    fn complete_with(program: &str, args: &[&str], language_id: &str, file_name: &str, src: &str, expected: &[&str]) -> Option<Vec<String>> {
        let program = find_program(program)?;
        let dir = std::env::temp_dir().join(format!("akolade-lsp-{}-{file_name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(file_name);
        std::fs::write(&file, src).unwrap();
        let options = if args == ["--stdio"] && program.ends_with("typescript-language-server") { tsserver_options(&program) } else { Value::Null };
        let client = Client::start(&program, args, options, &dir, Arc::default(), "test").unwrap();
        client.sync(&file, language_id, src);
        let (line, character) = position(src, src.len());
        let mut labels = vec![];
        for _ in 0..20 {
            let result = futures::executor::block_on(client.completion(&file, line, character)).unwrap();
            let items = result.get("items").unwrap_or(&result).as_array().cloned().unwrap_or_default();
            labels = items.iter().map(|i| i["label"].as_str().unwrap().trim().to_string()).collect();
            if expected.iter().all(|e| labels.iter().any(|l| l == e)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Some(labels)
    }

    // End to end against real servers; each skips itself when not installed.

    #[test]
    fn clangd_completes_struct_members() {
        let src = "struct Point { int x; int y; };\nint main(void) { struct Point p; p.";
        let Some(labels) = complete_with("clangd", &[], "c", "main.c", src, &["x", "y"]) else { return };
        assert!(labels.contains(&"x".into()) && labels.contains(&"y".into()), "{labels:?}");
    }

    #[test]
    fn edits_sent_as_ranges_reach_the_server() {
        let Some(clangd) = find_program("clangd") else { return };
        let dir = std::env::temp_dir().join(format!("akolade-lsp-{}-ranges", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.c");
        let client = Client::start(&clangd, &[], Value::Null, &dir, Arc::default(), "test").unwrap();
        let v1 = "struct Point { int x; };\nint main(void) { struct Point p; p.";
        client.sync(&file, "c", v1);
        let (line, character) = position(v1, v1.len());
        futures::executor::block_on(client.completion(&file, line, character)).unwrap(); // initialized by now
        assert!(client.incremental.load(Ordering::Relaxed), "clangd takes ranges");
        // Add a field in the middle: only that range is sent.
        let v2 = v1.replace("int x;", "int x; int zeta;");
        client.sync(&file, "c", &v2);
        let (line, character) = position(&v2, v2.len());
        let result = futures::executor::block_on(client.completion(&file, line, character)).unwrap();
        let labels: Vec<String> = result["items"].as_array().unwrap().iter().map(|i| i["label"].as_str().unwrap().trim().to_string()).collect();
        assert!(labels.contains(&"zeta".into()) && labels.contains(&"x".into()), "{labels:?}");
    }

    #[test]
    fn basedpyright_gets_the_standard_mode_and_others_their_defaults() {
        let items = json!([{ "section": "python" }, { "section": "basedpyright.analysis" }, { "section": "basedpyright" }, {}]);
        assert_eq!(configuration(&items), json!([null, { "typeCheckingMode": "standard" }, { "analysis": { "typeCheckingMode": "standard" } }, null]));
        assert_eq!(configuration(&Value::Null), json!([]));
    }

    #[test]
    fn basedpyright_knows_object_and_module_members() {
        let src = "import threading\nlock = threading.Lock()\nlock.";
        let Some(labels) = complete_with("basedpyright-langserver", &["--stdio"], "python", "a.py", src, &["acquire", "release", "locked"]) else { return };
        for m in ["acquire", "release", "locked"] {
            assert!(labels.contains(&m.into()), "{m} missing: {labels:?}");
        }
        let labels = complete_with("basedpyright-langserver", &["--stdio"], "python", "b.py", "import json\njson.", &["dumps", "loads"]).unwrap();
        assert!(labels.contains(&"dumps".into()) && labels.contains(&"loads".into()), "{labels:?}");
    }

    #[test]
    fn docker_langserver_completes_instructions() {
        let Some(labels) = complete_with("docker-langserver", &["--stdio"], "dockerfile", "Dockerfile", "FROM node:22\nWORK", &["WORKDIR"]) else { return };
        assert!(labels.contains(&"WORKDIR".into()), "{labels:?}");
    }

    #[test]
    fn basedpyright_describes_the_hovered_method() {
        let Some(program) = find_program("basedpyright-langserver") else { return };
        let dir = std::env::temp_dir().join(format!("akolade-lsp-{}-hover", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (file, src) = (dir.join("h.py"), "items = [1, 2]\nitems.append(3)\n");
        std::fs::write(&file, src).unwrap();
        let client = Client::start(&program, &["--stdio"], Value::Null, &dir, Arc::default(), "test").unwrap();
        client.sync(&file, "python", src);
        let (line, character) = position(src, src.find("append").unwrap() + 2);
        let text = (0..20)
            .map(|_| futures::executor::block_on(client.hover(&file, line, character)).unwrap())
            .map(|answer| answer["contents"]["value"].as_str().unwrap_or_default().to_string())
            .find(|t| !t.is_empty() || { std::thread::sleep(Duration::from_millis(500)); false })
            .unwrap_or_default();
        // Signature with parameter and return types, then the documentation.
        assert!(text.contains("append") && text.contains("object: int") && text.contains("-> None"), "{text}");
        assert!(text.contains("Append object to the end of the list"), "{text}");
    }

    #[test]
    fn typescript_knows_objects_and_imported_modules() {
        let src = "const user = { name: 'Ada', greet() {} };\nuser.";
        let Some(labels) = complete_with("typescript-language-server", &["--stdio"], "javascript", "a.js", src, &["name", "greet"]) else { return };
        assert!(labels.contains(&"name".into()) && labels.contains(&"greet".into()), "{labels:?}");
        // A module of the project (Node's own modules need `npm i -D @types/node`).
        let dir = std::env::temp_dir().join(format!("akolade-lsp-{}-b.js", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.js"), "export function hello() {}\nexport const VERSION = 1;\n").unwrap();
        let src = "import * as lib from './lib.js';\nlib.";
        let labels = complete_with("typescript-language-server", &["--stdio"], "javascript", "b.js", src, &["hello", "VERSION"]).unwrap();
        assert!(labels.contains(&"hello".into()) && labels.contains(&"VERSION".into()), "{labels:?}");
    }
}
