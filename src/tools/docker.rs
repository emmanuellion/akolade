//! "Docker": the containers (`docker ps -a`), grouped by Compose project, with
//! start / stop / restart / remove, and logs or a shell in the terminal.

use std::{process::Command, time::Duration};

use gpui::{
    Context, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, SharedString, StatefulInteractiveElement, Styled, Task, WeakEntity, Window, div,
    prelude::FluentBuilder,
};
use gpui_component::{ActiveTheme, Disableable, Icon, Sizable, WindowExt, button::ButtonVariants, h_flex, notification::Notification, v_flex};
use serde::Deserialize;

use crate::{
    tools::{header, in_terminal, tool},
    ui,
    workspace::{Workspace, button, r, shell_quote},
};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Container {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Names")]
    pub name: String,
    #[serde(rename = "Image")]
    pub image: String,
    /// "running", "exited", "paused"…
    #[serde(rename = "State")]
    pub state: String,
    /// "Up 2 hours", "Exited (0) 3 days ago".
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "Ports")]
    pub ports: String,
    #[serde(rename = "Labels")]
    labels: String,
}

impl Container {
    pub fn running(&self) -> bool {
        self.state == "running"
    }

    /// Its Compose project, if any.
    pub fn project(&self) -> Option<&str> {
        self.labels.split(',').find_map(|l| l.strip_prefix("com.docker.compose.project="))
    }
}

/// Docker's English status in French: "Up 2 hours (healthy)" → "En marche depuis
/// 2 heures", "Exited (0) 3 days ago" → "Arrêté (code 0) il y a 3 jours".
pub fn status_fr(status: &str) -> String {
    let duration = |en: &str| {
        let en = en.replace("About an hour", "1 hour").replace("About a minute", "1 minute").replace("Less than a second", "1 second");
        crate::git::relative_fr(&format!("{} ago", en.trim())).trim_start_matches("il y a ").to_string()
    };
    if let Some(up) = status.strip_prefix("Up ") {
        let up = up.split(" (").next().unwrap_or(up);
        return format!("En marche depuis {}", duration(up));
    }
    if let Some(rest) = status.strip_prefix("Exited (")
        && let Some((code, ago)) = rest.split_once(") ")
    {
        return format!("Arrêté (code {code}) il y a {}", duration(ago.trim_end_matches(" ago")));
    }
    match status {
        "Created" => "Créé, jamais démarré".into(),
        s => s.to_string(),
    }
}

