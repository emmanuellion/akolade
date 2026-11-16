//! "Dépendances": the packages a project declares (npm, Cargo, Python, Go) and
//! the versions it resolved. On demand only, the ecosystem's own tools tell
//! which have updates and which have known vulnerabilities. Never installs anything.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use gpui::{
    ClipboardItem, Context, Div, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity,
    Window, div, prelude::FluentBuilder,
};
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt, button::ButtonVariants, h_flex, v_flex};
use serde_json::Value;

use crate::{
    tools::{header, tool},
    ui,
    workspace::{Workspace, button, r},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eco {
    Npm,
    Cargo,
    Python,
    Go,
}

impl Eco {
    fn label(self) -> &'static str {
        match self {
            Eco::Npm => "npm",
            Eco::Cargo => "Rust",
            Eco::Python => "Python",
            Eco::Go => "Go",
        }
    }

    /// The name its tools report a package under (Python ignores case and `-_.`).
    fn key(self, name: &str) -> String {
        match self {
            Eco::Python => name.to_lowercase().replace(['_', '.'], "-"),
            _ => name.to_string(),
        }
    }

    fn update_command(self, d: &Dep, latest: &str) -> String {
        let n = &d.name;
        match (self, d.kind) {
            (Eco::Npm, Kind::Dev) => format!("npm install --save-dev {n}@{latest}"),
            (Eco::Npm, _) => format!("npm install {n}@{latest}"),
            (Eco::Cargo, Kind::Dev) => format!("cargo add --dev {n}@{latest}"),
            (Eco::Cargo, Kind::Build) => format!("cargo add --build {n}@{latest}"),
            (Eco::Cargo, _) => format!("cargo add {n}@{latest}"),
            (Eco::Python, _) => format!(".venv/bin/pip install -U '{n}=={latest}'"),
            (Eco::Go, _) => format!("go get {n}@{latest}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Prod,
    Dev,
    Build,
    Indirect,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Prod => "Production",
            Kind::Dev => "Développement",
            Kind::Build => "Build",
            Kind::Indirect => "Indirectes",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dep {
    pub name: String,
    pub kind: Kind,
    /// As declared ("^1.2", ">=2", "workspace"…), empty for any version.
    pub wanted: String,
    pub installed: Option<String>,
    pub file: PathBuf,
    /// 0-based, in `file`.
    pub line: u32,
}

fn dep(name: &str, kind: Kind, wanted: &str, line: usize) -> Dep {
    Dep { name: name.to_string(), kind, wanted: wanted.to_string(), installed: None, file: PathBuf::new(), line: line as u32 }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Note {
    /// A tool is missing: how to get it.
    Hint(String),
    /// A command failed: its first error line.
    Failed(String),
}

/// One ecosystem in one folder (Python: all its files together).
pub struct Manifest {
    pub eco: Eco,
    pub dir: PathBuf,
    /// "package.json", "backend/pyproject.toml, requirements.txt"…
    pub label: String,
    pub deps: Vec<Dep>,
    /// Latest versions, by `Eco::key`, once checked.
    pub latest: HashMap<String, String>,
    pub checked: bool,
    pub note: Option<Note>,
}

// ---- Versions ----

/// The first three numbers of "^1.2.3", "v0.4", ">=2.31.0"…
pub fn version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = s.split('.').map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u64>().ok());
    let major = parts.next()??;
    Some((major, parts.next().flatten().unwrap_or(0), parts.next().flatten().unwrap_or(0)))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

/// How big the step from `current` to `latest` is (none if not newer).
/// In 0.x, a minor step breaks like a major one (semver).
pub fn bump(current: &str, latest: &str) -> Option<Bump> {
    let (a, b) = (version(current)?, version(latest)?);
    if b <= a {
        return None;
    }
    Some(if a.0 != b.0 || (a.0 == 0 && a.1 != b.1) {
        Bump::Major
    } else if a.1 != b.1 {
        Bump::Minor
    } else {
        Bump::Patch
    })
}

/// The locked version of `name` that `wanted` accepts (same major, same minor in 0.x), else the first.
fn pick(locked: &[(String, String)], name: &str, wanted: &str) -> Option<String> {
    let all: Vec<&String> = locked.iter().filter(|(n, _)| n == name).map(|(_, v)| v).collect();
    let w = version(wanted);
    let fits = |v: &String| matches!((w, version(v)), (Some(a), Some(b)) if a.0 == b.0 && (a.0 > 0 || a.1 == b.1));
    all.iter().copied().find(|v| fits(v)).or(all.first().copied()).cloned()
}

// ---- Manifests ----

fn line_at(text: &str, byte: usize) -> u32 {
    text[..byte].matches('\n').count() as u32
}

/// A quoted TOML string: `"1.0"` or `'1.0'`.
fn unquote(v: &str) -> Option<String> {
    let q = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    v[1..].split(q).next().map(str::to_string)
}

fn toml_str(line: &str, key: &str) -> Option<String> {
    let (k, v) = line.split_once('=')?;
    if k.trim() != key {
        return None;
    }
    unquote(v.trim())
}

/// The `key = value` pairs of a one-line inline table.
fn table(v: &str) -> Vec<(&str, &str)> {
    v.trim().trim_start_matches('{').trim_end_matches('}').split(',').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.trim(), v.trim())).collect()
}

/// `[[package]]` names and versions of a Cargo.lock, uv.lock or poetry.lock.
pub fn parse_lock(text: &str) -> Vec<(String, String)> {
    let (mut out, mut in_package, mut name) = (vec![], false, None);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            (in_package, name) = (line == "[[package]]", None);
        } else if in_package && let Some(n) = toml_str(line, "name") {
            name = Some(n);
        } else if in_package && let Some(v) = toml_str(line, "version") && let Some(n) = name.take() {
            out.push((n, v));
        }
    }
    out
}

pub fn parse_package_json(text: &str, lock: Option<&str>, installed: impl Fn(&str) -> Option<String>) -> Result<Vec<Dep>, String> {
    let json: Value = serde_json::from_str(text).map_err(|e| format!("package.json illisible : {e}"))?;
    let lock: Value = lock.and_then(|l| serde_json::from_str(l).ok()).unwrap_or_default();
    let mut deps = vec![];
    for (key, kind) in [("dependencies", Kind::Prod), ("devDependencies", Kind::Dev)] {
        let Some(map) = json.get(key).and_then(Value::as_object) else { continue };
        let start = text.find(&format!("\"{key}\"")).unwrap_or(0);
        for (name, spec) in map {
            let at = text[start..].find(&format!("\"{name}\"")).map_or(start, |i| start + i);
            // package-lock v2/v3 ("packages"), then v1 ("dependencies").
            let locked = || {
                lock.get("packages")
                    .and_then(|p| p.get(format!("node_modules/{name}")))
                    .or_else(|| lock.get("dependencies").and_then(|p| p.get(name)))
                    .and_then(|p| p["version"].as_str())
                    .map(str::to_string)
            };
            deps.push(Dep { installed: installed(name).or_else(locked), ..dep(name, kind, spec.as_str().unwrap_or(""), line_at(text, at) as usize) });
        }
    }
    Ok(deps)
}

/// What a Cargo dependency key says: version, rename (`package`), or where it comes from.
fn cargo_key(d: &mut (Dep, String), key: &str, value: &str) {
    match key {
        "version" => d.0.wanted = unquote(value).unwrap_or_default(),
        "package" => d.1 = unquote(value).unwrap_or_else(|| d.1.clone()),
        "path" if d.0.wanted.is_empty() => d.0.wanted = "local".into(),
        "git" | "workspace" if d.0.wanted.is_empty() => d.0.wanted = key.into(),
        _ => {}
    }
}

