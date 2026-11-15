//! Git through the `git` command: status, diff, stage, commit, branches,
//! history, push/pull. Every call blocks: run them on the background executor.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// No commit yet: nothing to diff the index against.
    pub unborn: bool,
    pub changes: Vec<Change>,
}

/// One line of `git status --porcelain`: X = index, Y = work tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub path: String,
    /// A rename's or copy's original path (unstaging the rename needs it too).
    pub orig: Option<String>,
    pub index: char,
    pub worktree: char,
}

impl Change {
    pub fn untracked(&self) -> bool {
        self.index == '?'
    }

    pub fn staged(&self) -> bool {
        !matches!(self.index, ' ' | '?' | '!')
    }

    pub fn unstaged(&self) -> bool {
        self.untracked() || !matches!(self.worktree, ' ' | '!')
    }

    /// Both sides changed it in a merge (or rebase): it needs a choice.
    pub fn conflicted(&self) -> bool {
        matches!((self.index, self.worktree), ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D'))
    }

    /// Letter shown for the staged or the work tree side.
    pub fn letter(&self, staged: bool) -> char {
        match if staged { self.index } else { self.worktree } {
            '?' => 'U',
            c => c,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub subject: String,
    pub author: String,
    pub when: String,
}

/// Runs git in `root`. Ok: its output; Err: what it said went wrong.
pub fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    exec(root, args, None, false)
}

/// After a user action: saves the stat cache that background reads (no optional
/// locks) never write, so polls stop rehashing files touched with the same content.
pub fn refresh_index(root: &Path) {
    let _ = exec(root, &["update-index", "-q", "--refresh"], None, true);
}

/// A repository's own setting that makes git run a command. A project downloaded
/// with its .git (an archive) could run anything at the next `git status`.
fn risky_key(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let (section, rest) = name.split_once('.').unwrap_or((&name, ""));
    let last = name.rsplit('.').next().unwrap_or_default();
    match section {
        "core" => matches!(rest, "fsmonitor" | "hookspath" | "sshcommand" | "pager" | "editor" | "askpass" | "gitproxy"),
        "filter" | "include" | "includeif" | "uploadpack" => true,
        "diff" | "merge" => rest == "external" || matches!(last, "textconv" | "command" | "driver"),
        "credential" => last == "helper",
        "gpg" => last == "program",
        "sequence" => rest == "editor",
        "remote" => matches!(last, "uploadpack" | "receivepack"),
        _ => false,
    }
}

/// The first such setting (or hook) of the repository holding `root`; looked
/// for once per folder (only the user's own git can change them afterwards).
fn risky(root: &Path) -> Option<String> {
    static SEEN: Mutex<BTreeMap<PathBuf, Option<String>>> = Mutex::new(BTreeMap::new());
    if let Some(found) = SEEN.lock().unwrap_or_else(|e| e.into_inner()).get(root) {
        return found.clone();
    }
    let read = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(root).env("GIT_OPTIONAL_LOCKS", "0").stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    // `git config` runs no command; `--includes`: a setting hidden in an included file counts too.
    let setting = read(&["config", "--local", "--includes", "--name-only", "--list"]).and_then(|names| names.lines().find(|n| risky_key(n)).map(str::to_string));
    // Hooks run on commit, checkout, merge… from the Git tool (the samples git creates don't).
    let hook = || {
        let dir = read(&["rev-parse", "--git-path", "hooks"])?;
        std::fs::read_dir(root.join(dir.trim())).ok()?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| !n.ends_with(".sample") && !n.starts_with('.')).map(|n| format!("hook {n}"))
    };
    let found = setting.or_else(hook);
    SEEN.lock().unwrap_or_else(|e| e.into_inner()).insert(root.to_path_buf(), found.clone());
    found
}

/// Why git does not run in `root` yet: a command its repository configures, in
/// a folder not trusted.
pub fn paused(root: &Path) -> Option<String> {
    if crate::trust::is_trusted(root) { None } else { risky(root) }
}

/// `input`: written to git's stdin. `locks`: may take index.lock for optional writes.
fn exec(root: &Path, args: &[&str], input: Option<String>, locks: bool) -> Result<String, String> {
    if let Some(what) = paused(root) {
        return Err(format!("Git est en pause : ce dépôt configure une commande ({what}). Cliquez sur « Mode restreint » en bas de la fenêtre si vous lui faites confiance."));
    }
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(root)
        // Never wait for a password or an editor nobody can see.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        // Paths are file names, not globs: "app/[id]/page.tsx" must not match "app/i/page.tsx".
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !locks {
        // Background reads (status every few seconds) must not take index.lock from the user's git.
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
    }
    let spawned = cmd.spawn().and_then(|mut child| {
        if let (Some(mut stdin), Some(input)) = (child.stdin.take(), input) {
            // From a thread: git may fill its output pipe before reading all of stdin.
            std::thread::spawn(move || std::io::Write::write_all(&mut stdin, input.as_bytes()));
        }
        child.wait_with_output()
    });
    let out = spawned.map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("git introuvable ({e}) : installez-le (xcode-select --install)."),
        _ => format!("impossible de lancer git ({e})."),
    })?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = if err.trim().is_empty() { String::from_utf8_lossy(&out.stdout) } else { err };
        // The last lines say what failed (hints come first).
        let lines: Vec<&str> = err.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("hint:")).collect();
        Err(lines[lines.len().saturating_sub(3)..].join(" "))
    }
}

