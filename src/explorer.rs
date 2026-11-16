//! Project file tree: scan, open/closed folders, rows to show.

use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

pub const MAX_ENTRIES: usize = 5000;

/// What the explorer leaves out.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanOptions {
    pub show_hidden: bool,
    /// Lowercase folder names.
    pub ignored: Vec<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { show_hidden: false, ignored: ["node_modules", "target", "dist", "build"].map(String::from).to_vec() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Path relative to the project root.
    pub rel: PathBuf,
    pub is_dir: bool,
    pub depth: usize,
}

#[derive(Default)]
pub struct Explorer {
    /// The folders read so far, in display order (a folder's content is read when it opens).
    pub entries: Vec<Entry>,
    expanded: HashSet<PathBuf>,
    /// Indices of `entries` whose folders are all open, in display order.
    /// Recomputed on change, not on every frame.
    visible: Vec<usize>,
    root: PathBuf,
    options: ScanOptions,
    /// A folder had more than `MAX_ENTRIES` entries.
    truncated: bool,
}

/// One folder's entries, not its sub-folders': folders first, then
/// case-insensitive names, hidden files and ignored folders skipped (see
/// `ScanOptions`), at most `MAX_ENTRIES`.
fn list(root: &Path, dir: &Path, depth: usize, options: &ScanOptions) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(root.join(dir)) else { return vec![] };
    let mut items: Vec<(bool, String, PathBuf)> = rd
        .flatten()
        .map(|e| {
            let p = e.path();
            (!p.is_dir(), e.file_name().to_string_lossy().to_lowercase(), p)
        })
        .filter(|(is_file, name, _)| (options.show_hidden || !name.starts_with('.')) && (*is_file || !options.ignored.contains(name)))
        .collect();
    items.sort(); // folders first, then case-insensitive name
    items.truncate(MAX_ENTRIES);
    items.into_iter().filter_map(|(is_file, _, p)| Some(Entry { rel: p.strip_prefix(root).ok()?.to_path_buf(), is_dir: !is_file, depth })).collect()
}

/// The whole tree, shallow entries first (a huge folder such as .venv cannot
/// push the project's own files out), at most `MAX_ENTRIES`. For search.
pub fn scan(root: &Path, options: &ScanOptions) -> Vec<Entry> {
    let mut out = vec![];
    let mut folders = VecDeque::from([(PathBuf::new(), 0)]);
    while let Some((dir, depth)) = folders.pop_front() {
        for e in list(root, &dir, depth, options) {
            if out.len() >= MAX_ENTRIES {
                return out;
            }
            if e.is_dir {
                folders.push_back((e.rel.clone(), depth + 1));
            }
            out.push(e);
        }
    }
    out
}

impl Explorer {
    /// Reads `root` again, keeping the open folders open.
    pub fn load(&mut self, root: &Path, options: &ScanOptions) {
        (self.root, self.options) = (root.to_path_buf(), options.clone());
        self.entries = list(root, Path::new(""), 0, options);
        self.truncated = self.entries.len() >= MAX_ENTRIES;
        // Parents first: each open folder inserts its content under itself.
        let mut open: Vec<PathBuf> = self.expanded.iter().cloned().collect();
        open.sort_by_key(|p| p.components().count());
        for rel in open {
            self.read_folder(&rel);
        }
        self.update_visible();
    }

    /// Puts a folder's content under it, once.
    fn read_folder(&mut self, rel: &Path) {
        let Some(ix) = self.entries.iter().position(|e| e.is_dir && e.rel == rel) else { return };
        let depth = self.entries[ix].depth + 1;
        if self.entries.get(ix + 1).is_some_and(|next| next.depth == depth) {
            return;
        }
        let content = list(&self.root, rel, depth, &self.options);
        self.truncated |= content.len() >= MAX_ENTRIES;
        self.entries.splice(ix + 1..ix + 1, content);
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Reads the disk again with `rel`'s folders open (a file just created, renamed, moved).
    pub fn reveal(&mut self, rel: &Path) {
        self.expanded.extend(rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()).map(Path::to_path_buf));
        self.reload();
    }

