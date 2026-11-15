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

pub fn set(folders: &[PathBuf]) {
    *TRUSTED.write().unwrap_or_else(|e| e.into_inner()) = folders.to_vec();
}

/// `path` is a trusted folder, or inside one. A path with `..` never is:
/// "/projet/../ailleurs" starts with "/projet" but leads elsewhere.
pub fn is_trusted(path: &Path) -> bool {
    if path.components().any(|c| c == std::path::Component::ParentDir) {
        return false;
    }
    TRUSTED.read().unwrap_or_else(|e| e.into_inner()).iter().any(|t| path.starts_with(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trusted_folder_covers_its_subfolders_only() {
        let t = PathBuf::from("/akolade-trust-test/projet");
        set(&[t.clone()]);
        assert!(is_trusted(&t) && is_trusted(&t.join("src/a.rs")));
        assert!(!is_trusted(Path::new("/akolade-trust-test/projet-voisin")), "a name prefix is not a subfolder");
        assert!(!is_trusted(Path::new("/akolade-trust-test")));
        assert!(!is_trusted(&t.join("../ailleurs")), "`..` leaves the trusted folder");
    }
}
