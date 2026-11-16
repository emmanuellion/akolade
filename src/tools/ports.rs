//! "Ports ouverts": the programs listening on a TCP port (`lsof`), to open in
//! the browser or stop.

use std::time::Duration;

use gpui::{Context, FontWeight, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, SharedString, StatefulInteractiveElement, Styled, Task, Window, div, prelude::FluentBuilder};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, WindowExt, h_flex, notification::Notification, v_flex};

use crate::{
    tools::{header, tool},
    ui,
    workspace::{button, r},
};

#[derive(Clone, Debug, PartialEq)]
pub struct Listener {
    pub port: u16,
    /// "*", "127.0.0.1", "[::1]"…
    pub address: String,
    pub pid: u32,
    pub command: String,
}

/// `lsof -F pcn` output: `p<pid>`, then `c<command>`, then one `n<address:port>`
/// per socket. One entry per program and port (IPv4 and IPv6 alike).
pub fn parse_lsof(out: &str) -> Vec<Listener> {
    let (mut pid, mut command) = (0, String::new());
    let mut list: Vec<Listener> = vec![];
    for line in out.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "c" => command = value.to_string(),
            "n" => {
                let Some((address, port)) = value.rsplit_once(':') else { continue };
                let Ok(port) = port.parse() else { continue };
                if !list.iter().any(|l| l.pid == pid && l.port == port) {
                    list.push(Listener { port, address: address.to_string(), pid, command: command.clone() });
                }
            }
            _ => {}
        }
    }
    list.sort_by_key(|l| (l.port, l.pid));
    list
}

pub fn listeners() -> Result<Vec<Listener>, String> {
    let out = crate::platform::command("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pcn"]).output().map_err(|e| format!("lsof : {e}"))?;
    // Exit code 1 with no output: nothing listens.
    Ok(parse_lsof(&String::from_utf8_lossy(&out.stdout)))
}

pub struct PortsView {
    list: Result<Vec<Listener>, String>,
    loaded: bool,
    /// Drawn since the last poll: a hidden tab stops polling.
    rendered: bool,
    /// A poll was skipped while hidden: refresh once shown again.
    stale: bool,
    _poll: Task<()>,
}

impl PortsView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Servers start and stop on their own: look again every few seconds.
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
        let mut view = Self { list: Ok(vec![]), loaded: false, rendered: false, stale: false, _poll };
        view.refresh(cx);
        view
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let list = cx.background_executor().spawn(async { listeners() }).await;
            let _ = this.update(cx, |this, cx| {
                (this.list, this.loaded) = (list, true);
                cx.notify();
            });
        })
        .detach();
    }

    fn stop(&mut self, l: &Listener, window: &mut Window, cx: &mut Context<Self>) {
        if l.pid == std::process::id() {
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Arrêter {} (port {}) ?", l.command, l.port),
            Some("Le programme reçoit une demande d'arrêt, comme un Ctrl+C. Son travail en cours non enregistré peut être perdu."),
            &["Arrêter", "Annuler"],
            cx,
        );
        let (pid, port, name) = (l.pid, l.port, l.command.clone());
        cx.spawn_in(window, async move |_, cx| {
            if answer.await != Ok(0) {
                return;
            }
            // The prompt may have stayed open long enough for the program to quit and its PID to be reused.
            let check = name.clone();
            let killed = cx
                .background_executor()
                .spawn(async move {
                    let same = listeners().is_ok_and(|list| list.iter().any(|x| (x.pid, x.port, &x.command) == (pid, port, &check)));
                    same.then(|| crate::platform::kill(pid))
                })
                .await;
            let _ = cx.update(|window, cx| {
                let n = match killed {
                    Some(true) => Notification::success(format!("{name} arrêté.")),
                    Some(false) => Notification::error(format!("Impossible d'arrêter {name} (PID {pid}).")),
                    None => Notification::warning(format!("{name} n'écoute plus sur le port {port} : rien n'a été arrêté.")),
                };
                window.push_notification(n, cx);
            });
        })
        .detach();
    }
}

impl Render for PortsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rendered = true;
        if std::mem::take(&mut self.stale) {
            self.refresh(cx);
        }
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let subtitle = match &self.list {
            Ok(l) if self.loaded => format!("{} port{} en écoute · mis à jour toutes les 3 secondes", l.len(), if l.len() > 1 { "s" } else { "" }),
            _ => "Les programmes qui attendent des connexions sur votre Mac.".into(),
        };
        let rows: Vec<_> = match &self.list {
            Ok(list) => list.clone(),
            Err(_) => vec![],
        };
        let body = match &self.list {
            _ if !self.loaded => ui::loading("Lecture des ports…", cx).into_any_element(),
            Err(e) => ui::error_state("Impossible de lire les ports", e, cx).into_any_element(),
            Ok(l) if l.is_empty() => ui::empty_state((tool("ports").icon)(), theme.magenta, "Aucun port ouvert", "Lancez un serveur (npm run dev, python -m http.server…) : il apparaîtra ici.", cx).into_any_element(),
            Ok(_) => v_flex()
                .id("ports")
                .flex_1()
                .overflow_y_scroll()
                .p(r(16.))
                .gap(r(8.))
                .children(rows.into_iter().map(|l| {
                    let key = format!("{}-{}", l.pid, l.port);
                    let (open, stop) = (l.port, l.clone());
                    h_flex()
                        .flex_shrink_0()
                        .h(r(56.))
                        .px(r(16.))
                        .gap(r(16.))
                        .rounded(r(10.))
                        .border_1()
                        .border_color(border)
                        .bg(theme.foreground.opacity(0.02))
                        .child(div().w(r(84.)).flex_shrink_0().font_family(theme.mono_font_family.clone()).text_size(r(16.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.primary).child(format!(":{}", l.port)))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().font_weight(FontWeight::MEDIUM).child(l.command.clone()))
                                .child(div().text_size(r(12.)).text_color(muted).child(format!("PID {} · {}", l.pid, if l.address == "*" { "toutes les adresses".to_string() } else { l.address.clone() }))),
                        )
                        .child(button(SharedString::from(format!("open-{key}"))).outline().small().icon(Icon::new(IconName::ExternalLink)).label("Ouvrir").on_click(move |_, _, _| {
                            crate::platform::open(format!("http://localhost:{open}"));
                        }))
                        // Akolade's own servers (the preview): stopping one would quit Akolade without asking to save.
                        .when(l.pid != std::process::id(), |d| {
                            d.child(button(SharedString::from(format!("stop-{key}"))).outline().small().label("Arrêter").on_click(cx.listener(move |this, _, window, cx| this.stop(&stop, window, cx))))
                        })
                }))
                .into_any_element(),
        };
        v_flex().size_full().child(header(tool("ports"), subtitle, vec![], cx)).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_output_gives_one_entry_per_program_and_port() {
        let out = "p501\ncnode\nn*:3000\nn[::]:3000\np88\ncpostgres\nn127.0.0.1:5432\nn[::1]:5432\nnbad\n";
        let list = parse_lsof(out);
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].port, list[0].pid, list[0].command.as_str(), list[0].address.as_str()), (3000, 501, "node", "*"));
        assert_eq!((list[1].port, list[1].address.as_str()), (5432, "127.0.0.1"));
        assert!(parse_lsof("").is_empty());
    }
}