    /// Opens the folders down to `rel` (the file of the active tab), reading only
    /// those not read yet. True when something opened.
    pub fn open_ancestors(&mut self, rel: &Path) -> bool {
        let mut closed: Vec<PathBuf> = rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty() && !self.expanded.contains(*a)).map(Path::to_path_buf).collect();
        if closed.is_empty() {
            return false;
        }
        closed.reverse(); // parents first: each folder's row comes from its parent's content
        for dir in closed {
            self.expanded.insert(dir.clone());
            self.read_folder(&dir);
        }
        self.update_visible();
        true
    }

    /// Same folder, options and open folders, not read yet: to read again off the UI thread.
    pub fn fresh(&self) -> Explorer {
        Explorer { expanded: self.expanded.clone(), root: self.root.clone(), options: self.options.clone(), ..Default::default() }
    }

    /// Reads its folder again (a copy from `fresh`).
    pub fn reload(&mut self) {
        let (root, options) = (self.root.clone(), self.options.clone());
        self.load(&root, &options);
    }

    /// Takes a copy read again if the disk changed, unless a folder was opened
    /// or closed meanwhile (the copy would undo it). True when taken.
    pub fn take_if_changed(&mut self, other: Explorer) -> bool {
        if other.root != self.root || other.expanded != self.expanded || other.entries == self.entries {
            return false;
        }
        *self = other;
        true
    }

    /// Opens or closes a folder.
    pub fn toggle(&mut self, ix: usize) {
        let Some(e) = self.entries.get(ix) else { return };
        let rel = e.rel.clone();
        if !self.expanded.remove(&rel) {
            self.expanded.insert(rel.clone());
            // Its sub-folders still open from before, if a reload dropped their content.
            let mut open: Vec<PathBuf> = self.expanded.iter().filter(|p| p.starts_with(&rel)).cloned().collect();
            open.sort_by_key(|p| p.components().count());
            for p in open {
                self.read_folder(&p);
            }
        }
        self.update_visible();
    }

    pub fn is_open(&self, rel: &Path) -> bool {
        self.expanded.contains(rel)
    }

    pub fn visible(&self) -> &[usize] {
        &self.visible
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    fn update_visible(&mut self) {
        let expanded = &self.expanded;
        let shown = |e: &Entry| e.rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()).all(|a| expanded.contains(a));
        self.visible = self.entries.iter().enumerate().filter(|(_, e)| shown(e)).map(|(ix, _)| ix).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("akolade-explorer-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["src/ui", "node_modules/x", ".git", "Docs"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in ["src/ui/button.ts", "src/main.ts", "node_modules/x/i.js", ".git/HEAD", ".env", "README.md", "b.txt"] {
            fs::write(root.join(file), "").unwrap();
        }
        root
    }

    fn names(ex: &Explorer, ixs: &[usize]) -> Vec<String> {
        // With "/" on every system: the expected names read the same.
        ixs.iter().map(|&ix| ex.entries[ix].rel.to_string_lossy().replace('\\', "/")).collect()
    }

    #[test]
    fn scan_skips_hidden_and_dependency_folders_and_sorts_folders_first() {
        let root = project("scan");
        let all: Vec<String> = scan(&root, &ScanOptions::default()).iter().map(|e| e.rel.to_string_lossy().into_owned()).collect();
        assert_eq!(all, ["Docs", "src", "b.txt", "README.md", "src/ui", "src/main.ts", "src/ui/button.ts"], "level by level");
        let mut ex = Explorer::default();
        ex.load(&root, &ScanOptions::default());
        assert_eq!(names(&ex, &(0..ex.entries.len()).collect::<Vec<_>>()), ["Docs", "src", "b.txt", "README.md"], "folders read when opened");
        fs::remove_dir_all(root).unwrap();
    }

    /// The bug: shown hidden folders (.git, .venv: thousands of files) used up
    /// the whole listing, and the project's own files were missing.
    #[test]
    fn a_huge_hidden_folder_does_not_hide_the_project() {
        let root = project("huge");
        fs::create_dir_all(root.join(".venv")).unwrap();
        for i in 0..MAX_ENTRIES + 10 {
            fs::write(root.join(format!(".venv/f{i}")), "").unwrap();
        }
        let options = ScanOptions { show_hidden: true, ..Default::default() };
        let mut ex = Explorer::default();
        ex.load(&root, &options);
        let top = names(&ex, ex.visible());
        assert!(top.contains(&".venv".to_string()) && top.contains(&"src".to_string()) && top.contains(&"README.md".to_string()), "{top:?}");
        let src = ex.entries.iter().position(|e| e.rel == Path::new("src")).unwrap();
        ex.toggle(src);
        assert!(names(&ex, ex.visible()).contains(&"src/main.ts".to_string()));
        let venv = ex.entries.iter().position(|e| e.rel == Path::new(".venv")).unwrap();
        ex.toggle(venv);
        assert!(ex.truncated(), "the big folder is cut, not the project");
        assert!(scan(&root, &options).iter().any(|e| e.rel == Path::new("src")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_rows_of_open_folders_are_visible() {
        let root = project("visible");
        let mut ex = Explorer::default();
        ex.load(&root, &ScanOptions::default());
        assert_eq!(names(&ex, ex.visible()), ["Docs", "src", "b.txt", "README.md"]);
        ex.toggle(1); // open src
        assert_eq!(names(&ex, ex.visible()), ["Docs", "src", "src/ui", "src/main.ts", "b.txt", "README.md"]);
        ex.toggle(2); // open src/ui
        assert!(names(&ex, ex.visible()).contains(&"src/ui/button.ts".to_string()));
        ex.toggle(1); // closing src hides its whole subtree, even the open src/ui
        assert_eq!(names(&ex, ex.visible()), ["Docs", "src", "b.txt", "README.md"]);
        ex.load(&root, &ScanOptions::default()); // a rescan keeps open folders
        assert!(ex.is_open(Path::new("src/ui")));
        ex.toggle(1); // src again: src/ui, read before the rescan, shows its files
        assert!(names(&ex, ex.visible()).contains(&"src/ui/button.ts".to_string()), "{:?}", names(&ex, ex.visible()));
        // The active tab's file: its folders open, from a fresh, all-closed tree.
        let mut ex = Explorer::default();
        ex.load(&root, &ScanOptions::default());
        assert!(ex.open_ancestors(Path::new("src/ui/button.ts")));
        assert!(names(&ex, ex.visible()).contains(&"src/ui/button.ts".to_string()), "{:?}", names(&ex, ex.visible()));
        assert!(!ex.open_ancestors(Path::new("src/ui/button.ts")), "already open: nothing to do");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hidden_files_and_ignored_folders_follow_the_options() {
        let root = project("options");
        let mut ex = Explorer::default();
        ex.load(&root, &ScanOptions { show_hidden: true, ignored: vec!["docs".into()] });
        let all: Vec<usize> = (0..ex.entries.len()).collect();
        let names = names(&ex, &all);
        assert!(names.contains(&".env".to_string()) && names.contains(&".git".to_string()));
        assert!(names.contains(&"node_modules".to_string()), "no longer ignored");
        assert!(!names.iter().any(|n| n == "Docs"), "ignored, case-insensitive");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_reread_copy_brings_new_files_but_not_an_outdated_view() {
        let root = project("fresh");
        let mut ex = Explorer::default();
        ex.load(&root, &ScanOptions::default());
        let mut copy = ex.fresh();
        copy.reload();
        assert!(!ex.take_if_changed(copy), "nothing changed on disk");
        fs::write(root.join("new.txt"), "").unwrap();
        let mut copy = ex.fresh();
        copy.reload();
        assert!(ex.take_if_changed(copy) && names(&ex, ex.visible()).contains(&"new.txt".to_string()));
        let mut copy = ex.fresh();
        copy.reload();
        fs::write(root.join("other.txt"), "").unwrap();
        ex.toggle(1); // a folder opened while the copy was read
        assert!(!ex.take_if_changed(copy), "the copy does not know the open folder");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scan_is_capped() {
        let root = std::env::temp_dir().join(format!("akolade-explorer-{}-cap", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for i in 0..MAX_ENTRIES + 10 {
            fs::write(root.join(format!("f{i}")), "").unwrap();
        }
        let mut ex = Explorer::default();
        ex.load(&root, &ScanOptions::default());
        assert_eq!(ex.entries.len(), MAX_ENTRIES);
        assert!(ex.truncated());
        fs::remove_dir_all(root).unwrap();
    }
}
