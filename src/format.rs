//! Code formatters (⌥⇧F): the text goes through the formatter's stdin and
//! comes back on its stdout. JSON also has a built-in one (`pretty_json`).

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};

use crate::{lsp, settings::AppSettings};

pub struct Formatter {
    pub name: &'static str,
    pub program: &'static str,
    /// `{file}`: the file's path, for the formatter's config and parser.
    pub args: &'static [&'static str],
    /// `Language::name`s.
    pub languages: &'static [&'static str],
    /// `{dir}`: Akolade's servers folder. None: comes with a toolchain.
    pub install: Option<&'static str>,
    pub source: &'static str,
    pub about: &'static str,
}

pub static FORMATTERS: &[Formatter] = &[
    Formatter {
        name: "Prettier",
        program: "prettier",
        args: &["--stdin-filepath", "{file}"],
        languages: &["JavaScript", "TypeScript", "JSX", "TSX", "HTML", "CSS", "SCSS", "JSON", "Markdown", "YAML"],
        install: Some("npm install --prefix {dir} prettier"),
        source: "npm",
        about: "Met en forme le web : JavaScript, TypeScript, HTML, CSS, JSON, Markdown, YAML. Suit le .prettierrc du projet.",
    },
    Formatter {
        name: "Ruff",
        program: "ruff",
        args: &["format", "--stdin-filename", "{file}", "-"],
        languages: &["Python"],
        install: Some("brew install ruff"),
        source: "Homebrew",
        about: "Formateur Python ultra-rapide, compatible Black. Suit le pyproject.toml du projet.",
    },
    Formatter {
        name: "rustfmt",
        program: "rustfmt",
        // The crate's own edition: 2024 rules reject 2021 code (`gen` is a keyword).
        args: &["--emit", "stdout", "--edition", "{edition}"],
        languages: &["Rust"],
        install: None,
        source: "fourni avec Rust",
        about: "Le formateur officiel de Rust. Suit le rustfmt.toml du projet.",
    },
    Formatter {
        name: "gofmt",
        program: "gofmt",
        args: &[],
        languages: &["Go"],
        install: None,
        source: "fourni avec Go",
        about: "Le formateur officiel de Go.",
    },
    Formatter {
        name: "clang-format",
        program: "clang-format",
        args: &["--assume-filename={file}"],
        languages: &["C", "C++", "C#", "Java"],
        install: Some("brew install clang-format"),
        source: "Homebrew",
        about: "Le formateur de LLVM pour C, C++, C# et Java. Suit le .clang-format du projet.",
    },
    Formatter {
        name: "shfmt",
        program: "shfmt",
        args: &[],
        languages: &["Shell"],
        install: Some("brew install shfmt"),
        source: "Homebrew",
        about: "Met en forme les scripts shell : bash, sh, zsh.",
    },
    Formatter {
        name: "swift-format",
        program: "swift-format",
        args: &["format", "--assume-filename", "{file}", "-"],
        languages: &["Swift"],
        install: Some("brew install swift-format"),
        source: "Homebrew",
        about: "Le formateur officiel de Swift. Suit le .swift-format du projet.",
    },
    Formatter {
        name: "SQL Formatter",
        program: "sql-formatter",
        args: &[],
        languages: &["SQL"],
        install: Some("npm install --prefix {dir} sql-formatter"),
        source: "npm",
        about: "Met en forme les requêtes SQL : mots-clés, retours à la ligne, indentation.",
    },
    Formatter {
        name: "Taplo",
        program: "taplo",
        args: &["fmt", "-"],
        languages: &["TOML"],
        install: Some("npm install --prefix {dir} @taplo/cli"),
        source: "npm",
        about: "Met en forme les fichiers TOML (Cargo.toml, pyproject.toml…).",
    },
];

/// The formatter to use for this language: installed and not turned off.
pub fn for_language(language: &str, settings: &AppSettings) -> Option<(&'static Formatter, PathBuf)> {
    FORMATTERS
        .iter()
        .filter(|f| f.languages.contains(&language) && settings.plugin_enabled(f.program))
        .find_map(|f| Some((f, lsp::find_program(f.program)?)))
}

