//! Which project a session belongs to, and how it works there by default.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a session writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkMode {
    /// In the project's folder itself.
    Direct,
    /// In a git worktree of its own, on the branch `session/<session>`.
    Isolated,
    /// In a copy of the project's folder (no git needed), brought back when done (see `copy`).
    Copy,
}

impl WorkMode {
    pub fn label(self) -> &'static str {
        match self {
            WorkMode::Direct => "Directe",
            WorkMode::Isolated => "Isolée",
            WorkMode::Copy => "Copie",
        }
    }
}

/// The project of a folder: the root of its git repository (so `X/api` and `X/web`
/// are one project), else the folder itself.
pub fn project_of(dir: &Path) -> PathBuf {
    crate::git::toplevel(dir).unwrap_or_else(|| dir.to_path_buf())
}

/// A short, stable folder name for a project: its name and a hash of its path (two
/// projects named "api" do not mix).
pub fn key(project: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let norm = project.to_string_lossy().replace('\\', "/").to_lowercase();
    let norm = norm.trim_end_matches('/');
    let mut h = std::collections::hash_map::DefaultHasher::new();
    norm.hash(&mut h);
    let name: String = norm.rsplit('/').next().unwrap_or_default().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    format!("{name}-{:08x}", h.finish() as u32)
}

/// A session's name as a branch and folder name: `Front React` → `front-react`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().to_lowercase().chars() {
        let c = match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            c if c.is_ascii_alphanumeric() => c,
            _ => '-',
        };
        if !(c == '-' && (out.is_empty() || out.ends_with('-'))) {
            out.push(c);
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "session".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subfolders_of_a_repository_are_one_project() {
        if !crate::git::installed() {
            return;
        }
        let root = std::env::temp_dir().join(format!("akolade-mux-project-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("api/src")).unwrap();
        std::fs::create_dir_all(root.join("web")).unwrap();
        crate::git::git(&root, &["init", "-q"]).unwrap();
        let canon = crate::platform::canonical(&root).unwrap();
        assert_eq!(crate::platform::canonical(&project_of(&root.join("api/src"))).unwrap(), canon);
        assert_eq!(crate::platform::canonical(&project_of(&root.join("web"))).unwrap(), canon);
        // No git: the folder is its own project.
        let plain = std::env::temp_dir().join(format!("akolade-mux-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_of(&plain), plain);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[test]
    fn keys_and_slugs_are_safe_folder_names() {
        assert_eq!(slug("Front React"), "front-react");
        assert_eq!(slug("  Été / API  "), "ete-api");
        assert_eq!(slug("…"), "session");
        let (a, b) = (key(Path::new("/work/x/api")), key(Path::new("/other/api")));
        assert!(a.starts_with("api-") && b.starts_with("api-") && a != b, "{a} {b}");
        assert_eq!(key(Path::new("C:\\Work\\Api")), key(Path::new("c:/work/api")), "one key whatever the separators and case");
    }
}
