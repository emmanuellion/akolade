//! "Client HTTP": sends a request with `curl` and shows the response
//! (status, time, size, headers, and the body, JSON indented).

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};

use gpui::{
    AnyElement, AppContext, Context, Div, Entity, FontWeight, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, WeakEntity, Window, div, prelude::FluentBuilder,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    v_flex,
};
use serde::{Deserialize, Serialize};

use crate::{
    image_view::human_size,
    tools::{env::is_secret, header, tool},
    ui,
    workspace::{Save, Workspace, button, r},
};

pub const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];
/// Bigger bodies are cut: the editor is not a download manager.
const MAX_BODY: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub millis: u64,
    pub size: u64,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// The body is JSON (shown indented).
    pub json: bool,
}

/// The headers of the last response in curl's `-D` dump (after redirects or a
/// `100 Continue`, the final one is last).
pub fn parse_headers(dump: &str) -> Vec<(String, String)> {
    let dump = dump.replace("\r\n", "\n");
    let block = dump.split("\n\n").filter(|b| !b.trim().is_empty()).last().unwrap_or_default();
    block.lines().skip(1).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}

/// Sends the request (blocking: run it in the background). `headers`: one
/// `Name: value` per line.
/// `timeout`: seconds before curl gives up.
pub fn send(method: &str, url: &str, headers: &str, body: &str, timeout: u64) -> Result<Response, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("Entrez une adresse.".into());
    }
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("akolade-http-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = run_curl(&dir, method, url, headers, body, timeout);
    let _ = fs::remove_dir_all(&dir);
    result
}

fn run_curl(dir: &Path, method: &str, url: &str, headers: &str, body: &str, timeout: u64) -> Result<Response, String> {
    let (head, out) = (dir.join("head"), dir.join("body"));
    let mut cmd = crate::platform::command("curl");
    cmd.args(["-sS", "--max-time", &timeout.to_string(), "-X", method, "-D"]).arg(&head).arg("-o").arg(&out).args(["-w", "%{http_code} %{time_total} %{size_download}"]);
    for line in headers.lines().map(str::trim).filter(|l| l.contains(':')) {
        cmd.arg("-H").arg(line);
    }
    if !body.is_empty() {
        cmd.args(["--data-binary", "@-"]);
    }
    // `--url`: an address starting with "-" is never taken for an option.
    cmd.arg("--url").arg(url).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("curl : {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let body = body.to_string();
    let writer = std::thread::spawn(move || stdin.write_all(body.as_bytes()));
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(err.trim().trim_start_matches("curl: ").to_string());
    }
    let stats = String::from_utf8_lossy(&output.stdout);
    let mut stats = stats.split_whitespace();
    let status = stats.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let millis = stats.next().and_then(|s| s.parse::<f64>().ok()).map_or(0, |s| (s * 1000.).round() as u64);
    let size = stats.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let headers = parse_headers(&fs::read_to_string(&head).unwrap_or_default());
    // Only what is shown is read: a big download does not fill the memory.
    let mut bytes = vec![];
    let _ = fs::File::open(&out).and_then(|f| f.take(MAX_BODY as u64).read_to_end(&mut bytes));
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_str::<serde_json::Value>(&raw).ok();
    let body = json.as_ref().and_then(|v| serde_json::to_string_pretty(v).ok()).unwrap_or(raw);
    Ok(Response { status, millis, size, headers, body, json: json.is_some() })
}

/// A request kept in the project (`.akolade/http.json`, to commit with it).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Saved {
    pub name: String,
    pub method: String,
    pub url: String,
    /// `Name: value`, one per header.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
}

#[derive(Default, Deserialize, Serialize)]
struct SavedFile {
    #[serde(default)]
    requests: Vec<Saved>,
}

/// The project's own Akolade folder (like `.vscode`).
pub fn saved_path(root: &Path) -> PathBuf {
    root.join(".akolade").join("http.json")
}

