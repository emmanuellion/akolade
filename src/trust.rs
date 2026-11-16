//! The folders the user trusts. Only there may the project's own code run on its
//! own: ESLint configs, Cargo build scripts, a repository's git hooks and
//! filters, Claude Code's project hooks… A cloned or downloaded project starts
//! in restricted mode. Kept in the settings, mirrored here for the code that
//! runs without an app context (git, linters, servers).

use std::{
    path::{Path, PathBuf},
    sync::RwLock,
};

static TRUSTED: RwLock<Vec<PathBuf>> = RwLock::new(Vec::new());
/// Worktrees of the multiplexer and their project: each is trusted as its project is.
static LINKS: RwLock<Vec<(PathBuf, PathBuf)>> = RwLock::new(Vec::new());

pub fn set(folders: &[PathBuf]) {
    *TRUSTED.write().unwrap_or_else(|e| e.into_inner()) = folders.to_vec();
}

/// `path` is a trusted folder, or inside one. A path with `..` never is:
/// "/projet/../ailleurs" starts with "/projet" but leads elsewhere.
pub fn is_trusted(path: &Path) -> bool {
    if path.components().any(|c| c == std::path::Component::ParentDir) {
        return false;
    }
    if TRUSTED.read().unwrap_or_else(|e| e.into_inner()).iter().any(|t| path.starts_with(t)) {
        return true;
    }
    // A worktree of a trusted project (its own folder is elsewhere, in Akolade's).
    let project = LINKS.read().unwrap_or_else(|e| e.into_inner()).iter().find(|(wt, _)| path.starts_with(wt)).map(|(_, p)| p.clone());
    project.is_some_and(|p| TRUSTED.read().unwrap_or_else(|e| e.into_inner()).iter().any(|t| p.starts_with(t)))
}

/// `worktree` inherits the trust of `project` (given or withdrawn later, it follows).
pub fn link(worktree: &Path, project: &Path) {
    let mut links = LINKS.write().unwrap_or_else(|e| e.into_inner());
    links.retain(|(wt, _)| wt != worktree);
    links.push((worktree.to_path_buf(), project.to_path_buf()));
}

pub fn unlink(worktree: &Path) {
    LINKS.write().unwrap_or_else(|e| e.into_inner()).retain(|(wt, _)| wt != worktree);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trusted list is global: one test at a time changes it.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_trusted_folder_covers_its_subfolders_only() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let t = PathBuf::from("/akolade-trust-test/projet");
        set(&[t.clone()]);
        assert!(is_trusted(&t) && is_trusted(&t.join("src/a.rs")));
        assert!(!is_trusted(Path::new("/akolade-trust-test/projet-voisin")), "a name prefix is not a subfolder");
        assert!(!is_trusted(Path::new("/akolade-trust-test")));
        assert!(!is_trusted(&t.join("../ailleurs")), "`..` leaves the trusted folder");
    }

    #[test]
    fn a_worktree_is_trusted_as_its_project() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let (p, wt) = (PathBuf::from("/akolade-trust-link/projet"), PathBuf::from("/akolade-trust-link/worktrees/projet-1/a"));
        link(&wt, &p);
        set(&[]);
        assert!(!is_trusted(&wt.join("src")), "its project is not trusted yet");
        set(&[p.clone()]);
        assert!(is_trusted(&wt.join("src")) && !is_trusted(Path::new("/akolade-trust-link/worktrees/projet-1/b")));
        unlink(&wt);
        assert!(!is_trusted(&wt));
    }
}