/// The Rust edition of the crate holding `file`: `edition = "…"` of the nearest
/// Cargo.toml up the folders (a `edition.workspace = true` one looks further up,
/// at the workspace's). 2024 when none says.
fn rust_edition(file: &Path) -> String {
    for dir in file.ancestors().skip(1) {
        let Ok(toml) = std::fs::read_to_string(dir.join("Cargo.toml")) else { continue };
        // `edition = "2021" # MSRV`: the comment is not part of the value.
        let lines = || toml.lines().map(|l| l.split('#').next().unwrap_or_default().trim());
        let found = lines().find_map(|l| {
            let value = l.strip_prefix("edition")?.trim_start().strip_prefix('=')?;
            Some(value.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        });
        if let Some(edition) = found.filter(|e| e.len() == 4 && e.chars().all(|c| c.is_ascii_digit())) {
            return edition;
        }
        // A package that says nothing is 2015 for Cargo (unless it takes the workspace's).
        if lines().any(|l| l == "[package]") && !lines().any(|l| l.starts_with("edition.workspace")) {
            return "2015".into();
        }
    }
    "2024".into()
}

/// Formats `text` (blocking: run it in the background). Err: what the
/// formatter said, usually a syntax error.
pub fn run(f: &Formatter, program: &Path, file: &Path, root: &Path, text: String) -> Result<String, String> {
    let edition = if f.args.contains(&"{edition}") { rust_edition(file) } else { String::new() };
    let file = file.to_string_lossy();
    let args: Vec<String> = f.args.iter().map(|a| a.replace("{file}", &file).replace("{edition}", &edition)).collect();
    let mut child = crate::platform::command(program)
        .args(&args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{} : {e}", f.name))?;
    // Written from another thread: a big file would fill the pipes both ways.
    let mut stdin = child.stdin.take().expect("piped stdin");
    let text_was_empty = text.trim().is_empty();
    let writer = std::thread::spawn(move || stdin.write_all(text.as_bytes()));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    if out.status.success() && !out.stdout.is_empty() {
        return String::from_utf8(out.stdout).map_err(|_| "sortie non UTF-8".into());
    }
    // An empty file formats to nothing: nothing to do, not an error. (Any other
    // empty output is refused: it would wipe the buffer.)
    if out.status.success() && text_was_empty {
        return Ok(String::new());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(err.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("erreur inconnue").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustfmt_gets_the_crate_edition() {
        let d = std::env::temp_dir().join(format!("akolade-edition-{}", std::process::id()));
        std::fs::create_dir_all(d.join("member/src")).unwrap();
        std::fs::write(d.join("Cargo.toml"), "[workspace]\nmembers = [\"member\"]\n[workspace.package]\nedition = \"2021\"\n").unwrap();
        std::fs::write(d.join("member/Cargo.toml"), "[package]\nname = \"m\"\nedition.workspace = true\n").unwrap();
        assert_eq!(rust_edition(&d.join("member/src/main.rs")), "2021", "from the workspace");
        std::fs::write(d.join("member/Cargo.toml"), "[package]\nname = \"m\"\nedition = \"2018\"\n").unwrap();
        assert_eq!(rust_edition(&d.join("member/src/main.rs")), "2018");
        std::fs::write(d.join("member/Cargo.toml"), "[package]\nname = \"m\"\nedition = \"2021\" # MSRV 1.70\n").unwrap();
        assert_eq!(rust_edition(&d.join("member/src/main.rs")), "2021", "a comment after the value");
        std::fs::write(d.join("member/Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        assert_eq!(rust_edition(&d.join("member/src/main.rs")), "2015", "Cargo's default");
        assert_eq!(rust_edition(Path::new("/nowhere/a.rs")), "2024");
        std::fs::remove_dir_all(d).unwrap();
    }
    use crate::languages::LANGUAGES;

    #[test]
    fn formatters_name_real_languages() {
        for f in FORMATTERS {
            for l in f.languages {
                assert!(LANGUAGES.iter().any(|lang| lang.name == *l), "{}: unknown language {l}", f.name);
            }
        }
    }

    /// Every installed formatter really reads stdin and writes stdout.
    #[test]
    fn installed_formatters_format_through_stdin() {
        let samples = [
            ("a.sql", "select a,b from t"),
            ("a.toml", "a=1\n"),
            ("a.swift", "let  x=1\n"),
            ("a.sh", "if true;then echo a;fi\n"),
            ("a.c", "int  main( ){return 0;}\n"),
            ("a.go", "package main\nfunc  main(){}\n"),
            ("a.py", "x=1\n"),
            ("a.js", "let  x=1\n"),
        ];
        let dir = std::env::temp_dir();
        for (file, text) in samples {
            let lang = crate::languages::for_path(Path::new(file));
            for f in FORMATTERS.iter().filter(|f| f.languages.contains(&lang.name)) {
                let Some(program) = lsp::find_program(f.program) else { continue };
                let out = run(f, &program, &dir.join(file), &dir, text.into()).unwrap_or_else(|e| panic!("{}: {e}", f.name));
                assert_ne!(out, text, "{} changed nothing", f.name);
            }
        }
    }

    /// End to end with rustfmt (skipped when not installed).
    #[test]
    fn rustfmt_formats_through_stdin() {
        let f = FORMATTERS.iter().find(|f| f.name == "rustfmt").unwrap();
        let Some(program) = lsp::find_program(f.program) else { return };
        // rustup's proxy without its rustfmt component: not installed either.
        if !crate::platform::command(&program).arg("--version").output().is_ok_and(|o| o.status.success()) {
            return;
        }
        let dir = std::env::temp_dir();
        let out = run(f, &program, &dir.join("a.rs"), &dir, "fn  main( ){let x=1;}".into()).unwrap();
        assert_eq!(out, "fn main() {\n    let x = 1;\n}\n");
        assert!(run(f, &program, &dir.join("a.rs"), &dir, "fn main( {".into()).is_err());
    }
}
