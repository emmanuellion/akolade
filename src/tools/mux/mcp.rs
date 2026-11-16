//! The coordination MCP server. Claude Code starts `akolade mcp-coord --session <id>`
//! (see `config`): a stateless relay between its stdio and this Akolade, over TCP on
//! 127.0.0.1 with a random token (the same on macOS, Linux and Windows). The JSON-RPC
//! is answered here (`handle`), with the coordinator.

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::OnceLock,
    thread,
};

use serde_json::{Value, json};

use super::coord::{self, Coordinator, Scope};

/// The tools' names, as `--allowedTools` lists them (they run without a permission card).
pub const TOOLS: [&str; 8] = ["list_sessions", "send_message", "broadcast", "read_inbox", "claim_files", "release_files", "list_claims", "request_merge"];

fn tools() -> Value {
    let scope = json!({ "type": "string", "enum": ["projet", "tout"], "description": "projet (par défaut) : les sessions de ton projet ; tout : toutes, tous projets confondus" });
    let paths = json!({ "type": "array", "items": { "type": "string" }, "description": "Chemins relatifs à la racine de ton dossier de travail (ou absolus)" });
    json!([
        { "name": "list_sessions", "description": "Les sessions du multiplexeur Akolade : nom, projet, rôle, mode et branche.", "inputSchema": { "type": "object", "properties": { "scope": scope } } },
        { "name": "send_message", "description": "Écrit à une session (par son nom ou son identifiant), y compris d'un autre projet. Le message lui arrive aussitôt.", "inputSchema": { "type": "object", "properties": { "to": { "type": "string" }, "text": { "type": "string" } }, "required": ["to", "text"] } },
        { "name": "broadcast", "description": "Écrit aux autres sessions de ton projet (par défaut), ou à toutes.", "inputSchema": { "type": "object", "properties": { "text": { "type": "string" }, "scope": scope }, "required": ["text"] } },
        { "name": "read_inbox", "description": "Les messages reçus et pas encore lus.", "inputSchema": { "type": "object", "properties": {} } },
        { "name": "claim_files", "description": "Réserve des fichiers de ton projet avant de les modifier : aucune autre session du projet ne pourra les écrire tant que tu ne les libères pas.", "inputSchema": { "type": "object", "properties": { "paths": paths, "intent": { "type": "string", "description": "Ce que tu vas y faire" } }, "required": ["paths", "intent"] } },
        { "name": "release_files", "description": "Libère des fichiers réservés (tous si la liste est vide).", "inputSchema": { "type": "object", "properties": { "paths": paths } } },
        { "name": "list_claims", "description": "Les fichiers réservés dans ton projet, par qui et pourquoi.", "inputSchema": { "type": "object", "properties": {} } },
        { "name": "request_merge", "description": "Ton travail est fini : ta branche entre dans la file d'intégration du projet. Akolade la fusionne dans akolade/integration si le build et les tests passent, sinon il te renvoie l'erreur. Commite d'abord ton travail.", "inputSchema": { "type": "object", "properties": { "summary": { "type": "string", "description": "Ce que la branche apporte" } }, "required": ["summary"] } },
    ])
}

fn text_result(text: String, error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": error })
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
}

