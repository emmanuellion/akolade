//! "Copie" sessions: each works in its own copy of the project's folder (no git needed),
//! made at its start, its starting point kept beside it. Its work comes back into the
//! folder when it asks (`request_merge`): file by file against that starting point, the
//! files changed on both sides merged line by line, else settled by a Claude given both
//! sides' reasons. Then the copy starts again from the folder as it now is.

use std::{
    collections::BTreeSet,
    ffi::OsStr,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use super::project;

/// Folders neither compared nor brought back (the copy keeps its own, for its builds): git's
/// and the dependencies' anywhere…
const SKIP: [&str; 6] = [".git", "node_modules", ".next", "__pycache__", ".venv", "venv"];
/// …the builds' at the project's root only: `src/build/` is source, its changes come back.
const BUILDS: [&str; 3] = ["target", "dist", "build"];

fn skipped(name: &OsStr, at_root: bool, is_dir: bool) -> bool {
    SKIP.iter().any(|s| name == *s) || (at_root && is_dir && BUILDS.iter().any(|s| name == *s))
}

pub fn base() -> PathBuf {
    crate::settings::app_dir().join("copies")
}

pub fn dir_for(base: &Path, project: &Path, slug: &str) -> PathBuf {
    base.join(project::key(project)).join(slug)
}

/// A fresh name for a scratch folder.
fn unique() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

/// The copy's starting point: the project's files as they were when it was made (or last brought back).
fn start_of(copy: &Path) -> PathBuf {
    copy.with_extension("depart")
}

/// When the starting point was taken (seconds): what the project got since is the other side.
pub fn since(copy: &Path) -> u64 {
    std::fs::read_to_string(copy.with_extension("depuis")).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0)
}

fn mark(copy: &Path) {
    let _ = std::fs::write(copy.with_extension("depuis"), super::coord::now().to_string());
}

/// `project` copied to its session's folder (everything but `.git`), its starting point beside it.
/// shortcut: without a file system that clones (APFS, btrfs, xfs), a big node_modules or target costs time and disk; upgrade by linking them.
pub fn create(base: &Path, project: &Path, slug: &str) -> Result<PathBuf, String> {
    let dir = dir_for(base, project, slug);
    // A copy kept for its work (its session closed) is never replaced.
    if exists(&dir) && has_changes(&dir) {
        return Err(format!("{} garde du travail non réintégré : il n'a pas été remplacé.", dir.display()));
    }
    for d in [&dir, &start_of(&dir)] {
        if d.exists() {
            std::fs::remove_dir_all(d).map_err(|e| e.to_string())?;
        }
    }
    clone_tree(project, &dir, false, true).map_err(|e| format!("copie du projet impossible : {e}"))?;
    clone_tree(project, &start_of(&dir), true, true).map_err(|e| format!("copie du point de départ impossible : {e}"))?;
    mark(&dir);
    crate::trust::link(&dir, project);
    Ok(dir)
}

/// A copy still there (made again if it went).
pub fn exists(copy: &Path) -> bool {
    copy.is_dir() && start_of(copy).is_dir()
}

