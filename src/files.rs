//! The explorer's file operations: new, rename, move, duplicate, trash.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// A file or folder name typed by the user. Err: why not.
pub fn check_name(name: &str) -> Result<&str, &'static str> {
    let name = name.trim();
    match name {
        "" => Err("Le nom est vide."),
        "." | ".." => Err("Ce nom est réservé."),
        _ if name.contains('/') => Err("Un nom ne peut pas contenir « / »."),
        _ => Ok(name),
    }
}

/// `dir/name`, or Err if something already has that name.
pub fn target(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let name = check_name(name)?;
    let path = dir.join(name);
    // symlink_metadata: a broken link still takes the name.
    if fs::symlink_metadata(&path).is_ok() { Err(format!("« {name} » existe déjà ici.")) } else { Ok(path) }
}

/// Renames `from` to `name` in its folder. Changing only the case ("readme.md"
/// → "README.md") is allowed: on macOS's case-insensitive disks the new name
/// already "exists", as the same file.
pub fn rename(from: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = from.parent().unwrap_or(Path::new("/"));
    let to = match target(dir, name) {
        Ok(to) => to,
        Err(e) => {
            let to = dir.join(check_name(name)?);
            if !same_file(&to, from) {
                return Err(e);
            }
            to
        }
    };
    fs::rename(from, &to).map_err(|e| e.to_string())?;
    Ok(to)
}

/// The same file under two names (only the case differs, on a case-insensitive disk).
#[cfg(unix)]
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let id = |p: &Path| fs::symlink_metadata(p).map(|m| (m.dev(), m.ino())).ok();
    id(a).is_some() && id(a) == id(b)
}