fn call(c: &mut Coordinator, me: &str, name: &str, args: &Value, now: u64) -> Value {
    let arg = |k: &str| args[k].as_str().unwrap_or_default().to_string();
    let scope = if arg("scope") == "tout" { Scope::All } else { Scope::Project };
    let Some(mine) = c.session(me).cloned() else { return text_result("Session inconnue d'Akolade.".into(), true) };
    // The arguments the schema requires: missing (or misnamed), said so, nothing done.
    let required: &[&str] = match name {
        "send_message" => &["to", "text"],
        "broadcast" => &["text"],
        "claim_files" => &["paths", "intent"],
        "request_merge" => &["summary"],
        _ => &[],
    };
    let missing: Vec<&str> = required.iter().copied().filter(|k| match &args[*k] {
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => true,
    }).collect();
    if !missing.is_empty() {
        let given: Vec<&str> = args.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
        return text_result(format!("Paramètre manquant : {} (reçu : {}). Rien n'a été fait.", missing.join(", "), if given.is_empty() { "aucun".into() } else { given.join(", ") }), true);
    }
    let project_name = |p: &std::path::Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match name {
        "list_sessions" => {
            let list: Vec<String> = c
                .sessions
                .iter()
                .filter(|s| scope == Scope::All || s.project == mine.project)
                .map(|s| {
                    let me = if s.id == me { " (toi)" } else { "" };
                    let branch = s.branch.as_ref().map(|b| format!(", branche {b}")).unwrap_or_default();
                    let role = if s.role.is_empty() { String::new() } else { format!(" — {}", s.role) };
                    format!("- {}{me} [{}] · projet {} · {}{branch}{role}", s.name, s.id, project_name(&s.project), s.mode.label())
                })
                .collect();
            text_result(list.join("\n"), false)
        }
        "send_message" => match c.send(me, &arg("to"), &arg("text"), now) {
            Ok(to) => text_result(format!("Message remis à {to}."), false),
            Err(e) => text_result(e, true),
        },
        "broadcast" => {
            let to = c.broadcast(me, scope, &arg("text"), now);
            text_result(if to.is_empty() { "Personne d'autre pour l'instant.".into() } else { format!("Message remis à {}.", to.join(", ")) }, false)
        }
        "read_inbox" => {
            let msgs = c.inbox(me);
            let lines: Vec<String> = msgs.iter().map(|m| c.render(m)).collect();
            text_result(if lines.is_empty() { "Aucun message non lu.".into() } else { lines.join("\n") }, false)
        }
        "claim_files" => {
            let (taken, refused) = c.claim(me, &strings(&args["paths"]), &arg("intent"), now);
            let mut out = vec![];
            if !taken.is_empty() {
                out.push(format!("Réservés : {}.", taken.join(", ")));
                c.note(Some(&mine.project), format!("{} réserve {} ({})", mine.name, taken.join(", "), arg("intent")), now);
            }
            for (path, owner, intent) in &refused {
                out.push(if owner.is_empty() { format!("{path} : {intent}.") } else { format!("{path} : déjà réservé par {owner} ({intent}).") });
            }
            text_result(out.join("\n"), !refused.is_empty() && taken.is_empty())
        }
        "release_files" => {
            let n = c.release(me, &strings(&args["paths"]), now);
            if n > 0 {
                c.note(Some(&mine.project), format!("{} libère {}", mine.name, crate::ui::plural(n, "fichier")), now);
            }
            text_result(format!("{} libéré{}.", crate::ui::plural(n, "fichier"), if n > 1 { "s" } else { "" }), false)
        }
        "list_claims" => {
            let leases = c.leases(&mine.project, now);
            let lines: Vec<String> = leases
                .iter()
                .map(|l| {
                    let who = c.session(&l.owner).map_or(l.owner.clone(), |s| s.name.clone());
                    format!("- {} : {who} ({}), encore {} min", l.path, l.intent, l.until.saturating_sub(now) / 60)
                })
                .collect();
            text_result(if lines.is_empty() { "Aucun fichier réservé dans le projet.".into() } else { lines.join("\n") }, false)
        }
        "request_merge" => match c.request_merge(me, &arg("summary"), now) {
            Ok(1) => text_result("Ta branche est la prochaine à fusionner : Akolade lance le build et les tests, et t'écrit le résultat.".into(), false),
            Ok(n) => text_result(format!("Ta branche est en file d'intégration, position {n}. Akolade t'écrira le résultat."), false),
            Err(e) => text_result(e, true),
        },
        _ => text_result(format!("Outil inconnu : {name}"), true),
    }
}

/// One JSON-RPC message from `session`'s Claude Code; the answer (None for a notification).
pub fn handle(c: &mut Coordinator, session: &str, msg: &Value, now: u64) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2024-11-05"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "akolade", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Coordination avec les autres sessions du multiplexeur Akolade : messages, réservation de fichiers, file d'intégration.",
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools() }),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or_default();
            if !TOOLS.contains(&name) {
                return Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": format!("Outil inconnu : {name}") } }));
            }
            call(c, session, name, &msg["params"]["arguments"], now)
        }
        // `server/discover` and the like: Claude Code goes on without them.
        other => return Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Méthode non prise en charge : {other}") } })),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

// ---------------------------------------------------------------- transport

pub struct Endpoint {
    pub addr: String,
    pub token: String,
}

static SERVER: OnceLock<Endpoint> = OnceLock::new();

