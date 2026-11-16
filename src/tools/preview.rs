//! "Aperçu en direct": a small static server for the project (like VS Code's
//! Live Server). Served HTML pages reload themselves when a file changes.

use std::{
    collections::HashMap,
    fs,
    io::{self, BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

use chrono::{DateTime, Local};
use gpui::{Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div};
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable, button::ButtonVariants, h_flex, v_flex};
use percent_encoding::{percent_decode_str, utf8_percent_encode};

use crate::{
    explorer::{self, ScanOptions},
    settings::AppSettings,
    tools::{devtools::URL_SET, header, tool},
    ui,
    workspace::{Workspace, button, r},
};

const VERSION_PATH: &str = "/__akolade/version";

/// Polls the version and reloads when it changes; the id counts connected pages.
const RELOAD_SCRIPT: &str = "<script>(()=>{const id=Math.random().toString(36).slice(2);let v;setInterval(()=>fetch('/__akolade/version?id='+id,{cache:'no-store'}).then(r=>r.text()).then(t=>{if(v&&t!==v)location.reload();v=t}).catch(()=>{}),500)})()</script>";

const NOT_FOUND: &str = "<!doctype html><html lang=\"fr\"><head><meta charset=\"utf-8\"><title>Page introuvable</title></head><body style=\"font-family:system-ui;margin:4rem;color:#444\"><h1>Page introuvable</h1><p>Ce fichier n'existe pas dans le projet.</p></body></html>";

pub fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The file a URL path names under `root` (canonical); a folder gives its
/// index.html. None when missing, outside the root (`..`, symlinks) or hidden
/// (`.env`, `.git/config`: never served).
pub fn resolve(root: &Path, url_path: &str) -> Option<PathBuf> {
    let decoded = percent_decode_str(url_path).decode_utf8().ok()?;
    let rel = Path::new(decoded.trim_start_matches('/'));
    if !rel.components().all(|c| matches!(c, Component::Normal(n) if !n.as_encoded_bytes().starts_with(b"."))) {
        return None;
    }
    let mut path = root.join(rel).canonicalize().ok()?;
    if path.is_dir() {
        path = path.join("index.html").canonicalize().ok()?;
    }
    // The real file too: `cfg.txt -> .env` is as hidden as `.env`.
    let hidden = path.strip_prefix(root).ok()?.components().any(|c| c.as_os_str().as_encoded_bytes().starts_with(b"."));
    (!hidden && path.is_file()).then_some(path)
}

/// The `Host:` header names this server: a page from another site that points
/// its own name at 127.0.0.1 (DNS rebinding) is refused.
pub fn host_ok(host: &str, port: u16) -> bool {
    [format!("127.0.0.1:{port}"), format!("localhost:{port}")].iter().any(|h| h.eq_ignore_ascii_case(host.trim()))
}

/// The reload script before `</body>`, or at the end when there is none.
pub fn inject(html: &str) -> String {
    // ASCII lowercase keeps byte offsets.
    let at = html.to_ascii_lowercase().rfind("</body>").unwrap_or(html.len());
    format!("{}{RELOAD_SCRIPT}{}", &html[..at], &html[at..])
}

/// What changes when any file changes (count + newest mtime), and the HTML pages.
// shortcut: explorer::scan stops at its entry cap, so changes past it are not seen; use a file watcher if big projects need it.
fn snapshot(root: &Path, options: &ScanOptions) -> ((usize, Option<SystemTime>), Vec<PathBuf>) {
    let entries = explorer::scan(root, options);
    let newest = entries.iter().filter_map(|e| fs::metadata(root.join(&e.rel)).and_then(|m| m.modified()).ok()).max();
    let pages = entries
        .iter()
        .filter(|e| !e.is_dir && e.rel.extension().is_some_and(|x| x.eq_ignore_ascii_case("html") || x.eq_ignore_ascii_case("htm")))
        .map(|e| e.rel.clone())
        .collect();
    ((entries.len(), newest), pages)
}

impl Shared {
    fn connected(&self) -> usize {
        self.clients.lock().unwrap().values().filter(|t| t.elapsed() < Duration::from_secs(2)).count()
    }
}

struct Shared {
    root: PathBuf,
    stop: AtomicBool,
    version: AtomicU64,
    pages: Mutex<Vec<PathBuf>>,
    /// Page id → its last poll.
    clients: Mutex<HashMap<String, Instant>>,
}

/// Runs until stopped or dropped; stopping frees the port.
pub struct Server {
    pub port: u16,
    shared: Arc<Shared>,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    /// Binds 127.0.0.1 on the first free port from 5500. Scans the project once (call off the UI thread).
    pub fn start(root: &Path, options: ScanOptions) -> io::Result<Server> {
        let root = root.canonicalize()?;
        let listener = (5500..5600)
            .find_map(|p| TcpListener::bind(("127.0.0.1", p)).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::AddrInUse, "aucun port libre entre 5500 et 5599"))?;
        let port = listener.local_addr()?.port();
        // Non-blocking accept: the loop sees the stop flag, then drops the listener.
        listener.set_nonblocking(true)?;
        let (mut last, pages) = snapshot(&root, &options);
        let shared = Arc::new(Shared { root, stop: AtomicBool::new(false), version: AtomicU64::new(1), pages: Mutex::new(pages), clients: Mutex::default() });

        let s = shared.clone();
        thread::spawn(move || {
            for tick in 1u64.. {
                if s.stop.load(Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(500));
                // No page open: nothing to reload, a scan every 5 s keeps the page list fresh.
                if s.connected() == 0 && tick % 10 != 0 {
                    continue;
                }
                let (now, pages) = snapshot(&s.root, &options);
                if now != last {
                    last = now;
                    *s.pages.lock().unwrap() = pages;
                    s.version.fetch_add(1, Relaxed);
                }
            }
        });
        let s = shared.clone();
        let accept = thread::spawn(move || {
            while !s.stop.load(Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let s = s.clone();
                        thread::spawn(move || {
                            let _ = handle(stream, &s);
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(30)),
                }
            }
        });
        Ok(Server { port, shared, accept: Some(accept) })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn version(&self) -> u64 {
        self.shared.version.load(Relaxed)
    }

    pub fn pages(&self) -> Vec<PathBuf> {
        self.shared.pages.lock().unwrap().clone()
    }

    /// Pages that polled in the last 2 seconds.
    pub fn connected(&self) -> usize {
        self.shared.connected()
    }

    /// Returns once the port is free.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Relaxed);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle(stream: TcpStream, s: &Shared) -> io::Result<()> {
    // Accepted sockets inherit non-blocking on macOS.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    // Read the headers too: closing with unread data can reset the connection.
    let (mut h, mut host) = (String::new(), String::new());
    while reader.read_line(&mut h)? > 2 {
        if let Some((name, value)) = h.split_once(':')
            && name.eq_ignore_ascii_case("host")
        {
            host = value.trim().to_string();
        }
        h.clear();
    }
    let target = line.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let (status, ctype, body) = if !host_ok(&host, stream.local_addr()?.port()) {
        ("403 Forbidden", "text/plain; charset=utf-8", "Adresse refusée : ouvrez la page via 127.0.0.1 ou localhost.".as_bytes().to_vec())
    } else if path == VERSION_PATH {
        if let Some(id) = query.strip_prefix("id=") {
            let mut clients = s.clients.lock().unwrap();
            clients.retain(|_, t| t.elapsed() < Duration::from_secs(2));
            clients.insert(id.to_string(), Instant::now());
        }
        ("200 OK", "text/plain; charset=utf-8", s.version.load(Relaxed).to_string().into_bytes())
    } else {
        match resolve(&s.root, path).and_then(|p| Some((fs::read(&p).ok()?, content_type(&p)))) {
            // shortcut: non-UTF-8 HTML gets replacement characters, decode the meta charset if anyone needs it.
            Some((body, ctype)) if ctype.starts_with("text/html") => ("200 OK", ctype, inject(&String::from_utf8_lossy(&body)).into_bytes()),
            Some((body, ctype)) => ("200 OK", ctype, body),
            None => ("404 Not Found", "text/html; charset=utf-8", inject(NOT_FOUND).into_bytes()),
        }
    };
    let mut w = &stream;
    write!(w, "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len())?;
    w.write_all(&body)
}

fn open(url: &str) {
    crate::platform::open(url);
}

pub struct PreviewView {
    workspace: WeakEntity<Workspace>,
    root: Option<PathBuf>,
    server: Option<Server>,
    starting: bool,
    error: Option<String>,
    version: u64,
    reloaded: Option<DateTime<Local>>,
    _poll: Task<()>,
}

impl PreviewView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let _poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let alive = this.update(cx, |this, cx| {
                    let Some(s) = &this.server else { return };
                    if s.version() != this.version {
                        (this.version, this.reloaded) = (s.version(), Some(Local::now()));
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self { workspace, root, server: None, starting: false, error: None, version: 0, reloaded: None, _poll }
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if self.server.take().is_some() {
            cx.notify();
            return;
        }
        let Some(root) = self.root.clone() else { return };
        let options = AppSettings::get(cx).scan_options();
        (self.starting, self.error) = (true, None);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let started = cx.background_executor().spawn({
                let root = root.clone();
                async move { Server::start(&root, options) }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                this.starting = false;
                // Another folder opened meanwhile: this server would serve the old one.
                if this.root.as_ref() != Some(&root) {
                    return;
                }
                match started {
                    Ok(s) => (this.version, this.reloaded, this.server) = (s.version(), None, Some(s)),
                    Err(e) => this.error = Some(format!("Impossible de démarrer le serveur : {e}.")),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for PreviewView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Another folder opened: the server of the old one stops.
        let root = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf));
        if root != self.root {
            (self.root, self.server, self.error) = (root, None, None);
        }
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let running = self.server.is_some();
        let subtitle = match &self.server {
            Some(s) => format!("En ligne sur {} · les pages se rechargent à chaque enregistrement", s.url()),
            None => "Un serveur local pour vos pages HTML, qui les recharge à chaque enregistrement.".into(),
        };
        let toggle = button("toggle")
            .small()
            .icon(Icon::new(if running { IconName::CircleX } else { IconName::Eye }))
            .label(if running { "Arrêter" } else { "Démarrer" })
            .loading(self.starting)
            .disabled(self.root.is_none())
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)));
        let toggle = if running { toggle.outline() } else { toggle.primary() };
        let empty = |title: &str, text: &str| ui::empty_state((tool("preview").icon)(), theme.green, title, text, cx).into_any_element();
        let body = match (&self.server, &self.error) {
            _ if self.root.is_none() => empty("Aucun dossier ouvert", "Ouvrez un dossier pour prévisualiser ses pages."),
            (None, Some(e)) => ui::error_state("Serveur arrêté", e, cx).into_any_element(),
            (None, None) => empty("Serveur arrêté", "Cliquez sur Démarrer : vos pages s'ouvrent dans le navigateur et se rechargent à chaque enregistrement."),
            (Some(s), _) => {
                let url = s.url();
                let pages = s.pages();
                let connected = s.connected();
                let status = format!(
                    "{} · {} page{} connectée{}",
                    self.reloaded.map_or("Aucun changement depuis le démarrage".into(), |t| format!("Dernier rechargement à {}", t.format("%H:%M:%S"))),
                    connected,
                    if connected > 1 { "s" } else { "" },
                    if connected > 1 { "s" } else { "" },
                );
                let open_url = url.clone();
                v_flex()
                    .id("preview")
                    .flex_1()
                    .overflow_y_scroll()
                    .p(r(16.))
                    .gap(r(8.))
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .px(r(16.))
                            .py(r(12.))
                            .gap(r(16.))
                            .rounded(r(10.))
                            .border_1()
                            .border_color(border)
                            .bg(theme.foreground.opacity(0.02))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().truncate().font_family(theme.mono_font_family.clone()).text_size(r(16.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.primary).child(url.clone()))
                                    .child(div().text_size(r(12.)).text_color(muted).child(status)),
                            )
                            .child(button("open").outline().small().icon(Icon::new(IconName::ExternalLink)).label("Ouvrir").tooltip("Ouvrir dans le navigateur").on_click(move |_, _, _| open(&open_url))),
                    )
                    .child(div().flex_shrink_0().pt(r(8.)).text_size(r(12.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child("PAGES HTML"))
                    .children((pages.is_empty()).then(|| div().flex_shrink_0().text_color(muted).child("Aucune page .html dans ce projet.")))
                    .children(pages.into_iter().enumerate().map(|(i, rel)| {
                        let path: Vec<String> = rel.components().map(|c| utf8_percent_encode(&c.as_os_str().to_string_lossy(), URL_SET).to_string()).collect();
                        let page_url = format!("{url}/{}", path.join("/"));
                        h_flex()
                            .id(SharedString::from(format!("page-{i}")))
                            .flex_shrink_0()
                            .h(r(36.))
                            .px(r(12.))
                            .gap(r(10.))
                            .rounded(r(8.))
                            .cursor_pointer()
                            .hover(|d| d.bg(ui::hover_bg(cx)))
                            .child(Icon::new(IconName::ExternalLink).size(r(14.)).flex_shrink_0().text_color(muted))
                            .child(div().min_w_0().truncate().font_family(theme.mono_font_family.clone()).child(rel.to_string_lossy().into_owned()))
                            .on_click(move |_, _, _| open(&page_url))
                    }))
                    .into_any_element()
            }
        };
        v_flex().size_full().child(header(tool("preview"), subtitle, vec![toggle.into_any_element()], cx)).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("akolade-preview-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn content_types_follow_the_extension() {
        let ct = |p: &str| content_type(Path::new(p));
        assert_eq!(ct("a/index.HTML"), "text/html; charset=utf-8");
        assert_eq!(ct("app.mjs"), "text/javascript; charset=utf-8");
        assert_eq!(ct("style.css"), "text/css; charset=utf-8");
        assert_eq!(ct("photo.JPEG"), "image/jpeg");
        assert_eq!(ct("logo.svg"), "image/svg+xml");
        assert_eq!(ct("font.woff2"), "font/woff2");
        assert_eq!(ct("data.json"), "application/json");
        assert_eq!(ct("Makefile"), "application/octet-stream");
    }

    #[cfg(unix)]
    #[test]
    fn paths_outside_the_root_are_refused() {
        let base = temp("escape");
        let root = base.join("site");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("index.html"), "hi").unwrap();
        fs::write(root.join("sub/a b.css"), "x").unwrap();
        fs::write(base.join("secret.txt"), "no").unwrap();
        std::os::unix::fs::symlink(base.join("secret.txt"), root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(root.join(".env"), root.join("cfg.txt")).unwrap();

        assert_eq!(resolve(&root, "/"), Some(root.join("index.html")));
        assert_eq!(resolve(&root, "/sub/a%20b.css"), Some(root.join("sub/a b.css")));
        assert_eq!(resolve(&root, "/sub"), None); // a folder without index.html
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".env"), "KEY=1").unwrap();
        fs::write(root.join(".git/config"), "x").unwrap();
        for bad in ["/.env", "/.git/config", "/%2egit/config", "/sub/../.env", "/../secret.txt", "/%2e%2e/secret.txt", "/sub/../../secret.txt", "/%2F..%2Fsecret.txt", "//etc/passwd", "/link.txt", "/cfg.txt", "/missing.html"] {
            assert_eq!(resolve(&root, bad), None, "{bad}");
        }
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn the_reload_script_goes_before_the_body_end() {
        let page = inject("<html><BODY><p>Salut</p></BODY></html>");
        assert!(page.starts_with("<html><BODY><p>Salut</p><script>"));
        assert!(page.ends_with("</script></BODY></html>"));
        assert_eq!(inject("<p>bout</p>"), format!("<p>bout</p>{RELOAD_SCRIPT}"));
    }

    fn get(port: u16, path: &str) -> String {
        get_as(port, path, &format!("localhost:{port}"))
    }

    fn get_as(port: u16, path: &str, host: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn serves_reloads_and_frees_the_port() {
        let root = temp("e2e");
        fs::write(root.join("index.html"), "<body>Bonjour</body>").unwrap();
        let mut server = Server::start(&root, ScanOptions::default()).unwrap();
        let port = server.port;
        assert!((5500..5600).contains(&port));

        let page = get(port, "/");
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n") && page.contains("Content-Type: text/html"), "{page}");
        assert!(page.contains("Bonjour<script>") && page.contains(VERSION_PATH));
        assert!(get(port, "/nope.css").starts_with("HTTP/1.1 404"));
        // Another site's name pointed at 127.0.0.1 (DNS rebinding) reads nothing.
        for host in ["evil.example:5500", "evil.example", "", "localhost", "127.0.0.1:1"] {
            assert!(get_as(port, "/", host).starts_with("HTTP/1.1 403"), "{host}");
        }
        assert!(get_as(port, "/", &format!("127.0.0.1:{port}")).starts_with("HTTP/1.1 200"));
        assert_eq!(server.pages(), vec![PathBuf::from("index.html")]);

        let version = |id: &str| get(port, &format!("{VERSION_PATH}?id={id}")).rsplit("\r\n\r\n").next().unwrap().to_string();
        assert_eq!(version("t"), "1");
        assert_eq!(server.connected(), 1);

        fs::write(root.join("index.html"), "<body>Bonsoir</body>").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while version("t") == "1" {
            assert!(Instant::now() < deadline, "the version never changed");
            thread::sleep(Duration::from_millis(100));
        }
        assert!(get(port, "/index.html").contains("Bonsoir"));

        server.stop();
        // The system can take a moment to hand the port back (it failed now and then
        // with the whole suite running): a short wait, not an instant check.
        let deadline = Instant::now() + Duration::from_secs(2);
        while TcpListener::bind(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "port {port} still taken");
            thread::sleep(Duration::from_millis(50));
        }
        drop(server);
        let _ = fs::remove_dir_all(root);
    }
}
