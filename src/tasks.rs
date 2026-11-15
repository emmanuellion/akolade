//! "Scripts" panel: the project's own commands (package.json scripts, Makefile
//! targets, Cargo, Go), run in the integrated terminal with one click.

use std::{fs, path::Path};

use gpui::{Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div, prelude::FluentBuilder};
use gpui_component::{ActiveTheme, Icon, Sizable, button::ButtonVariants, h_flex, tooltip::Tooltip, v_flex};

use crate::{
    icons,
    workspace::{Workspace, button, r, shell_quote},
};

#[derive(Clone, Debug, PartialEq)]
pub struct Script {
    /// "npm", "Make", "Cargo", "Go".
    pub source: &'static str,
    pub name: String,
    pub command: String,
    /// What it really runs: a package.json script's own command line, else `command`.
    pub runs: String,
}

/// A name the shell takes as is, else quoted.
fn arg(name: &str) -> String {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || "-_:./@+=".contains(c)) { name.to_string() } else { shell_quote(name) }
}

/// `package.json` scripts, run with the project's package manager (from its lock file).
fn npm(root: &Path) -> Vec<Script> {
    let Ok(text) = fs::read_to_string(root.join("package.json")) else { return vec![] };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![] };
    let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) else { return vec![] };
    let runner = match () {
        _ if root.join("pnpm-lock.yaml").exists() => "pnpm run",
        _ if root.join("yarn.lock").exists() => "yarn",
        _ if root.join("bun.lockb").exists() || root.join("bun.lock").exists() => "bun run",
        _ => "npm run",
    };
    scripts
        .iter()
        .map(|(name, body)| Script { source: "npm", name: name.clone(), command: format!("{runner} {}", arg(name)), runs: body.as_str().unwrap_or_default().to_string() })
        .collect()
}

/// Targets of a Makefile: `name:` at the start of a line, not `.PHONY`, not `x := y`.
pub fn make_targets(makefile: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for line in makefile.lines() {
        let Some((head, rest)) = line.split_once(':') else { continue };
        if rest.starts_with('=') || head.starts_with(['.', '\t', ' ', '#']) || head.contains(['=', '$', '%']) {
            continue;
        }
        for target in head.split_whitespace() {
            if !out.iter().any(|t| t == target) {
                out.push(target.to_string());
            }
        }
    }
    out
}

/// Every script of the project, in this order: npm, Make, Cargo, Go.
pub fn detect(root: &Path) -> Vec<Script> {
    let mut out = npm(root);
    if let Some(text) = ["Makefile", "makefile", "GNUmakefile"].iter().find_map(|f| fs::read_to_string(root.join(f)).ok()) {
        out.extend(make_targets(&text).into_iter().map(|t| {
            let command = format!("make {}", arg(&t));
            Script { source: "Make", runs: command.clone(), command, name: t }
        }));
    }
    let fixed = |source, list: &[(&str, &str)]| list.iter().map(|(name, command)| Script { source, name: name.to_string(), command: command.to_string(), runs: command.to_string() }).collect::<Vec<_>>();
    if root.join("Cargo.toml").exists() {
        out.extend(fixed("Cargo", &[("build", "cargo build"), ("run", "cargo run"), ("test", "cargo test"), ("check", "cargo check")]));
    }
    if root.join("go.mod").exists() {
        out.extend(fixed("Go", &[("run", "go run ."), ("build", "go build ./..."), ("test", "go test ./...")]));
    }
    out
}

fn source_icon(source: &str) -> &'static str {
    match source {
        "npm" => "icons/files/npm.svg",
        "Cargo" => "icons/files/rust.svg",
        "Go" => "icons/files/go.svg",
        _ => "icons/files/shell.svg",
    }
}

pub struct TasksView {
    workspace: WeakEntity<Workspace>,
    scripts: Vec<Script>,
    has_root: bool,
}

impl TasksView {
    pub fn new(workspace: WeakEntity<Workspace>) -> Self {
        Self { workspace, scripts: vec![], has_root: false }
    }

    /// Reads the project's scripts again (the folder given: called inside a workspace update).
    pub fn load(&mut self, root: Option<&Path>, cx: &mut Context<Self>) {
        self.has_root = root.is_some();
        self.scripts = root.map(detect).unwrap_or_default();
        cx.notify();
    }

