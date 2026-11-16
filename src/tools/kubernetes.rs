//! "Kubernetes": the pods, deployments and services of kubectl's current
//! context, with logs, a shell, `describe` or port-forward in the terminal,
//! and delete / restart / scale after a confirmation.

use std::{ffi::OsStr, time::Duration};

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, AppContext, Context, Div, Entity, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder,
};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    popover::Popover,
    v_flex,
};
use serde::{Deserialize, de::DeserializeOwned, de::IgnoredAny};

use crate::{
    tools::{header, in_terminal, tool},
    ui,
    workspace::{Workspace, button, r, shell_quote},
};

const UNREACHABLE: &str = "Aucun cluster joignable : démarrez Docker Desktop Kubernetes, minikube… ou vérifiez le contexte.";
const NO_CONTEXTS: &str = "Aucun contexte Kubernetes configuré : kubectl ne connaît aucun cluster (~/.kube/config).";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Pods,
    Deployments,
    Services,
}

impl Kind {
    fn resource(self) -> &'static str {
        match self {
            Kind::Pods => "pods",
            Kind::Deployments => "deployments",
            Kind::Services => "services",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pod {
    pub name: String,
    pub namespace: String,
    /// Ready containers, containers.
    pub ready: (usize, usize),
    /// As kubectl shows it: "Running", "CrashLoopBackOff", "Completed"…
    pub status: String,
    pub restarts: u32,
    pub created: String,
    pub node: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Deployment {
    pub name: String,
    pub namespace: String,
    /// Wanted pods.
    pub replicas: u32,
    pub ready: u32,
    pub up_to_date: u32,
    pub available: u32,
    pub created: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    pub name: String,
    pub namespace: String,
    /// "ClusterIP", "NodePort", "LoadBalancer"…
    pub kind: String,
    pub cluster_ip: String,
    pub ports: Vec<SvcPort>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct SvcPort {
    pub port: u16,
    pub protocol: String,
    pub node_port: Option<u16>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Items {
    Pods(Vec<Pod>),
    Deployments(Vec<Deployment>),
    Services(Vec<Service>),
}

impl Items {
    fn kind(&self) -> Kind {
        match self {
            Items::Pods(_) => Kind::Pods,
            Items::Deployments(_) => Kind::Deployments,
            Items::Services(_) => Kind::Services,
        }
    }
}

// The bits of `kubectl get <kind> -o json` we read.
#[derive(Deserialize)]
struct List<T> {
    items: Vec<T>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Meta {
    name: String,
    namespace: String,
    creation_timestamp: String,
    deletion_timestamp: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct PodJson {
    metadata: Meta,
    spec: PodSpec,
    status: PodStatus,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PodSpec {
    node_name: String,
    containers: Vec<IgnoredAny>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PodStatus {
    phase: String,
    reason: String,
    init_container_statuses: Vec<ContainerStatus>,
    container_statuses: Vec<ContainerStatus>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ContainerStatus {
    ready: bool,
    restart_count: u32,
    state: ContainerState,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ContainerState {
    running: Option<IgnoredAny>,
    waiting: Option<StateReason>,
    terminated: Option<StateReason>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StateReason {
    reason: String,
    exit_code: i32,
    signal: i32,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct DeployJson {
    metadata: Meta,
    spec: DeploySpec,
    status: DeployStatus,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct DeploySpec {
    replicas: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct DeployStatus {
    ready_replicas: u32,
    updated_replicas: u32,
    available_replicas: u32,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SvcJson {
    metadata: Meta,
    spec: SvcSpec,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct SvcSpec {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "clusterIP")]
    cluster_ip: String,
    ports: Vec<SvcPort>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Named {
    metadata: Meta,
}

fn items<T: DeserializeOwned>(json: &str) -> Result<Vec<T>, String> {
    serde_json::from_str::<List<T>>(json).map(|l| l.items).map_err(|e| format!("Réponse de kubectl illisible : {e}"))
}

fn terminated(t: &StateReason) -> String {
    match () {
        _ if !t.reason.is_empty() => t.reason.clone(),
        _ if t.signal != 0 => format!("Signal:{}", t.signal),
        _ => format!("ExitCode:{}", t.exit_code),
    }
}

/// The STATUS column of `kubectl get pods` (same rules as kubectl's printer).
fn pod_status(p: &PodJson) -> String {
    let s = &p.status;
    let mut reason = if s.reason.is_empty() { s.phase.clone() } else { s.reason.clone() };
    let mut initializing = false;
    for (i, c) in s.init_container_statuses.iter().enumerate() {
        match (&c.state.terminated, &c.state.waiting) {
            (Some(t), _) if t.exit_code == 0 => continue,
            (Some(t), _) => reason = format!("Init:{}", terminated(t)),
            (_, Some(w)) if !w.reason.is_empty() && w.reason != "PodInitializing" => reason = format!("Init:{}", w.reason),
            _ => reason = format!("Init:{i}/{}", s.init_container_statuses.len()),
        }
        initializing = true;
        break;
    }
    if !initializing {
        let mut running = false;
        // Backwards, like kubectl: the first container's reason wins.
        for c in s.container_statuses.iter().rev() {
            if let Some(w) = c.state.waiting.as_ref().filter(|w| !w.reason.is_empty()) {
                reason = w.reason.clone();
            } else if let Some(t) = &c.state.terminated {
                reason = terminated(t);
            } else if c.ready && c.state.running.is_some() {
                running = true;
            }
        }
        if reason == "Completed" && running {
            reason = "Running".into();
        }
    }
    if p.metadata.deletion_timestamp.is_some() {
        reason = "Terminating".into();
    }
    reason
}

pub fn parse_pods(json: &str) -> Result<Vec<Pod>, String> {
    Ok(items::<PodJson>(json)?
        .into_iter()
        .map(|p| Pod {
            status: pod_status(&p),
            ready: (p.status.container_statuses.iter().filter(|c| c.ready).count(), p.spec.containers.len()),
            restarts: p.status.container_statuses.iter().map(|c| c.restart_count).sum(),
            node: p.spec.node_name,
            name: p.metadata.name,
            namespace: p.metadata.namespace,
            created: p.metadata.creation_timestamp,
        })
        .collect())
}

pub fn parse_deployments(json: &str) -> Result<Vec<Deployment>, String> {
    Ok(items::<DeployJson>(json)?
        .into_iter()
        .map(|d| Deployment {
            name: d.metadata.name,
            namespace: d.metadata.namespace,
            created: d.metadata.creation_timestamp,
            replicas: d.spec.replicas.unwrap_or(1),
            ready: d.status.ready_replicas,
            up_to_date: d.status.updated_replicas,
            available: d.status.available_replicas,
        })
        .collect())
}

pub fn parse_services(json: &str) -> Result<Vec<Service>, String> {
    Ok(items::<SvcJson>(json)?
        .into_iter()
        .map(|s| Service { name: s.metadata.name, namespace: s.metadata.namespace, kind: s.spec.kind, cluster_ip: s.spec.cluster_ip, ports: s.spec.ports })
        .collect())
}

pub fn parse_namespaces(json: &str) -> Result<Vec<String>, String> {
    Ok(items::<Named>(json)?.into_iter().map(|n| n.metadata.name).collect())
}

/// "80:30080/TCP, 443/TCP", like kubectl's PORT(S) column.
pub fn ports_text(ports: &[SvcPort]) -> String {
    let text: Vec<String> = ports.iter().map(|p| format!("{}{}/{}", p.port, p.node_port.map(|n| format!(":{n}")).unwrap_or_default(), p.protocol)).collect();
    if text.is_empty() { "—".into() } else { text.join(", ") }
}

/// A resource's age, short and in French: "40 s", "12 min", "5 h", "3 j", "2 ans".
pub fn age_fr(created: &str, now: DateTime<Utc>) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(created) else { return "—".into() };
    match (now - t.with_timezone(&Utc)).num_seconds().max(0) {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86400 => format!("{} h", s / 3600),
        s if s < 365 * 86400 => format!("{} j", s / 86400),
        s => {
            let y = s / (365 * 86400);
            format!("{y} an{}", if y > 1 { "s" } else { "" })
        }
    }
}

/// kubectl's error, in plain French when it is a common one.
pub fn explain(stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    if ["unable to connect to the server", "connection refused", "no such host", "i/o timeout", "couldn't get current server api group list", "context deadline exceeded", "timeout"]
        .iter()
        .any(|m| lower.contains(m))
    {
        return UNREACHABLE.into();
    }
    if lower.contains("forbidden") {
        return "Accès refusé : votre compte n'a pas ce droit sur ce cluster (essayez un autre namespace ou contexte).".into();
    }
    let line = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("erreur inconnue");
    line.trim().trim_start_matches("error: ").to_string()
}

fn kubectl<S: AsRef<OsStr>>(args: &[S]) -> Result<String, String> {
    let out = crate::platform::command("kubectl").args(args).output().map_err(|e| format!("kubectl : {e}"))?;
    if out.status.success() { Ok(String::from_utf8_lossy(&out.stdout).into_owned()) } else { Err(explain(&String::from_utf8_lossy(&out.stderr))) }
}

/// What is shown: kubectl's config, then the cluster's lists.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub contexts: Vec<String>,
    pub context: String,
    /// The context's own namespace.
    pub default_ns: String,
    pub namespaces: Vec<String>,
    pub items: Result<Items, String>,
}

/// What to list: the kind, the namespace picked (else the context's own), or every namespace.
type Key = (Kind, Option<String>, bool);

/// Blocking: run it in the background.
fn load((kind, chosen, all): &Key) -> Snapshot {
    let lines = |o: String| o.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect::<Vec<_>>();
    let contexts = kubectl(&["config", "get-contexts", "-o", "name"]).map(lines).unwrap_or_default();
    let context = kubectl(&["config", "current-context"]).map(|o| o.trim().to_string()).unwrap_or_default();
    let default_ns = kubectl(&["config", "view", "--minify", "-o", "jsonpath={..namespace}"]).ok().map(|o| o.trim().to_string()).filter(|n| !n.is_empty()).unwrap_or("default".into());
    // Pinned: the list comes from `context` even if the current one changes meanwhile.
    let context_arg = format!("--context={context}");
    let items = if contexts.is_empty() {
        Err(NO_CONTEXTS.into())
    } else if context.is_empty() {
        Err("Aucun contexte choisi : choisissez-en un en haut à droite.".into())
    } else {
        let ns = chosen.clone().unwrap_or(default_ns.clone());
        let mut args = vec!["get", kind.resource(), "-o", "json", "--request-timeout=5s", &context_arg];
        args.extend(if *all { vec!["-A"] } else { vec!["-n", &ns] });
        kubectl(&args).and_then(|json| match kind {
            Kind::Pods => parse_pods(&json).map(Items::Pods),
            Kind::Deployments => parse_deployments(&json).map(Items::Deployments),
            Kind::Services => parse_services(&json).map(Items::Services),
        })
    };
    // Listing namespaces may be forbidden: then only the context's own is offered.
    let namespaces = if items.is_ok() { kubectl(&["get", "namespaces", "-o", "json", "--request-timeout=5s", &context_arg]).and_then(|j| parse_namespaces(&j)).unwrap_or_default() } else { vec![] };
    Snapshot { contexts, context, default_ns, namespaces, items }
}

/// A kubectl command line for the terminal, pinned to the context shown.
fn kubectl_line(context: &str, rest: &str) -> String {
    format!("kubectl --context={} {rest}", shell_quote(context))
}

pub struct KubernetesView {
    workspace: WeakEntity<Workspace>,
    kind: Kind,
    chosen: Option<String>,
    all: bool,
    snap: Option<Snapshot>,
    /// "ns/name" of the resource whose action runs now.
    busy: Option<String>,
    show_contexts: bool,
    show_namespaces: bool,
    /// The deployment whose replica count is being typed.
    scaling: Option<Deployment>,
    replicas: Entity<InputState>,
    /// Drawn since the last poll: a hidden tab stops polling.
    rendered: bool,
    _poll: Task<()>,
    _subs: Vec<Subscription>,
}

impl KubernetesView {
    pub fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let replicas = cx.new(|cx| InputState::new(window, cx).placeholder("Répliques"));
        let _subs = vec![cx.subscribe_in(&replicas, window, |this, _, ev, window, cx| {
            if let (InputEvent::PressEnter { .. }, Some(d)) = (ev, this.scaling.clone()) {
                this.scale(d, window, cx);
            }
        })];
        let _poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(key) = this.update(cx, |this, _| std::mem::take(&mut this.rendered).then(|| this.key())) else { break };
                let Some(key) = key else {
                    cx.background_executor().timer(Duration::from_secs(5)).await;
                    continue;
                };
                let snap = cx.background_executor().spawn({
                    let key = key.clone();
                    async move { load(&key) }
                });
                let snap = snap.await;
                if this.update(cx, |this, cx| this.apply(key, snap, cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        });
        Self { workspace, kind: Kind::Pods, chosen: None, all: false, snap: None, busy: None, show_contexts: false, show_namespaces: false, scaling: None, replicas, rendered: true, _poll, _subs }
    }

    /// The context the shown lists come from: every action targets it.
    fn context(&self) -> String {
        self.snap.as_ref().map(|s| s.context.clone()).unwrap_or_default()
    }

    fn key(&self) -> Key {
        (self.kind, self.chosen.clone(), self.all)
    }

    /// A late answer for another kind or namespace is dropped.
    fn apply(&mut self, key: Key, snap: Snapshot, cx: &mut Context<Self>) {
        if key == self.key() {
            self.snap = Some(snap);
            cx.notify();
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let key = self.key();
        let task = cx.background_executor().spawn({
            let key = key.clone();
            async move { load(&key) }
        });
        cx.spawn(async move |this, cx| {
            let snap = task.await;
            let _ = this.update(cx, |this, cx| this.apply(key, snap, cx));
        })
        .detach();
        cx.notify();
    }

    fn run(&mut self, busy: String, args: Vec<String>, done: String, window: &mut Window, cx: &mut Context<Self>) {
        self.busy = Some(busy);
        cx.notify();
        let task = cx.background_executor().spawn(async move { kubectl(&args) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = None;
                match result {
                    Ok(_) => window.push_notification(Notification::success(done), cx),
                    Err(e) => window.push_notification(Notification::error(e), cx),
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    fn confirm(&mut self, title: &str, detail: &str, yes: &str, then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(PromptLevel::Warning, title, Some(detail), &[yes, "Annuler"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                let _ = this.update_in(cx, |this, window, cx| then(this, window, cx));
            }
        })
        .detach();
    }

    fn use_context(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.show_contexts = false;
        let title = format!("Passer au contexte {name} ?");
        let detail = "C'est le réglage global de kubectl : il change aussi pour votre terminal et vos autres outils.";
        self.confirm(
            &title,
            detail,
            "Changer",
            move |this, window, cx| {
                (this.chosen, this.all) = (None, false);
                this.run(String::new(), vec!["config".into(), "use-context".into(), name.clone()], format!("Contexte {name} actif."), window, cx);
            },
            window,
            cx,
        );
    }

    fn pick_namespace(&mut self, ns: Option<String>, cx: &mut Context<Self>) {
        self.show_namespaces = false;
        (self.all, self.chosen) = (ns.is_none(), ns);
        self.refresh(cx);
    }

    fn delete_pod(&mut self, p: &Pod, window: &mut Window, cx: &mut Context<Self>) {
        let ctx = self.context();
        let args = vec![format!("--context={ctx}"), "delete".into(), "pod".into(), "-n".into(), p.namespace.clone(), p.name.clone(), "--wait=false".into()];
        let (busy, done) = (format!("{}/{}", p.namespace, p.name), format!("Pod {} supprimé.", p.name));
        self.confirm(
            &format!("Supprimer le pod {} du contexte {ctx} ?", p.name),
            "S'il vient d'un déploiement, Kubernetes en recrée un aussitôt (c'est une façon de le redémarrer). Sinon, il disparaît pour de bon.",
            "Supprimer",
            move |this, window, cx| this.run(busy, args, done, window, cx),
            window,
            cx,
        );
    }

    fn restart(&mut self, d: &Deployment, window: &mut Window, cx: &mut Context<Self>) {
        let ctx = self.context();
        let args = vec![format!("--context={ctx}"), "rollout".into(), "restart".into(), format!("deployment/{}", d.name), "-n".into(), d.namespace.clone()];
        let (busy, done) = (format!("{}/{}", d.namespace, d.name), format!("{} redémarre.", d.name));
        self.confirm(
            &format!("Redémarrer le déploiement {} du contexte {ctx} ?", d.name),
            "Ses pods sont remplacés un à un par des neufs.",
            "Redémarrer",
            move |this, window, cx| this.run(busy, args, done, window, cx),
            window,
            cx,
        );
    }

    fn scale(&mut self, d: Deployment, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(n) = self.replicas.read(cx).value().trim().parse::<u32>() else {
            return window.push_notification(Notification::error("Entrez un nombre de répliques : 0, 1, 2…"), cx);
        };
        self.scaling = None;
        cx.notify();
        let ctx = self.context();
        let args = vec![format!("--context={ctx}"), "scale".into(), format!("deployment/{}", d.name), format!("--replicas={n}"), "-n".into(), d.namespace.clone()];
        let plural = if n > 1 { "s" } else { "" };
        let (busy, done) = (format!("{}/{}", d.namespace, d.name), format!("{} passe à {n} réplique{plural}.", d.name));
        let detail = if n == 0 { "Plus aucun pod ne tournera : l'application sera arrêtée." } else { "Kubernetes ajoute ou retire des pods pour arriver à ce nombre." };
        self.confirm(&format!("Passer {} à {n} réplique{plural} (contexte {ctx}) ?", d.name), detail, "Appliquer", move |this, window, cx| this.run(busy, args, done, window, cx), window, cx);
    }

    fn pick_row(&self, id: String, label: String, on: bool, cx: &mut Context<Self>, click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static) -> Stateful<Div> {
        let (primary, hover) = (cx.theme().primary, ui::hover_bg(cx));
        // The current one is not clickable: no pointer, no hover.
        h_flex()
            .id(SharedString::from(id))
            .flex_shrink_0()
            .h(r(30.))
            .px(r(8.))
            .gap(r(8.))
            .rounded(r(6.))
            .when(on, |d| d.bg(primary.opacity(0.12)))
            .child(div().flex_1().min_w_0().truncate().when(on, |d| d.font_weight(FontWeight::SEMIBOLD)).child(label))
            .when(on, |d| d.child(Icon::new(IconName::Check).small().text_color(primary)))
            .when(!on, |d| d.cursor_pointer().hover(|d| d.bg(hover)).on_click(cx.listener(move |this, _, window, cx| click(this, window, cx))))
    }

    fn picker(&self, id: &'static str, caption: &str, value: String, open: bool, list: Div, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let trigger = button(id).outline().small().child(
            h_flex().gap(r(6.)).child(div().text_color(muted).child(caption.to_string())).child(div().max_w(r(180.)).truncate().font_weight(FontWeight::MEDIUM).child(value)).child(Icon::new(IconName::ChevronDown).small().text_color(muted)),
        );
        let this = cx.entity().downgrade();
        Popover::new(SharedString::from(format!("{id}-popover")))
            .open(open)
            .on_open_change(move |open, _, cx| {
                let _ = this.update(cx, |this, cx| {
                    if id == "k8s-context" { this.show_contexts = *open } else { this.show_namespaces = *open }
                    cx.notify();
                });
            })
            .trigger(trigger)
            .child(v_flex().id(SharedString::from(format!("{id}-list"))).w(r(280.)).max_h(r(320.)).overflow_y_scroll().gap(r(2.)).child(list))
            .into_any_element()
    }

    fn header_controls(&self, snap: &Snapshot, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if snap.contexts.is_empty() {
            return vec![];
        }
        let mut contexts = v_flex().gap(r(2.));
        for name in &snap.contexts {
            let target = name.clone();
            contexts = contexts.child(self.pick_row(format!("ctx-{name}"), name.clone(), *name == snap.context, cx, move |this, window, cx| this.use_context(target.clone(), window, cx)));
        }
        let current = self.chosen.clone().unwrap_or(snap.default_ns.clone());
        let mut names = snap.namespaces.clone();
        if !names.contains(&current) {
            names.insert(0, current.clone());
        }
        let mut namespaces = v_flex().gap(r(2.)).child(self.pick_row("ns-*".into(), "Tous les namespaces".into(), self.all, cx, |this, _, cx| this.pick_namespace(None, cx)));
        for name in names {
            let target = name.clone();
            namespaces = namespaces.child(self.pick_row(format!("ns-{name}"), name.clone(), !self.all && name == current, cx, move |this, _, cx| this.pick_namespace(Some(target.clone()), cx)));
        }
        let context = if snap.context.is_empty() { "aucun".into() } else { snap.context.clone() };
        vec![
            self.picker("k8s-context", "Contexte", context, self.show_contexts, contexts, cx),
            self.picker("k8s-namespace", "Namespace", if self.all { "tous".into() } else { current }, self.show_namespaces, namespaces, cx),
        ]
    }

    /// "Pods | Déploiements | Services".
    fn render_kinds(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let (fg, muted, active) = (theme.foreground, theme.muted_foreground, theme.foreground.opacity(0.1));
        let mut bar = h_flex().flex_shrink_0().w(r(420.)).m(r(12.)).p(r(3.)).gap(r(3.)).rounded(r(9.)).bg(fg.opacity(0.05));
        for (kind, label) in [(Kind::Pods, "Pods"), (Kind::Deployments, "Déploiements"), (Kind::Services, "Services")] {
            let on = self.kind == kind;
            bar = bar.child(
                h_flex()
                    .id(label)
                    .flex_1()
                    .h(r(28.))
                    .justify_center()
                    .rounded(r(7.))
                    .cursor_pointer()
                    .text_size(r(13.))
                    .map(|d| if on { d.bg(active).text_color(fg).font_weight(FontWeight::MEDIUM) } else { d.text_color(muted).hover(|d| d.text_color(fg)) })
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.kind = kind;
                        this.refresh(cx);
                    })),
            );
        }
        bar
    }

    fn render_table(&self, items: &Items, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, green, yellow) = (theme.muted_foreground, theme.border, theme.green, theme.yellow);
        let (cols, actions_w): (&[(&str, f32)], f32) = match items {
            Items::Pods(_) => (&[("Nom", 0.), ("Prêts", 56.), ("État", 150.), ("Redémarrages", 100.), ("Âge", 64.), ("Nœud", 140.)], 290.),
            Items::Deployments(_) => (&[("Nom", 0.), ("Prêts", 64.), ("À jour", 64.), ("Disponibles", 90.), ("Âge", 64.)], 290.),
            Items::Services(_) => (&[("Nom", 0.), ("Type", 100.), ("IP du cluster", 120.), ("Ports", 180.)], 200.),
        };
        let cell = |w: f32| if w == 0. { div().flex_1().min_w_0().truncate() } else { div().w(r(w)).flex_shrink_0().truncate() };
        let line = || h_flex().flex_shrink_0().px(r(16.)).gap(r(12.)).border_b_1().border_color(border);
        let now = Utc::now();
        let all = self.all;
        let name_cell = |ns: &str, name: &str| h_flex().min_w_0().when(all, |d| d.child(div().text_color(muted).child(format!("{ns} · ")))).child(div().min_w_0().truncate().font_weight(FontWeight::MEDIUM).child(name.to_string())).into_any_element();
        let text = |s: String| s.into_any_element();
        let mut rows: Vec<(Vec<AnyElement>, Div)> = vec![];
        match items {
            Items::Pods(list) => {
                for p in list {
                    let color = status_color(&p.status, p.ready, cx);
                    let cells = vec![
                        name_cell(&p.namespace, &p.name),
                        text(format!("{}/{}", p.ready.0, p.ready.1)),
                        div().text_color(color).child(p.status.clone()).into_any_element(),
                        text(p.restarts.to_string()),
                        text(age_fr(&p.created, now)),
                        div().text_color(muted).child(p.node.clone()).into_any_element(),
                    ];
                    rows.push((cells, self.pod_actions(p, cx)));
                }
            }
            Items::Deployments(list) => {
                for d in list {
                    let color = if d.ready >= d.replicas { green } else { yellow };
                    let cells = vec![
                        name_cell(&d.namespace, &d.name),
                        div().text_color(color).child(format!("{}/{}", d.ready, d.replicas)).into_any_element(),
                        text(d.up_to_date.to_string()),
                        text(d.available.to_string()),
                        text(age_fr(&d.created, now)),
                    ];
                    rows.push((cells, self.deployment_actions(d, cx)));
                }
            }
            Items::Services(list) => {
                for s in list {
                    let cells = vec![name_cell(&s.namespace, &s.name), text(s.kind.clone()), text(if s.cluster_ip.is_empty() { "—".into() } else { s.cluster_ip.clone() }), text(ports_text(&s.ports))];
                    rows.push((cells, self.service_actions(s, cx)));
                }
            }
        }
        // shortcut: every row is laid out each frame; switch to uniform_list if clusters with thousands of pods show up.
        v_flex()
            .id("k8s-table")
            .flex_1()
            .overflow_y_scroll()
            .text_size(r(13.))
            .child(
                line()
                    .h(r(30.))
                    .text_size(r(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .children(cols.iter().map(|(label, w)| cell(*w).child(label.to_uppercase())))
                    .child(div().w(r(actions_w)).flex_shrink_0()),
            )
            .children(rows.into_iter().map(|(cells, actions)| {
                line().h(r(40.)).children(cols.iter().zip(cells).map(|((_, w), c)| cell(*w).child(c))).child(h_flex().w(r(actions_w)).flex_shrink_0().justify_end().gap(r(2.)).child(actions))
            }))
            .into_any_element()
    }

    fn pod_actions(&self, p: &Pod, cx: &mut Context<Self>) -> Div {
        let key = format!("{}/{}", p.namespace, p.name);
        let busy = self.busy.as_deref() == Some(key.as_str());
        let btn = |id: &str| button(SharedString::from(format!("{id}-{key}"))).ghost().xsmall().disabled(busy);
        let (ns, name) = (shell_quote(&p.namespace), shell_quote(&p.name));
        let ctx = self.context();
        let logs = kubectl_line(&ctx, &format!("logs -f --tail 200 -n {ns} {name}"));
        let shell = kubectl_line(&ctx, &format!("exec -it -n {ns} {name} -- sh"));
        let describe = kubectl_line(&ctx, &format!("describe pod -n {ns} {name}"));
        let pod = p.clone();
        h_flex()
            .gap(r(2.))
            .child(btn("logs").icon(Icon::empty().path("icons/list-play.svg")).label("Logs").on_click(cx.listener(move |this, _, window, cx| in_terminal(&this.workspace, &logs, window, cx))))
            .when(p.status == "Running", |d| {
                d.child(btn("shell").icon(IconName::SquareTerminal).label("Shell").on_click(cx.listener(move |this, _, window, cx| in_terminal(&this.workspace, &shell, window, cx))))
            })
            .child(btn("describe").label("Décrire").on_click(cx.listener(move |this, _, window, cx| in_terminal(&this.workspace, &describe, window, cx))))
            .child(btn("delete").icon(IconName::Delete).loading(busy).tooltip("Supprimer").on_click(cx.listener(move |this, _, window, cx| this.delete_pod(&pod, window, cx))))
    }

    fn deployment_actions(&self, d: &Deployment, cx: &mut Context<Self>) -> Div {
        let key = format!("{}/{}", d.namespace, d.name);
        let busy = self.busy.as_deref() == Some(key.as_str());
        let open = self.scaling.as_ref().is_some_and(|s| s.name == d.name && s.namespace == d.namespace);
        let (d1, d2, d3) = (d.clone(), d.clone(), d.clone());
        let this = cx.entity().downgrade();
        let scale = Popover::new(SharedString::from(format!("scale-{key}")))
            .open(open)
            .on_open_change(move |open, window, cx| {
                let d = d1.clone();
                let _ = this.update(cx, |this, cx| {
                    if *open {
                        this.replicas.update(cx, |i, cx| {
                            i.set_value(d.replicas.to_string(), window, cx);
                            i.focus(window, cx);
                        });
                    }
                    this.scaling = open.then_some(d);
                    cx.notify();
                });
            })
            .trigger(button(SharedString::from(format!("scale-btn-{key}"))).ghost().xsmall().disabled(busy).label("Mettre à l'échelle"))
            .child(
                v_flex()
                    .w(r(240.))
                    .gap(r(8.))
                    .child(div().text_size(r(12.5)).child(format!("Nombre de pods voulus pour {}", d.name)))
                    .child(
                        h_flex()
                            .gap(r(6.))
                            .child(Input::new(&self.replicas).small().flex_1())
                            .child(button(SharedString::from(format!("scale-ok-{key}"))).primary().small().label("Appliquer").on_click(cx.listener(move |this, _, window, cx| this.scale(d2.clone(), window, cx)))),
                    ),
            );
        h_flex()
            .gap(r(2.))
            .child(button(SharedString::from(format!("restart-{key}"))).ghost().xsmall().disabled(busy).loading(busy).icon(Icon::empty().path("icons/refresh.svg")).label("Redémarrer").on_click(cx.listener(move |this, _, window, cx| this.restart(&d3, window, cx))))
            .child(scale)
    }

    fn service_actions(&self, s: &Service, cx: &mut Context<Self>) -> Div {
        let ns = shell_quote(&s.namespace);
        let target = shell_quote(&format!("svc/{}", s.name));
        let ctx = self.context();
        h_flex().gap(r(2.)).children(s.ports.iter().filter(|p| p.protocol != "UDP").map(|p| {
            let command = kubectl_line(&ctx, &format!("port-forward -n {ns} {target} {0}:{0}", p.port));
            button(SharedString::from(format!("pf-{}-{}-{}", s.namespace, s.name, p.port)))
                .ghost()
                .xsmall()
                .icon(IconName::ArrowRight)
                .label(format!("localhost:{}", p.port))
                .tooltip("Rediriger ce port vers votre Mac (port-forward) dans le terminal")
                .on_click(cx.listener(move |this, _, window, cx| in_terminal(&this.workspace, &command, window, cx)))
        }))
    }
}

fn status_color(status: &str, (ready, total): (usize, usize), cx: &gpui::App) -> Hsla {
    let theme = cx.theme();
    match status {
        "Running" if ready == total => theme.green,
        "Completed" | "Succeeded" => theme.muted_foreground,
        "Running" | "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" => theme.yellow,
        s if s.starts_with("Init:") && s.contains('/') => theme.yellow,
        _ => theme.red,
    }
}

impl Render for KubernetesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rendered = true;
        let snap = self.snap.clone();
        let actions = snap.as_ref().map(|s| self.header_controls(s, cx)).unwrap_or_default();
        let header = header(tool("kubernetes"), "Les pods, déploiements et services de votre cluster.", actions, cx);
        let kinds = self.render_kinds(cx);
        let body = match snap.map(|s| s.items) {
            None => ui::loading("Lecture du cluster…", cx).into_any_element(),
            Some(Err(e)) => ui::error_state("Kubernetes indisponible", &e, cx).into_any_element(),
            Some(Ok(items)) if items.kind() != self.kind => ui::loading("Lecture du cluster…", cx).into_any_element(),
            Some(Ok(items)) => {
                let empty = match &items {
                    Items::Pods(l) if l.is_empty() => Some("Aucun pod"),
                    Items::Deployments(l) if l.is_empty() => Some("Aucun déploiement"),
                    Items::Services(l) if l.is_empty() => Some("Aucun service"),
                    _ => None,
                };
                match empty {
                    Some(title) => ui::empty_state((tool("kubernetes").icon)(), cx.theme().blue, title, "Choisissez un autre namespace ou contexte en haut à droite.", cx).into_any_element(),
                    None => self.render_table(&items, cx),
                }
            }
        };
        v_flex().size_full().child(header).child(kinds).child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_commands_name_the_context() {
        assert_eq!(kubectl_line("prod's", "logs -n 'a' 'b'"), r"kubectl --context='prod'\''s' logs -n 'a' 'b'");
    }

    const PODS: &str = r#"{
  "apiVersion": "v1", "kind": "List", "metadata": {"resourceVersion": ""},
  "items": [
    {
      "apiVersion": "v1", "kind": "Pod",
      "metadata": {"name": "web-7d4b9c-x2k9p", "namespace": "shop", "creationTimestamp": "2026-11-14T09:00:00Z", "labels": {"app": "web"}},
      "spec": {"nodeName": "docker-desktop", "containers": [{"name": "web", "image": "nginx:1.27"}, {"name": "sidecar", "image": "envoy"}]},
      "status": {"phase": "Running", "podIP": "10.1.0.12",
        "containerStatuses": [
          {"name": "web", "ready": true, "restartCount": 0, "state": {"running": {"startedAt": "2026-11-14T09:00:05Z"}}},
          {"name": "sidecar", "ready": true, "restartCount": 1, "state": {"running": {"startedAt": "2026-11-14T09:01:00Z"}}}
        ]}
    },
    {
      "metadata": {"name": "api-5f6c8-bbq7z", "namespace": "shop", "creationTimestamp": "2026-11-12T12:00:00Z"},
      "spec": {"nodeName": "docker-desktop", "containers": [{"name": "api"}]},
      "status": {"phase": "Running",
        "containerStatuses": [{"name": "api", "ready": false, "restartCount": 14,
          "lastState": {"terminated": {"exitCode": 1, "reason": "Error"}},
          "state": {"waiting": {"reason": "CrashLoopBackOff", "message": "back-off 5m0s restarting failed container"}}}]}
    },
    {
      "metadata": {"name": "migrate-29xk1", "namespace": "shop", "creationTimestamp": "2026-11-14T11:59:20Z"},
      "spec": {"nodeName": "docker-desktop", "containers": [{"name": "migrate"}]},
      "status": {"phase": "Succeeded",
        "containerStatuses": [{"name": "migrate", "ready": false, "restartCount": 0,
          "state": {"terminated": {"exitCode": 0, "reason": "Completed", "finishedAt": "2026-11-14T11:59:40Z"}}}]}
    },
    {
      "metadata": {"name": "worker-1", "namespace": "shop", "creationTimestamp": "2026-11-14T11:50:00Z"},
      "spec": {"containers": [{"name": "worker"}]},
      "status": {"phase": "Pending",
        "containerStatuses": [{"name": "worker", "ready": false, "restartCount": 0,
          "state": {"waiting": {"reason": "ImagePullBackOff", "message": "Back-off pulling image"}}}]}
    },
    {
      "metadata": {"name": "db-0", "namespace": "shop", "creationTimestamp": "2026-11-14T11:58:00Z"},
      "spec": {"nodeName": "docker-desktop", "containers": [{"name": "db"}]},
      "status": {"phase": "Pending",
        "initContainerStatuses": [{"name": "init-perms", "ready": false, "restartCount": 0, "state": {"running": {}}}],
        "containerStatuses": [{"name": "db", "ready": false, "restartCount": 0, "state": {"waiting": {"reason": "PodInitializing"}}}]}
    },
    {
      "metadata": {"name": "old-1", "namespace": "shop", "creationTimestamp": "2026-11-14T10:00:00Z", "deletionTimestamp": "2026-11-14T11:59:00Z"},
      "spec": {"containers": [{"name": "old"}]},
      "status": {"phase": "Running", "containerStatuses": [{"name": "old", "ready": true, "restartCount": 0, "state": {"running": {}}}]}
    },
    {
      "metadata": {"name": "evicted-1", "namespace": "shop", "creationTimestamp": "2026-11-01T10:00:00Z"},
      "spec": {"containers": [{"name": "x"}]},
      "status": {"phase": "Failed", "reason": "Evicted", "message": "The node was low on resource: memory."}
    }
  ]
}"#;

    #[test]
    fn pods_read_like_kubectl_get_pods() {
        let pods = parse_pods(PODS).unwrap();
        let rows: Vec<(&str, (usize, usize), &str, u32)> = pods.iter().map(|p| (p.name.as_str(), p.ready, p.status.as_str(), p.restarts)).collect();
        assert_eq!(
            rows,
            vec![
                ("web-7d4b9c-x2k9p", (2, 2), "Running", 1),
                ("api-5f6c8-bbq7z", (0, 1), "CrashLoopBackOff", 14),
                ("migrate-29xk1", (0, 1), "Completed", 0),
                ("worker-1", (0, 1), "ImagePullBackOff", 0),
                ("db-0", (0, 1), "Init:0/1", 0),
                ("old-1", (1, 1), "Terminating", 0),
                ("evicted-1", (0, 1), "Evicted", 0),
            ]
        );
        assert_eq!((pods[0].namespace.as_str(), pods[0].node.as_str()), ("shop", "docker-desktop"));
        assert_eq!(pods[3].node, "", "not scheduled yet");
        assert!(parse_pods("not json").is_err());
    }

    #[test]
    fn deployments_services_and_namespaces() {
        let deployments = r#"{"apiVersion":"v1","kind":"List","items":[
          {"metadata":{"name":"web","namespace":"shop","creationTimestamp":"2026-11-04T12:00:00Z","generation":3},
           "spec":{"replicas":3,"selector":{"matchLabels":{"app":"web"}}},
           "status":{"observedGeneration":3,"replicas":3,"updatedReplicas":3,"readyReplicas":2,"availableReplicas":2}},
          {"metadata":{"name":"paused","namespace":"shop","creationTimestamp":"2026-11-04T12:00:00Z"},
           "spec":{"replicas":0},"status":{"observedGeneration":1}}]}"#;
        let d = parse_deployments(deployments).unwrap();
        assert_eq!((d[0].name.as_str(), d[0].ready, d[0].replicas, d[0].up_to_date, d[0].available), ("web", 2, 3, 3, 2));
        assert_eq!((d[1].ready, d[1].replicas, d[1].available), (0, 0, 0));

        let services = r#"{"apiVersion":"v1","kind":"List","items":[
          {"metadata":{"name":"kubernetes","namespace":"default","creationTimestamp":"2026-10-01T08:00:00Z"},
           "spec":{"type":"ClusterIP","clusterIP":"10.96.0.1","clusterIPs":["10.96.0.1"],"ports":[{"name":"https","port":443,"protocol":"TCP","targetPort":6443}]}},
          {"metadata":{"name":"web","namespace":"shop"},
           "spec":{"type":"NodePort","clusterIP":"10.96.12.7","ports":[{"port":80,"protocol":"TCP","targetPort":"http","nodePort":30080},{"port":53,"protocol":"UDP","targetPort":53}]}},
          {"metadata":{"name":"ext","namespace":"shop"},"spec":{"type":"ExternalName","externalName":"db.example.com"}}]}"#;
        let s = parse_services(services).unwrap();
        assert_eq!((s[0].kind.as_str(), s[0].cluster_ip.as_str(), ports_text(&s[0].ports)), ("ClusterIP", "10.96.0.1", "443/TCP".to_string()));
        assert_eq!(ports_text(&s[1].ports), "80:30080/TCP, 53/UDP");
        assert_eq!((s[2].cluster_ip.as_str(), ports_text(&s[2].ports)), ("", "—".to_string()));

        let namespaces = r#"{"apiVersion":"v1","items":[{"metadata":{"name":"default"},"status":{"phase":"Active"}},{"metadata":{"name":"kube-system"}},{"metadata":{"name":"shop"}}],"kind":"List"}"#;
        assert_eq!(parse_namespaces(namespaces).unwrap(), vec!["default", "kube-system", "shop"]);
    }

    #[test]
    fn ages_read_in_french() {
        let now = DateTime::parse_from_rfc3339("2026-11-14T12:00:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(age_fr("2026-11-14T11:59:20Z", now), "40 s");
        assert_eq!(age_fr("2026-11-14T11:48:00Z", now), "12 min");
        assert_eq!(age_fr("2026-11-14T07:00:00+00:00", now), "5 h");
        assert_eq!(age_fr("2026-11-11T12:00:00Z", now), "3 j");
        assert_eq!(age_fr("2025-11-01T12:00:00Z", now), "1 an");
        assert_eq!(age_fr("2023-01-01T00:00:00Z", now), "3 ans");
        assert_eq!(age_fr("2026-11-14T12:00:30Z", now), "0 s", "clock skew");
        assert_eq!(age_fr("", now), "—");
    }

    #[test]
    fn kubectl_errors_in_plain_french() {
        assert_eq!(explain("E1114 12:00:00 memcache.go:265] couldn't get current server API group list: Get \"https://127.0.0.1:6443/api\": dial tcp 127.0.0.1:6443: connect: connection refused\nThe connection to the server 127.0.0.1:6443 was refused"), UNREACHABLE);
        assert_eq!(explain("Unable to connect to the server: dial tcp: lookup k8s.example.com: no such host"), UNREACHABLE);
        assert!(explain("Error from server (Forbidden): pods is forbidden: User \"dev\" cannot list resource \"pods\" in API group \"\" in the namespace \"kube-system\"").starts_with("Accès refusé"));
        assert_eq!(explain("error: no context exists with the name: \"prod\"\n"), "no context exists with the name: \"prod\"");
    }
}