/// The project's saved requests; none when the file does not exist yet.
pub fn load_saved(root: &Path) -> Result<Vec<Saved>, String> {
    match fs::read_to_string(saved_path(root)) {
        Ok(text) => serde_json::from_str::<SavedFile>(&text).map(|f| f.requests).map_err(|e| format!(".akolade/http.json illisible : {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.to_string()),
    }
}

/// Writes them (whole or not at all, see `write_file`).
pub fn store_saved(root: &Path, requests: &[Saved]) -> Result<(), String> {
    let path = saved_path(root);
    fs::create_dir_all(path.parent().expect("in .akolade")).map_err(|e| e.to_string())?;
    let text = serde_json::to_string_pretty(&SavedFile { requests: requests.to_vec() }).map_err(|e| e.to_string())? + "\n";
    crate::workspace::write_file(&path, &text).map_err(|e| e.to_string())
}

/// "GET /users" from the address, for a request saved without a name.
fn default_name(method: &str, url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let path = rest.find('/').map_or("/", |i| &rest[i..]);
    format!("{method} {}", path.split('?').next().unwrap_or(path))
}

/// `Authorization: Bearer x` → `Authorization:` (any secret name, see `env::is_secret`):
/// the saved file is meant to be committed. The name stays, to retype the value.
pub fn without_secret(header: &str) -> String {
    match header.split_once(':') {
        Some((name, value)) if !value.trim().is_empty() && (is_secret(name.trim()) || name.to_uppercase().contains("COOKIE")) => format!("{}:", name.trim()),
        _ => header.to_string(),
    }
}

/// `https://u:p@x/a?api_key=k&page=2` → `https://x/a?api_key=&page=2`, for the same reason.
pub fn url_without_secret(url: &str) -> String {
    // The query first: an `@` in it is not userinfo.
    let (base, query) = url.split_once('?').map_or((url, None), |(b, q)| (b, Some(q)));
    let base = crate::tools::forge::without_userinfo(base);
    let Some(query) = query else { return base };
    let params: Vec<String> = query
        .split('&')
        .map(|p| match p.split_once('=') {
            Some((k, v)) if !v.is_empty() && is_secret(k) => format!("{k}={}", v.split_once('#').map_or(String::new(), |(_, f)| format!("#{f}"))),
            _ => p.to_string(),
        })
        .collect();
    format!("{base}?{}", params.join("&"))
}

pub struct HttpView {
    workspace: WeakEntity<Workspace>,
    /// The project whose requests are listed.
    root: Option<PathBuf>,
    saved: Vec<Saved>,
    /// The saved request being edited.
    current: Option<usize>,
    /// The file could not be read: never written over.
    load_error: Option<String>,
    name: Entity<InputState>,
    method: usize,
    url: Entity<InputState>,
    headers: Entity<InputState>,
    body: Entity<InputState>,
    /// The request shows its body (else its headers); same for the response.
    request_body: bool,
    response_body: bool,
    response: Option<Result<Response, String>>,
    output: Entity<InputState>,
    busy: bool,
    _subs: Vec<Subscription>,
}

impl HttpView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("facultatif"));
        let url = cx.new(|cx| InputState::new(window, cx).placeholder("https://api.exemple.com/utilisateurs"));
        let headers = cx.new(|cx| InputState::new(window, cx).multi_line(true).placeholder("Authorization: Bearer …\nContent-Type: application/json"));
        let body = cx.new(|cx| InputState::new(window, cx).code_editor("json"));
        let output = cx.new(|cx| InputState::new(window, cx).code_editor("json"));
        let send_on_enter = |this: &mut Self, ev: &InputEvent, window: &mut Window, cx: &mut Context<Self>, secondary_only: bool| {
            if let InputEvent::PressEnter { secondary } = ev
                && (*secondary || !secondary_only)
            {
                this.send(window, cx);
            }
        };
        let _subs = vec![
            cx.subscribe_in(&url, window, move |this, _, ev, window, cx| send_on_enter(this, ev, window, cx, false)),
            cx.subscribe_in(&body, window, move |this, _, ev, window, cx| send_on_enter(this, ev, window, cx, true)),
            cx.subscribe_in(&headers, window, move |this, _, ev, window, cx| send_on_enter(this, ev, window, cx, true)),
            // Typing marks the request as modified.
            cx.subscribe(&name, |_, _, _: &InputEvent, cx| cx.notify()),
        ];
        url.update(cx, |u, cx| u.focus(window, cx));
        let mut view = Self { workspace, root: None, saved: vec![], current: None, load_error: None, name, method: 0, url, headers, body, request_body: true, response_body: true, response: None, output, busy: false, _subs };
        view.reload(root);
        view
    }

    fn reload(&mut self, root: Option<PathBuf>) {
        (self.saved, self.load_error) = match root.as_deref().map(load_saved) {
            Some(Ok(saved)) => (saved, None),
            Some(Err(e)) => (vec![], Some(e)),
            None => (vec![], None),
        };
        (self.root, self.current) = (root, None);
    }

    /// The request as typed now, as it would be saved (secret header values left out).
    fn draft(&self, cx: &gpui::App) -> Saved {
        let method = METHODS[self.method].to_string();
        let url = url_without_secret(self.url.read(cx).value().trim());
        let name = self.name.read(cx).value().trim().to_string();
        Saved {
            name: if name.is_empty() { default_name(&method, &url) } else { name },
            headers: self.headers.read(cx).value().lines().map(str::trim).filter(|l| !l.is_empty()).map(without_secret).collect(),
            body: self.body.read(cx).value().to_string(),
            method,
            url,
        }
    }

    /// Changed since it was saved (or typed and never saved).
    fn modified(&self, cx: &gpui::App) -> bool {
        let draft = self.draft(cx);
        match self.current.and_then(|i| self.saved.get(i)) {
            Some(saved) => *saved != draft,
            None => !draft.url.is_empty(),
        }
    }

    fn fill(&mut self, r: Saved, window: &mut Window, cx: &mut Context<Self>) {
        self.method = METHODS.iter().position(|m| *m == r.method).unwrap_or(0);
        self.name.update(cx, |i, cx| i.set_value(r.name, window, cx));
        self.url.update(cx, |i, cx| i.set_value(r.url, window, cx));
        self.headers.update(cx, |i, cx| i.set_value(r.headers.join("\n"), window, cx));
        self.body.update(cx, |i, cx| i.set_value(r.body, window, cx));
        cx.notify();
    }

    fn open_saved(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.saved.get(ix).cloned() else { return };
        self.current = Some(ix);
        self.fill(r, window, cx);
    }

    fn new_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.current = None;
        self.fill(Saved::default(), window, cx);
        self.url.update(cx, |u, cx| u.focus(window, cx));
    }

    fn write(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(root) = self.root.clone() else {
            window.push_notification("Ouvrez un dossier pour y enregistrer vos requêtes.", cx);
            return false;
        };
        if let Some(e) = &self.load_error {
            window.push_notification(Notification::error(format!("{e} : corrigez-le d'abord, Akolade ne l'écrase pas.")), cx);
            return false;
        }
        match store_saved(&root, &self.saved) {
            Ok(()) => true,
            Err(e) => {
                window.push_notification(Notification::error(format!("Enregistrement impossible : {e}")), cx);
                false
            }
        }
    }

    /// ⌘S: keeps the request in the project (over the one being edited).
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.draft(cx);
        if draft.url.is_empty() {
            return window.push_notification("Entrez une adresse avant d'enregistrer.", cx);
        }
        let hidden = self.headers.read(cx).value().lines().map(str::trim).any(|l| without_secret(l) != l) || draft.url != self.url.read(cx).value().trim();
        let before = (self.saved.clone(), self.current);
        match self.current.filter(|i| *i < self.saved.len()) {
            Some(i) => self.saved[i] = draft.clone(),
            None => {
                self.saved.push(draft.clone());
                self.current = Some(self.saved.len() - 1);
            }
        }
        if self.write(window, cx) {
            self.name.update(cx, |i, cx| i.set_value(draft.name, window, cx));
            let note = if hidden { " (les secrets des en-têtes et de l'adresse ne sont pas enregistrés)" } else { "" };
            window.push_notification(Notification::success(format!("Requête enregistrée dans .akolade/http.json{note}")), cx);
        } else {
            (self.saved, self.current) = before;
        }
        cx.notify();
    }

    fn delete(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.saved.get(ix).map(|r| r.name.clone()) else { return };
        let answer = window.prompt(PromptLevel::Warning, &format!("Supprimer « {name} » ?"), Some("Elle est retirée de .akolade/http.json."), &["Supprimer", "Annuler"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                let before = (this.saved.clone(), this.current);
                this.saved.remove(ix);
                this.current = match this.current {
                    Some(c) if c == ix => None,
                    Some(c) if c > ix => Some(c - 1),
                    c => c,
                };
                if !this.write(window, cx) {
                    (this.saved, this.current) = before;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_saved(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let empty = match () {
            _ if self.root.is_none() => Some("Ouvrez un dossier pour y enregistrer des requêtes.".to_string()),
            _ if self.load_error.is_some() => self.load_error.clone(),
            _ if self.saved.is_empty() => Some("Configurez une requête puis « Enregistrer » (⌘S) : elle est gardée dans le projet.".into()),
            _ => None,
        };
        v_flex()
            .w(r(240.))
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
                    .child(div().flex_1().text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(format!("REQUÊTES · {}", self.saved.len())))
                    .child(button("new-request").ghost().xsmall().icon(IconName::Plus).tooltip("Nouvelle requête").on_click(cx.listener(|this, _, window, cx| this.new_request(window, cx)))),
            )
            .child(
                v_flex()
                    .id("saved-requests")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(r(6.))
                    .gap(r(1.))
                    .when_some(empty, |d, text| d.child(div().px(r(10.)).pt(r(4.)).text_size(r(12.5)).text_color(muted).child(text)))
                    .children(self.saved.iter().enumerate().map(|(ix, req)| {
                        let on = self.current == Some(ix);
                        let color = method_color(&req.method, cx);
                        let group = SharedString::from(format!("saved-{ix}"));
                        h_flex()
                            .id(group.clone())
                            .group(group.clone())
                            .flex_shrink_0()
                            .h(r(32.))
                            .px(r(8.))
                            .gap(r(8.))
                            .rounded(r(6.))
                            .cursor_pointer()
                            .when(on, |d| d.bg(theme.primary.opacity(0.14)))
                            .when(!on, |d| d.hover(|d| d.bg(fg.opacity(0.05))))
                            .child(div().w(r(44.)).flex_shrink_0().text_size(r(10.)).font_weight(FontWeight::BOLD).text_color(color).child(req.method.clone()))
                            .child(div().flex_1().min_w_0().truncate().text_size(r(13.)).child(req.name.clone()))
                            .child(
                                div().invisible().group_hover(group, |d| d.visible()).child(
                                    button(SharedString::from(format!("delete-{ix}"))).ghost().xsmall().icon(IconName::Delete).tooltip("Supprimer").on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.delete(ix, window, cx)
                                    })),
                                ),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| this.open_saved(ix, window, cx)))
                    })),
            )
            .child(div().flex_shrink_0().px(r(16.)).py(r(10.)).border_t_1().border_color(border).text_size(r(11.)).text_color(muted).child(".akolade/http.json"))
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let method = METHODS[self.method];
        let url = self.url.read(cx).value().to_string();
        let headers = self.headers.read(cx).value().to_string();
        let body = self.body.read(cx).value().to_string();
        let timeout = crate::settings::AppSettings::get(cx).http_timeout as u64;
        self.busy = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move { send(method, &url, &headers, &body, timeout) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                if let Ok(r) = &result {
                    let text = r.body.clone();
                    let lang = if r.json { "json" } else if r.body.trim_start().starts_with('<') { "html" } else { "text" };
                    this.output = cx.new(|cx| InputState::new(window, cx).code_editor(lang));
                    this.output.update(cx, |o, cx| o.set_value(text, window, cx));
                }
                this.response = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    fn tabs(&self, id: &'static str, body: bool, headers_label: String, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let tab = |label: String, on: bool, value: bool, cx: &mut Context<Self>| {
            div()
                .id(SharedString::from(format!("{id}-{value}")))
                .px(r(10.))
                .py(r(4.))
                .rounded(r(6.))
                .cursor_pointer()
                .text_size(r(12.5))
                .map(|d| if on { d.bg(fg.opacity(0.08)).text_color(fg).font_weight(FontWeight::MEDIUM) } else { d.text_color(muted).hover(|d| d.text_color(fg)) })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if id == "req" { this.request_body = value } else { this.response_body = value }
                    cx.notify();
                }))
        };
        h_flex().gap(r(4.)).child(tab("Corps".into(), body, true, cx)).child(tab(headers_label, !body, false, cx))
    }

    fn render_response(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(response) = &self.response else {
            return ui::empty_state((tool("http").icon)(), cx.theme().blue, "Pas encore de réponse", "Entrez une adresse puis « Envoyer » (ou Entrée).", cx).into_any_element();
        };
        let r_ = match response {
            Err(e) => return ui::error_state("La requête a échoué", e, cx).into_any_element(),
            Ok(r_) => r_.clone(),
        };
        let tabs = self.tabs("resp", self.response_body, format!("En-têtes · {}", r_.headers.len()), cx);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let color = match r_.status {
            200..=299 => theme.green,
            300..=399 => theme.yellow,
            _ => theme.red,
        };
        let bar = h_flex()
            .flex_shrink_0()
            .px(r(16.))
            .py(r(10.))
            .gap(r(10.))
            .border_b_1()
            .border_color(theme.border)
            .child(div().px(r(9.)).py(r(2.)).rounded_full().bg(color.opacity(0.14)).text_color(color).font_weight(FontWeight::SEMIBOLD).text_size(r(12.5)).child(r_.status.to_string()))
            .child(div().text_size(r(12.5)).text_color(muted).child(format!("{} ms · {}", r_.millis, human_size(r_.size))))
            .child(div().flex_1())
            .child(tabs);
        let content = if self.response_body {
            div().flex_1().min_h_0().child(Input::new(&self.output).h_full().appearance(false).font_family(theme.mono_font_family.clone()).text_size(theme.mono_font_size)).into_any_element()
        } else {
            v_flex()
                .id("resp-headers")
                .flex_1()
                .overflow_y_scroll()
                .p(r(16.))
                .gap(r(6.))
                .children(r_.headers.iter().map(|(k, v)| {
                    h_flex()
                        .flex_shrink_0()
                        .gap(r(12.))
                        .text_size(r(12.5))
                        .child(div().w(r(180.)).flex_shrink_0().truncate().font_weight(FontWeight::MEDIUM).child(k.clone()))
                        .child(div().flex_1().min_w_0().text_color(muted).child(v.clone()))
                }))
                .into_any_element()
        };
        v_flex().flex_1().min_h_0().child(bar).child(content).into_any_element()
    }
}