/// Windows disks ignore the case: same folder, same name but for the case.
#[cfg(windows)]
fn same_file(a: &Path, b: &Path) -> bool {
    a.parent() == b.parent() && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// Moves `from` into the folder `to_dir` (drag and drop). Err if it would go
/// into itself or replace something.
pub fn move_into(from: &Path, to_dir: &Path) -> Result<PathBuf, String> {
    if to_dir.starts_with(from) {
        return Err("Un dossier ne peut pas aller dans lui-même.".into());
    }
    if from.parent() == Some(to_dir) {
        return Ok(from.to_path_buf()); // already there
    }
    let name = from.file_name().unwrap_or_default().to_string_lossy();
    let to = target(to_dir, &name)?;
    fs::rename(from, &to).map_err(|e| e.to_string())?;
    Ok(to)
}

/// A copy of `from` (a file or a whole folder, dropped from outside) into the folder `to_dir`;
/// a name already taken there gets " copie". Err if it would go into itself.
pub fn copy_into(from: &Path, to_dir: &Path) -> Result<PathBuf, String> {
    if to_dir.starts_with(from) {
        return Err("Un dossier ne peut pas aller dans lui-même.".into());
    }
    let to = to_dir.join(from.file_name().ok_or("Ce chemin n'a pas de nom.")?);
    let to = if fs::symlink_metadata(&to).is_ok() { copy_name(&to) } else { to };
    copy_all(from, &to).map_err(|e| e.to_string())?;
    Ok(to)
}

/// "rapport.pdf" → "rapport copie.pdf", then "rapport copie 2.pdf"…
fn copy_name(path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let (stem, ext) = match (path.file_stem(), path.extension()) {
        (Some(stem), Some(ext)) if path.is_file() => (stem.to_string_lossy().into_owned(), format!(".{}", ext.to_string_lossy())),
        _ => (path.file_name().unwrap_or_default().to_string_lossy().into_owned(), String::new()),
    };
    (1..)
        .map(|n| dir.join(if n == 1 { format!("{stem} copie{ext}") } else { format!("{stem} copie {n}{ext}") }))
        .find(|p| fs::symlink_metadata(p).is_err())
        .expect("a free name")
}

fn copy_all(from: &Path, to: &Path) -> io::Result<()> {
    // A link stays a link: following one to a parent folder would copy forever.
    let kind = fs::symlink_metadata(from)?.file_type();
    if kind.is_symlink() {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
        // shortcut: Windows needs a privilege to create links; the link is left out of the copy.
        #[cfg(windows)]
        return Ok(());
    }
    // A named pipe or socket: reading it would wait forever, and it holds no content.
    if !kind.is_file() && !kind.is_dir() {
        return Ok(());
    }
    if from.is_dir() {
        fs::create_dir(to)?;
        for e in fs::read_dir(from)? {
            let e = e?;
            copy_all(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// A copy next to it ("… copie"), folders with all their content.
pub fn duplicate(path: &Path) -> io::Result<PathBuf> {
    let to = copy_name(path);
    copy_all(path, &to)?;
    Ok(to)
}

/// To the system's trash (it can be put back from there), never deleted for good.
pub fn trash(path: &Path) -> Result<(), String> {
    crate::platform::trash(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_files_and_folders_are_copied_in() {
        let (src, proj) = (dir("drop-src"), dir("drop-proj"));
        fs::write(src.join("notes.txt"), "a").unwrap();
        fs::create_dir_all(src.join("photos/2026")).unwrap();
        fs::write(src.join("photos/2026/plage.jpg"), "jpg").unwrap();
        assert_eq!(copy_into(&src.join("notes.txt"), &proj).unwrap(), proj.join("notes.txt"));
        assert_eq!(copy_into(&src.join("notes.txt"), &proj).unwrap(), proj.join("notes copie.txt"), "a taken name");
        copy_into(&src.join("photos"), &proj).unwrap();
        assert_eq!(fs::read_to_string(proj.join("photos/2026/plage.jpg")).unwrap(), "jpg");
        assert!(src.join("notes.txt").exists(), "copied, not moved");
        assert!(copy_into(&proj, &proj.join("photos")).is_err(), "not into itself");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_dir_all(&proj);
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("akolade-files-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(check_name("  notes.md "), Ok("notes.md"));
        assert!(check_name("").is_err() && check_name("a/b").is_err() && check_name("..").is_err());
        let d = dir("names");
        fs::write(d.join("a.txt"), "").unwrap();
        assert!(target(&d, "a.txt").is_err());
        assert_eq!(target(&d, "b.txt").unwrap(), d.join("b.txt"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn duplicates_get_a_free_name_and_their_content() {
        let d = dir("dup");
        fs::write(d.join("rapport.md"), "x").unwrap();
        fs::create_dir_all(d.join("src/ui")).unwrap();
        fs::write(d.join("src/ui/b.rs"), "y").unwrap();
        assert_eq!(duplicate(&d.join("rapport.md")).unwrap(), d.join("rapport copie.md"));
        assert_eq!(duplicate(&d.join("rapport.md")).unwrap(), d.join("rapport copie 2.md"));
        let copy = duplicate(&d.join("src")).unwrap();
        assert_eq!(copy, d.join("src copie"));
        assert_eq!(fs::read_to_string(copy.join("ui/b.rs")).unwrap(), "y");
        fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn duplicating_keeps_links_as_links() {
        let d = dir("links");
        fs::create_dir_all(d.join("app")).unwrap();
        fs::write(d.join("app/a.txt"), "x").unwrap();
        // A link back to its own folder: followed, the copy would never end.
        std::os::unix::fs::symlink("..", d.join("app/up")).unwrap();
        assert!(crate::platform::command("mkfifo").arg(d.join("app/pipe")).status().unwrap().success());
        let copy = duplicate(&d.join("app")).unwrap();
        assert_eq!(fs::read_link(copy.join("up")).unwrap(), Path::new(".."));
        assert!(!copy.join("pipe").exists(), "a pipe is skipped, not read forever");
        assert_eq!(fs::read_to_string(copy.join("a.txt")).unwrap(), "x");
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn renaming_checks_names_but_allows_a_case_change() {
        let d = dir("rename");
        fs::write(d.join("readme.md"), "x").unwrap();
        fs::write(d.join("other.md"), "y").unwrap();
        assert!(rename(&d.join("readme.md"), "other.md").is_err(), "would replace another file");
        assert_eq!(rename(&d.join("readme.md"), "README.md").unwrap(), d.join("README.md"));
        assert_eq!(fs::read_to_string(d.join("README.md")).unwrap(), "x");
        assert_eq!(rename(&d.join("README.md"), "notes.md").unwrap(), d.join("notes.md"));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn moving_refuses_itself_and_existing_names() {
        let d = dir("move");
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::create_dir_all(d.join("c")).unwrap();
        fs::write(d.join("f.txt"), "1").unwrap();
        fs::write(d.join("c/f.txt"), "2").unwrap();
        assert!(move_into(&d.join("a"), &d.join("a/b")).is_err(), "into itself");
        assert!(move_into(&d.join("f.txt"), &d.join("c")).is_err(), "would replace c/f.txt");
        assert_eq!(move_into(&d.join("a"), &d.join("c")).unwrap(), d.join("c/a"));
        assert!(d.join("c/a/b").is_dir());
        assert_eq!(move_into(&d.join("f.txt"), &d).unwrap(), d.join("f.txt"), "already there: nothing moves");
        fs::remove_dir_all(d).unwrap();
    }
}