#[cfg(test)]
pub fn installed() -> bool {
    Command::new("git").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// The top folder of the repository holding `root` (None: not in one).
pub fn toplevel(root: &Path) -> Option<PathBuf> {
    git(root, &["rev-parse", "--show-toplevel"]).ok().map(|o| PathBuf::from(o.trim())).filter(|p| !p.as_os_str().is_empty())
}

/// `git status --porcelain=v1 -b -z` → branch and changes.
pub fn parse_status(out: &str) -> Status {
    let mut status = Status::default();
    let mut records = out.split('\0').filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        if let Some(head) = record.strip_prefix("## ") {
            parse_branch(head, &mut status);
            continue;
        }
        let mut chars = record.chars();
        let (Some(index), Some(worktree)) = (chars.next(), chars.next()) else { continue };
        let path = record.get(3..).unwrap_or_default().to_string();
        // Renames and copies are followed by their original path.
        let orig = (matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C')).then(|| records.next().map(String::from)).flatten();
        status.changes.push(Change { path, orig, index, worktree });
    }
    status
}

fn parse_branch(head: &str, status: &mut Status) {
    if let Some(name) = head.strip_prefix("No commits yet on ").or(head.strip_prefix("Initial commit on ")) {
        status.branch = name.to_string();
        status.unborn = true;
        return;
    }
    let (names, counts) = head.split_once(" [").map_or((head, ""), |(n, c)| (n, c.trim_end_matches(']')));
    match names.split_once("...") {
        Some((branch, upstream)) => {
            status.branch = branch.to_string();
            status.upstream = Some(upstream.to_string());
        }
        None => status.branch = names.trim_end_matches(" (no branch)").to_string(),
    }
    for part in counts.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            status.ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            status.behind = n.parse().unwrap_or(0);
        }
    }
}

pub fn status(root: &Path) -> Result<Status, String> {
    git(root, &["status", "--porcelain=v1", "-b", "-z", "--untracked-files=all"]).map(|o| parse_status(&o))
}

/// What the rest of the window shows of the repository (activity bar badge,
/// explorer colors, status bar), with paths relative to the project folder.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub branch: String,
    pub ahead: u32,
    pub behind: u32,
    /// Changed files, in the whole repository.
    pub count: usize,
    /// Changed file → its letter (the work tree's, else the index's).
    pub files: HashMap<PathBuf, char>,
    /// Folders holding a changed file.
    pub dirs: HashSet<PathBuf>,
}

/// None when `root` is not in a repository.
pub fn summary(root: &Path) -> Option<Summary> {
    // Git paths start at the repository's top; the project may be one of its folders.
    let prefix = git(root, &["rev-parse", "--show-prefix"]).ok()?;
    let status = status(root).ok()?;
    let mut s = Summary { branch: status.branch, ahead: status.ahead, behind: status.behind, count: status.changes.len(), ..Default::default() };
    for c in &status.changes {
        let Ok(rel) = Path::new(&c.path).strip_prefix(prefix.trim()) else { continue };
        s.dirs.extend(rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()).map(Path::to_path_buf));
        s.files.insert(rel.to_path_buf(), if c.conflicted() { '!' } else { c.letter(!c.unstaged()) });
    }
    Some(s)
}

const FIELD: char = '\u{1f}';
const RECORD: char = '\u{1e}';

/// A file in conflict, cut into plain text and conflicts (`<<<<<<<` … `>>>>>>>`).
#[derive(Clone, Debug, PartialEq)]
pub enum Segment {
    Text(String),
    Conflict {
        /// Ours (HEAD), theirs (the merged branch), and what the markers name them.
        ours: String,
        theirs: String,
        ours_label: String,
        theirs_label: String,
        /// The block as written, markers included (kept until resolved).
        raw: String,
    },
}