    fn run(&self, s: &Script, window: &mut Window, cx: &mut Context<Self>) {
        let (command, runs) = (s.command.clone(), s.runs.clone());
        if let Some(ws) = self.workspace.upgrade() {
            ws.update(cx, |ws, cx| ws.run_project_command(command, runs, window, cx));
        }
    }
}

impl Render for TasksView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, hover) = (theme.muted_foreground, theme.foreground.opacity(0.05));
        let mut sources: Vec<&'static str> = self.scripts.iter().map(|s| s.source).collect();
        sources.dedup();
        let empty = match () {
            _ if !self.has_root => Some("Ouvrez un dossier pour voir ses scripts."),
            _ if self.scripts.is_empty() => Some("Aucun script : Akolade lit package.json, Makefile, Cargo.toml et go.mod à la racine du projet."),
            _ => None,
        };
        v_flex()
            .id("scripts")
            .size_full()
            .overflow_y_scroll()
            .px(r(6.))
            .pb(r(8.))
            .gap(r(10.))
            .when_some(empty, |d, text| d.child(div().px(r(8.)).pt(r(4.)).text_size(r(12.5)).text_color(muted).child(text)))
            .children(sources.into_iter().map(|source| {
                v_flex()
                    .flex_shrink_0()
                    .child(
                        h_flex()
                            .h(r(24.))
                            .px(r(8.))
                            .gap(r(6.))
                            .child(icons::asset(source_icon(source)).size(r(13.)))
                            .child(div().text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(source.to_uppercase())),
                    )
                    .children(self.scripts.iter().filter(|s| s.source == source).map(|s| {
                        let id = SharedString::from(format!("script-{}-{}", s.source, s.name));
                        let script = s.clone();
                        h_flex()
                            .id(id.clone())
                            .group(id.clone())
                            .flex_shrink_0()
                            .py(r(5.))
                            .px(r(8.))
                            .gap(r(8.))
                            .rounded(r(6.))
                            .cursor_pointer()
                            .hover(|d| d.bg(hover))
                            .tooltip({
                                let runs = SharedString::from(s.runs.clone());
                                move |window, cx| Tooltip::new(runs.clone()).build(window, cx)
                            })
                            .on_click(cx.listener(move |this, _, window, cx| this.run(&script, window, cx)))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().truncate().text_size(r(13.)).font_weight(FontWeight::MEDIUM).child(s.name.clone()))
                                    .child(div().truncate().font_family(theme.mono_font_family.clone()).text_size(r(11.)).text_color(muted).child(s.runs.clone())),
                            )
                            .child(
                                button(SharedString::from(format!("run-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(Icon::empty().path("icons/play.svg").text_color(theme.green))
                                    // The click goes on to the row, which runs it.
                                    .tooltip("Lancer dans le terminal"),
                            )
                    }))
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makefile_targets_skip_variables_and_specials() {
        let mk = "CC := gcc\nX = 1\n.PHONY: all test\nall: build\nbuild test: deps\n\t@echo a:b\n%.o: %.c\n# note: x\n$(OUT): y\n";
        assert_eq!(make_targets(mk), vec!["all", "build", "test"]);
    }

    #[test]
    fn detects_scripts_with_the_right_runner() {
        let dir = std::env::temp_dir().join(format!("akolade-tasks-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("package.json"), r#"{"scripts": {"dev": "vite", "build:prod": "vite build", "a b": "x"}}"#).unwrap();
        fs::write(dir.join("pnpm-lock.yaml"), "").unwrap();
        fs::write(dir.join("go.mod"), "module x").unwrap();
        let scripts = detect(&dir);
        assert_eq!(scripts[1].runs, "vite build", "the script's own command, shown before running it");
        let commands: Vec<String> = scripts.into_iter().map(|s| s.command).collect();
        assert_eq!(&commands[..3], ["pnpm run dev", "pnpm run build:prod", "pnpm run 'a b'"], "in package.json's order");
        assert!(commands.contains(&"go test ./...".to_string()));
        fs::remove_dir_all(dir).unwrap();
    }
}