/// `docker ps -a --format '{{json .}}'`: one JSON object per line.
pub fn parse_ps(out: &str) -> Vec<Container> {
    out.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn docker(args: &[&str]) -> Result<String, String> {
    let out = Command::new("docker").args(args).output().map_err(|e| format!("docker : {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if ["Cannot connect to the Docker daemon", "docker daemon", "failed to connect to the docker API", "docker.sock"].iter().any(|m| err.contains(m)) {
        return Err("Docker n'est pas lancé : ouvrez Docker Desktop.".into());
    }
    Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("erreur inconnue").to_string())
}

pub fn containers() -> Result<Vec<Container>, String> {
    docker(&["ps", "-a", "--format", "{{json .}}"]).map(|o| parse_ps(&o))
}

pub struct DockerView {
    workspace: WeakEntity<Workspace>,
    list: Result<Vec<Container>, String>,
    loaded: bool,
    /// Container whose action runs now.
    busy: Option<String>,
    /// Drawn since the last poll: a hidden tab stops polling.
    rendered: bool,
    /// A poll was skipped while hidden: refresh once shown again.
    stale: bool,
    _poll: Task<()>,
}

impl DockerView {
    pub fn new(workspace: WeakEntity<Workspace>, cx: &mut Context<Self>) -> Self {
        let _poll = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let shown = this.update(cx, |this, cx| match std::mem::take(&mut this.rendered) {
                    true => this.refresh(cx),
                    false => this.stale = true,
                });
                if shown.is_err() {
                    break;
                }
            }
        });
        let mut view = Self { workspace, list: Ok(vec![]), loaded: false, busy: None, rendered: false, stale: false, _poll };
        view.refresh(cx);
        view
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let list = cx.background_executor().spawn(async { containers() }).await;
            let _ = this.update(cx, |this, cx| {
                (this.list, this.loaded) = (list, true);
                cx.notify();
            });
        })
        .detach();
    }

    fn act(&mut self, c: &Container, action: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let (id, name) = (c.id.clone(), c.name.clone());
        self.busy = Some(id.clone());
        cx.notify();
        let task = cx.background_executor().spawn(async move { docker(&[action, &id]).and_then(|_| containers()) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(list) => this.list = Ok(list),
                    Err(e) => window.push_notification(Notification::error(format!("{name} : {e}")), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn remove(&mut self, c: &Container, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Supprimer le conteneur {} ?", c.name),
            Some("Ses fichiers qui ne sont pas dans un volume seront perdus. L'image reste installée."),
            &["Supprimer", "Annuler"],
            cx,
        );
        let c = c.clone();
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| this.act(&c, "rm", window, cx));
            }
        })
        .detach();
    }

    fn render_row(&self, c: &Container, cx: &mut Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let color: Hsla = match c.state.as_str() {
            "running" => theme.green,
            "paused" | "restarting" => theme.yellow,
            _ => muted,
        };
        let busy = self.busy.as_deref() == Some(c.id.as_str());
        let key = c.id.clone();
        let btn = |id: &str| button(SharedString::from(format!("{id}-{key}"))).ghost().small().disabled(busy);
        let (c1, c2, c3, c4, c5) = (c.clone(), c.clone(), c.clone(), c.clone(), c.clone());
        let quoted = shell_quote(&c.name);
        let (logs, shell) = (format!("docker logs -f --tail 200 {quoted}"), format!("docker exec -it {quoted} sh"));
        h_flex()
            .flex_shrink_0()
            .h(r(60.))
            .px(r(16.))
            .gap(r(14.))
            .rounded(r(10.))
            .border_1()
            .border_color(border)
            .bg(theme.foreground.opacity(0.02))
            .child(div().size(r(9.)).flex_shrink_0().rounded_full().bg(color))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(h_flex().min_w_0().gap(r(8.)).child(div().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(c.name.clone())).child(div().min_w_0().truncate().text_size(r(12.)).text_color(muted).child(c.image.clone())))
                    .child(div().truncate().text_size(r(12.)).text_color(muted).child(if c.ports.is_empty() { status_fr(&c.status) } else { format!("{} · {}", status_fr(&c.status), c.ports) })),
            )
            .map(|row| {
                if c.running() {
                    row.child(btn("logs").icon(Icon::empty().path("icons/list-play.svg")).label("Logs").on_click(cx.listener(move |this, _, window, cx| in_terminal(&this.workspace, &logs, window, cx))))
                        .child(btn("shell").icon(gpui_component::IconName::SquareTerminal).label("Shell").on_click(cx.listener(move |this, _, window, cx| in_terminal(&this.workspace, &shell, window, cx))))
                        .child(btn("restart").icon(Icon::empty().path("icons/refresh.svg")).tooltip("Redémarrer").on_click(cx.listener(move |this, _, window, cx| this.act(&c1, "restart", window, cx))))
                        .child(btn("stop").outline().label("Arrêter").loading(busy).on_click(cx.listener(move |this, _, window, cx| this.act(&c2, "stop", window, cx))))
                } else if c.state == "paused" {
                    // Docker refuses start and rm on a paused container.
                    row.child(btn("unpause").primary().icon(Icon::empty().path("icons/play.svg")).label("Reprendre").loading(busy).on_click(cx.listener(move |this, _, window, cx| this.act(&c5, "unpause", window, cx))))
                } else {
                    row.child(btn("rm").label("Supprimer").on_click(cx.listener(move |this, _, window, cx| this.remove(&c3, window, cx))))
                        .child(btn("start").primary().icon(Icon::empty().path("icons/play.svg")).label("Démarrer").loading(busy).on_click(cx.listener(move |this, _, window, cx| this.act(&c4, "start", window, cx))))
                }
            })
    }
}