fn method_color(method: &str, cx: &gpui::App) -> gpui::Hsla {
    let theme = cx.theme();
    match method {
        "GET" => theme.green,
        "POST" => theme.blue,
        "PUT" | "PATCH" => theme.yellow,
        _ => theme.red,
    }
}

impl Render for HttpView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Another folder opened: its own requests.
        let root = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf));
        if root != self.root {
            self.reload(root);
        }
        let modified = self.modified(cx);
        let saved_list = self.render_saved(cx);
        let req_tabs = self.tabs("req", self.request_body, "En-têtes".into(), cx);
        let response = self.render_response(cx);
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let methods = h_flex().flex_shrink_0().p(r(2.)).gap(r(2.)).rounded(r(8.)).bg(fg.opacity(0.05)).children(METHODS.iter().enumerate().map(|(i, m)| {
            let on = self.method == i;
            div()
                .id(SharedString::from(format!("method-{m}")))
                .px(r(9.))
                .py(r(4.))
                .rounded(r(6.))
                .cursor_pointer()
                .text_size(r(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .map(|d| if on { d.bg(theme.primary).text_color(theme.primary_foreground) } else { d.text_color(muted).hover(|d| d.text_color(fg)) })
                .child(*m)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.method = i;
                    cx.notify();
                }))
        }));
        let editor = if self.request_body { &self.body } else { &self.headers };
        let name_row = h_flex()
            .flex_shrink_0()
            .px(r(24.))
            .pt(r(14.))
            .gap(r(10.))
            .child(Input::new(&self.name).small().flex_1().prefix(div().text_size(r(12.)).text_color(muted).child("Nom")))
            .when(modified, |d| d.child(div().text_size(r(12.)).text_color(theme.yellow).child(if self.current.is_some() { "● modifiée" } else { "● pas enregistrée" })))
            .child(button("save-request").outline().small().icon(Icon::empty().path("icons/save.svg")).label("Enregistrer").tooltip("Garder dans le projet (⌘S)").on_click(cx.listener(|this, _, window, cx| this.save(window, cx))));
        let main = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(name_row)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px(r(24.))
                    .py(r(14.))
                    .gap(r(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(methods)
                    .child(Input::new(&self.url).flex_1())
                    .child(button("send").primary().icon(IconName::ArrowRight).label("Envoyer").loading(self.busy).disabled(self.busy).on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w_1_2()
                            .h_full()
                            .border_r_1()
                            .border_color(border)
                            .child(
                                h_flex()
                                    .flex_shrink_0()
                                    .px(r(16.))
                                    .py(r(10.))
                                    .gap(r(10.))
                                    .border_b_1()
                                    .border_color(border)
                                    .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).text_size(r(13.)).child("Requête"))
                                    .child(req_tabs),
                            )
                            .child(div().flex_1().min_h_0().child(Input::new(editor).h_full().appearance(false).font_family(theme.mono_font_family.clone()).text_size(theme.mono_font_size)))
                            .child(div().flex_shrink_0().px(r(16.)).py(r(8.)).text_size(r(11.5)).text_color(muted).child(if self.request_body { "⌘⏎ pour envoyer. Vide : pas de corps." } else { "Un en-tête par ligne : Nom: valeur." })),
                    )
                    .child(v_flex().w_1_2().h_full().child(response)),
            );
        v_flex()
            .size_full()
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(window, cx)))
            .child(header(tool("http"), "Envoyez une requête, lisez la réponse, gardez-la dans le projet.", vec![], cx))
            .child(h_flex().flex_1().min_h_0().child(saved_list).child(main))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Read},
        net::TcpListener,
    };

    #[test]
    fn the_last_response_headers_are_kept() {
        let dump = "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Type: application/json\r\nX-Id: a:b\r\n\r\n";
        assert_eq!(parse_headers(dump), vec![("Content-Type".into(), "application/json".into()), ("X-Id".into(), "a:b".into())]);
    }

    #[test]
    fn secret_header_values_are_not_saved() {
        assert_eq!(without_secret("Authorization: Bearer abc"), "Authorization:");
        assert_eq!(without_secret("x-api-key:k"), "x-api-key:");
        assert_eq!(without_secret("Authorization:"), "Authorization:");
        assert_eq!(without_secret("Content-Type: application/json"), "Content-Type: application/json");
        assert_eq!(without_secret("Cookie: session=abc"), "Cookie:");
        assert_eq!(without_secret("Set-Cookie: a=b"), "Set-Cookie:");
        assert_eq!(without_secret("Proxy-Authorization: Basic x"), "Proxy-Authorization:");
        assert_eq!(url_without_secret("https://user:pass@api.x.com/v1?api_key=k&page=2&token=t#top"), "https://api.x.com/v1?api_key=&page=2&token=#top");
        assert_eq!(url_without_secret("http://localhost:3000/u?email=a@b.c"), "http://localhost:3000/u?email=a@b.c");
        assert_eq!(url_without_secret("https://x/a"), "https://x/a");
    }

    #[test]
    fn saved_requests_round_trip_in_the_project_folder() {
        let root = std::env::temp_dir().join(format!("akolade-http-saved-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert_eq!(load_saved(&root).unwrap(), vec![], "no file yet");
        let reqs = vec![
            Saved { name: "Utilisateurs".into(), method: "GET".into(), url: "http://localhost:3000/users".into(), ..Default::default() },
            Saved { name: "Créer".into(), method: "POST".into(), url: "http://x/u".into(), headers: vec!["Content-Type: application/json".into()], body: "{\"a\":1}".into() },
        ];
        store_saved(&root, &reqs).unwrap();
        assert_eq!(load_saved(&root).unwrap(), reqs);
        let text = fs::read_to_string(saved_path(&root)).unwrap();
        assert!(text.contains("\"name\": \"Utilisateurs\"") && !text.contains("\"body\": \"\""), "readable, empty fields left out:\n{text}");
        fs::write(saved_path(&root), "{ oops").unwrap();
        assert!(load_saved(&root).is_err());
        fs::remove_dir_all(root).unwrap();
        assert_eq!(default_name("GET", "https://api.x.com/v1/users?page=2"), "GET /v1/users");
        assert_eq!(default_name("POST", "localhost:3000"), "POST /");
    }

    /// End to end against a one-shot local server.
    #[test]
    fn sends_method_headers_and_body_and_reads_the_response() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = server.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            request.push_str(&String::from_utf8(body).unwrap());
            let reply = "{\"id\":7,\"ok\":true}";
            let mut stream = stream;
            write!(stream, "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}", reply.len()).unwrap();
            request
        });
        let r = send("POST", &format!("http://127.0.0.1:{port}/users"), "X-Token: abc\n\nnot a header", "{\"name\":\"Ada\"}", 60).unwrap();
        let request = handle.join().unwrap();
        assert!(request.starts_with("POST /users HTTP/1.1"), "{request}");
        assert!(request.contains("X-Token: abc") && request.ends_with("{\"name\":\"Ada\"}"));
        assert_eq!((r.status, r.json, r.size), (201, true, 18));
        assert_eq!(r.body, "{\n  \"id\": 7,\n  \"ok\": true\n}");
        assert!(r.headers.contains(&("Content-Type".into(), "application/json".into())));
        assert!(send("GET", "http://127.0.0.1:1/", "", "", 60).is_err(), "nothing listens on port 1");
        assert!(send("GET", "  ", "", "", 60).is_err());
    }
}
