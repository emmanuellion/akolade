//! What differs between macOS, Linux and Windows, in one place: starting a
//! program, opening or showing a file, stopping a process tree, the shell, the
//! trash, the home folder.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Command,
};

/// A program to run. On Windows, a GUI app starting a console program flashes a
/// console window unless told not to (git runs every few seconds).
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Its own process group (Unix), so `kill_tree` also stops what it starts
/// (a dev server from a test, Claude's commands). Windows: `taskkill /T` finds the tree.
pub fn own_group(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(cmd, 0);
    cmd
}

/// Stops a process and everything it started (a group made by `own_group`).
pub fn kill_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    let _ = command("kill").args(["-TERM", "--", &format!("-{pid}")]).status();
    #[cfg(windows)]
    let _ = command("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).status();
}

/// Stops one process; true if it was.
pub fn kill(pid: u32) -> bool {
    #[cfg(unix)]
    let status = command("kill").arg(pid.to_string()).status();
    #[cfg(windows)]
    let status = command("taskkill").args(["/PID", &pid.to_string(), "/F"]).status();
    status.is_ok_and(|s| s.success())
}

/// Opens a file, a folder or a URL with the system's default app; false if it could not start.
pub fn open(target: impl AsRef<OsStr>) -> bool {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "linux")]
    let opener = "xdg-open";
    #[cfg(windows)]
    let opener = "explorer";
    command(opener).arg(target).spawn().is_ok()
}

/// The file manager's name, for menus: "Afficher dans le Finder".
pub const FILE_MANAGER: &str = if cfg!(target_os = "macos") {
    "le Finder"
} else if cfg!(windows) {
    "l'Explorateur"
} else {
    "le gestionnaire de fichiers"
};

/// The menu entry of `reveal`.
pub const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Afficher dans le Finder"
} else if cfg!(windows) {
    "Afficher dans l'Explorateur"
} else {
    "Afficher dans le gestionnaire de fichiers"
};

/// Shows `path` in the file manager, selected where the system can.
pub fn reveal(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = command("open").arg("-R").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = open(path.parent().unwrap_or(path));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Explorer reads its own quoting: `/select,"C:\a b\c.txt"` as one raw argument.
        let _ = command("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
    }
}

/// `canonicalize`, without the `\\?\` prefix Windows adds (shown everywhere, and some
/// programs can't read it).
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let p = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    if let Some(s) = p.to_str().and_then(|s| s.strip_prefix(r"\\?\")).filter(|s| !s.starts_with("UNC")) {
        return Ok(PathBuf::from(s));
    }
    Ok(p)
}

/// The home folder (Windows has no HOME).
pub fn home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| "/".into())
}

/// The terminal's shell: the user's, else the system's.
pub fn default_shell() -> String {
    if cfg!(windows) {
        // PowerShell reads the single-quoted paths of `shell_quote` (cmd.exe does not).
        return "powershell.exe".into();
    }
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

/// A command line run by the user's shell, interactive and login like their
/// terminal (a Finder-launched app has a bare PATH, without npm or brew).
pub fn shell_command(line: &str) -> Command {
    if cfg!(windows) {
        let mut cmd = command("powershell.exe");
        cmd.args(["-NoProfile", "-Command", line]);
        return cmd;
    }
    let mut cmd = command(default_shell());
    cmd.arg("-ilc").arg(line);
    cmd
}

/// Discards a program's input file (sqlite3's -init).
pub const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// `program` in `dir`: on Windows also `program.exe`, `.cmd` (npm's), `.bat`.
pub fn executable_in(dir: &Path, program: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &["", ".exe", ".cmd", ".bat"] } else { &[""] };
    exts.iter().map(|e| dir.join(format!("{program}{e}"))).find(|p| p.is_file())
}

/// To the system's trash (it can be put back from there), never deleted for good.
pub fn trash(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // macOS 14+ has `trash`: Finder's own "Put Back" then works.
        if Path::new("/usr/bin/trash").exists() {
            let out = command("/usr/bin/trash").arg(path).output().map_err(|e| e.to_string())?;
            if out.status.success() {
                return Ok(());
            }
        }
        let bin = home().join(".Trash");
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let to = (1..).map(|n| bin.join(if n == 1 { name.clone() } else { format!("{name} {n}") })).find(|p| std::fs::symlink_metadata(p).is_err()).expect("a free name");
        std::fs::rename(path, &to).map_err(|e| e.to_string())
    }
    #[cfg(target_os = "linux")]
    {
        // gio (GLib, on every desktop) follows the freedesktop trash spec, "Restore" included.
        let out = command("gio").arg("trash").arg(path).output().map_err(|e| format!("gio : {e}"))?;
        if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
    }
    #[cfg(windows)]
    {
        // The Recycle Bin, through .NET: no extra program, and "Restore" works.
        let kind = if path.is_dir() { "DeleteDirectory" } else { "DeleteFile" };
        let quoted = path.display().to_string().replace('\'', "''");
        let script = format!("Add-Type -AssemblyName Microsoft.VisualBasic; [Microsoft.VisualBasic.FileIO.FileSystem]::{kind}('{quoted}', 'OnlyErrorDialogs', 'SendToRecycleBin')");
        let out = command("powershell.exe").args(["-NoProfile", "-Command", &script]).output().map_err(|e| e.to_string())?;
        if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_found_with_their_extension_where_needed() {
        let dir = std::env::temp_dir().join(format!("akolade-platform-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) { "outil.cmd" } else { "outil" };
        std::fs::write(dir.join(name), "").unwrap();
        assert_eq!(executable_in(&dir, "outil"), Some(dir.join(name)));
        assert_eq!(executable_in(&dir, "absent"), None);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(home().is_absolute());
    }
}