/// Where the relays reach this Akolade (the server starts on first use).
pub fn endpoint() -> &'static Endpoint {
    SERVER.get_or_init(|| {
        let mut raw = [0u8; 16];
        let _ = getrandom::fill(&mut raw);
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a local port");
        let addr = listener.local_addr().map(|a| a.to_string()).unwrap_or_default();
        let secret = token.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let secret = secret.clone();
                thread::spawn(move || serve(stream, &secret));
            }
        });
        Endpoint { addr, token }
    })
}

/// One relay's lines: `{"token","session","msg"}` in, the JSON-RPC answer out. A wrong
/// token closes the connection; a session speaks only for itself (its id is fixed by its relay).
fn serve(stream: TcpStream, secret: &str) {
    let Ok(mut out) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if v["token"].as_str() != Some(secret) {
            return;
        }
        let session = v["session"].as_str().unwrap_or_default().to_string();
        let answer = coord::with(|c| handle(c, &session, &v["msg"], coord::now()));
        if v["msg"]["method"] == "tools/call" {
            coord::changed();
        }
        if let Some(answer) = answer
            && writeln!(out, "{answer}").and_then(|_| out.flush()).is_err()
        {
            return;
        }
    }
}

/// The `--mcp-config` of a session: this program as the server, the endpoint in its environment.
pub fn config(session: &str) -> String {
    let e = endpoint();
    let exe = std::env::current_exe().unwrap_or_default();
    json!({ "mcpServers": { "akolade": {
        "command": exe,
        "args": ["mcp-coord", "--session", session],
        "env": { "AKOLADE_COORD": e.addr, "AKOLADE_COORD_TOKEN": e.token },
    } } })
    .to_string()
}

/// `--allowedTools`: the coordination tools need no card.
pub fn allowed_tools() -> String {
    TOOLS.map(|t| format!("mcp__akolade__{t}")).join(",")
}