/// `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, their
/// `[target.….dependencies]` and `[workspace.dependencies]` forms, and
/// `[dependencies.name]` tables; resolved versions from Cargo.lock.
pub fn parse_cargo_toml(text: &str, lock: Option<&str>) -> Vec<Dep> {
    let locked = lock.map(parse_lock).unwrap_or_default();
    // Each dep with the name Cargo.lock knows it by.
    let mut deps: Vec<(Dep, String)> = vec![];
    let (mut section, mut own_table) = (None, false);
    for (i, raw) in text.lines().enumerate() {
        // shortcut: a " #" inside a quoted value is taken for a comment; fine for dependency lines.
        let line = raw.split(" #").next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            let h = line.trim_matches(['[', ']']).trim();
            (section, own_table) = (None, false);
            for (key, kind) in [("dev-dependencies", Kind::Dev), ("build-dependencies", Kind::Build), ("dependencies", Kind::Prod)] {
                if h == key || h.ends_with(&format!(".{key}")) {
                    section = Some(kind);
                } else if let Some(name) = h.strip_prefix(&format!("{key}.")).or_else(|| h.split_once(&format!(".{key}.")).map(|(_, n)| n)) {
                    let name = name.trim_matches('"');
                    deps.push((dep(name, kind, "", i), name.to_string()));
                    own_table = true;
                } else {
                    continue;
                }
                break;
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim().trim_matches('"'), v.trim());
        if own_table {
            if let Some(d) = deps.last_mut() {
                cargo_key(d, k, v);
            }
        } else if let Some(kind) = section {
            // `serde.workspace = true`
            let (name, sub) = k.split_once('.').map_or((k, None), |(n, s)| (n, Some(s)));
            let mut d = (dep(name, kind, "", i), name.to_string());
            match sub {
                Some(s) => cargo_key(&mut d, s, v),
                None if v.starts_with('{') => table(v).into_iter().for_each(|(k, v)| cargo_key(&mut d, k, v)),
                None => d.0.wanted = unquote(v).unwrap_or_default(),
            }
            deps.push(d);
        }
    }
    deps.into_iter().map(|(d, lock_name)| Dep { installed: pick(&locked, &lock_name, &d.wanted), ..d }).collect()
}

/// "requests[socks]>=2.31 ; python_version>'3.8'" → ("requests", ">=2.31").
fn pep508(s: &str) -> (&str, &str) {
    let s = s.split(';').next().unwrap_or("").trim();
    let end = s.find(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c))).unwrap_or(s.len());
    let (name, mut rest) = s.split_at(end);
    rest = rest.trim_start();
    if rest.starts_with('[') {
        rest = rest.split_once(']').map_or("", |(_, r)| r);
    }
    (name, rest.trim())
}

/// The strings of a TOML array line as PEP 508 deps; true once the array closes.
fn scan_array(line: &str, i: usize, kind: Kind, deps: &mut Vec<Dep>) -> bool {
    let (mut quote, mut start, mut in_table) = (None, 0, false);
    for (j, c) in line.char_indices() {
        match quote {
            Some(q) if c == q => {
                let (name, spec) = pep508(&line[start..j]);
                if !in_table && !name.is_empty() {
                    deps.push(dep(name, kind, spec, i));
                }
                quote = None;
            }
            Some(_) => {}
            None if c == '"' || c == '\'' => (quote, start) = (Some(c), j + 1),
            // `{ include-group = "test" }` (PEP 735) names a group, not a package.
            None if c == '{' || c == '}' => in_table = c == '{',
            None if c == ']' => return true,
            None if c == '#' => return false,
            None => {}
        }
    }
    false
}

/// `[project]` dependencies, optional dependencies and `[dependency-groups]`,
/// uv's dev-dependencies, and Poetry's tables.
pub fn parse_pyproject(text: &str) -> Vec<Dep> {
    let (mut deps, mut table, mut array) = (vec![], String::new(), None);
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if let Some(kind) = array {
            if scan_array(line, i, kind, &mut deps) {
                array = None;
            }
            continue;
        }
        if line.starts_with('[') {
            table = line.trim_matches(['[', ']']).trim().to_string();
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim().trim_matches('"'), v.trim());
        let list = match table.as_str() {
            "project" if k == "dependencies" => Some(Kind::Prod),
            "project.optional-dependencies" | "dependency-groups" => Some(Kind::Dev),
            "tool.uv" if k == "dev-dependencies" => Some(Kind::Dev),
            _ => None,
        };
        if let Some(kind) = list {
            if let Some(rest) = v.strip_prefix('[')
                && !scan_array(rest, i, kind, &mut deps)
            {
                array = Some(kind);
            }
            continue;
        }
        let kind = match table.as_str() {
            "tool.poetry.dependencies" => Kind::Prod,
            t if t == "tool.poetry.dev-dependencies" || (t.starts_with("tool.poetry.group.") && t.ends_with(".dependencies")) => Kind::Dev,
            _ => continue,
        };
        if k != "python" {
            let spec = unquote(v).or_else(|| table_version(v)).unwrap_or_default();
            deps.push(dep(k, kind, &spec, i));
        }
    }
    deps
}

fn table_version(v: &str) -> Option<String> {
    table(v).into_iter().find(|(k, _)| *k == "version").and_then(|(_, v)| unquote(v))
}

pub fn parse_requirements(text: &str, kind: Kind) -> Vec<Dep> {
    text.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let l = l.split(" #").next()?.trim();
            // Options (-r, -e, --index-url), paths and URLs: not a package name.
            if l.is_empty() || l.starts_with(['#', '-', '.', '/']) || l.contains("://") {
                return None;
            }
            let (name, spec) = pep508(l);
            (!name.is_empty()).then(|| dep(name, kind, spec, i))
        })
        .collect()
}

/// go.mod's `require` lines and blocks. Go builds with exactly these versions.
pub fn parse_go_mod(text: &str) -> Vec<Dep> {
    let (mut deps, mut block) = (vec![], false);
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let body = if block {
            if line.starts_with(')') {
                block = false;
                continue;
            }
            line
        } else if let Some(rest) = line.strip_prefix("require") {
            if rest.trim() == "(" {
                block = true;
                continue;
            }
            rest
        } else {
            continue;
        };
        let (spec, comment) = body.split_once("//").unwrap_or((body, ""));
        let mut parts = spec.split_whitespace();
        let (Some(path), Some(v)) = (parts.next(), parts.next()) else { continue };
        let kind = if comment.trim() == "indirect" { Kind::Indirect } else { Kind::Prod };
        deps.push(Dep { installed: Some(v.to_string()), ..dep(path, kind, v, i) });
    }
    deps
}

fn manifest(eco: Eco, root: &Path, dir: &Path, files: Vec<(String, Result<Vec<Dep>, String>)>) -> Manifest {
    let rel = dir.strip_prefix(root).ok().filter(|p| !p.as_os_str().is_empty()).map(|p| format!("{}/", p.display())).unwrap_or_default();
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    let mut m = Manifest { eco, dir: dir.to_path_buf(), label: format!("{rel}{}", names.join(", ")), deps: vec![], latest: HashMap::new(), checked: false, note: None };
    for (name, deps) in files {
        match deps {
            Ok(deps) => m.deps.extend(deps.into_iter().map(|d| Dep { file: dir.join(&name), ..d })),
            Err(e) => m.note = Some(Note::Failed(e)),
        }
    }
    m.deps.sort_by_key(|d| (d.kind, d.name.to_lowercase()));
    m
}

