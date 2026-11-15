//! Opened from the Finder or the Dock, the app gets a bare PATH (/usr/bin:/bin…):
//! without Homebrew, node, cargo or claude, most tools would say "not installed".
//! The user's interactive login shell knows the real one, as in their terminal.
//!
//! Reading it takes the time the shell config takes (half a second with nvm), so the
//! last one read is kept in Akolade's folder: used at once, read again in the
//! background for the next launch. Only the very first launch waits for the shell.

use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MARK: &str = "__AKOLADE_PATH__";

/// The PATH between the markers: a shell config may print things of its own.
pub fn between_marks(out: &str) -> Option<&str> {
    let start = out.find(MARK)? + MARK.len();
    let end = start + out[start..].find(MARK)?;
    Some(out[start..end].trim()).filter(|p| !p.is_empty())
}

/// The PATH of the user's interactive login shell; None if it fails or takes over 5 s.
fn from_shell() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let script = format!("printf '{MARK}%s{MARK}' \"$PATH\"");
    let mut child = Command::new(shell).arg("-ilc").arg(script).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let started = Instant::now();
    // A stuck shell config (a prompt waiting for input…) must not keep the app from opening.
    while child.try_wait().ok()?.is_none() {
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    between_marks(&out).map(str::to_string)
}

/// Call first thing in `main`, before any thread exists (it sets the environment).
pub fn import() {
    // From a terminal, the PATH is already the user's own.
    if std::env::var_os("TERM").is_some() {
        return;
    }
    let cache = crate::settings::app_dir().join("shell-path");
    let known = fs::read_to_string(&cache).ok().map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
    let path = match &known {
        Some(p) => p.clone(),
        None => {
            let fresh = from_shell();
            save(&cache, fresh.clone());
            match fresh {
                Some(p) => p,
                None => return,
            }
        }
    };
    // SAFETY: called at the very start of `main`, before the app (or anything else) starts a
    // thread: no one can read the environment meanwhile.
    unsafe { std::env::set_var("PATH", path) };
    // Read again for the next launch (a tool installed meanwhile), once the environment is set:
    // nothing waits for it, and it only writes a file.
    if known.is_some() {
        std::thread::spawn(move || save(&cache, from_shell()));
    }
}

fn save(cache: &Path, path: Option<String>) {
    if let Some(path) = path {
        let _ = fs::create_dir_all(crate::settings::app_dir());
        let _ = crate::workspace::write_file(cache, &path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_is_read_between_the_marks() {
        let out = format!("Bienvenue !\nnvm: v24\n{MARK}/opt/homebrew/bin:/usr/bin{MARK}");
        assert_eq!(between_marks(&out), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(between_marks("pas de marque"), None);
        assert_eq!(between_marks(&format!("{MARK}{MARK}")), None, "an empty PATH is no PATH");
    }
}