/// `from` into `to`: files cloned where the file system can (`fs::copy`), links as links.
fn clone_tree(from: &Path, to: &Path, skip_all: bool, at_root: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (name, kind) = (e.file_name(), e.file_type()?);
        if if skip_all { skipped(&name, at_root, kind.is_dir()) } else { name == ".git" } {
            continue;
        }
        let (src, dst) = (e.path(), to.join(&name));
        if kind.is_dir() {
            clone_tree(&src, &dst, skip_all, false)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// The files under `root` (relative, with `/`), the skipped folders left out.
fn files(root: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, rel: &str, out: &mut BTreeSet<String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let Ok(kind) = e.file_type() else { continue };
            if skipped(&e.file_name(), rel.is_empty(), kind.is_dir()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            if kind.is_dir() {
                walk(&e.path(), &path, out);
            } else {
                out.insert(path);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(root, "", &mut out);
    out
}

fn read(root: &Path, rel: &str) -> Option<Vec<u8>> {
    std::fs::read(root.join(rel)).ok()
}

/// A file (or a link) gone; nothing if it is not there.
fn delete(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if !m.is_dir() => std::fs::remove_file(path),
        _ => Ok(()),
    }
}

/// A merged or settled text into `root` (None: deleted), written whole or not at all.
fn put(root: &Path, rel: &str, content: Option<&str>) -> std::io::Result<()> {
    let path = root.join(rel);
    match content {
        Some(c) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::workspace::write_file(&path, c)
        }
        None => delete(&path),
    }
}

/// `rel` in `to` made as `from` has it: its permissions (a script stays executable), a link
/// as a link; removed if `from` has none. Copied beside it then renamed: never half a file.
fn take(to: &Path, from: &Path, rel: &str) -> std::io::Result<()> {
    let (src, dst) = (from.join(rel), to.join(rel));
    let Ok(meta) = std::fs::symlink_metadata(&src) else { return delete(&dst) };
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dst.with_file_name(format!(".akolade-{}-{}.tmp", std::process::id(), unique()));
    let made = match meta.file_type().is_symlink() {
        #[cfg(unix)]
        true => std::fs::read_link(&src).and_then(|target| std::os::unix::fs::symlink(target, &tmp)),
        _ => std::fs::copy(&src, &tmp).map(drop),
    };
    made.and_then(|_| std::fs::rename(&tmp, &dst)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The session changed something its starting point did not have.
pub fn has_changes(copy: &Path) -> bool {
    let start = start_of(copy);
    files(copy).union(&files(&start)).any(|rel| read(copy, rel) != read(&start, rel))
}

/// A file both the project and the copy changed since the starting point (None: absent there).
pub struct Conflict {
    pub path: String,
    pub start: Option<String>,
    pub project: Option<String>,
    pub copy: Option<String>,
    /// The line merge with its conflict markers, when there is one.
    pub markers: Option<String>,
}

/// What came back.
#[derive(Debug, Default, PartialEq)]
pub struct Merged {
    /// Changed in the copy only: taken as they are.
    pub taken: Vec<String>,
    /// Changed on both sides, in different places: merged line by line.
    pub merged: Vec<String>,
    /// Changed on both sides at the same places: settled by the resolver (its one-line choice).
    pub settled: Vec<(String, String)>,
    /// Not settled (binary, or the resolver failed): the project's version kept, the copy's left in the copy.
    pub left: Vec<(String, String)>,
}

impl Merged {
    pub fn files(&self) -> Vec<String> {
        self.taken.iter().chain(&self.merged).chain(self.settled.iter().map(|s| &s.0)).cloned().collect()
    }

    pub fn report(&self, project: &Path) -> String {
        let n = self.taken.len() + self.merged.len() + self.settled.len();
        let mut out = if n == 0 { "Rien à réintégrer : la copie n'a rien changé.".to_string() } else { format!("Réintégrée dans {} : {}.", project.display(), crate::ui::plural(n, "fichier")) };
        if !self.merged.is_empty() {
            out += &format!("\nFusionnés ligne à ligne (modifiés des deux côtés, à des endroits différents) : {}.", self.merged.join(", "));
        }
        for (path, why) in &self.settled {
            out += &format!("\nConflit tranché par Claude sur {path} : {why}");
        }
        for (path, why) in &self.left {
            out += &format!("\nNon réintégré, {path} : {why} Ta version reste dans ta copie, celle du projet est gardée.");
        }
        out
    }
}

/// The copy's work into `project`, then the copy (and its starting point) brought to the project's
/// state, but for what was left. `resolve` settles a conflict: the final content (None: deleted), and why.
pub fn merge(project: &Path, copy: &Path, resolve: impl Fn(&Conflict) -> Result<(Option<String>, String), String>) -> Merged {
    let start = start_of(copy);
    let mut out = Merged::default();
    let paths: BTreeSet<String> = files(copy).union(&files(&start)).cloned().collect();
    for rel in &paths {
        let (s, c, p) = (read(&start, rel), read(copy, rel), read(project, rel));
        if c == s || c == p {
            continue;
        }
        if p == s {
            match take(project, copy, rel) {
                Ok(()) => out.taken.push(rel.clone()),
                Err(e) => out.left.push((rel.clone(), format!("écriture impossible ({e})."))),
            }
            continue;
        }
        let text = |b: &Option<Vec<u8>>| match b {
            Some(b) => String::from_utf8(b.clone()).ok().map(Some),
            None => Some(None),
        };
        let (Some(st), Some(pt), Some(ct)) = (text(&s), text(&p), text(&c)) else {
            out.left.push((rel.clone(), "fichier binaire modifié des deux côtés.".into()));
            continue;
        };
        let lines = merge_lines(st.as_deref().unwrap_or(""), pt.as_deref().unwrap_or(""), ct.as_deref().unwrap_or(""));
        match lines {
            // Both kept the file: their changes are in different places.
            Some(Ok(merged)) if pt.is_some() && ct.is_some() => match put(project, rel, Some(&merged)) {
                Ok(()) => out.merged.push(rel.clone()),
                Err(e) => out.left.push((rel.clone(), format!("écriture impossible ({e})."))),
            },
            lines => {
                let conflict = Conflict { path: rel.clone(), start: st, project: pt, copy: ct, markers: lines.and_then(Result::err) };
                match resolve(&conflict) {
                    Ok((content, why)) => match put(project, rel, content.as_deref()) {
                        Ok(()) => out.settled.push((rel.clone(), why)),
                        Err(e) => out.left.push((rel.clone(), format!("écriture impossible ({e})."))),
                    },
                    Err(e) => out.left.push((rel.clone(), e)),
                }
            }
        }
    }
    // The copy starts again from the project as it now is (others' work included).
    let left: BTreeSet<&String> = out.left.iter().map(|l| &l.0).collect();
    let all: BTreeSet<String> = files(project).union(&paths).cloned().chain(files(copy)).collect();
    for rel in all.iter().filter(|r| !left.contains(r)) {
        let now = read(project, rel);
        if read(&start, rel) != now {
            let _ = take(&start, project, rel);
        }
        if read(copy, rel) != now {
            let _ = take(copy, project, rel);
        }
    }
    mark(copy);
    out
}

/// A three-way line merge (`git merge-file`): Ok(merged), Err(with conflict markers); None without git.
fn merge_lines(start: &str, project: &str, copy: &str) -> Option<Result<String, String>> {
    let dir = std::env::temp_dir().join(format!("akolade-merge-{}-{}", std::process::id(), unique()));
    std::fs::create_dir_all(&dir).ok()?;
    let write = |name: &str, text: &str| std::fs::write(dir.join(name), text).ok();
    write("projet", project)?;
    write("depart", start)?;
    write("copie", copy)?;
    let out = crate::platform::command("git").args(["merge-file", "-p", "-L", "projet", "-L", "départ", "-L", "copie", "projet", "depart", "copie"]).current_dir(&dir).output();
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    match out.status.code() {
        Some(0) => Some(Ok(text)),
        Some(n) if n > 0 => Some(Err(text)),
        _ => None,
    }
}

/// A conflict settled by Claude Code (`claude -p`, no tools, in an empty folder): given the
/// file's three versions and why each side changed it, it writes the final file.
pub fn resolve_with_claude(program: &Path, conflict: &Conflict, project: &str, session: &str, why_copy: &str, why_project: &str) -> Result<(Option<String>, String), String> {
    let show = |label: &str, text: &Option<String>| match text {
        Some(t) => format!("<<<{label}\n{t}\n{label}>>>"),
        None => format!("({label} : le fichier n'existe pas de ce côté)"),
    };
    let prompt = format!(
        "Tu règles un conflit de fusion, seul et sans poser de question, dans le fichier {path} du projet {project}. \
Depuis un même point de départ, deux versions l'ont modifié aux mêmes endroits.\n\n\
Version PROJET — le dossier du projet aujourd'hui. Pourquoi elle a changé : {why_project}\n\
Version COPIE — le travail de la session « {session} ». Pourquoi : {why_copy}\n\n\
Écris le fichier final qui garde les intentions des deux côtés. Si elles s'opposent vraiment, tranche pour ce qui sert le mieux le projet. N'utilise aucun outil.\n\n\
Réponds exactement ainsi : une ligne « Choix : » qui dit en une phrase ce que tu as gardé et pourquoi, puis le contenu complet du fichier final entre <<<FICHIER et FICHIER>>> \
(ou la seule ligne <<<SUPPRIMER>>> si le fichier doit disparaître).\n\n{}\n\n{}\n\n{}{}",
        show("DEPART", &conflict.start),
        show("PROJET", &conflict.project),
        show("COPIE", &conflict.copy),
        conflict.markers.as_ref().map_or(String::new(), |m| format!("\n\nFusion ligne à ligne, avec ses marqueurs de conflit :\n<<<MARQUEURS\n{m}\nMARQUEURS>>>")),
        path = conflict.path,
    );
    // Always the same empty folder: Claude Code keeps a folder of its own per place it runs in.
    let dir = std::env::temp_dir().join("akolade-arbitre");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut child = crate::platform::command(program)
        // Not saved: an arbiter is no conversation to come back to.
        .args(["-p", "--no-session-persistence", "--output-format", "text", "--disallowedTools", "Bash,Edit,Write,MultiEdit,NotebookEdit,WebFetch,WebSearch,Task"])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Claude Code n'a pas pu démarrer ({e})."))?;
    // Its answer read as it comes: left in the pipe (64 KB), a big file's would block it until the timeout.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut out);
            }
            out
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let _ = child.stdin.take().map(|mut i| i.write_all(prompt.as_bytes()));
    // Five minutes at most: a stuck arbiter must not hold the project's queue.
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if started.elapsed() > Duration::from_secs(300) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Claude n'a pas tranché en cinq minutes.".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let text = String::from_utf8_lossy(&stdout.join().unwrap_or_default()).into_owned();
    if !status.success() {
        let err = stderr.join().unwrap_or_default();
        return Err(format!("Claude Code a échoué : {}", String::from_utf8_lossy(&err).lines().last().unwrap_or("").trim()));
    }
    parse_answer(&text).ok_or_else(|| "la réponse de Claude n'avait pas la forme attendue.".into())
}

/// "Choix : …" then the file between its marks (or the deletion mark).
fn parse_answer(text: &str) -> Option<(Option<String>, String)> {
    let why = text.lines().find_map(|l| l.trim().strip_prefix("Choix :").or_else(|| l.trim().strip_prefix("Choix:"))).unwrap_or("").trim().to_string();
    if text.contains("<<<SUPPRIMER>>>") && !text.contains("<<<FICHIER") {
        return Some((None, why));
    }
    let start = text.find("<<<FICHIER")?;
    let body = &text[start + "<<<FICHIER".len()..];
    let body = body.strip_prefix('\n').unwrap_or(body);
    let end = body.rfind("FICHIER>>>")?;
    // One final newline, whatever blank lines Claude left before the mark.
    // shortcut: a file meant to end without a newline gets one; upgrade by following the copy's ending.
    let content = body[..end].trim_end_matches('\n');
    Some((Some(format!("{content}\n")), why))
}

/// The copies under `base` no session uses (`keep`) and without work to bring back: removed.
pub fn cleanup(base: &Path, keep: &[PathBuf]) {
    for p in std::fs::read_dir(base).into_iter().flatten().flatten() {
        for e in std::fs::read_dir(p.path()).into_iter().flatten().flatten() {
            let dir = e.path();
            if dir.is_dir() && dir.extension().is_none() && !keep.contains(&dir) && !has_changes(&dir) {
                remove(&dir);
            }
        }
    }
}

/// The copy and its starting point go.
pub fn remove(copy: &Path) {
    crate::trust::unlink(copy);
    let _ = std::fs::remove_dir_all(copy);
    let _ = std::fs::remove_dir_all(start_of(copy));
    let _ = std::fs::remove_file(copy.with_extension("depuis"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copys_work_comes_back_merged_and_conflicts_are_settled() {
        let tmp = std::env::temp_dir().join(format!("akolade-mux-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (proj, base) = (tmp.join("meteo"), tmp.join("copies"));
        std::fs::create_dir_all(proj.join("src")).unwrap();
        std::fs::create_dir_all(proj.join("node_modules/x")).unwrap();
        std::fs::write(proj.join("node_modules/x/index.js"), "dep").unwrap();
        std::fs::write(proj.join("src/a.txt"), "un\ndeux\ntrois\nquatre\ncinq\n").unwrap();
        std::fs::write(proj.join("src/b.txt"), "b\n").unwrap();
        std::fs::write(proj.join("vieux.txt"), "à supprimer\n").unwrap();
        // A source folder named like a build one, and the real build at the root.
        std::fs::create_dir_all(proj.join("src/build")).unwrap();
        std::fs::write(proj.join("src/build/gen.txt"), "a\n").unwrap();
        std::fs::create_dir_all(proj.join("build")).unwrap();
        std::fs::write(proj.join("build/out.txt"), "o\n").unwrap();
        let copy = create(&base, &proj, "api").unwrap();
        assert!(copy.join("node_modules/x/index.js").exists(), "the copy has its dependencies");
        assert!(!has_changes(&copy));
        // The session: a new file, a deletion, a change at the top of a.txt, and b.txt rewritten.
        std::fs::write(copy.join("src/c.txt"), "nouveau\n").unwrap();
        std::fs::remove_file(copy.join("vieux.txt")).unwrap();
        std::fs::write(copy.join("src/a.txt"), "UN\ndeux\ntrois\nquatre\ncinq\n").unwrap();
        std::fs::write(copy.join("src/b.txt"), "b de la copie\n").unwrap();
        std::fs::write(copy.join("src/build/gen.txt"), "b\n").unwrap();
        std::fs::write(copy.join("build/out.txt"), "sortie de la copie\n").unwrap();
        std::fs::write(copy.join("run.sh"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(copy.join("run.sh"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(has_changes(&copy));
        // Meanwhile in the project: a.txt at its bottom (merged by lines, if git is there), b.txt the same line.
        std::fs::write(proj.join("src/a.txt"), "un\ndeux\ntrois\nquatre\nCINQ\n").unwrap();
        std::fs::write(proj.join("src/b.txt"), "b du projet\n").unwrap();
        let asked = std::cell::RefCell::new(vec![]);
        let done = merge(&proj, &copy, |c| {
            asked.borrow_mut().push(c.path.clone());
            Ok((Some(format!("{}{}", c.project.clone().unwrap(), c.copy.clone().unwrap())), "les deux lignes".into()))
        });
        assert_eq!(std::fs::read_to_string(proj.join("src/c.txt")).unwrap(), "nouveau\n");
        assert!(!proj.join("vieux.txt").exists(), "deleted by the session");
        assert_eq!(std::fs::read_to_string(proj.join("src/build/gen.txt")).unwrap(), "b\n", "src/build is source");
        assert_eq!(std::fs::read_to_string(proj.join("build/out.txt")).unwrap(), "o\n", "the root's build is not brought back");
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(proj.join("run.sh")).unwrap().permissions()) & 0o111, 0o111, "still executable");
        assert_eq!(std::fs::read_to_string(proj.join("src/b.txt")).unwrap(), "b du projet\nb de la copie\n", "settled");
        assert_eq!(done.settled, vec![("src/b.txt".to_string(), "les deux lignes".to_string())]);
        if crate::git::installed() {
            assert_eq!(std::fs::read_to_string(proj.join("src/a.txt")).unwrap(), "UN\ndeux\ntrois\nquatre\nCINQ\n");
            assert_eq!(done.merged, vec!["src/a.txt".to_string()]);
            assert_eq!(*asked.borrow(), vec!["src/b.txt".to_string()]);
        }
        assert!(done.taken.contains(&"src/c.txt".to_string()) && done.taken.contains(&"vieux.txt".to_string()));
        // The copy starts again from the project: nothing left to bring back.
        assert!(!has_changes(&copy));
        assert_eq!(std::fs::read_to_string(copy.join("src/b.txt")).unwrap(), "b du projet\nb de la copie\n");
        // A failed arbiter: the project's version kept, the copy's left in the copy.
        std::fs::write(copy.join("src/b.txt"), "encore la copie\n").unwrap();
        std::fs::write(proj.join("src/b.txt"), "encore le projet\n").unwrap();
        let done = merge(&proj, &copy, |_| Err("Claude n'a pas répondu.".into()));
        assert_eq!(done.left.len(), 1);
        assert_eq!(std::fs::read_to_string(proj.join("src/b.txt")).unwrap(), "encore le projet\n");
        assert_eq!(std::fs::read_to_string(copy.join("src/b.txt")).unwrap(), "encore la copie\n");
        assert!(done.report(&proj).contains("Non réintégré, src/b.txt"));
        // Its work still there: a new session with the same id does not replace the copy.
        assert!(create(&base, &proj, "api").is_err());
        assert_eq!(std::fs::read_to_string(copy.join("src/b.txt")).unwrap(), "encore la copie\n");
        remove(&copy);
        assert!(!copy.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The real Claude Code settles a conflict (run by hand: it uses the account).
    #[test]
    #[ignore]
    fn claude_settles_a_real_conflict() {
        let program = crate::tools::claude::find_claude().expect("claude");
        let conflict = Conflict {
            path: "src/config.rs".into(),
            start: Some("pub const TIMEOUT_SECS: u64 = 30;\n".into()),
            project: Some("pub const TIMEOUT_SECS: u64 = 60;\n".into()),
            copy: Some("pub const TIMEOUT_SECS: u64 = 30;\npub const RETRIES: u32 = 3;\n".into()),
            markers: None,
        };
        let (content, why) = resolve_with_claude(&program, &conflict, "meteo", "api", "Ajout de RETRIES = 3 pour réessayer les appels réseau ; TIMEOUT_SECS inchangé.", "la session « perf » : délai porté à 60 s, les stations lentes dépassaient 30 s.").unwrap();
        let content = content.unwrap();
        eprintln!("Choix : {why}\n{content}");
        assert!(content.contains("60") && content.contains("RETRIES"), "both intentions kept: {content}");
        assert!(!why.is_empty());
    }

    /// A big file's answer (over a pipe's 64 KB) comes back at once, not after the timeout.
    #[cfg(unix)]
    #[test]
    fn a_big_answer_does_not_block_the_arbiter() {
        let dir = std::env::temp_dir().join(format!("akolade-arbitre-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("claude");
        std::fs::write(&fake, "#!/bin/sh\ncat >/dev/null\nprintf 'Choix : tout.\\n<<<FICHIER\\n'\nhead -c 300000 /dev/zero | tr '\\0' a\nprintf '\\nFICHIER>>>\\n'\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let conflict = Conflict { path: "a.txt".into(), start: None, project: Some("p\n".into()), copy: Some("c\n".into()), markers: None };
        let started = Instant::now();
        let (content, why) = resolve_with_claude(&fake, &conflict, "p", "s", "", "").unwrap();
        assert!(started.elapsed() < Duration::from_secs(30));
        assert_eq!((content.unwrap().len(), why.as_str()), (300_001, "tout."));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_arbiters_answer_is_read() {
        let (content, why) = parse_answer("Choix : les deux.\n<<<FICHIER\nligne\n\nFICHIER>>>\n").unwrap();
        assert_eq!((content.as_deref(), why.as_str()), (Some("ligne\n"), "les deux."));
        assert_eq!(parse_answer("Choix : inutile.\n<<<SUPPRIMER>>>").unwrap().0, None);
        assert!(parse_answer("je ne sais pas").is_none());
    }
}