/// `akolade mcp-coord --session <id>`: stdin's JSON-RPC to Akolade, its answers to stdout.
/// Ends with stdin (Claude Code gone).
pub fn relay(session: &str) -> i32 {
    let (Ok(addr), Ok(token)) = (std::env::var("AKOLADE_COORD"), std::env::var("AKOLADE_COORD_TOKEN")) else {
        eprintln!("akolade mcp-coord : lancé par le multiplexeur d'Akolade seulement (AKOLADE_COORD absent).");
        return 2;
    };
    let mut conn = TcpStream::connect(&addr).ok();
    let mut answers = conn.as_ref().and_then(|c| c.try_clone().ok()).map(BufReader::new);
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let wants = msg.get("id").cloned();
        let sent = conn.as_mut().is_some_and(|c| writeln!(c, "{}", json!({ "token": token, "session": session, "msg": msg })).is_ok());
        let answer = match (&wants, sent, answers.as_mut()) {
            (None, _, _) => continue,
            (Some(_), true, Some(r)) => {
                let mut buf = String::new();
                r.read_line(&mut buf).ok().filter(|n| *n > 0).and_then(|_| serde_json::from_str::<Value>(&buf).ok())
            }
            _ => None,
        };
        let answer = answer.unwrap_or_else(|| json!({ "jsonrpc": "2.0", "id": wants, "error": { "code": -32000, "message": "Akolade ne répond plus (multiplexeur fermé ?)" } }));
        let mut out = stdout.lock();
        if writeln!(out, "{answer}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::mux::{coord::{SessionInfo, WriteCheck}, project::WorkMode};
    use std::path::PathBuf;

    fn session(id: &str, project: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            name: id.into(),
            role: "tests".into(),
            project: PathBuf::from(project),
            cwd: PathBuf::from(format!("/wt/{id}")),
            checkout: PathBuf::from(format!("/wt/{id}")),
            mode: WorkMode::Isolated,
            branch: Some(format!("akolade/{id}")),
            conversation: None,
            model: 0,
            perm: 0,
            effort: 0,
            budget: None,
            flat: false,
        }
    }

    fn rpc(c: &mut Coordinator, who: &str, id: u64, method: &str, params: Value) -> Value {
        handle(c, who, &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }), 10).unwrap()
    }

    fn tool(c: &mut Coordinator, who: &str, name: &str, args: Value) -> (String, bool) {
        let r = rpc(c, who, 9, "tools/call", json!({ "name": name, "arguments": args }));
        (r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string(), r["result"]["isError"] == true)
    }

    #[test]
    fn the_json_rpc_handshake_and_errors() {
        let mut c = Coordinator::default();
        c.add(session("a", "/x"));
        let init = rpc(&mut c, "a", 1, "initialize", json!({ "protocolVersion": "2025-11-25" }));
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(init["id"], 1);
        assert!(handle(&mut c, "a", &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), 0).is_none(), "no answer to a notification");
        let discover = handle(&mut c, "a", &json!({ "jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover" }), 0).unwrap();
        assert_eq!(discover["error"]["code"], -32601);
        assert_eq!(discover["id"], "server-discover-probe-1");
        let list = rpc(&mut c, "a", 2, "tools/list", json!({}));
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOLS);
        assert_eq!(rpc(&mut c, "a", 3, "tools/call", json!({ "name": "rm_rf", "arguments": {} }))["error"]["code"], -32602);
        assert_eq!(tool(&mut c, "inconnue", "read_inbox", json!({})), ("Session inconnue d'Akolade.".into(), true));
    }

    #[test]
    fn the_tools_coordinate_sessions() {
        let mut c = Coordinator::default();
        c.add(session("front", "/x"));
        c.add(session("back", "/x"));
        c.add(session("client", "/y"));
        let (list, _) = tool(&mut c, "front", "list_sessions", json!({}));
        assert!(list.contains("front (toi)") && list.contains("back") && !list.contains("client"), "{list}");
        assert!(tool(&mut c, "front", "list_sessions", json!({ "scope": "tout" })).0.contains("client"));
        assert_eq!(tool(&mut c, "front", "claim_files", json!({ "paths": ["src/api.rs"], "intent": "nouvelle route" })), ("Réservés : src/api.rs.".into(), false));
        let (refused, error) = tool(&mut c, "back", "claim_files", json!({ "paths": ["src/api.rs"], "intent": "x" }));
        assert!(error && refused.contains("déjà réservé par front (nouvelle route)"), "{refused}");
        assert!(tool(&mut c, "back", "list_claims", json!({})).0.contains("src/api.rs : front (nouvelle route)"));
        assert!(tool(&mut c, "client", "list_claims", json!({})).0.contains("Aucun"), "another project's claims are not seen");
        assert_eq!(tool(&mut c, "front", "send_message", json!({ "to": "client", "text": "/users renvoie un tableau" })).0, "Message remis à client.");
        assert!(tool(&mut c, "client", "read_inbox", json!({})).0.contains("[Message de front · projet x] /users renvoie un tableau"));
        assert_eq!(tool(&mut c, "front", "broadcast", json!({ "text": "je libère" })).0, "Message remis à back.");
        assert_eq!(tool(&mut c, "front", "release_files", json!({ "paths": [] })).0, "1 fichier libéré.");
        assert_eq!(c.check_write("back", "src/api.rs", 11), WriteCheck::Granted);
        assert!(tool(&mut c, "back", "request_merge", json!({ "summary": "écran" })).0.contains("prochaine"));
        // Misnamed arguments (as a model may send them): refused, nothing sent.
        let (said, error) = tool(&mut c, "front", "send_message", json!({ "to": "back", "message": "?" }));
        assert!(error && said.contains("Paramètre manquant : text (reçu : to, message)"), "{said}");
        assert!(tool(&mut c, "front", "claim_files", json!({ "files": ["a.rs"] })).1);
        assert!(c.inbox("back").iter().all(|m| !m.text.is_empty()));
    }

    /// The whole chain with the real CLI: `claude` → `akolade mcp-coord` → this server → the coordinator.
    /// Needs `claude` and `cargo build` (target/debug/akolade). Run: cargo test claude_reaches -- --ignored
    #[test]
    #[ignore]
    fn claude_reaches_the_coordinator_through_the_relay() {
        let Some(program) = which("claude") else { return };
        let exe = std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join(if cfg!(windows) { "akolade.exe" } else { "akolade" });
        assert!(exe.exists(), "cargo build first: {}", exe.display());
        let dir = std::env::temp_dir().join(format!("akolade-mcp-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        coord::with(|c| {
            c.add(session("e2e-front", "/e2e"));
            c.add(session("e2e-client", "/autre"));
        });
        let e = endpoint();
        let cfg = json!({ "mcpServers": { "akolade": { "command": exe, "args": ["mcp-coord", "--session", "e2e-front"], "env": { "AKOLADE_COORD": e.addr, "AKOLADE_COORD_TOKEN": e.token } } } }).to_string();
        let out = crate::platform::command(program)
            .args(["-p", "--model", "haiku", "--mcp-config", &cfg, "--allowedTools", &allowed_tools(), "--output-format", "text"])
            .arg("Appelle l'outil list_sessions avec scope \"tout\", puis envoie avec send_message à la session e2e-client le texte exact « bonjour du test ». Réponds juste FAIT.")
            .current_dir(&dir)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).to_string();
        let inbox = coord::with(|c| c.inbox("e2e-client"));
        coord::with(|c| {
            c.remove("e2e-front");
            c.remove("e2e-client");
        });
        assert!(inbox.iter().any(|m| m.from == "e2e-front" && m.text.contains("bonjour du test")), "claude said: {said}\ninbox: {inbox:?}");
    }

    fn which(program: &str) -> Option<std::path::PathBuf> {
        std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|p| p.join(program)).find(|p| p.is_file()))
    }

    #[test]
    fn the_relay_talks_to_the_server_with_the_token() {
        coord::with(|c| c.add(session("relay-test", "/r")));
        let e = endpoint();
        let mut conn = TcpStream::connect(&e.addr).unwrap();
        let mut answers = BufReader::new(conn.try_clone().unwrap());
        writeln!(conn, "{}", json!({ "token": e.token, "session": "relay-test", "msg": { "jsonrpc": "2.0", "id": 7, "method": "tools/list" } })).unwrap();
        let mut line = String::new();
        answers.read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["id"], 7);
        // A wrong token: the connection closes, nothing answered.
        let mut bad = TcpStream::connect(&e.addr).unwrap();
        let mut bad_answers = BufReader::new(bad.try_clone().unwrap());
        writeln!(bad, "{}", json!({ "token": "x", "session": "relay-test", "msg": { "jsonrpc": "2.0", "id": 1, "method": "ping" } })).unwrap();
        let mut nothing = String::new();
        assert_eq!(bad_answers.read_line(&mut nothing).unwrap_or(0), 0);
        assert!(config("relay-test").contains("mcp-coord") && allowed_tools().split(',').count() == 8);
        coord::with(|c| c.remove("relay-test"));
    }

    /// The real `akolade mcp-coord` process, as Claude Code starts it: JSON-RPC on its stdin,
    /// the answers on its stdout, and it ends with stdin. Needs the built binary (`cargo build`).
    #[test]
    fn the_akolade_binary_relays_stdio_to_the_coordinator() {
        use std::process::{Command, Stdio};
        let exe = std::env::current_exe().unwrap();
        // target/<profile>/akolade, above the test binary (deps/ or build/<pkg>/<hash>/, by cargo version).
        let name = if cfg!(windows) { "akolade.exe" } else { "akolade" };
        let Some(bin) = exe.ancestors().skip(1).map(|d| d.join(name)).find(|p| p.is_file()) else {
            eprintln!("skipped: akolade not built above {}", exe.display());
            return;
        };
        coord::with(|c| c.add(session("stdio-test", "/s")));
        let e = endpoint();
        let mut child = Command::new(&bin)
            .args(["mcp-coord", "--session", "stdio-test"])
            .env("AKOLADE_COORD", &e.addr)
            .env("AKOLADE_COORD_TOKEN", &e.token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut ask = |msg: Value| {
            writeln!(input, "{msg}").unwrap();
            let mut line = String::new();
            output.read_line(&mut line).unwrap();
            serde_json::from_str::<Value>(&line).unwrap()
        };
        assert_eq!(ask(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))["result"]["tools"].as_array().map(Vec::len), Some(8));
        let claimed = ask(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "claim_files", "arguments": { "paths": ["src/a.rs"], "intent": "test" } } }));
        assert_eq!(claimed["id"], 2, "{claimed}");
        assert!(coord::with(|c| c.projects.values().any(|p| p.leases.iter().any(|l| l.owner == "stdio-test"))), "the lease reached this process");
        drop(ask);
        drop(input);
        assert!(child.wait().unwrap().success(), "ends with stdin");
        coord::with(|c| c.remove("stdio-test"));
    }
}