/// Cuts a file in conflict into its segments (a base section `|||||||` is dropped).
pub fn parse_conflicts(text: &str) -> Vec<Segment> {
    #[derive(PartialEq)]
    enum Part {
        Text,
        Ours,
        Base,
        Theirs,
    }
    let (mut out, mut part, mut buf) = (vec![], Part::Text, String::new());
    let (mut ours, mut theirs, mut raw, mut ours_label) = (String::new(), String::new(), String::new(), String::new());
    for line in text.split_inclusive('\n') {
        // Exactly the 7 characters, then a label or the line's end: "==========" is a Markdown underline.
        let marker = |m: &str| line.strip_prefix(m).is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\n', '\r']));
        match part {
            Part::Text if marker("<<<<<<<") => {
                if !buf.is_empty() {
                    out.push(Segment::Text(std::mem::take(&mut buf)));
                }
                (part, ours_label, raw) = (Part::Ours, line[7..].trim().to_string(), line.to_string());
                ours.clear();
                theirs.clear();
            }
            Part::Text => buf.push_str(line),
            Part::Ours | Part::Base if marker("=======") => {
                raw.push_str(line);
                part = Part::Theirs;
            }
            Part::Ours if marker("|||||||") => {
                raw.push_str(line);
                part = Part::Base;
            }
            Part::Ours => {
                raw.push_str(line);
                ours.push_str(line);
            }
            Part::Base => raw.push_str(line),
            Part::Theirs if marker(">>>>>>>") => {
                raw.push_str(line);
                let theirs_label = line[7..].trim().to_string();
                out.push(Segment::Conflict { ours: std::mem::take(&mut ours), theirs: std::mem::take(&mut theirs), ours_label: std::mem::take(&mut ours_label), theirs_label, raw: std::mem::take(&mut raw) });
                part = Part::Text;
            }
            Part::Theirs => {
                raw.push_str(line);
                theirs.push_str(line);
            }
        }
    }
    // An unfinished block stays as it was written.
    buf.push_str(&raw);
    if !buf.is_empty() {
        out.push(Segment::Text(buf));
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Keep {
    Ours,
    Theirs,
    Both,
}

/// The file's text with its `index`-th conflict resolved, the others left as they are.
pub fn resolve(segments: &[Segment], index: usize, keep: Keep) -> String {
    let mut n = 0;
    segments
        .iter()
        .map(|seg| match seg {
            Segment::Text(t) => t.clone(),
            Segment::Conflict { ours, theirs, raw, .. } => {
                n += 1;
                match (n - 1 == index, keep) {
                    (false, _) => raw.clone(),
                    (true, Keep::Ours) => ours.clone(),
                    (true, Keep::Theirs) => theirs.clone(),
                    (true, Keep::Both) => format!("{ours}{theirs}"),
                }
            }
        })
        .collect()
}

/// A merge is under way (to finish with a commit, or to abort).
pub fn merging(root: &Path) -> bool {
    git(root, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_ok()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Stash {
    /// "stash@{0}".
    pub name: String,
    /// The stash's commit: its "stash@{n}" shifts when another stash comes or goes.
    pub hash: String,
    pub message: String,
    pub when: String,
}

pub fn parse_stashes(out: &str) -> Vec<Stash> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\u{1f}');
            Some(Stash { name: f.next()?.to_string(), hash: f.next()?.to_string(), message: f.next()?.to_string(), when: relative_fr(f.next()?) })
        })
        .collect()
}

pub fn stashes(root: &Path) -> Vec<Stash> {
    git(root, &["stash", "list", "--format=%gd%x1f%H%x1f%s%x1f%cr"]).map(|o| parse_stashes(&o)).unwrap_or_default()
}

/// The current "stash@{n}" of the stash with this commit, read right before acting on it.
pub fn stash_ref(root: &Path, hash: &str) -> Result<String, String> {
    let out = git(root, &["stash", "list", "--format=%gd%x1f%H"])?;
    out.lines().find_map(|l| l.split_once('\u{1f}').filter(|(_, h)| *h == hash).map(|(name, _)| name.to_string())).ok_or_else(|| "cette remise n'existe plus.".to_string())
}

/// How a line differs from the last commit (the editor's margin marks).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineChange {
    Added,
    Modified,
    /// Lines removed just above this row (an empty range).
    Deleted,
}

/// `git diff -U0` hunk headers (`@@ -12,3 +12,4 @@`) → 0-based row ranges of the new text.
pub fn parse_hunks(diff: &str) -> Vec<(std::ops::Range<usize>, LineChange)> {
    let count = |s: &str| -> (usize, usize) {
        let (start, len) = s.split_once(',').unwrap_or((s, "1"));
        (start.parse().unwrap_or(0), len.parse().unwrap_or(1))
    };
    diff.lines()
        .filter_map(|l| {
            let mut parts = l.strip_prefix("@@ ")?.split_whitespace();
            let (_, old_len) = count(parts.next()?.strip_prefix('-')?);
            let (new_start, new_len) = count(parts.next()?.strip_prefix('+')?);
            Some(match (old_len, new_len) {
                // "+12,0": lines removed after line 12, so above row 12 (0-based).
                (_, 0) => (new_start..new_start, LineChange::Deleted),
                (0, n) => (new_start - 1..new_start - 1 + n, LineChange::Added),
                (_, n) => (new_start - 1..new_start - 1 + n, LineChange::Modified),
            })
        })
        .collect()
}