fn manifests_in(root: &Path, dir: &Path) -> Vec<Manifest> {
    let read = |name: &str| fs::read_to_string(dir.join(name)).ok();
    let mut out = vec![];
    if let Some(text) = read("package.json") {
        let installed = |name: &str| read(&format!("node_modules/{name}/package.json")).and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v["version"].as_str().map(str::to_string));
        let deps = parse_package_json(&text, read("package-lock.json").as_deref(), installed);
        out.push(manifest(Eco::Npm, root, dir, vec![("package.json".into(), deps)]));
    }
    if let Some(text) = read("Cargo.toml") {
        // A workspace member's lock is the workspace's.
        let lock = read("Cargo.lock").or_else(|| fs::read_to_string(root.join("Cargo.lock")).ok());
        out.push(manifest(Eco::Cargo, root, dir, vec![("Cargo.toml".into(), Ok(parse_cargo_toml(&text, lock.as_deref())))]));
    }
    let mut python = vec![];
    if let Some(text) = read("pyproject.toml") {
        python.push(("pyproject.toml".to_string(), Ok(parse_pyproject(&text))));
    }
    let mut requirements: Vec<String> = fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.starts_with("requirements") && n.ends_with(".txt")).collect();
    requirements.sort();
    for name in requirements {
        let kind = if name.contains("dev") || name.contains("test") { Kind::Dev } else { Kind::Prod };
        python.push((name.clone(), Ok(parse_requirements(&read(&name).unwrap_or_default(), kind))));
    }
    if !python.is_empty() {
        let locked: Vec<(String, String)> = read("uv.lock").or_else(|| read("poetry.lock")).map(|t| parse_lock(&t)).unwrap_or_default().into_iter().map(|(n, v)| (Eco::Python.key(&n), v)).collect();
        let mut m = manifest(Eco::Python, root, dir, python);
        for d in &mut m.deps {
            d.installed = pick(&locked, &Eco::Python.key(&d.name), &d.wanted);
        }
        out.push(m);
    }
    if let Some(text) = read("go.mod") {
        out.push(manifest(Eco::Go, root, dir, vec![("go.mod".into(), Ok(parse_go_mod(&text)))]));
    }
    out
}

/// The manifests at the project's root and in its direct subfolders (front/, back/…).
// shortcut: deeper folders (packages/*/ monorepos) are not looked at; walk them if asked.
pub fn load(root: &Path) -> Vec<Manifest> {
    const SKIP: [&str; 6] = ["node_modules", "target", "vendor", "dist", "build", "venv"];
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| !n.starts_with('.') && !SKIP.contains(&n)))
        .collect();
    dirs.sort();
    dirs.insert(0, root.to_path_buf());
    dirs.iter().flat_map(|d| manifests_in(root, d)).collect()
}

// ---- Command outputs ----

fn json(out: &str) -> Result<Value, String> {
    if out.trim().is_empty() {
        return Ok(Value::Null);
    }
    let v: Value = serde_json::from_str(out).map_err(|e| format!("Réponse illisible : {e}"))?;
    // npm reports its own failures as JSON ("This command requires an existing lockfile.").
    match v.pointer("/error/summary").and_then(Value::as_str) {
        Some(e) => Err(e.lines().next().unwrap_or(e).to_string()),
        None => Ok(v),
    }
}

/// `npm outdated --json`: name → latest.
pub fn parse_npm_outdated(out: &str) -> Result<HashMap<String, String>, String> {
    let v = json(out)?;
    Ok(v.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, info)| {
            // In npm workspaces: one entry per package using it.
            let info = info.as_array().and_then(|a| a.first()).unwrap_or(info);
            Some((name.clone(), info["latest"].as_str()?.to_string()))
        })
        .collect())
}

/// `cargo outdated --format json`: one object per crate of the workspace.
pub fn parse_cargo_outdated(out: &str) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for line in out.lines().filter(|l| l.trim_start().starts_with('{')) {
        let v = json(line)?;
        for d in v["dependencies"].as_array().into_iter().flatten() {
            // "---" or "Removed" when there is no newer version.
            if let (Some(name), Some(latest)) = (d["name"].as_str(), d["latest"].as_str())
                && latest.starts_with(|c: char| c.is_ascii_digit())
            {
                map.insert(name.to_string(), latest.to_string());
            }
        }
    }
    Ok(map)
}

/// `pip list --outdated --format=json` (and uv's): name → latest.
pub fn parse_pip_outdated(out: &str) -> Result<HashMap<String, String>, String> {
    let v = json(out)?;
    Ok(v.as_array().into_iter().flatten().filter_map(|p| Some((p["name"].as_str()?.to_string(), p["latest_version"].as_str()?.to_string()))).collect())
}

/// `go list -m -u -json all`: a stream of module objects, `Update` when newer.
pub fn parse_go_updates(out: &str) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for v in serde_json::Deserializer::from_str(out).into_iter::<Value>() {
        let v = v.map_err(|e| format!("Réponse illisible : {e}"))?;
        if let (Some(path), Some(latest)) = (v["Path"].as_str(), v.pointer("/Update/Version").and_then(Value::as_str)) {
            map.insert(path.to_string(), latest.to_string());
        }
    }
    Ok(map)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Critical,
    High,
    Moderate,
    Low,
    Unknown,
}

