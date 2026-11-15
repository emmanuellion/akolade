//! "Pull requests": the repository's open pull requests (GitHub, `gh`) or merge
//! requests (GitLab, `glab`): read one, fetch its branch, open it, create one.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, AppContext, Context, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, StatefulInteractiveElement, Styled,
    Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::ButtonVariants,
    checkbox::Checkbox,
    h_flex,
    input::{Escape, Input, InputEvent, InputState},
    notification::Notification,
    text::TextView,
    v_flex,
};
use serde::Deserialize;

use crate::{
    git::{git, relative_fr},
    lsp::find_program,
    tools::{header, in_terminal, tool},
    ui,
    workspace::{Workspace, button, r},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Host {
    GitHub,
    GitLab,
}

impl Host {
    fn cli(self) -> &'static str {
        match self {
            Host::GitHub => "gh",
            Host::GitLab => "glab",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Host::GitHub => "GitHub",
            Host::GitLab => "GitLab",
        }
    }
}

/// The address without its userinfo: `https://oauth2:TOKEN@host/x` → `https://host/x`.
pub fn without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let end = rest.find('/').unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

/// `git remote get-url origin` (https, ssh:// or scp-like `git@host:path`) → where its PRs live.
pub fn host_of(url: &str) -> Option<Host> {
    let url = url.trim();
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default().split(':').next().unwrap_or_default().to_ascii_lowercase();
    match () {
        _ if host == "github.com" || host.ends_with(".github.com") => Some(Host::GitHub),
        _ if host.contains("gitlab") => Some(Host::GitLab),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Checks {
    Success,
    Failure,
    Pending,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head: String,
    pub base: String,
    pub draft: bool,
    /// GitHub's reviewDecision: APPROVED, CHANGES_REQUESTED, REVIEW_REQUIRED or "".
    pub review: String,
    pub checks: Option<Checks>,
    /// ISO 8601.
    pub updated: String,
    pub url: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Person {
    login: String,
    username: String,
}

fn name(p: Option<Person>) -> String {
    p.map(|p| if p.login.is_empty() { p.username } else { p.login }).unwrap_or_default()
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Check {
    status: Option<String>,
    conclusion: Option<String>,
    state: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhPr {
    number: u64,
    title: String,
    author: Option<Person>,
    head_ref_name: String,
    base_ref_name: String,
    is_draft: bool,
    review_decision: Option<String>,
    updated_at: String,
    url: String,
    status_check_rollup: Option<Vec<Check>>,
}

/// Check runs (status + conclusion) and commit statuses (state), summed up.
fn rollup(checks: &[Check]) -> Option<Checks> {
    let failed = |s: &str| matches!(s, "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE");
    let mut out = None;
    for c in checks {
        let (status, conclusion, state) = (c.status.as_deref().unwrap_or(""), c.conclusion.as_deref().unwrap_or(""), c.state.as_deref().unwrap_or(""));
        if failed(conclusion) || failed(state) {
            return Some(Checks::Failure);
        }
        let pending = matches!(state, "PENDING" | "EXPECTED") || (!status.is_empty() && status != "COMPLETED");
        out = Some(if pending || out == Some(Checks::Pending) { Checks::Pending } else { Checks::Success });
    }
    out
}

const GH_FIELDS: &str = "number,title,author,headRefName,baseRefName,isDraft,reviewDecision,updatedAt,url,statusCheckRollup";

pub fn parse_gh(out: &str) -> Result<Vec<Pr>, String> {
    let list: Vec<GhPr> = serde_json::from_str(out).map_err(|e| format!("Réponse de gh illisible : {e}"))?;
    Ok(list
        .into_iter()
        .map(|p| Pr {
            number: p.number,
            title: p.title,
            author: name(p.author),
            head: p.head_ref_name,
            base: p.base_ref_name,
            draft: p.is_draft,
            review: p.review_decision.unwrap_or_default(),
            checks: rollup(p.status_check_rollup.as_deref().unwrap_or_default()),
            updated: p.updated_at,
            url: p.url,
        })
        .collect())
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct GlMr {
    iid: u64,
    title: String,
    author: Option<Person>,
    source_branch: String,
    target_branch: String,
    draft: bool,
    work_in_progress: bool,
    updated_at: String,
    web_url: String,
    detailed_merge_status: Option<String>,
}

/// `glab mr list -F json`: GitLab's API objects. The list carries no pipeline.
pub fn parse_glab(out: &str) -> Result<Vec<Pr>, String> {
    let list: Vec<GlMr> = serde_json::from_str(out).map_err(|e| format!("Réponse de glab illisible : {e}"))?;
    Ok(list
        .into_iter()
        .map(|m| Pr {
            number: m.iid,
            title: m.title,
            author: name(m.author),
            head: m.source_branch,
            base: m.target_branch,
            draft: m.draft || m.work_in_progress,
            review: if m.detailed_merge_status.as_deref() == Some("not_approved") { "REVIEW_REQUIRED".into() } else { String::new() },
            checks: None,
            updated: m.updated_at,
            url: m.web_url,
        })
        .collect())
}

/// "2026-11-13T16:02:48Z" → "il y a 3 heures".
pub fn ago_fr(iso: &str, now: DateTime<Utc>) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let s = (now - t.with_timezone(&Utc)).num_seconds().max(0);
    let (n, unit) = match s {
        ..60 => return "à l'instant".into(),
        ..3_600 => (s / 60, "minute"),
        ..86_400 => (s / 3_600, "hour"),
        ..2_592_000 => (s / 86_400, "day"),
        ..31_536_000 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    relative_fr(&format!("{n} {unit} ago"))
}

/// PR templates are full of `<!-- -->` hints nobody should read.
fn strip_html_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        rest = rest[start..].find("-->").map_or("", |end| &rest[start + end + 3..]);
    }
    out.push_str(rest);
    out.trim().to_string()
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct File {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub author: String,
    pub body: String,
    /// ISO 8601.
    pub when: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Details {
    pub body: String,
    pub files: Vec<File>,
    pub comments: Vec<Comment>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GhComment {
    author: Option<Person>,
    body: String,
    created_at: Option<String>,
    submitted_at: Option<String>,
    state: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct GhView {
    body: String,
    files: Vec<File>,
    comments: Vec<GhComment>,
    reviews: Vec<GhComment>,
}

/// `gh pr view N --json body,files,comments,reviews`: reviews with a verdict or
/// a text read as comments, everything by date.
pub fn parse_gh_view(out: &str) -> Result<Details, String> {
    let v: GhView = serde_json::from_str(out).map_err(|e| format!("Réponse de gh illisible : {e}"))?;
    let mut comments: Vec<Comment> = v
        .comments
        .into_iter()
        .chain(v.reviews)
        .filter_map(|c| {
            let verdict = match c.state.as_str() {
                "APPROVED" => "*A approuvé.*\n\n",
                "CHANGES_REQUESTED" => "*A demandé des modifications.*\n\n",
                _ => "",
            };
            let body = format!("{verdict}{}", strip_html_comments(&c.body)).trim().to_string();
            (!body.is_empty()).then(|| Comment { author: name(c.author), body, when: c.created_at.or(c.submitted_at).unwrap_or_default() })
        })
        .collect();
    comments.sort_by(|a, b| a.when.cmp(&b.when));
    Ok(Details { body: strip_html_comments(&v.body), files: v.files, comments })
}

/// `glab mr view N -F json`: its description.
pub fn parse_glab_view(out: &str) -> Result<String, String> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Mr {
        description: Option<String>,
    }
    let mr: Mr = serde_json::from_str(out).map_err(|e| format!("Réponse de glab illisible : {e}"))?;
    Ok(strip_html_comments(&mr.description.unwrap_or_default()))
}

/// The files of a unified diff, with their added and removed lines.
pub fn parse_diff_files(diff: &str) -> Vec<File> {
    let mut files: Vec<File> = vec![];
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.split_once(" b/").map_or(rest, |(_, b)| b);
            files.push(File { path: path.to_string(), ..File::default() });
        } else if let Some(f) = files.last_mut().filter(|_| !line.starts_with("+++") && !line.starts_with("---")) {
            match line.as_bytes().first() {
                Some(b'+') => f.additions += 1,
                Some(b'-') => f.deletions += 1,
                _ => {}
            }
        }
    }
    files
}

#[derive(Clone, Copy, PartialEq)]
enum Filter {
    Open,
    Mine,
    Review,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    Plain(String),
    /// Something the user fixes in the terminal.
    Fix { title: String, hint: String, command: String, action: &'static str },
}

fn run(program: &Path, root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(root)
        // Never wait for an answer nobody can type.
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { err };
    let lines: Vec<&str> = err.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    Err(if lines.is_empty() { "erreur inconnue".into() } else { lines.join(" ") })
}

/// The host, its CLI and the open PRs (blocking: run it in the background).
fn load(root: &Path, filter: Filter) -> Result<(Host, PathBuf, Vec<Pr>), Problem> {
    let url = git(root, &["remote", "get-url", "origin"]).map_err(|_| Problem::Plain("Ce dossier n'a pas de dépôt distant « origin » : pas de pull requests à afficher.".into()))?;
    let host = host_of(&url).ok_or_else(|| Problem::Plain(format!("Le dépôt distant ({}) n'est ni sur GitHub ni sur GitLab.", without_userinfo(url.trim()))))?;
    let cli = host.cli();
    let program = find_program(cli).ok_or_else(|| Problem::Fix {
        title: format!("{cli} n'est pas installé"),
        hint: format!("Installez {cli} : brew install {cli}"),
        command: format!("brew install {cli}"),
        action: "Installer dans le terminal",
    })?;
    let mut args = match host {
        Host::GitHub => vec!["pr", "list", "--limit", "100", "--json", GH_FIELDS],
        Host::GitLab => vec!["mr", "list", "--per-page", "100", "-F", "json"],
    };
    args.extend(match (host, filter) {
        (_, Filter::Open) => vec![],
        (Host::GitHub, Filter::Mine) => vec!["--author", "@me"],
        (Host::GitHub, Filter::Review) => vec!["--search", "review-requested:@me"],
        (Host::GitLab, Filter::Mine) => vec!["--author=@me"],
        (Host::GitLab, Filter::Review) => vec!["--reviewer=@me"],
    });
    let out = run(&program, root, &args).map_err(|e| {
        if e.contains("auth login") || e.contains("401") {
            Problem::Fix {
                title: format!("Vous n'êtes pas connecté à {}", host.name()),
                hint: format!("Connectez-vous dans le terminal : {cli} auth login"),
                command: format!("{cli} auth login"),
                action: "Se connecter dans le terminal",
            }
        } else {
            Problem::Plain(e)
        }
    })?;
    let list = match host {
        Host::GitHub => parse_gh(&out),
        Host::GitLab => parse_glab(&out),
    }
    .map_err(Problem::Plain)?;
    Ok((host, program, list))
}

fn details(host: Host, program: &Path, root: &Path, number: u64) -> Result<Details, String> {
    let n = number.to_string();
    match host {
        Host::GitHub => parse_gh_view(&run(program, root, &["pr", "view", &n, "--json", "body,files,comments,reviews"])?),
        Host::GitLab => {
            let body = parse_glab_view(&run(program, root, &["mr", "view", &n, "-F", "json"])?)?;
            // shortcut: no comments for GitLab (glab's JSON for notes is not documented), files from the diff.
            let files = run(program, root, &["mr", "diff", &n, "--raw"]).map(|d| parse_diff_files(&d)).unwrap_or_default();
            Ok(Details { body, files, comments: vec![] })
        }
    }
}

/// The current branch, its last commit (subject, body) and the default base.
fn new_pr_info(root: &Path) -> Result<(String, String, String, String), String> {
    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?.trim().to_string();
    let base = git(root, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).map_or("main".to_string(), |b| b.trim().trim_start_matches("origin/").to_string());
    if branch == "HEAD" {
        return Err("Aucune branche active : créez d'abord une branche pour votre PR.".into());
    }
    if branch == base {
        return Err(format!("Vous êtes sur {base} : créez d'abord une branche pour votre PR."));
    }
    let log = git(root, &["log", "-1", "--format=%s%x1f%b"])?;
    let (subject, body) = log.split_once('\x1f').unwrap_or((&log, ""));
    Ok((branch, subject.trim().to_string(), body.trim().to_string(), base))
}

/// Pushes the branch, then creates the PR; returns its address.
fn create(host: Host, program: &Path, root: &Path, branch: &str, title: &str, body: &str, base: &str, draft: bool) -> Result<String, String> {
    // The branch named in the dialog, not whatever is checked out by now.
    git(root, &["push", "-u", "origin", branch])?;
    // `--flag=value`: a title starting with "-" is never taken for an option.
    let (title, base) = (format!("--title={title}"), format!("--base={base}"));
    let mut args: Vec<String> = match host {
        Host::GitHub => vec!["pr".into(), "create".into(), title, format!("--body={body}"), base, format!("--head={branch}")],
        Host::GitLab => vec!["mr".into(), "create".into(), title, format!("--description={body}"), base.replace("--base=", "--target-branch="), format!("--source-branch={branch}"), "--yes".into()],
    };
    if draft {
        args.push("--draft".into());
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run(program, root, &args)?;
    Ok(out.lines().rev().find(|l| l.contains("://")).unwrap_or_default().trim().to_string())
}

struct Form {
    branch: String,
    title: Entity<InputState>,
    body: Entity<InputState>,
    base: Entity<InputState>,
    draft: bool,
    _subs: Vec<Subscription>,
}

pub struct ForgeView {
    workspace: WeakEntity<Workspace>,
    root: Option<PathBuf>,
    filter: Filter,
    /// Its host and CLI, once found.
    cli: Option<(Host, PathBuf)>,
    list: Result<Vec<Pr>, Problem>,
    loaded: bool,
    loading: bool,
    selected: Option<u64>,
    details: Option<Result<Details, String>>,
    form: Option<Form>,
    /// A checkout or a creation runs.
    busy: bool,
    /// The new PR form is being prepared.
    opening: bool,
    _load: Task<()>,
    _details: Task<()>,
    _poll: Task<()>,
}

impl ForgeView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let _poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(60)).await;
                if this.update(cx, |this, cx| this.refresh(cx)).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            workspace,
            root,
            filter: Filter::Open,
            cli: None,
            list: Ok(vec![]),
            loaded: false,
            loading: false,
            selected: None,
            details: None,
            form: None,
            busy: false,
            opening: false,
            _load: Task::ready(()),
            _details: Task::ready(()),
            _poll,
        };
        this.refresh(cx);
        this
    }

    /// GitLab says "merge request" (MR), GitHub "pull request" (PR).
    fn kind(&self) -> (&'static str, &'static str) {
        match self.cli {
            Some((Host::GitLab, _)) => ("merge request", "MR"),
            _ => ("pull request", "PR"),
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            (self.list, self.loaded) = (Err(Problem::Plain("Ouvrez un dossier pour voir ses pull requests.".into())), true);
            return;
        };
        let filter = self.filter;
        self.loading = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move { load(&root, filter) });
        // Replacing the task drops (cancels) a slower load for another filter.
        self._load = cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                (this.loading, this.loaded) = (false, true);
                match result {
                    Ok((host, program, list)) => (this.cli, this.list) = (Some((host, program)), Ok(list)),
                    Err(p) => (this.cli, this.list) = (None, Err(p)),
                }
                cx.notify();
            });
        });
    }

    fn set_filter(&mut self, filter: Filter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.list = Ok(vec![]);
        self.loaded = false;
        self.refresh(cx);
    }

    fn select(&mut self, number: u64, cx: &mut Context<Self>) {
        let (Some((host, program)), Some(root)) = (self.cli.clone(), self.root.clone()) else { return };
        (self.selected, self.details, self.form) = (Some(number), None, None);
        let task = cx.background_executor().spawn(async move { details(host, &program, &root, number) });
        self._details = cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selected == Some(number) {
                    this.details = Some(result);
                    cx.notify();
                }
            });
        });
        cx.notify();
    }

    fn open_web(&self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        // The address comes from the server: only a web page is handed to `open`.
        if !(url.starts_with("https://") || url.starts_with("http://")) || Command::new("open").arg(url).spawn().is_err() {
            window.push_notification(Notification::error(format!("Impossible d'ouvrir {url}")), cx);
        }
    }

    fn checkout(&mut self, pr: &Pr, window: &mut Window, cx: &mut Context<Self>) {
        let (Some((host, program)), Some(root)) = (self.cli.clone(), self.root.clone()) else { return };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Récupérer la branche {} ?", pr.head),
            Some("Le projet passe sur cette branche : les fichiers ouverts changent. Des modifications pas encore commitées peuvent bloquer l'opération."),
            &["Récupérer", "Annuler"],
            cx,
        );
        let (number, head) = (pr.number.to_string(), pr.head.clone());
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let Ok(task) = this.update(cx, |this, cx| {
                this.busy = true;
                cx.notify();
                let sub = if host == Host::GitHub { "pr" } else { "mr" };
                cx.background_executor().spawn(async move { run(&program, &root, &[sub, "checkout", &number]) })
            }) else {
                return;
            };
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(_) => {
                        window.push_notification(Notification::success(format!("Vous êtes sur la branche {head}.")), cx);
                        if let Some(ws) = this.workspace.upgrade() {
                            ws.update(cx, |ws, cx| ws.reload_from_disk(window, cx));
                        }
                    }
                    Err(e) => window.push_notification(Notification::error(format!("Branche non récupérée : {e}")), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        self.opening = true;
        cx.notify();
        let task = cx.background_executor().spawn(async move { new_pr_info(&root) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.opening = false;
                match result {
                    Ok((branch, subject, body, base)) => {
                        let short = this.kind().1;
                        let title = cx.new(|cx| InputState::new(window, cx).default_value(subject).placeholder(format!("Titre de la {short}")));
                        let body = cx.new(|cx| InputState::new(window, cx).multi_line(true).default_value(body).placeholder("Ce qui change, et pourquoi (Markdown)"));
                        let base = cx.new(|cx| InputState::new(window, cx).default_value(base));
                        // ⌘⏎ in any field submits.
                        let _subs = [&title, &body, &base]
                            .map(|input| {
                                cx.subscribe_in(input, window, |this, _, ev: &InputEvent, window, cx| {
                                    if let InputEvent::PressEnter { secondary: true } = ev {
                                        this.submit(window, cx);
                                    }
                                })
                            })
                            .into();
                        title.update(cx, |i, cx| i.focus(window, cx));
                        (this.form, this.selected) = (Some(Form { branch, title, body, base, draft: false, _subs }), None);
                    }
                    Err(e) => window.push_notification(Notification::error(e), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(form), Some((host, program)), Some(root)) = (&self.form, self.cli.clone(), self.root.clone()) else { return };
        let title = form.title.read(cx).value().trim().to_string();
        let body = form.body.read(cx).value().to_string();
        let base = form.base.read(cx).value().trim().to_string();
        let (branch, draft) = (form.branch.clone(), form.draft);
        let (long, short) = self.kind();
        if self.busy {
            return;
        }
        if title.is_empty() || base.is_empty() {
            return window.push_notification(Notification::error("Donnez un titre et une branche de destination."), cx);
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Créer la {long} « {title} » ?"),
            Some(&format!(
                "La branche {branch} est envoyée sur {} (git push), puis la {short} est ouverte vers {base}{}. Elle sera visible par les membres du dépôt.",
                host.name(),
                if draft { " en brouillon" } else { "" }
            )),
            &["Créer", "Annuler"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let Ok(task) = this.update(cx, |this, cx| {
                this.busy = true;
                cx.notify();
                cx.background_executor().spawn(async move { create(host, &program, &root, &branch, &title, &body, &base, draft) })
            }) else {
                return;
            };
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(url) => {
                        window.push_notification(Notification::success(format!("{} créée. {url}", if short == "MR" { "Merge request" } else { "Pull request" })), cx);
                        this.form = None;
                        this.refresh(cx);
                    }
                    Err(e) => window.push_notification(Notification::error(format!("{short} non créée : {e}")), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_row(&self, pr: &Pr, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let theme = cx.theme();
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let on = self.selected == Some(pr.number);
        let checks = pr.checks.map(|c| match c {
            Checks::Success => ("Tests OK", theme.green),
            Checks::Failure => ("Tests en échec", theme.red),
            Checks::Pending => ("Tests en cours", theme.yellow),
        });
        let review = match pr.review.as_str() {
            "APPROVED" => Some(("Approuvée", theme.green)),
            "CHANGES_REQUESTED" => Some(("Modifications demandées", theme.red)),
            "REVIEW_REQUIRED" => Some(("Relecture attendue", muted)),
            _ => None,
        };
        let status = |(label, color): (&'static str, Hsla)| h_flex().gap(r(5.)).child(div().size(r(7.)).rounded_full().bg(color)).child(div().text_color(color).child(label));
        let number = pr.number;
        v_flex()
            .id(("pr", number))
            .flex_shrink_0()
            .px(r(12.))
            .py(r(9.))
            .gap(r(3.))
            .rounded(r(8.))
            .cursor_pointer()
            .when(on, |d| d.bg(theme.primary.opacity(0.14)))
            .when(!on, |d| d.hover(|d| d.bg(fg.opacity(0.05))))
            .child(
                h_flex()
                    .gap(r(8.))
                    .child(div().flex_shrink_0().text_size(r(12.5)).text_color(muted).child(format!("#{number}")))
                    .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(pr.title.clone()))
                    .when(pr.draft, |d| d.child(div().flex_shrink_0().px(r(6.)).rounded(r(4.)).border_1().border_color(muted).text_size(r(11.)).text_color(muted).child("Brouillon"))),
            )
            .child(div().truncate().text_size(r(12.)).text_color(muted).child(format!("{} · {} → {} · {}", pr.author, pr.head, pr.base, ago_fr(&pr.updated, Utc::now()))))
            .when(checks.is_some() || review.is_some(), |d| d.child(h_flex().gap(r(12.)).text_size(r(11.5)).children(checks.map(status)).children(review.map(status))))
            .on_click(cx.listener(move |this, _, _, cx| this.select(number, cx)))
    }

    fn render_form(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let (long, short) = self.kind();
        let label = |text: &'static str| div().text_size(r(12.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(text);
        v_flex()
            .id("new-pr")
            .size_full()
            // Escape in a field (it lets the key through) closes the form.
            .on_action(cx.listener(|this, _: &Escape, _, cx| {
                this.form = None;
                cx.notify();
            }))
            .overflow_y_scroll()
            .p(r(24.))
            .gap(r(10.))
            .child(div().text_size(r(15.)).font_weight(FontWeight::SEMIBOLD).child(format!("Nouvelle {long}")))
            .child(div().text_size(r(12.5)).text_color(muted).child(format!("Depuis la branche {}", form.branch)))
            .child(label("TITRE"))
            .child(Input::new(&form.title))
            .child(label("DESCRIPTION"))
            .child(div().h(r(180.)).flex_shrink_0().child(Input::new(&form.body).h_full()))
            .child(label("VERS LA BRANCHE"))
            .child(Input::new(&form.base))
            .child(Checkbox::new("pr-draft").label("Brouillon (pas encore prête à relire)").checked(form.draft).on_click(cx.listener(|this, checked: &bool, _, cx| {
                if let Some(form) = &mut this.form {
                    form.draft = *checked;
                }
                cx.notify();
            })))
            .child(
                h_flex()
                    .pt(r(6.))
                    .gap(r(8.))
                    .justify_end()
                    .child(button("pr-cancel").ghost().label("Annuler").on_click(cx.listener(|this, _, _, cx| {
                        this.form = None;
                        cx.notify();
                    })))
                    .child(button("pr-create").primary().label(format!("Créer la {short}")).loading(self.busy).disabled(self.busy).on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))),
            )
            .into_any_element()
    }

    fn render_details(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(form) = &self.form {
            return self.render_form(form, cx);
        }
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let long = self.kind().0;
        let Some(pr) = self.list.as_ref().ok().and_then(|l| l.iter().find(|p| Some(p.number) == self.selected)).cloned() else {
            return ui::empty_state((tool("forge").icon)(), theme.magenta, "Aucune sélection", &format!("Choisissez une {long} pour la lire."), cx).into_any_element();
        };
        let section = |text: String| div().flex_shrink_0().pt(r(18.)).pb(r(6.)).text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(text);
        let (url, p) = (pr.url.clone(), pr.clone());
        let top = h_flex()
            .flex_shrink_0()
            .px(r(20.))
            .py(r(12.))
            .gap(r(8.))
            .border_b_1()
            .border_color(border)
            .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(format!("#{} {}", pr.number, pr.title)))
            .child(button("pr-web").outline().small().icon(IconName::ExternalLink).label("Ouvrir").tooltip("Ouvrir dans le navigateur").on_click(cx.listener(move |this, _, window, cx| this.open_web(&url, window, cx))))
            .child(
                button("pr-checkout")
                    .outline()
                    .small()
                    .icon(Icon::empty().path("icons/git-branch.svg"))
                    .label("Récupérer la branche")
                    .loading(self.busy)
                    .disabled(self.busy)
                    .on_click(cx.listener(move |this, _, window, cx| this.checkout(&p, window, cx))),
            );
        let body = match &self.details {
            None => ui::loading("Lecture…", cx).into_any_element(),
            Some(Err(e)) => ui::error_state("Lecture impossible", e, cx).into_any_element(),
            Some(Ok(d)) => {
                let markdown = |id: (&'static str, u64), text: &str, window: &mut Window, cx: &mut Context<Self>| {
                    div().flex_shrink_0().text_size(r(14.)).child(TextView::markdown(id, text.to_string(), window, cx).selectable(true))
                };
                let mut col = v_flex().id("pr-details").size_full().overflow_y_scroll().px(r(20.)).py(r(14.));
                col = if d.body.is_empty() {
                    col.child(div().flex_shrink_0().text_color(muted).child("Pas de description."))
                } else {
                    col.child(markdown(("pr-body", pr.number), &d.body, window, cx))
                };
                col = col.child(section(format!("FICHIERS MODIFIÉS · {}", d.files.len()))).children(d.files.iter().map(|f| {
                    h_flex()
                        .flex_shrink_0()
                        .gap(r(8.))
                        .py(r(2.))
                        .text_size(r(12.5))
                        .child(div().flex_1().min_w_0().truncate().font_family(cx.theme().mono_font_family.clone()).child(f.path.clone()))
                        .child(div().flex_shrink_0().text_color(cx.theme().green).child(format!("+{}", f.additions)))
                        .child(div().flex_shrink_0().text_color(cx.theme().red).child(format!("−{}", f.deletions)))
                }));
                col = col.child(section(format!("COMMENTAIRES · {}", d.comments.len())));
                for (i, c) in d.comments.iter().enumerate() {
                    let who = div().flex_shrink_0().pt(r(8.)).text_size(r(12.)).text_color(muted).child(format!("{} · {}", c.author, ago_fr(&c.when, Utc::now())));
                    col = col.child(who).child(markdown(("pr-comment", pr.number * 1000 + i as u64), &c.body, window, cx));
                }
                col.into_any_element()
            }
        };
        v_flex().size_full().child(top).child(div().flex_1().min_h_0().child(body)).into_any_element()
    }
}

impl Render for ForgeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Another folder opened: its own pull requests.
        let root = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf));
        if root != self.root {
            (self.root, self.selected, self.details, self.form, self.cli) = (root, None, None, None, None);
            self.refresh(cx);
        }
        let host = self.cli.as_ref().map(|(h, _)| *h);
        let (kind, short) = self.kind();
        let subtitle = match (&self.list, host) {
            (Ok(l), Some(h)) => format!("{} {kind}{} · {}", l.len(), if l.len() > 1 { "s" } else { "" }, h.name()),
            _ => "Les pull requests du dépôt, sur GitHub ou GitLab.".into(),
        };
        let mut actions = vec![
            button("pr-refresh")
                .ghost()
                .small()
                .icon(Icon::empty().path("icons/refresh.svg"))
                .tooltip("Actualiser")
                .loading(self.loading)
                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
                .into_any_element(),
        ];
        if host.is_some() {
            let busy = self.busy || self.opening;
            actions.push(button("pr-new").primary().small().icon(IconName::Plus).label(format!("Créer une {short}")).loading(busy).disabled(busy).on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))).into_any_element());
        }
        let header = header(tool("forge"), subtitle, actions, cx);
        let details = self.render_details(window, cx);
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let body = match self.list.clone() {
            Err(Problem::Plain(text)) => ui::empty_state((tool("forge").icon)(), theme.magenta, &format!("Pas de {kind}s ici"), &text, cx).into_any_element(),
            Err(Problem::Fix { title, hint, command, action }) => ui::empty_state((tool("forge").icon)(), theme.magenta, &title, &hint, cx)
                .child(button("pr-fix").outline().small().icon(IconName::SquareTerminal).label(action).on_click(cx.listener(move |this, _, window, cx| in_terminal(&this.workspace, &command, window, cx))))
                .into_any_element(),
            Ok(list) => {
                let filters = h_flex().flex_shrink_0().px(r(12.)).py(r(10.)).gap(r(4.)).border_b_1().border_color(border).children(
                    [(Filter::Open, "Ouvertes"), (Filter::Mine, "Les miennes"), (Filter::Review, "À relire")].map(|(f, label)| {
                        let on = self.filter == f;
                        div()
                            .id(label)
                            .px(r(10.))
                            .py(r(4.))
                            .rounded(r(6.))
                            .cursor_pointer()
                            .text_size(r(12.5))
                            .map(|d| if on { d.bg(fg.opacity(0.08)).text_color(fg).font_weight(FontWeight::MEDIUM) } else { d.text_color(muted).hover(|d| d.text_color(fg)) })
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| this.set_filter(f, cx)))
                    }),
                );
                let empty = match () {
                    _ if !self.loaded => Some(ui::loading(&format!("Lecture des {kind}s…"), cx)),
                    _ if list.is_empty() => Some(ui::empty_state((tool("forge").icon)(), theme.magenta, &format!("Aucune {kind}"), "Rien pour ce filtre.", cx)),
                    _ => None,
                };
                let rows: Vec<_> = list.iter().map(|pr| self.render_row(pr, cx)).collect();
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .w(r(420.))
                            .h_full()
                            .flex_shrink_0()
                            .border_r_1()
                            .border_color(border)
                            .child(filters)
                            .child(match empty {
                                Some(state) => div().flex_1().min_h_0().child(state).into_any_element(),
                                None => v_flex().id("pr-list").flex_1().min_h_0().overflow_y_scroll().p(r(6.)).gap(r(2.)).children(rows).into_any_element(),
                            }),
                    )
                    .child(div().flex_1().min_w_0().h_full().child(details))
                    .into_any_element()
            }
        };
        v_flex().size_full().child(header).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_tell_the_host() {
        assert_eq!(host_of("https://github.com/cli/cli.git"), Some(Host::GitHub));
        assert_eq!(host_of("git@github.com:cli/cli.git\n"), Some(Host::GitHub));
        assert_eq!(host_of("ssh://git@ssh.github.com:443/cli/cli.git"), Some(Host::GitHub));
        assert_eq!(host_of("https://user:token@github.com/a/b"), Some(Host::GitHub));
        assert_eq!(host_of("https://gitlab.com/group/sub/proj.git"), Some(Host::GitLab));
        assert_eq!(host_of("git@gitlab.example.org:team/proj.git"), Some(Host::GitLab));
        assert_eq!(host_of("ssh://git@gitlab.corp:2222/team/proj"), Some(Host::GitLab));
        assert_eq!(host_of("https://bitbucket.org/a/github.com"), None);
    }

    #[test]
    fn tokens_in_remotes_are_not_shown() {
        assert_eq!(without_userinfo("https://oauth2:glpat-XXX@gitlab.com/a/b.git"), "https://gitlab.com/a/b.git");
        assert_eq!(without_userinfo("https://u:p@w@host.org/a@b"), "https://host.org/a@b");
        assert_eq!(without_userinfo("https://bitbucket.org/a/b"), "https://bitbucket.org/a/b");
        assert_eq!(without_userinfo("git@bitbucket.org:a/b.git"), "git@bitbucket.org:a/b.git");
    }

    #[test]
    fn gh_list_with_and_without_checks_and_review() {
        let out = r#"[
{"author":{"id":"U1","is_bot":false,"login":"BagToad","name":"Kynan"},"baseRefName":"trunk","headRefName":"bagtoad/skill","isDraft":true,"number":15052,"reviewDecision":"REVIEW_REQUIRED",
 "statusCheckRollup":[{"__typename":"CheckRun","conclusion":"SUCCESS","name":"build","status":"COMPLETED"},{"__typename":"CheckRun","conclusion":"SKIPPED","status":"COMPLETED"},{"__typename":"CheckRun","conclusion":"FAILURE","status":"COMPLETED"}],
 "title":"Triage skill","updatedAt":"2026-11-13T16:06:33Z","url":"https://github.com/cli/cli/pull/15052"},
{"author":{"login":"jane"},"baseRefName":"main","headRefName":"fix","isDraft":false,"number":7,"reviewDecision":"","statusCheckRollup":[],"title":"Fix","updatedAt":"2026-11-13T16:00:00Z","url":"u"},
{"author":null,"baseRefName":"main","headRefName":"wip","isDraft":false,"number":8,"reviewDecision":null,"statusCheckRollup":[{"__typename":"CheckRun","conclusion":null,"status":"IN_PROGRESS"},{"__typename":"StatusContext","context":"ci","state":"SUCCESS"}],"title":"Wip","updatedAt":"","url":"u"},
{"number":9,"title":"Ok","statusCheckRollup":[{"__typename":"StatusContext","state":"SUCCESS"}],"reviewDecision":"APPROVED"},
{"number":10,"title":"Red","statusCheckRollup":[{"__typename":"StatusContext","state":"ERROR"}]}
]"#;
        let list = parse_gh(out).unwrap();
        assert_eq!(list.len(), 5);
        let p = &list[0];
        assert_eq!((p.number, p.author.as_str(), p.head.as_str(), p.base.as_str(), p.draft, p.review.as_str(), p.checks), (15052, "BagToad", "bagtoad/skill", "trunk", true, "REVIEW_REQUIRED", Some(Checks::Failure)));
        assert_eq!((list[1].review.as_str(), list[1].checks), ("", None));
        assert_eq!((list[2].author.as_str(), list[2].review.as_str(), list[2].checks), ("", "", Some(Checks::Pending)));
        assert_eq!((list[3].review.as_str(), list[3].checks), ("APPROVED", Some(Checks::Success)));
        assert_eq!(list[4].checks, Some(Checks::Failure));
        assert!(parse_gh("To get started with GitHub CLI").is_err());
    }

    #[test]
    fn glab_list_reads_gitlab_merge_requests() {
        let out = r#"[{"id":101,"iid":42,"project_id":3,"title":"Ajoute la recherche","description":"x","state":"opened",
"created_at":"2026-11-10T09:00:00.000Z","updated_at":"2026-11-14T08:30:00.123Z","target_branch":"main","source_branch":"feat/search",
"author":{"id":1,"username":"marie","name":"Marie"},"draft":false,"work_in_progress":false,"detailed_merge_status":"not_approved",
"web_url":"https://gitlab.com/team/app/-/merge_requests/42"},
{"iid":43,"title":"Draft: essai","source_branch":"try","target_branch":"main","author":{"username":"paul"},"draft":true,"updated_at":"2026-11-14T08:30:00Z","web_url":"w","detailed_merge_status":"mergeable"}]"#;
        let list = parse_glab(out).unwrap();
        assert_eq!(list.len(), 2);
        let m = &list[0];
        assert_eq!((m.number, m.author.as_str(), m.head.as_str(), m.base.as_str(), m.draft, m.review.as_str(), m.checks), (42, "marie", "feat/search", "main", false, "REVIEW_REQUIRED", None));
        assert_eq!(m.url, "https://gitlab.com/team/app/-/merge_requests/42");
        assert_eq!((list[1].draft, list[1].review.as_str()), (true, ""));
        let now = DateTime::parse_from_rfc3339("2026-11-14T10:31:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(ago_fr(&m.updated, now), "il y a 2 heures");
    }

    #[test]
    fn gh_view_gives_body_files_and_comments_by_date() {
        let out = r#"{"body":"<!-- template hint -->\n### Description\n\nFixes it.","comments":[{"author":{"login":"bob"},"body":"Merci !","createdAt":"2026-11-13T12:00:00Z"}],
"files":[{"additions":59,"changeType":"MODIFIED","deletions":33,"path":"src/a.rs"}],
"reviews":[{"author":{"login":"ann"},"body":"","state":"APPROVED","submittedAt":"2026-11-13T13:00:00Z"},{"author":{"login":"bot"},"body":"<!-- x -->","state":"COMMENTED","submittedAt":"2026-11-13T11:00:00Z"},
{"author":{"login":"cy"},"body":"Un détail","state":"COMMENTED","submittedAt":"2026-11-13T10:00:00Z"}]}"#;
        let d = parse_gh_view(out).unwrap();
        assert_eq!(d.body, "### Description\n\nFixes it.");
        assert_eq!(d.files, vec![File { path: "src/a.rs".into(), additions: 59, deletions: 33 }]);
        let who: Vec<&str> = d.comments.iter().map(|c| c.author.as_str()).collect();
        assert_eq!(who, ["cy", "bob", "ann"]);
        assert_eq!(d.comments[2].body, "*A approuvé.*");
    }

    #[test]
    fn glab_view_and_diff() {
        assert_eq!(parse_glab_view(r#"{"iid":42,"description":"Le *pourquoi*.","web_url":"w"}"#).unwrap(), "Le *pourquoi*.");
        assert_eq!(parse_glab_view(r#"{"iid":42,"description":null}"#).unwrap(), "");
        let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n+more\n ctx\ndiff --git a/b.txt b/b.txt\nnew file mode 100644\n--- /dev/null\n+++ b/b.txt\n@@ -0,0 +1 @@\n+hi\n";
        assert_eq!(
            parse_diff_files(diff),
            vec![File { path: "src/a.rs".into(), additions: 2, deletions: 1 }, File { path: "b.txt".into(), additions: 1, deletions: 0 }]
        );
    }

    #[test]
    fn update_times_read_in_french() {
        let now = DateTime::parse_from_rfc3339("2026-11-14T12:00:00Z").unwrap().with_timezone(&Utc);
        let ago = |iso: &str| ago_fr(iso, now);
        assert_eq!(ago("2026-11-14T11:59:30Z"), "à l'instant");
        assert_eq!(ago("2026-11-14T11:59:00Z"), "il y a 1 minute");
        assert_eq!(ago("2026-11-14T11:15:00Z"), "il y a 45 minutes");
        assert_eq!(ago("2026-11-14T09:00:00Z"), "il y a 3 heures");
        assert_eq!(ago("2026-11-13T12:00:00Z"), "il y a 1 jour");
        assert_eq!(ago("2026-09-01T12:00:00Z"), "il y a 2 mois");
        assert_eq!(ago("2024-11-01T12:00:00Z"), "il y a 2 ans");
        assert_eq!(ago("2026-11-14T13:00:00+02:00"), "il y a 1 heure");
        assert_eq!(ago(""), "");
    }
}