/// The lines of `text` (a file's text in the editor) that differ from the last
/// commit. Empty for files Git does not follow.
pub fn line_changes(file: &Path, text: &str) -> Vec<(std::ops::Range<usize>, LineChange)> {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else { return vec![] };
    // "./name": relative to the file's folder, wherever the repository starts.
    let Ok(head) = git(dir, &["show", &format!("HEAD:./{}", name.to_string_lossy())]) else { return vec![] };
    let tmp = std::env::temp_dir().join(format!("akolade-gutter-{}-{:?}", std::process::id(), std::thread::current().id()));
    if std::fs::create_dir_all(&tmp).is_err() {
        return vec![];
    }
    let (a, b) = (tmp.join("a"), tmp.join("b"));
    let ok = std::fs::write(&a, head).and_then(|_| std::fs::write(&b, text)).is_ok();
    // `--no-index` exits 1 when the files differ: its output is what matters.
    let out = ok.then(|| Command::new("git").args(["diff", "--no-index", "--no-ext-diff", "--no-color", "-U0", "--"]).arg(&a).arg(&b).stdin(Stdio::null()).output().ok()).flatten();
    let _ = std::fs::remove_dir_all(&tmp);
    out.map(|o| parse_hunks(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default()
}

/// git's English relative dates ("3 hours ago", "2 years, 1 month ago") in French.
pub fn relative_fr(en: &str) -> String {
    let Some(body) = en.strip_suffix(" ago") else { return en.to_string() };
    let parts: Vec<String> = body
        .split(", ")
        .map(|part| {
            let (n, unit) = part.split_once(' ').unwrap_or((part, ""));
            let fr = match unit.trim_end_matches('s') {
                "second" => "seconde",
                "minute" => "minute",
                "hour" => "heure",
                "day" => "jour",
                "week" => "semaine",
                "month" => "mois",
                "year" => "an",
                _ => return part.to_string(),
            };
            let s = if n != "1" && !fr.ends_with('s') { "s" } else { "" };
            format!("{n} {fr}{s}")
        })
        .collect();
    format!("il y a {}", parts.join(" et "))
}

pub fn log(root: &Path, count: usize) -> Vec<Commit> {
    let n = format!("-n{count}");
    let Ok(out) = git(root, &["log", &n, "--pretty=format:%h%x1f%s%x1f%an%x1f%ar%x1e"]) else { return vec![] };
    out.split(RECORD)
        .filter_map(|r| {
            let mut f = r.trim_start_matches('\n').split(FIELD);
            Some(Commit { hash: f.next()?.into(), subject: f.next()?.into(), author: f.next()?.into(), when: relative_fr(f.next()?) })
        })
        .collect()
}

pub fn branches(root: &Path) -> Vec<String> {
    // Not `git branch`: a detached HEAD would be listed as "(HEAD detached at …)".
    git(root, &["for-each-ref", "--format=%(refname:short)", "refs/heads"]).map(|o| o.lines().map(String::from).collect()).unwrap_or_default()
}

/// Unified diff of one file: what is staged, or what is not yet. An
/// untracked file is shown whole, as added.
pub fn diff(root: &Path, change: &Change, staged: bool) -> String {
    if change.untracked() {
        let text = std::fs::read(root.join(&change.path)).map(|b| String::from_utf8_lossy(&b[..b.len().min(1 << 20)]).into_owned()).unwrap_or_default();
        let lines: Vec<String> = text.lines().map(|l| format!("+{l}")).collect();
        return format!("@@ -0,0 +1,{} @@\n{}", lines.len(), lines.join("\n"));
    }
    let args: &[&str] = if staged { &["diff", "--cached", "--no-ext-diff", "--no-color", "--"] } else { &["diff", "--no-ext-diff", "--no-color", "--"] };
    git(root, &[args, &[change.path.as_str()]].concat()).unwrap_or_else(|e| e)
}

pub fn show(root: &Path, hash: &str) -> String {
    git(root, &["show", "--no-ext-diff", "--no-color", "--format=%H%n%an <%ae>%n%ad%n%n%B", hash]).unwrap_or_else(|e| e)
}

/// Past this many paths they go through stdin: a big merge's list passes the
/// command line's limit ("Argument list too long").
const MAX_ARG_PATHS: usize = 1000;

/// `args` then `paths`, on the command line or (many) through stdin.
fn with_paths(root: &Path, args: &[&str], paths: &[&str]) -> Result<String, String> {
    if paths.len() <= MAX_ARG_PATHS {
        return git(root, &[args, &["--"], paths].concat());
    }
    exec(root, &[args, &["--pathspec-from-file=-", "--pathspec-file-nul"]].concat(), Some(paths.join("\0")), false)
}

pub fn stage(root: &Path, paths: &[&str]) -> Result<String, String> {
    with_paths(root, &["add", "-A"], paths)
}

pub fn unstage(root: &Path, paths: &[&str], unborn: bool) -> Result<String, String> {
    // Before the first commit there is no HEAD to restore from.
    if unborn { with_paths(root, &["rm", "-r", "-q", "--cached"], paths) } else { with_paths(root, &["restore", "--staged"], paths) }
}

/// Throws away the work tree changes of tracked files.
pub fn discard(root: &Path, paths: &[&str]) -> Result<String, String> {
    with_paths(root, &["restore"], paths)
}

/// Ends a conflict with no readable blocks (a binary file) by keeping one side whole.
pub fn take_side(root: &Path, path: &str, ours: bool) -> Result<String, String> {
    git(root, &["checkout", if ours { "--ours" } else { "--theirs" }, "--", path])?;
    stage(root, &[path])
}

pub fn commit(root: &Path, message: &str) -> Result<String, String> {
    git(root, &["commit", "-q", "-m", message])
}

/// One line of a diff, ready to show.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// `@@ … @@` and the commit header of `git show`.
    Info,
}

/// Unified diff → lines with their old and new numbers (file headers dropped).
pub fn parse_diff(diff: &str) -> Vec<DiffLine> {
    let (mut old, mut new, mut in_hunk) = (0, 0, false);
    let mut out = vec![];
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@ context
            let mut nums = rest.split(' ').take(2).map(|r| r.get(1..).unwrap_or_default().split(',').next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(1));
            (old, new, in_hunk) = (nums.next().unwrap_or(1), nums.next().unwrap_or(1), true);
            out.push(DiffLine { kind: LineKind::Info, old: None, new: None, text: line.to_string() });
        } else if line.starts_with("diff --git") {
            in_hunk = false;
        } else if !in_hunk {
            // `git show` header (hash, author, message) is worth reading; file headers are not.
            if !["index ", "--- ", "+++ ", "new file", "deleted file", "similarity", "rename ", "old mode", "new mode"].iter().any(|p| line.starts_with(p)) {
                out.push(DiffLine { kind: LineKind::Info, old: None, new: None, text: line.to_string() });
            }
        } else if let Some(text) = line.strip_prefix('+') {
            out.push(DiffLine { kind: LineKind::Added, old: None, new: Some(new), text: text.into() });
            new += 1;
        } else if let Some(text) = line.strip_prefix('-') {
            out.push(DiffLine { kind: LineKind::Removed, old: Some(old), new: None, text: text.into() });
            old += 1;
        } else if !line.starts_with('\\') {
            out.push(DiffLine { kind: LineKind::Context, old: Some(old), new: Some(new), text: line.strip_prefix(' ').unwrap_or(line).into() });
            (old, new) = (old + 1, new + 1);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reads_branch_counts_and_changes() {
        let s = parse_status("## main...origin/main [ahead 2, behind 1]\0M  src/a.rs\0 M b.txt\0R  new.rs\0old.rs\0?? notes/todo.md\0AM c.rs\0");
        assert_eq!((s.branch.as_str(), s.upstream.as_deref(), s.ahead, s.behind, s.unborn), ("main", Some("origin/main"), 2, 1, false));
        let paths: Vec<&str> = s.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["src/a.rs", "b.txt", "new.rs", "notes/todo.md", "c.rs"], "the rename's old path is not a change");
        assert_eq!((s.changes[2].orig.as_deref(), s.changes[0].orig.as_deref()), (Some("old.rs"), None), "but it is kept");
        let c = &s.changes[4];
        assert!(c.staged() && c.unstaged(), "added then modified: on both sides");
        assert!(s.changes[3].untracked() && !s.changes[3].staged() && s.changes[3].letter(false) == 'U');
        assert!(!s.changes[1].staged() && s.changes[1].unstaged());

        let fresh = parse_status("## No commits yet on main\0?? a\0");
        assert!(fresh.unborn && fresh.branch == "main");
        assert_eq!(parse_status("## feature\0").upstream, None);
        assert_eq!(parse_status("## HEAD (no branch)\0").branch, "HEAD");
    }

    #[test]
    fn diff_lines_get_their_numbers() {
        let d = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -3,3 +3,4 @@ fn main\n keep\n-old\n+new\n+more\n end\n\\ No newline at end of file\n";
        let lines = parse_diff(d);
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        use LineKind::*;
        assert_eq!(kinds, [Info, Context, Removed, Added, Added, Context]);
        assert_eq!((lines[1].old, lines[1].new), (Some(3), Some(3)));
        assert_eq!((lines[2].old, lines[3].new, lines[4].new), (Some(4), Some(4), Some(5)));
        assert_eq!((lines[5].old, lines[5].new, lines[5].text.as_str()), (Some(5), Some(6), "end"));
        // A commit message line shown by `git show` can look like a hunk header.
        assert_eq!(parse_diff("@@  x\n@@ é\n").len(), 2, "no panic");
    }

    #[test]
    fn conflicts_are_cut_and_resolved_one_at_a_time() {
        let text = "a\n<<<<<<< HEAD\nmine\n||||||| base\nold\n=======\ntheirs\n>>>>>>> feature\nb\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> feature\n";
        let segs = parse_conflicts(text);
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0], Segment::Text("a\n".into()));
        let Segment::Conflict { ours, theirs, ours_label, theirs_label, .. } = &segs[1] else { panic!() };
        assert_eq!((ours.as_str(), theirs.as_str(), ours_label.as_str(), theirs_label.as_str()), ("mine\n", "theirs\n", "HEAD", "feature"));
        let once = resolve(&segs, 0, Keep::Theirs);
        assert_eq!(once, "a\ntheirs\nb\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> feature\n", "the other conflict is untouched");
        assert_eq!(resolve(&parse_conflicts(&once), 0, Keep::Both), "a\ntheirs\nb\nx\ny\n");
        assert_eq!(parse_conflicts("plain\n"), vec![Segment::Text("plain\n".into())]);
        assert_eq!(resolve(&parse_conflicts("<<<<<<< HEAD\nhalf\n"), 0, Keep::Ours), "<<<<<<< HEAD\nhalf\n", "unfinished: kept");
        let md = "<<<<<<< HEAD\nTitre\n==========\n=======\nAutre\n>>>>>>> feature\n";
        let Segment::Conflict { ours, theirs, .. } = &parse_conflicts(md)[0] else { panic!() };
        assert_eq!((ours.as_str(), theirs.as_str()), ("Titre\n==========\n", "Autre\n"), "an underline is not the separator");
    }

    #[test]
    fn stash_list_lines_become_stashes() {
        let out = "stash@{0}\u{1f}aaa\u{1f}On main: essai\u{1f}2 hours ago\nstash@{1}\u{1f}bbb\u{1f}WIP on main: 629efd6 x\u{1f}3 days ago\n";
        let list = parse_stashes(out);
        assert_eq!((list.len(), list[0].name.as_str(), list[1].hash.as_str(), list[0].message.as_str(), list[1].when.as_str()), (2, "stash@{0}", "bbb", "On main: essai", "il y a 3 jours"));
    }

    #[test]
    fn hunks_become_marked_rows() {
        let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -3,0 +4,2 @@\n+a\n+b\n@@ -10 +12 @@\n-x\n+y\n@@ -20,3 +21,0 @@\n-z\n";
        assert_eq!(parse_hunks(diff), vec![(3..5, LineChange::Added), (11..12, LineChange::Modified), (21..21, LineChange::Deleted)]);
    }

    #[test]
    fn relative_dates_read_in_french() {
        assert_eq!(relative_fr("9 hours ago"), "il y a 9 heures");
        assert_eq!(relative_fr("1 minute ago"), "il y a 1 minute");
        assert_eq!(relative_fr("2 years, 1 month ago"), "il y a 2 ans et 1 mois");
        assert_eq!(relative_fr("in the future"), "in the future");
    }

    #[test]
    fn settings_that_run_commands_are_spotted() {
        for key in ["core.fsmonitor", "core.hooksPath", "filter.lfs.clean", "diff.pdf.textconv", "diff.external", "include.path", "includeIf.gitdir:~/.path", "credential.helper", "gpg.ssh.program", "remote.origin.uploadpack"] {
            assert!(risky_key(key), "{key}");
        }
        for key in ["core.bare", "core.filemode", "remote.origin.url", "branch.main.merge", "user.email", "diff.renames"] {
            assert!(!risky_key(key), "{key}");
        }
    }

    /// A downloaded repository whose .git/config runs a command: git waits for trust.
    #[test]
    fn git_pauses_in_a_repository_that_runs_commands() {
        if !installed() {
            return;
        }
        let root = std::env::temp_dir().join(format!("akolade-git-risky-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let run = |args: &[&str]| Command::new("git").args(args).current_dir(&root).output().unwrap();
        run(&["init", "-q"]);
        run(&["config", "core.fsmonitor", "touch pwned"]);
        let err = git(&root, &["status"]).unwrap_err();
        assert!(err.contains("core.fsmonitor"), "{err}");
        assert!(!root.join("pwned").exists(), "the command must not have run");
        // A plain repository works as before.
        let plain = root.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        Command::new("git").args(["init", "-q"]).current_dir(&plain).output().unwrap();
        assert!(git(&plain, &["status"]).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// End to end on a real repository (skipped without git).
    #[test]
    fn stage_commit_and_branch_in_a_real_repository() {
        if !installed() {
            return;
        }
        let root = std::env::temp_dir().join(format!("akolade-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let g = |args: &[&str]| git(&root, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["config", "user.name", "Test"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        assert!(toplevel(&root).is_some());

        let s = status(&root).unwrap();
        assert!(s.unborn && s.changes[0].untracked());
        stage(&root, &["a.txt"]).unwrap();
        unstage(&root, &["a.txt"], true).unwrap();
        assert!(status(&root).unwrap().changes[0].untracked(), "unstaged before the first commit");
        stage(&root, &["a.txt"]).unwrap();
        commit(&root, "First").unwrap();

        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
        let s = status(&root).unwrap();
        assert!(!s.unborn && s.branch == "main" && s.changes[0].unstaged());
        let lines = parse_diff(&diff(&root, &s.changes[0], false));
        assert!(lines.iter().any(|l| l.kind == LineKind::Added && l.text == "two" && l.new == Some(2)), "{lines:?}");
        // An external diff tool (difftastic) would replace the patch we read.
        g(&["config", "diff.external", "true"]);
        assert!(parse_diff(&diff(&root, &s.changes[0], false)).iter().any(|l| l.text == "two"), "external diff ignored");
        // A repository that runs a command (here: as its diff) pauses git in folders not trusted yet.
        g(&["config", "--unset", "diff.external"]);
        discard(&root, &["a.txt"]).unwrap();
        assert!(status(&root).unwrap().changes.is_empty());

        g(&["switch", "-q", "-c", "feature"]);
        assert_eq!(branches(&root), ["feature", "main"]);
        let log = log(&root, 10);
        assert_eq!((log.len(), log[0].subject.as_str(), log[0].author.as_str()), (1, "First", "Test"));
        assert!(commit(&root, "nothing staged").is_err());

        // Seen from a sub-folder opened as the project: paths start there.
        std::fs::create_dir_all(root.join("web/src")).unwrap();
        std::fs::write(root.join("web/src/app.js"), "x").unwrap();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        let sum = summary(&root.join("web")).unwrap();
        assert_eq!((sum.branch.as_str(), sum.count), ("feature", 2));
        assert_eq!(sum.files.get(Path::new("src/app.js")), Some(&'U'));
        assert!(sum.dirs.contains(Path::new("src")) && sum.files.len() == 1, "a.txt is outside the project");
        assert_eq!(summary(&root).unwrap().files.get(Path::new("a.txt")), Some(&'M'));
        assert!(summary(&std::env::temp_dir()).is_none());
        // The Git tab works from the top: a sub-folder's file stages by its repository path.
        let top = toplevel(&root.join("web")).unwrap();
        assert_eq!(top.canonicalize().unwrap(), root.canonicalize().unwrap());
        assert!(git(&root.join("web"), &["add", "-A", "--", "web/src/app.js"]).is_err(), "the bug: from the sub-folder the path does not match");
        stage(&top, &["web/src/app.js"]).unwrap();
        assert!(status(&top).unwrap().changes.iter().any(|c| c.path == "web/src/app.js" && c.staged()));
        assert!(toplevel(&std::env::temp_dir()).is_none());
        // The editor's view of a file: margin marks, from its own text.
        std::fs::write(root.join("b.txt"), "one\ntwo\nthree\n").unwrap();
        g(&["add", "b.txt"]);
        commit(&root, "b").unwrap();
        let edited = "one\nTWO\nthree\nfour\n";
        assert_eq!(line_changes(&root.join("b.txt"), edited), vec![(1..2, LineChange::Modified), (3..4, LineChange::Added)]);
        assert!(line_changes(&root.join("web/src/app.js"), "x").is_empty(), "untracked: no marks");

        // Paths are literal: "a[bc].txt" is not a glob matching "ab.txt".
        std::fs::write(root.join("a[bc].txt"), "1\n").unwrap();
        std::fs::write(root.join("ab.txt"), "1\n").unwrap();
        stage(&root, &["a[bc].txt", "ab.txt"]).unwrap();
        commit(&root, "globs").unwrap();
        std::fs::write(root.join("a[bc].txt"), "2\n").unwrap();
        std::fs::write(root.join("ab.txt"), "2\n").unwrap();
        discard(&root, &["a[bc].txt"]).unwrap();
        let left: Vec<String> = status(&root).unwrap().changes.into_iter().map(|c| c.path).collect();
        assert!(left.contains(&"ab.txt".to_string()) && !left.contains(&"a[bc].txt".to_string()), "{left:?}");

        // A stash is found by its commit, whatever its number is now.
        std::fs::write(root.join("ab.txt"), "s1\n").unwrap();
        g(&["stash", "push", "-m", "one"]);
        let first = stashes(&root)[0].hash.clone();
        std::fs::write(root.join("ab.txt"), "s2\n").unwrap();
        g(&["stash", "push", "-m", "two"]);
        assert_eq!(stash_ref(&root, &first).unwrap(), "stash@{1}");
        assert!(stash_ref(&root, "nope").is_err());
        g(&["stash", "clear"]);

        // Detached HEAD: not listed as a branch.
        g(&["switch", "-q", "--detach"]);
        assert_eq!(branches(&root), ["feature", "main"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Renames, binary conflicts, an empty merge and long path lists (skipped without git).
    #[test]
    fn renames_merges_and_many_paths_in_a_real_repository() {
        if !installed() {
            return;
        }
        let root = std::env::temp_dir().join(format!("akolade-git-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("dossier é")).unwrap();
        let g = |args: &[&str]| git(&root, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "test@example.com"]);
        g(&["config", "user.name", "Test"]);

        // A staged rename unstages whole only with its original path too.
        std::fs::write(root.join("dossier é/fichier un.txt"), "texte\n").unwrap();
        stage(&root, &["dossier é/fichier un.txt"]).unwrap();
        commit(&root, "un").unwrap();
        g(&["mv", "dossier é/fichier un.txt", "dossier é/fichier deux.txt"]);
        let s = status(&root).unwrap();
        let c = &s.changes[0];
        assert_eq!((c.index, c.path.as_str(), c.orig.as_deref()), ('R', "dossier é/fichier deux.txt", Some("dossier é/fichier un.txt")));
        unstage(&root, &[&c.path, c.orig.as_deref().unwrap()], false).unwrap();
        assert!(!status(&root).unwrap().changes.iter().any(Change::staged), "nothing left staged");
        std::fs::rename(root.join("dossier é/fichier deux.txt"), root.join("dossier é/fichier un.txt")).unwrap();

        // More paths than a command line takes go through stdin.
        let many: Vec<String> = (0..=MAX_ARG_PATHS).map(|i| format!("dossier é/f {i}.txt")).collect();
        for p in &many {
            std::fs::write(root.join(p), "x").unwrap();
        }
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        stage(&root, &refs).unwrap();
        assert_eq!(status(&root).unwrap().changes.iter().filter(|c| c.index == 'A').count(), many.len());
        unstage(&root, &refs, false).unwrap();
        assert!(status(&root).unwrap().changes.iter().all(Change::untracked));
        for p in &many {
            std::fs::remove_file(root.join(p)).unwrap();
        }

        // A binary conflict: no blocks, one side is kept whole.
        let merge = |content: [&[u8]; 3]| {
            std::fs::write(root.join("img.bin"), content[0]).unwrap();
            stage(&root, &["img.bin"]).unwrap();
            commit(&root, "base").unwrap();
            g(&["switch", "-q", "-c", "feature"]);
            std::fs::write(root.join("img.bin"), content[1]).unwrap();
            stage(&root, &["img.bin"]).unwrap();
            commit(&root, "theirs").unwrap();
            g(&["switch", "-q", "main"]);
            std::fs::write(root.join("img.bin"), content[2]).unwrap();
            stage(&root, &["img.bin"]).unwrap();
            commit(&root, "ours").unwrap();
            assert!(git(&root, &["merge", "--no-edit", "feature"]).is_err(), "conflict");
            assert!(merging(&root) && status(&root).unwrap().changes.iter().any(|c| c.path == "img.bin" && (c.index, c.worktree) == ('U', 'U')));
        };
        merge([b"\0base", b"\0theirs", b"\0ours"]);
        assert!(std::fs::read_to_string(root.join("img.bin")).unwrap().contains('\0'), "binary: read as not text");
        take_side(&root, "img.bin", false).unwrap();
        assert_eq!(std::fs::read(root.join("img.bin")).unwrap(), b"\0theirs");
        assert!(!status(&root).unwrap().changes.iter().any(Change::conflicted));
        commit(&root, "merged").unwrap();
        g(&["branch", "-q", "-D", "feature"]);

        // Every conflict kept "ours": nothing changes, but the merge still commits.
        merge([b"\0base2", b"\0theirs2", b"\0ours2"]);
        take_side(&root, "img.bin", true).unwrap();
        assert!(status(&root).unwrap().changes.is_empty() && merging(&root));
        commit(&root, "merged, ours").unwrap();
        assert!(!merging(&root), "merge concluded");
        refresh_index(&root);
        std::fs::remove_dir_all(root).unwrap();
    }
}