impl Severity {
    const ALL: [Severity; 5] = [Severity::Critical, Severity::High, Severity::Moderate, Severity::Low, Severity::Unknown];

    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "critical" => Severity::Critical,
            "high" => Severity::High,
            "moderate" | "medium" => Severity::Moderate,
            "low" | "info" | "none" => Severity::Low,
            _ => Severity::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Severity::Critical => "critique",
            Severity::High => "élevée",
            Severity::Moderate => "moyenne",
            Severity::Low => "faible",
            Severity::Unknown => "non classée",
        }
    }

    /// A CVSS 3.x vector's base score, as a rating (RustSec advisories only give the vector).
    pub fn from_cvss(vector: &str) -> Self {
        let m: HashMap<&str, &str> = vector.split('/').filter_map(|p| p.split_once(':')).collect();
        let get = |k: &str, table: &[(&str, f64)]| m.get(k).and_then(|v| table.iter().find(|(n, _)| n == v)).map(|(_, x)| *x);
        let changed = m.get("S") == Some(&"C");
        let cia = [("H", 0.56), ("L", 0.22), ("N", 0.)];
        let pr = if changed { [("N", 0.85), ("L", 0.68), ("H", 0.5)] } else { [("N", 0.85), ("L", 0.62), ("H", 0.27)] };
        let (Some(av), Some(ac), Some(pr), Some(ui), Some(c), Some(i), Some(a)) = (
            get("AV", &[("N", 0.85), ("A", 0.62), ("L", 0.55), ("P", 0.2)]),
            get("AC", &[("L", 0.77), ("H", 0.44)]),
            get("PR", &pr),
            get("UI", &[("N", 0.85), ("R", 0.62)]),
            get("C", &cia),
            get("I", &cia),
            get("A", &cia),
        ) else {
            return Severity::Unknown;
        };
        let iss = 1. - (1. - c) * (1. - i) * (1. - a);
        let impact = if changed { 7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15) } else { 6.42 * iss };
        let exploit = 8.22 * av * ac * pr * ui;
        let score = if impact <= 0. { 0. } else { (((impact + exploit) * if changed { 1.08 } else { 1. }).min(10.) * 10.).ceil() / 10. };
        match score {
            s if s >= 9. => Severity::Critical,
            s if s >= 7. => Severity::High,
            s if s >= 4. => Severity::Moderate,
            _ => Severity::Low,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vuln {
    pub package: String,
    /// The installed version, or the affected range (npm).
    pub version: String,
    pub severity: Severity,
    pub title: String,
    /// The version(s) that fix it, empty if none.
    pub fixed: String,
}

fn by_severity(mut list: Vec<Vuln>) -> Vec<Vuln> {
    list.sort_by(|a, b| (a.severity, &a.package).cmp(&(b.severity, &b.package)));
    list
}

/// `npm audit --json` (npm 7+).
pub fn parse_npm_audit(out: &str) -> Result<Vec<Vuln>, String> {
    let v = json(out)?;
    let list = v["vulnerabilities"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, x)| {
            let via = x["via"].as_array().map(Vec::as_slice).unwrap_or(&[]);
            let titles: Vec<&str> = via.iter().filter_map(|w| w["title"].as_str()).collect();
            // Only through other packages: name them.
            let title = if titles.is_empty() { format!("Par {}", via.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")) } else { titles.join(" · ") };
            let fix = &x["fixAvailable"];
            let fixed = match (fix["name"].as_str(), fix["version"].as_str()) {
                (Some(n), Some(v)) => format!("{n}@{v}"),
                _ if fix == &Value::Bool(true) => "npm audit fix".into(),
                _ => String::new(),
            };
            Vuln { package: name.clone(), version: x["range"].as_str().unwrap_or("").to_string(), severity: Severity::from(x["severity"].as_str().unwrap_or("")), title, fixed }
        })
        .collect();
    Ok(by_severity(list))
}

/// `cargo audit --json`.
// shortcut: its "warnings" (unmaintained, yanked crates) are not shown; add them if asked.
pub fn parse_cargo_audit(out: &str) -> Result<Vec<Vuln>, String> {
    let v = json(out)?;
    let list = v
        .pointer("/vulnerabilities/list")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|x| {
            let a = &x["advisory"];
            let patched: Vec<&str> = x.pointer("/versions/patched").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            Vuln {
                package: x.pointer("/package/name").and_then(Value::as_str).unwrap_or("").to_string(),
                version: x.pointer("/package/version").and_then(Value::as_str).unwrap_or("").to_string(),
                severity: a["cvss"].as_str().map_or(Severity::Unknown, Severity::from_cvss),
                title: format!("{} · {}", a["id"].as_str().unwrap_or(""), a["title"].as_str().unwrap_or("")),
                fixed: patched.join(", "),
            }
        })
        .collect();
    Ok(by_severity(list))
}

/// `pip-audit -f json` (an object since 2.x, a bare list before). It gives no severity.
pub fn parse_pip_audit(out: &str) -> Result<Vec<Vuln>, String> {
    let v = json(out)?;
    let deps = v.get("dependencies").unwrap_or(&v);
    let list = deps
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|d| {
            d["vulns"].as_array().into_iter().flatten().map(move |x| {
                let fixes: Vec<&str> = x["fix_versions"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                let description = x["description"].as_str().unwrap_or("").lines().next().unwrap_or("");
                Vuln {
                    package: d["name"].as_str().unwrap_or("").to_string(),
                    version: d["version"].as_str().unwrap_or("").to_string(),
                    severity: Severity::Unknown,
                    title: [x["id"].as_str().unwrap_or(""), description].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "),
                    fixed: fixes.join(", "),
                }
            })
        })
        .collect();
    Ok(by_severity(list))
}

// ---- Running the tools (blocking: in the background) ----

fn program(name: &str, hint: &str) -> Result<PathBuf, Note> {
    crate::lsp::find_program(name).ok_or_else(|| Note::Hint(hint.to_string()))
}

/// The tool's stdout. These tools exit with an error when they find something: their JSON still counts.
fn run(dir: &Path, program: &Path, args: &[&str], envs: &[(&str, PathBuf)]) -> Result<String, String> {
    let name = program.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let out = crate::platform::command(program)
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().map(|(k, v)| (k, v)))
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Impossible de lancer {name} : {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() || stdout.trim_start().starts_with(['{', '[']) {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(stderr.lines().map(str::trim).find(|l| !l.is_empty()).map_or_else(|| format!("{name} a échoué ({}).", out.status), str::to_string))
}

fn updates(eco: Eco, dir: &Path) -> Result<HashMap<String, String>, Note> {
    let venv = dir.join(".venv");
    let found = match eco {
        Eco::Npm => run(dir, &program("npm", "npm est introuvable : installez Node.js (nodejs.org).")?, &["outdated", "--json"], &[]).and_then(|o| parse_npm_outdated(&o)),
        Eco::Cargo => {
            program("cargo-outdated", "Installez cargo-outdated pour voir les mises à jour Rust : cargo install cargo-outdated")?;
            run(dir, &program("cargo", "cargo est introuvable : installez Rust (rustup.rs).")?, &["outdated", "-R", "--format", "json"], &[]).and_then(|o| parse_cargo_outdated(&o))
        }
        Eco::Python if venv.join("bin/pip").is_file() => run(dir, &venv.join("bin/pip"), &["list", "--outdated", "--format=json", "--disable-pip-version-check"], &[]).and_then(|o| parse_pip_outdated(&o)),
        // uv's environments have no pip.
        Eco::Python if venv.is_dir() && let Some(uv) = crate::lsp::find_program("uv") => {
            run(dir, &uv, &["pip", "list", "--outdated", "--format", "json"], &[("VIRTUAL_ENV", venv)]).and_then(|o| parse_pip_outdated(&o))
        }
        Eco::Python => return Err(Note::Hint("Aucun environnement .venv dans ce dossier : créez-le pour comparer avec les dernières versions.".into())),
        Eco::Go => run(dir, &program("go", "go est introuvable : installez Go (go.dev).")?, &["list", "-m", "-u", "-json", "all"], &[]).and_then(|o| parse_go_updates(&o)),
    };
    found.map_err(Note::Failed)
}

fn audit(eco: Eco, dir: &Path) -> Result<Vec<Vuln>, Note> {
    let found = match eco {
        Eco::Npm => run(dir, &program("npm", "npm est introuvable : installez Node.js (nodejs.org).")?, &["audit", "--json"], &[]).and_then(|o| parse_npm_audit(&o)),
        Eco::Cargo => {
            program("cargo-audit", "Installez cargo-audit pour l'audit Rust : cargo install cargo-audit")?;
            run(dir, &program("cargo", "cargo est introuvable : installez Rust (rustup.rs).")?, &["audit", "--json"], &[]).and_then(|o| parse_cargo_audit(&o))
        }
        Eco::Python => {
            let pip_audit = program("pip-audit", "Installez pip-audit pour l'audit Python : pipx install pip-audit")?;
            let (venv, python) = (dir.join(".venv"), dir.join(".venv/bin/python"));
            if !python.is_file() {
                return Err(Note::Hint("Aucun environnement .venv dans ce dossier : pip-audit vérifie les paquets qui y sont installés.".into()));
            }
            // Both: newer pip-audit reads VIRTUAL_ENV, older ones pip-api's variable.
            run(dir, &pip_audit, &["-f", "json", "--progress-spinner", "off"], &[("VIRTUAL_ENV", venv), ("PIPAPI_PYTHON_LOCATION", python)]).and_then(|o| parse_pip_audit(&o))
        }
        Eco::Go => return Err(Note::Hint("Pour Go, lancez govulncheck ./... dans le terminal (go install golang.org/x/vuln/cmd/govulncheck@latest).".into())),
    };
    found.map_err(Note::Failed)
}

// ---- View ----

pub struct Audit {
    vulns: Vec<Vuln>,
    /// Manifest label and what kept it from being audited.
    notes: Vec<(String, Note)>,
    audited: usize,
}

pub struct DepsView {
    workspace: WeakEntity<Workspace>,
    root: Option<PathBuf>,
    /// None while reading.
    manifests: Option<Vec<Manifest>>,
    /// What runs right now ("Recherche des mises à jour…").
    busy: Option<&'static str>,
    audit: Option<Audit>,
    /// Every command of the last run failed: title and first error.
    failure: Option<(&'static str, String)>,
    _task: Task<()>,
}

fn pill(text: impl Into<SharedString>, color: Hsla) -> Div {
    div().flex_shrink_0().px(r(6.)).py(r(1.)).rounded(r(4.)).bg(color.opacity(0.14)).text_color(color).text_size(r(11.5)).font_weight(FontWeight::MEDIUM).child(text.into())
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n > 1 { "s" } else { "" })
}

/// A newer version than the installed one (or the declared one, if not installed).
fn update_of<'a>(m: &'a Manifest, d: &Dep) -> Option<(&'a str, Bump)> {
    let latest = m.latest.get(&m.eco.key(&d.name))?;
    Some((latest, bump(d.installed.as_deref().unwrap_or(&d.wanted), latest)?))
}

impl DepsView {
    pub fn new(workspace: WeakEntity<Workspace>, root: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let mut view = Self { workspace, root, manifests: None, busy: None, audit: None, failure: None, _task: Task::ready(()) };
        view.reload(cx);
        view
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        (self.busy, self.audit, self.failure) = (None, None, None);
        let Some(root) = self.root.clone() else {
            self.manifests = Some(vec![]);
            return;
        };
        self.manifests = None;
        let task = cx.background_executor().spawn(async move { load(&root) });
        self._task = cx.spawn(async move |this, cx| {
            let manifests = task.await;
            let _ = this.update(cx, |this, cx| {
                this.manifests = Some(manifests);
                cx.notify();
            });
        });
    }

    fn start(&mut self, auditing: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(manifests) = &self.manifests else { return };
        if self.busy.is_some() || manifests.is_empty() {
            return;
        }
        let targets: Vec<(Eco, PathBuf)> = manifests.iter().map(|m| (m.eco, m.dir.clone())).collect();
        // The project's own code may run: the Python of its .venv, or whatever its
        // .cargo/config.toml says (an alias named `outdated` or `audit`, another rustc…).
        let runs_project = targets.iter().any(|(e, d)| *e == Eco::Cargo || *e == Eco::Python && d.join(".venv").is_dir());
        if runs_project && !self.workspace.upgrade().is_some_and(|w| w.read(cx).trusted()) {
            let answer = window.prompt(
                PromptLevel::Warning,
                "Faire confiance à ce projet ?",
                Some("La vérification lance Cargo ou le Python du dossier .venv, qui suivent la configuration du projet et peuvent exécuter son code.\n\nN'acceptez que si vous faites confiance à ce projet."),
                &["Continuer", "Annuler"],
                cx,
            );
            let root = self.root.clone();
            cx.spawn_in(window, async move |this, cx| {
                if answer.await == Ok(0) {
                    let _ = this.update_in(cx, |this, window, cx| {
                        // The yes was for this project only, not one opened meanwhile.
                        if this.root != root {
                            return;
                        }
                        let _ = this.workspace.update(cx, |w, cx| {
                            if w.root() == root.as_deref() {
                                w.trust(cx);
                            }
                        });
                        this.start(auditing, window, cx);
                    });
                }
            })
            .detach();
            return;
        }
        self.failure = None;
        if auditing {
            self.busy = Some("Audit de sécurité en cours…");
            let task = cx.background_executor().spawn(async move { targets.iter().map(|(e, d)| audit(*e, d)).collect::<Vec<_>>() });
            self._task = cx.spawn(async move |this, cx| {
                let results = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.finish_audit(results);
                    cx.notify();
                });
            });
        } else {
            self.busy = Some("Recherche des mises à jour…");
            let task = cx.background_executor().spawn(async move { targets.iter().map(|(e, d)| updates(*e, d)).collect::<Vec<_>>() });
            self._task = cx.spawn(async move |this, cx| {
                let results = task.await;
                let _ = this.update(cx, |this, cx| {
                    this.finish_updates(results);
                    cx.notify();
                });
            });
        }
        cx.notify();
    }

    fn finish_updates(&mut self, results: Vec<Result<HashMap<String, String>, Note>>) {
        self.busy = None;
        let Some(manifests) = &mut self.manifests else { return };
        let mut errors = vec![];
        for (m, result) in manifests.iter_mut().zip(results) {
            match result {
                Ok(latest) => (m.latest, m.checked, m.note) = (latest.into_iter().map(|(k, v)| (m.eco.key(&k), v)).collect(), true, None),
                Err(note) => {
                    if let Note::Failed(e) = &note {
                        errors.push(e.clone());
                    }
                    (m.latest, m.checked, m.note) = (HashMap::new(), false, Some(note));
                }
            }
        }
        if errors.len() == manifests.len() {
            self.failure = Some(("Impossible de vérifier les mises à jour", errors.swap_remove(0)));
        }
    }

    fn finish_audit(&mut self, results: Vec<Result<Vec<Vuln>, Note>>) {
        self.busy = None;
        let Some(manifests) = &self.manifests else { return };
        let mut a = Audit { vulns: vec![], notes: vec![], audited: 0 };
        for (m, result) in manifests.iter().zip(results) {
            match result {
                Ok(v) => (a.vulns, a.audited) = ([a.vulns, v].concat(), a.audited + 1),
                Err(note) => a.notes.push((m.label.clone(), note)),
            }
        }
        a.vulns = by_severity(a.vulns);
        if a.notes.iter().all(|(_, n)| matches!(n, Note::Failed(_)))
            && a.audited == 0
            && let Some((_, Note::Failed(e))) = a.notes.first()
        {
            self.failure = Some(("L'audit de sécurité a échoué", e.clone()));
        }
        self.audit = Some(a);
    }

    fn open(&self, file: PathBuf, line: u32, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| ws.open_at(file, line, 0, window, cx));
        }
    }

    fn note_line(text: String, note: &Note, cx: &Context<Self>) -> Div {
        let theme = cx.theme();
        let (color, icon) = match note {
            Note::Hint(_) => (theme.muted_foreground, IconName::Info),
            Note::Failed(_) => (theme.red, IconName::TriangleAlert),
        };
        h_flex().flex_shrink_0().gap(r(6.)).px(r(8.)).py(r(4.)).text_size(r(12.5)).text_color(color).child(Icon::new(icon).small()).child(div().flex_1().min_w_0().child(text))
    }

    fn render_audit(&self, a: &Audit, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (muted, border, mono) = (theme.muted_foreground, theme.border, theme.mono_font_family.clone());
        let color = |s: Severity| match s {
            Severity::Critical | Severity::High => theme.red,
            Severity::Moderate => theme.yellow,
            Severity::Low => theme.blue,
            Severity::Unknown => muted,
        };
        let counts = Severity::ALL.into_iter().filter_map(|s| {
            let n = a.vulns.iter().filter(|v| v.severity == s).count();
            (n > 0).then(|| pill(plural(n, s.label()), color(s)))
        });
        let rows = a.vulns.iter().map(|v| {
            h_flex()
                .flex_shrink_0()
                .h(r(30.))
                .px(r(8.))
                .gap(r(12.))
                .text_size(r(13.))
                .child(div().w(r(96.)).flex_shrink_0().child(pill(v.severity.label(), color(v.severity))))
                .child(div().w(r(200.)).flex_shrink_0().min_w_0().truncate().font_family(mono.clone()).text_size(r(12.)).child(format!("{} {}", v.package, v.version)))
                .child(div().flex_1().min_w_0().truncate().child(v.title.clone()))
                .child(div().w(r(150.)).flex_shrink_0().truncate().text_size(r(12.)).text_color(muted).child(if v.fixed.is_empty() { "pas de correctif".to_string() } else { format!("corrigé : {}", v.fixed) }))
        });
        v_flex()
            .flex_shrink_0()
            .mb(r(8.))
            .p(r(12.))
            .gap(r(4.))
            .rounded(r(10.))
            .border_1()
            .border_color(border)
            .child(
                h_flex()
                    .gap(r(8.))
                    .px(r(8.))
                    .pb(r(4.))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Audit de sécurité"))
                    .children(counts)
                    .child(div().flex_1())
                    .child(button("close-audit").ghost().xsmall().icon(IconName::Close).tooltip("Fermer").on_click(cx.listener(|this, _, _, cx| {
                        this.audit = None;
                        cx.notify();
                    }))),
            )
            .when(a.audited > 0 && a.vulns.is_empty(), |d| d.child(Self::note_line("Aucune faille connue dans les paquets audités.".into(), &Note::Hint(String::new()), cx).text_color(theme.green)))
            .children(a.notes.iter().map(|(label, n)| {
                let (Note::Hint(t) | Note::Failed(t)) = n;
                Self::note_line(format!("{label} : {t}"), n, cx)
            }))
            .children(rows)
    }

    fn render_dep(&self, id: String, m: &Manifest, d: &Dep, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (muted, mono, hover) = (theme.muted_foreground, theme.mono_font_family.clone(), ui::hover_bg(cx));
        let update = update_of(m, d);
        let badge = match update {
            Some((latest, b)) => Some(pill(latest.to_string(), match b {
                Bump::Patch => theme.green,
                Bump::Minor => theme.yellow,
                Bump::Major => theme.red,
            })),
            None if m.checked => Some(div().text_size(r(12.)).text_color(muted).child("à jour")),
            None => None,
        };
        let command = update.map(|(latest, _)| m.eco.update_command(d, latest));
        let (file, line) = (d.file.clone(), d.line);
        let wanted = if d.wanted.is_empty() { "*".to_string() } else { d.wanted.clone() };
        h_flex()
            .id(SharedString::from(id.clone()))
            .flex_shrink_0()
            .h(r(30.))
            .px(r(8.))
            .gap(r(12.))
            .rounded(r(6.))
            .text_size(r(13.))
            .cursor_pointer()
            .hover(|s| s.bg(hover))
            .on_click(cx.listener(move |this, _, window, cx| this.open(file.clone(), line, window, cx)))
            .child(div().flex_1().min_w_0().truncate().child(d.name.clone()))
            .child(div().w(r(130.)).flex_shrink_0().truncate().font_family(mono.clone()).text_size(r(12.)).text_color(muted).child(wanted))
            .child(div().w(r(110.)).flex_shrink_0().truncate().font_family(mono).text_size(r(12.)).child(d.installed.clone().unwrap_or_else(|| "—".into())))
            .child(div().w(r(110.)).flex_shrink_0().children(badge))
            .child(div().w(r(24.)).flex_shrink_0().children(command.map(|c| {
                button(SharedString::from(format!("copy-{id}"))).ghost().xsmall().icon(IconName::Copy).tooltip("Copier la commande de mise à jour").on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    cx.write_to_clipboard(ClipboardItem::new_string(c.clone()));
                    window.push_notification("Commande copiée.", cx);
                })
            })))
    }

    fn render_list(&self, manifests: &[Manifest], cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let accent = (tool("deps").accent)(theme);
        // shortcut: every row is drawn (no virtual list); fine for a few hundred dependencies.
        let mut list = v_flex().id("deps").flex_1().overflow_y_scroll().p(r(16.)).gap(r(2.));
        if let Some(a) = &self.audit {
            list = list.child(self.render_audit(a, cx));
        }
        for (mi, m) in manifests.iter().enumerate() {
            list = list.child(
                h_flex()
                    .flex_shrink_0()
                    .px(r(8.))
                    .pt(r(if mi == 0 { 4. } else { 20. }))
                    .pb(r(4.))
                    .gap(r(8.))
                    .child(pill(m.eco.label(), accent))
                    .child(div().flex_1().min_w_0().truncate().text_size(r(13.)).font_weight(FontWeight::MEDIUM).child(m.label.clone()))
                    .child(div().text_size(r(12.)).text_color(muted).child(plural(m.deps.len(), "dépendance"))),
            );
            if let Some(n) = &m.note {
                let (Note::Hint(t) | Note::Failed(t)) = n;
                list = list.child(Self::note_line(t.clone(), n, cx));
            }
            if m.deps.is_empty() {
                list = list.child(div().flex_shrink_0().px(r(8.)).py(r(4.)).text_size(r(13.)).text_color(muted).child("Aucune dépendance déclarée."));
                continue;
            }
            let column = |w: Option<f32>, text: &'static str| {
                let d = div().text_size(r(11.)).text_color(muted);
                match w {
                    Some(w) => d.w(r(w)).flex_shrink_0(),
                    None => d.flex_1(),
                }
                .child(text)
            };
            list = list.child(
                h_flex()
                    .flex_shrink_0()
                    .px(r(8.))
                    .gap(r(12.))
                    .child(column(None, ""))
                    .child(column(Some(130.), "Demandée"))
                    .child(column(Some(110.), "Installée"))
                    .child(column(Some(110.), if m.checked { "Dernière" } else { "" }))
                    .child(column(Some(24.), "")),
            );
            let mut kind = None;
            for (di, d) in m.deps.iter().enumerate() {
                if kind != Some(d.kind) {
                    kind = Some(d.kind);
                    let n = m.deps.iter().filter(|x| x.kind == d.kind).count();
                    list = list.child(ui::section_title(&format!("{} · {n}", d.kind.label()), cx).flex_shrink_0().px(r(8.)).pt(r(8.)).pb(r(2.)));
                }
                list = list.child(self.render_dep(format!("dep-{mi}-{di}"), m, d, cx));
            }
        }
        list.into_any_element()
    }
}

