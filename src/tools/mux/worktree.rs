//! The Isolated sessions' worktrees: in Akolade's folder, out of the projects, one
//! folder per project (`worktrees/<project key>/<session>`), on `session/<session>`.

use std::path::{Path, PathBuf};

use super::project;

/// Where the worktrees live.
pub fn base() -> PathBuf {
    crate::settings::app_dir().join("worktrees")
}

pub fn dir_for(base: &Path, project: &Path, slug: &str) -> PathBuf {
    base.join(project::key(project)).join(slug)
}

/// A new session's branch (0.2's were `akolade/<session>`: those sessions keep theirs).
pub fn branch_for(slug: &str) -> String {
    format!("session/{slug}")
}

/// A worktree of `project` for the session `slug`, on `branch`, trusted as its project is.
pub fn create(base: &Path, project: &Path, slug: &str, branch: &str) -> Result<PathBuf, String> {
    let dir = dir_for(base, project, slug);
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    crate::git::worktree_add(project, &dir, branch)?;
    crate::trust::link(&dir, project);
    Ok(dir)
}

/// The repository a worktree belongs to, read from its `.git` file ("gitdir: <repo>/.git/worktrees/<name>").
fn repo_of(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dir.join(".git")).ok()?;
    let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    // <repo>/.git/worktrees/<name> → <repo>
    gitdir.parent()?.parent()?.parent().map(Path::to_path_buf)
}

/// The worktree goes; what it had not committed is committed first on its branch
/// ("travail en cours"), so the branch keeps everything.
pub fn remove(dir: &Path) {
    if crate::git::status(dir).is_ok_and(|s| !s.changes.is_empty()) {
        let _ = crate::git::git(dir, &["add", "-A"]);
        let _ = crate::git::git(dir, &["-c", "user.name=Akolade", "-c", "user.email=akolade@localhost", "commit", "-q", "--no-verify", "-m", "Travail en cours de la session, gardé à sa fermeture"]);
    }
    crate::trust::unlink(dir);
    match repo_of(dir) {
        Some(repo) if crate::git::worktree_remove(&repo, dir).is_ok() => {}
        repo => {
            let _ = std::fs::remove_dir_all(dir);
            if let Some(repo) = repo {
                crate::git::worktree_prune(&repo);
            }
        }
    }
}

/// The worktree folders under `base` that no session uses (`keep`): left by a crash or a kill.
pub fn orphans(base: &Path, keep: &[PathBuf]) -> Vec<PathBuf> {
    let Ok(projects) = std::fs::read_dir(base) else { return vec![] };
    let mut out = vec![];
    for p in projects.flatten().filter(|e| e.path().is_dir()) {
        for w in std::fs::read_dir(p.path()).into_iter().flatten().flatten() {
            let dir = w.path();
            if dir.is_dir() && !keep.contains(&dir) {
                out.push(dir);
            }
        }
    }
    out.sort();
    out
}

/// Removes the orphans (at launch); empty project folders go too.
pub fn cleanup(base: &Path, keep: &[PathBuf]) -> Vec<PathBuf> {
    let gone = orphans(base, keep);
    for dir in &gone {
        remove(dir);
        if let Some(parent) = dir.parent() {
            let _ = std::fs::remove_dir(parent); // only if empty
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktrees_are_made_kept_and_cleaned_up() {
        if !crate::git::installed() {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("akolade-mux-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (repo, base) = (tmp.join("projet"), tmp.join("worktrees"));
        std::fs::create_dir_all(&repo).unwrap();
        let g = |args: &[&str]| crate::git::git(&repo, args).unwrap();
        g(&["init", "-q", "-b", "main"]);
        g(&["config", "user.email", "t@e.st"]);
        g(&["config", "user.name", "T"]);
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "init"]);

        let a = create(&base, &repo, "front", &branch_for("front")).unwrap();
        let b = create(&base, &repo, "back", &branch_for("back")).unwrap();
        assert_eq!(a, dir_for(&base, &repo, "front"));
        assert!(a.join("a.txt").exists() && repo_of(&a).is_some());
        assert!(crate::git::branches(&repo).contains(&"session/front".to_string()));
        // `a` had work not committed: kept on its branch when it goes.
        std::fs::write(a.join("nouveau.txt"), "brouillon").unwrap();
        // `b` belongs to a running session; `a` was left behind.
        assert_eq!(orphans(&base, std::slice::from_ref(&b)), vec![a.clone()]);
        assert_eq!(cleanup(&base, std::slice::from_ref(&b)), vec![a.clone()]);
        assert!(!a.exists() && b.exists());
        assert!(crate::git::branches(&repo).contains(&"session/front".to_string()), "its branch stays");
        assert_eq!(g(&["show", "session/front:nouveau.txt"]), "brouillon", "its uncommitted work is on its branch");
        // A worktree whose project is gone: its folder is deleted all the same.
        std::fs::remove_dir_all(&repo).unwrap();
        cleanup(&base, &[]);
        assert!(!b.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
