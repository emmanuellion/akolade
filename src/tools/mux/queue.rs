//! The integration queue: each project's `akolade/integration` branch, in a worktree of
//! its own, takes the sessions' branches one by one (first asked, first merged), each
//! only if the project still builds and passes its tests. Never pushed; merged into your
//! branch only by your click.

use std::path::{Path, PathBuf};

use crate::git::{self, Integration};

use super::{coord::MergeRequest, project};

pub const BRANCH: &str = "akolade/integration";

pub fn dir(base: &Path, project: &Path) -> PathBuf {
    base.join(project::key(project)).join("integration")
}

/// The project's integration worktree, made on first use from its current HEAD.
pub fn prepare(base: &Path, project: &Path) -> Result<PathBuf, String> {
    let dir = dir(base, project);
    if dir.join(".git").exists() {
        return Ok(dir);
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    git::worktree_add(project, &dir, BRANCH)?;
    crate::trust::link(&dir, project);
    Ok(dir)
}

/// Merges `req`'s branch into the integration worktree `dir`, then runs `check` there
/// (build and tests: Ok(summary) or Err(output)). Kept only if both pass; else the
/// worktree is back where it was. (merged, report for the session and the panel).
pub fn integrate(dir: &Path, req: &MergeRequest, name: &str, check: impl Fn(&Path) -> Result<String, String>) -> (bool, String) {
    let message = format!("akolade : fusion de {} ({name}) — {}", req.branch, req.summary);
    match git::merge(dir, &req.branch, &message) {
        Integration::Conflict(files) => {
            return (false, format!("Conflit avec akolade/integration sur : {}. Rien n'a été fusionné. Dans ton worktree : `git merge akolade/integration` (ou `git rebase akolade/integration`), résous les conflits, commite, puis request_merge à nouveau.", files.join(", ")));
        }
        Integration::Failed(e) => return (false, format!("La fusion n'a pas pu se faire : {e}")),
        Integration::Done => {}
    }
    match check(dir) {
        Ok(summary) => (true, format!("Fusionnée dans akolade/integration. {summary}")),
        Err(output) => {
            // Undone: the integration branch stays as it was before this merge.
            let _ = git::git(dir, &["reset", "--hard", "-q", "ORIG_HEAD"]);
            (false, format!("Build ou tests en échec après la fusion : rien n'a été gardé. Corrige dans ton worktree (en partant de akolade/integration si besoin), commite, puis request_merge à nouveau.\n\n{output}"))
        }
    }
}

/// The project's own build and tests, as the Tests tool runs them (cargo, go, pytest, jest, vitest).
pub fn project_check(dir: &Path) -> Result<String, String> {
    use crate::tools::tests::{Status, detect, execute};
    let Some(runner) = detect(dir) else {
        // shortcut: nothing to run is not a failure; a project without tests is merged unchecked (said so).
        return Ok("Aucun test détecté dans le projet : la fusion n'a pas été vérifiée.".into());
    };
    let report = execute(runner, dir, &[], &std::sync::atomic::AtomicU32::new(0));
    // Each failure with its first lines (the panic's place, then what was expected).
    let failed: Vec<String> = report.tests.iter().filter(|t| t.status == Status::Failed).map(|t| format!("- {} : {}", t.name, t.message.lines().take(4).collect::<Vec<_>>().join(" "))).collect();
    match (report.error, failed.is_empty()) {
        (Some(e), _) => Err(format!("{} :\n{e}", runner.label())),
        (None, false) => Err(format!("{} : {} en échec\n{}", runner.label(), crate::ui::plural(failed.len(), "test"), failed.join("\n"))),
        (None, true) => Ok(format!("{} : {} réussis.", runner.label(), crate::ui::plural(report.tests.len(), "test"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_join_integration_in_order_and_a_breaking_one_is_refused() {
        if !git::installed() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("akolade-mux-queue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (repo, base) = (tmp.join("projet"), tmp.join("worktrees"));
        std::fs::create_dir_all(&repo).unwrap();
        let g = |root: &Path, args: &[&str]| git::git(root, args).unwrap();
        g(&repo, &["init", "-q", "-b", "main"]);
        g(&repo, &["config", "user.email", "t@e.st"]);
        g(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("calc.txt"), "1+1=2\n").unwrap();
        g(&repo, &["add", "."]);
        g(&repo, &["commit", "-qm", "init"]);
        // The project's "tests": every line of calc.txt must be right.
        let check = |dir: &Path| {
            let text = std::fs::read_to_string(dir.join("calc.txt")).unwrap_or_default();
            match text.lines().find(|l| l.contains("=5")) {
                Some(bad) => Err(format!("faux : {bad}")),
                None => Ok(format!("{} lignes justes.", text.lines().count())),
            }
        };
        let session = |name: &str, line: &str| {
            let wt = super::super::worktree::create(&base, &repo, name).unwrap();
            std::fs::write(wt.join(format!("{name}.txt")), line).unwrap();
            g(&wt, &["add", "."]);
            g(&wt, &["-c", "user.name=T", "-c", "user.email=t@e.st", "commit", "-qm", name]);
            MergeRequest { session: name.into(), branch: super::super::worktree::branch_for(name), summary: format!("travail de {name}"), at: 0 }
        };
        let (a, b) = (session("a", "a"), session("b", "b"));
        let dir = prepare(&base, &repo).unwrap();
        assert_eq!(prepare(&base, &repo).unwrap(), dir, "made once");
        assert!(integrate(&dir, &a, "a", check).0);
        assert!(integrate(&dir, &b, "b", check).0);
        let log = g(&dir, &["log", "--format=%s", "-3"]);
        assert!(log.starts_with("akolade : fusion de akolade/b") && log.contains("akolade/a"), "in order: {log}");
        // A branch that breaks the tests: refused, the integration branch unchanged.
        let wt = super::super::worktree::create(&base, &repo, "c").unwrap();
        std::fs::write(wt.join("calc.txt"), "1+1=2\n2+2=5\n").unwrap();
        g(&wt, &["-c", "user.name=T", "-c", "user.email=t@e.st", "commit", "-qam", "c"]);
        let head = g(&dir, &["rev-parse", "HEAD"]);
        let c = MergeRequest { session: "c".into(), branch: "akolade/c".into(), summary: "calculs".into(), at: 0 };
        let (merged, report) = integrate(&dir, &c, "c", check);
        assert!(!merged && report.contains("faux : 2+2=5"), "{report}");
        assert_eq!(g(&dir, &["rev-parse", "HEAD"]), head, "nothing kept");
        // A conflict: refused too, nothing left behind.
        std::fs::write(wt.join("calc.txt"), "1+1=2\n3+3=6\n").unwrap();
        g(&wt, &["-c", "user.name=T", "-c", "user.email=t@e.st", "commit", "-qam", "c2"]);
        let wd = super::super::worktree::create(&base, &repo, "d").unwrap();
        std::fs::write(wd.join("calc.txt"), "1+1=2\n4+4=8\n").unwrap();
        g(&wd, &["-c", "user.name=T", "-c", "user.email=t@e.st", "commit", "-qam", "d"]);
        assert!(integrate(&dir, &MergeRequest { branch: "akolade/c".into(), ..c.clone() }, "c", check).0);
        let (merged, report) = integrate(&dir, &MergeRequest { session: "d".into(), branch: "akolade/d".into(), summary: "d".into(), at: 0 }, "d", check);
        assert!(!merged && report.contains("Conflit") && report.contains("calc.txt"), "{report}");
        assert!(git::status(&dir).unwrap().changes.is_empty());
        // The user's branch never moved.
        assert_eq!(g(&repo, &["log", "--format=%s", "-1"]).trim(), "init");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