impl Render for DepsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Another folder opened: its own dependencies.
        let root = self.workspace.upgrade().and_then(|w| w.read(cx).root().map(Path::to_path_buf));
        if root != self.root {
            self.root = root;
            self.reload(cx);
        }
        let me = tool("deps");
        let busy = self.busy.is_some();
        let manifests = self.manifests.take();
        let subtitle = match &manifests {
            None => "Lecture des manifestes…".to_string(),
            Some(ms) if ms.is_empty() => "Les paquets dont votre projet dépend.".to_string(),
            Some(ms) => {
                let n: usize = ms.iter().map(|m| m.deps.len()).sum();
                let mut ecos: Vec<&str> = ms.iter().map(|m| m.eco.label()).collect();
                ecos.dedup();
                let mut s = format!("{} · {}", plural(n, "dépendance"), ecos.join(", "));
                if ms.iter().any(|m| m.checked) {
                    let outdated = ms.iter().flat_map(|m| m.deps.iter().map(move |d| update_of(m, d))).flatten().count();
                    s += &match outdated {
                        0 => " · tout est à jour".to_string(),
                        1 => " · 1 mise à jour".to_string(),
                        k => format!(" · {k} mises à jour"),
                    };
                }
                s
            }
        };
        let empty = manifests.as_ref().is_none_or(|m| m.is_empty());
        let actions = vec![
            button("reload").ghost().small().icon(Icon::empty().path("icons/refresh.svg")).tooltip("Relire les manifestes").disabled(busy).on_click(cx.listener(|this, _, _, cx| {
                this.reload(cx);
                cx.notify();
            })),
            button("check").outline().small().label("Vérifier les mises à jour").disabled(busy || empty).on_click(cx.listener(|this, _, window, cx| this.start(false, window, cx))),
            button("audit").outline().small().label("Audit de sécurité").disabled(busy || empty).on_click(cx.listener(|this, _, window, cx| this.start(true, window, cx))),
        ]
        .into_iter()
        .map(IntoElement::into_any_element)
        .collect();
        let theme = cx.theme();
        let body = match (&manifests, self.busy, &self.failure) {
            _ if self.root.is_none() => ui::empty_state((me.icon)(), (me.accent)(theme), "Aucun dossier ouvert", "Ouvrez un projet pour voir ses dépendances.", cx).into_any_element(),
            (None, _, _) => ui::loading("Lecture des manifestes…", cx).into_any_element(),
            (Some(ms), _, _) if ms.is_empty() => ui::empty_state(
                (me.icon)(),
                (me.accent)(theme),
                "Aucun manifeste trouvé",
                "Akolade cherche package.json, Cargo.toml, pyproject.toml, requirements.txt et go.mod à la racine du projet et dans ses sous-dossiers directs.",
                cx,
            )
            .into_any_element(),
            (_, Some(text), _) => ui::loading(text, cx).into_any_element(),
            (_, _, Some((title, e))) => ui::error_state(title, e, cx)
                .child(button("back").outline().small().label("Retour à la liste").on_click(cx.listener(|this, _, _, cx| {
                    this.failure = None;
                    cx.notify();
                })))
                .into_any_element(),
            (Some(ms), _, _) => self.render_list(ms, cx),
        };
        self.manifests = manifests;
        v_flex().size_full().child(header(me, subtitle, actions, cx)).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_semver_step() {
        assert_eq!(version("^1.2.3"), Some((1, 2, 3)));
        assert_eq!(version("v0.4"), Some((0, 4, 0)));
        assert_eq!(version("*"), None);
        assert_eq!(bump("1.2.3", "1.2.4"), Some(Bump::Patch));
        assert_eq!(bump("1.2.3", "1.4.0"), Some(Bump::Minor));
        assert_eq!(bump("1.2.3", "2.0.0"), Some(Bump::Major));
        assert_eq!(bump("0.25.1", "0.26.0"), Some(Bump::Major));
        assert_eq!(bump("v1.2.3", "v1.2.3"), None);
        assert_eq!(bump("2.0.0", "1.9.0"), None);
    }

    #[test]
    fn package_json_with_lock_and_node_modules() {
        let text = r#"{
  "name": "web",
  "dependencies": {
    "react": "^18.2.0",
    "@tanstack/query": "~5.0.0"
  },
  "devDependencies": {
    "vite": "^5.0.0"
  }
}"#;
        let lock = r#"{"lockfileVersion": 3, "packages": {"": {}, "node_modules/react": {"version": "18.2.0"}, "node_modules/@tanstack/query": {"version": "5.0.5"}}}"#;
        let deps = parse_package_json(text, Some(lock), |n| (n == "vite").then(|| "5.1.0".to_string())).unwrap();
        let react = deps.iter().find(|d| d.name == "react").unwrap();
        assert_eq!((react.kind, react.wanted.as_str(), react.installed.as_deref(), react.line), (Kind::Prod, "^18.2.0", Some("18.2.0"), 3));
        let query = deps.iter().find(|d| d.name == "@tanstack/query").unwrap();
        assert_eq!((query.installed.as_deref(), query.line), (Some("5.0.5"), 4));
        let vite = deps.iter().find(|d| d.name == "vite").unwrap();
        assert_eq!((vite.kind, vite.installed.as_deref(), vite.line), (Kind::Dev, Some("5.1.0"), 7));
        assert!(parse_package_json("{ nope", None, |_| None).is_err());
    }

    #[test]
    fn cargo_toml_sections_and_lock() {
        let text = r#"[package]
name = "app"
version = "0.1.0"

[dependencies]
serde = { version = "1", features = ["derive"] } # serialisation
anyhow = "1.0"
gpui = { git = "https://github.com/zed-industries/zed" }
local = { path = "../local" }
tokio.workspace = true
img = { package = "image", version = "0.25" }

[target.'cfg(target_os = "macos")'.dependencies]
cocoa = "0.26"

[dev-dependencies]
pretty_assertions = "1"

[build-dependencies.cc]
version = "1.0"
features = ["parallel"]
"#;
        let lock = r#"version = 4

[[package]]
name = "anyhow"
version = "1.0.86"

[[package]]
name = "image"
version = "0.24.9"

[[package]]
name = "image"
version = "0.25.2"
dependencies = [
 "byteorder",
]

[[package]]
name = "serde"
version = "1.0.203"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;
        let deps = parse_cargo_toml(text, Some(lock));
        let get = |n: &str| deps.iter().find(|d| d.name == n).unwrap();
        assert_eq!((get("serde").wanted.as_str(), get("serde").installed.as_deref(), get("serde").line), ("1", Some("1.0.203"), 5));
        assert_eq!(get("anyhow").installed.as_deref(), Some("1.0.86"));
        assert_eq!(get("gpui").wanted, "git");
        assert_eq!(get("local").wanted, "local");
        assert_eq!(get("tokio").wanted, "workspace");
        assert_eq!(get("img").installed.as_deref(), Some("0.25.2"));
        assert_eq!(get("cocoa").kind, Kind::Prod);
        assert_eq!(get("pretty_assertions").kind, Kind::Dev);
        assert_eq!((get("cc").kind, get("cc").wanted.as_str(), get("cc").line), (Kind::Build, "1.0", 18));
        assert_eq!(deps.len(), 9);
    }

    #[test]
    fn pyproject_requirements_and_go_mod() {
        let text = r#"[project]
name = "api"
dependencies = [
    "fastapi>=0.110",
    "requests[socks] >=2.31 ; python_version > '3.8'",
]

[project.optional-dependencies]
dev = ["pytest>=8", "ruff"]

[dependency-groups]
test = ["coverage", { include-group = "dev" }]

[tool.poetry.dependencies]
python = "^3.11"
httpx = { version = "^0.27", extras = ["http2"] }
"#;
        let deps = parse_pyproject(text);
        let names: Vec<(&str, Kind, &str)> = deps.iter().map(|d| (d.name.as_str(), d.kind, d.wanted.as_str())).collect();
        assert_eq!(
            names,
            [
                ("fastapi", Kind::Prod, ">=0.110"),
                ("requests", Kind::Prod, ">=2.31"),
                ("pytest", Kind::Dev, ">=8"),
                ("ruff", Kind::Dev, ""),
                ("coverage", Kind::Dev, ""),
                ("httpx", Kind::Prod, "^0.27")
            ]
        );
        assert_eq!(deps[1].line, 4);

        let reqs = parse_requirements("# app\n-r base.txt\nDjango==5.0.1  # web\n\n-e .\ngit+https://x/y.git\nnumpy\n", Kind::Prod);
        assert_eq!(reqs.iter().map(|d| (d.name.as_str(), d.wanted.as_str(), d.line)).collect::<Vec<_>>(), [("Django", "==5.0.1", 2), ("numpy", "", 6)]);

        let lock = "version = 1\n\n[[package]]\nname = \"django\"\nversion = \"5.0.1\"\nsource = { registry = \"https://pypi.org/simple\" }\ndependencies = [\n    { name = \"asgiref\" },\n]\n";
        let locked: Vec<_> = parse_lock(lock).into_iter().map(|(n, v)| (Eco::Python.key(&n), v)).collect();
        assert_eq!(pick(&locked, &Eco::Python.key("Django"), "==5.0.1").as_deref(), Some("5.0.1"));

        let go = "module example.com/app\n\ngo 1.22\n\nrequire github.com/spf13/cobra v1.8.0\n\nrequire (\n\tgolang.org/x/text v0.14.0 // indirect\n\tgithub.com/gin-gonic/gin v1.9.1\n)\n";
        let deps = parse_go_mod(go);
        assert_eq!(
            deps.iter().map(|d| (d.name.as_str(), d.kind, d.installed.as_deref(), d.line)).collect::<Vec<_>>(),
            [("github.com/spf13/cobra", Kind::Prod, Some("v1.8.0"), 4), ("golang.org/x/text", Kind::Indirect, Some("v0.14.0"), 7), ("github.com/gin-gonic/gin", Kind::Prod, Some("v1.9.1"), 8)]
        );
    }

    #[test]
    fn update_outputs() {
        let npm = r#"{
  "react": {"current": "18.2.0", "wanted": "18.3.1", "latest": "19.0.0", "dependent": "web", "location": "/p/node_modules/react"},
  "vite": [{"current": "5.0.0", "wanted": "5.4.0", "latest": "5.4.0", "dependent": "a"}]
}"#;
        let map = parse_npm_outdated(npm).unwrap();
        assert_eq!((map["react"].as_str(), map["vite"].as_str()), ("19.0.0", "5.4.0"));
        assert!(parse_npm_outdated("").unwrap().is_empty());
        assert_eq!(parse_npm_outdated(r#"{"error": {"code": "ENOLOCK", "summary": "No lockfile.\nRun npm i", "detail": ""}}"#), Err("No lockfile.".into()));

        let cargo = r#"{"crate_name":"app","dependencies":[{"name":"anyhow","project":"1.0.86","compat":"1.0.89","latest":"1.0.89","kind":"Normal","platform":null},{"name":"old","project":"0.1.0","compat":"---","latest":"Removed","kind":"Normal","platform":null}]}"#;
        let map = parse_cargo_outdated(&format!("    Updating crates.io index\n{cargo}\n")).unwrap();
        assert_eq!(map.get("anyhow").map(String::as_str), Some("1.0.89"));
        assert!(!map.contains_key("old"));

        let pip = r#"[{"name": "requests", "version": "2.31.0", "latest_version": "2.32.3", "latest_filetype": "wheel"}]"#;
        assert_eq!(parse_pip_outdated(pip).unwrap()["requests"], "2.32.3");

        let go = "{\n\t\"Path\": \"example.com/app\",\n\t\"Main\": true\n}\n{\n\t\"Path\": \"github.com/spf13/cobra\",\n\t\"Version\": \"v1.8.0\",\n\t\"Update\": {\n\t\t\"Path\": \"github.com/spf13/cobra\",\n\t\t\"Version\": \"v1.8.1\"\n\t}\n}\n{\"Path\": \"golang.org/x/text\", \"Version\": \"v0.14.0\"}\n";
        let map = parse_go_updates(go).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map["github.com/spf13/cobra"], "v1.8.1");
    }

    #[test]
    fn audit_outputs() {
        let npm = r#"{"auditReportVersion": 2, "vulnerabilities": {
  "minimist": {"name": "minimist", "severity": "critical", "isDirect": false, "via": [{"source": 1096, "name": "minimist", "title": "Prototype Pollution in minimist", "severity": "critical", "range": "<0.2.4"}], "effects": ["mkdirp"], "range": "<0.2.4", "fixAvailable": true},
  "mkdirp": {"name": "mkdirp", "severity": "moderate", "isDirect": true, "via": ["minimist"], "range": "0.4.1 - 0.5.1", "fixAvailable": {"name": "mkdirp", "version": "3.0.1", "isSemVerMajor": true}}
}, "metadata": {"vulnerabilities": {"critical": 1, "moderate": 1, "total": 2}}}"#;
        let list = parse_npm_audit(npm).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].package.as_str(), list[0].severity, list[0].title.as_str(), list[0].fixed.as_str()), ("minimist", Severity::Critical, "Prototype Pollution in minimist", "npm audit fix"));
        assert_eq!((list[1].title.as_str(), list[1].fixed.as_str()), ("Par minimist", "mkdirp@3.0.1"));

        let cargo = r#"{"database": {"advisory-count": 600}, "lockfile": {"dependency-count": 120}, "vulnerabilities": {"found": true, "count": 1, "list": [
  {"advisory": {"id": "RUSTSEC-2020-0071", "package": "time", "title": "Potential segfault in the time crate", "cvss": "CVSS:3.1/AV:N/AC:H/PR:N/UI:N/S:U/C:N/I:N/A:H"},
   "versions": {"patched": [">=0.2.23"], "unaffected": ["=0.2.0"]}, "package": {"name": "time", "version": "0.1.45"}}]}, "warnings": {}}"#;
        let list = parse_cargo_audit(cargo).unwrap();
        assert_eq!(
            (list[0].package.as_str(), list[0].version.as_str(), list[0].severity, list[0].title.as_str(), list[0].fixed.as_str()),
            ("time", "0.1.45", Severity::Moderate, "RUSTSEC-2020-0071 · Potential segfault in the time crate", ">=0.2.23")
        );
        assert!(parse_cargo_audit(r#"{"vulnerabilities": {"found": false, "count": 0, "list": []}}"#).unwrap().is_empty());

        let pip = r#"{"dependencies": [{"name": "jinja2", "version": "2.4.1", "vulns": [{"id": "PYSEC-2019-217", "fix_versions": ["2.10.1"], "aliases": ["CVE-2019-10906"], "description": "In Pallets Jinja before 2.10.1, str.format_map allows a sandbox escape.\nMore."}]}, {"name": "flask", "version": "3.0.0", "vulns": []}, {"name": "mypkg", "skip_reason": "not on PyPI"}], "fixes": []}"#;
        let list = parse_pip_audit(pip).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].package.as_str(), list[0].fixed.as_str()), ("jinja2", "2.10.1"));
        assert_eq!(list[0].title, "PYSEC-2019-217 · In Pallets Jinja before 2.10.1, str.format_map allows a sandbox escape.");
    }

    #[test]
    fn cvss_vectors_give_a_rating() {
        assert_eq!(Severity::from_cvss("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"), Severity::Critical); // 9.8
        assert_eq!(Severity::from_cvss("CVSS:3.1/AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N"), Severity::Moderate); // 6.1
        assert_eq!(Severity::from_cvss("CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N"), Severity::Moderate); // 5.5
        assert_eq!(Severity::from_cvss("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:H"), Severity::High); // 7.5
        assert_eq!(Severity::from_cvss("nonsense"), Severity::Unknown);
    }
}