impl Render for DockerView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rendered = true;
        if std::mem::take(&mut self.stale) {
            self.refresh(cx);
        }
        let muted = cx.theme().muted_foreground;
        let list = self.list.clone();
        let subtitle = match &list {
            Ok(l) if self.loaded => {
                let running = l.iter().filter(|c| c.running()).count();
                format!("{running} en marche sur {} conteneur{}", l.len(), if l.len() > 1 { "s" } else { "" })
            }
            _ => "Vos conteneurs Docker.".into(),
        };
        let header = header(tool("docker"), subtitle, vec![], cx);
        let body = match list {
            Err(e) => ui::error_state("Docker indisponible", &e, cx).into_any_element(),
            Ok(_) if !self.loaded => ui::loading("Lecture des conteneurs…", cx).into_any_element(),
            Ok(l) if l.is_empty() => ui::empty_state((tool("docker").icon)(), cx.theme().cyan, "Aucun conteneur", "Lancez un conteneur (docker run, docker compose up…) : il apparaîtra ici.", cx).into_any_element(),
            Ok(l) => {
                // Compose projects first, by name; lone containers last.
                let mut groups: Vec<(Option<String>, Vec<Container>)> = vec![];
                for c in l {
                    let project = c.project().map(String::from);
                    match groups.iter_mut().find(|(p, _)| *p == project) {
                        Some((_, list)) => list.push(c),
                        None => groups.push((project, vec![c])),
                    }
                }
                groups.sort_by_key(|(p, _)| (p.is_none(), p.clone()));
                v_flex()
                    .id("containers")
                    .flex_1()
                    .overflow_y_scroll()
                    .p(r(16.))
                    .gap(r(18.))
                    .children(groups.into_iter().map(|(project, list)| {
                        let rows: Vec<gpui::Div> = list.iter().map(|c| self.render_row(c, cx)).collect();
                        v_flex()
                            .flex_shrink_0()
                            .gap(r(8.))
                            .child(
                                div()
                                    .text_size(r(12.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(muted)
                                    .child(project.map_or("CONTENEURS SEULS".to_string(), |p| format!("PROJET {}", p.to_uppercase()))),
                            )
                            .children(rows)
                    }))
                    .into_any_element()
            }
        };
        v_flex().size_full().child(header).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_ps_lines_become_containers_with_their_project() {
        let out = r#"{"ID":"1019b5","Image":"qdrant/qdrant","Labels":"a=b,com.docker.compose.project=poc-rag,c=d","Names":"poc-rag-qdrant-1","Ports":"","State":"exited","Status":"Exited (143) 42 hours ago","Platform":{"os":"linux"}}
{"ID":"22","Image":"redis","Labels":"","Names":"cache","Ports":"0.0.0.0:6379->6379/tcp","State":"running","Status":"Up 2 hours"}
not json"#;
        let list = parse_ps(out);
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].name.as_str(), list[0].project(), list[0].running()), ("poc-rag-qdrant-1", Some("poc-rag"), false));
        assert_eq!((list[1].project(), list[1].running()), (None, true));
    }

    #[test]
    fn statuses_read_in_french() {
        assert_eq!(status_fr("Exited (143) 42 hours ago"), "Arrêté (code 143) il y a 42 heures");
        assert_eq!(status_fr("Up 2 hours (healthy)"), "En marche depuis 2 heures");
        assert_eq!(status_fr("Up About a minute"), "En marche depuis 1 minute");
        assert_eq!(status_fr("Created"), "Créé, jamais démarré");
    }
}
