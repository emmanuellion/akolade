//! Store tools: each opens its own tab and, once added, gets an icon in the
//! activity bar. Git is one of them (added by default).

pub mod claude;
pub mod compare;
pub mod database;
pub mod deps;
pub mod devtools;
pub mod docker;
pub mod env;
pub mod forge;
pub mod http;
pub mod kubernetes;
pub mod mux;
pub mod ports;
pub mod preview;
pub mod stats;
pub mod tests;

use std::path::PathBuf;

use gpui::{AnyElement, AnyView, App, AppContext, Div, FontWeight, Hsla, IntoElement, ParentElement, Styled, WeakEntity, Window, div};
use gpui_component::{ActiveTheme, Icon, IconName, Theme, h_flex, v_flex};

use crate::{git_view::GitView, lsp, workspace::{Workspace, r}};

pub struct Tool {
    pub id: &'static str,
    pub name: &'static str,
    /// Shortcut, or what it works with.
    pub meta: &'static str,
    pub about: &'static str,
    /// A program it needs, and how to get it.
    pub needs: Option<(&'static str, &'static str)>,
    pub icon: fn() -> Icon,
    pub accent: fn(&Theme) -> Hsla,
    /// Its tab (the project's folder given: built inside a workspace update).
    pub build: fn(WeakEntity<Workspace>, Option<PathBuf>, &mut Window, &mut App) -> AnyView,
}

pub static TOOLS: &[Tool] = &[
    Tool {
        id: "git",
        name: "Git",
        meta: "⌘⇧G · branches, commits, push",
        about: "Le dépôt dans un onglet dédié : modifications, diff, commits, branches, push et pull. Les fichiers modifiés se colorent dans l'explorateur.",
        needs: Some(("git", "xcode-select --install")),
        icon: || Icon::empty().path("icons/git-branch.svg"),
        accent: |t| t.red,
        build: |ws, root, window, cx| cx.new(|cx| GitView::new(ws, root, window, cx)).into(),
    },
    Tool {
        id: "claude",
        name: "Claude Code",
        meta: "conversation, tokens, coût",
        about: "Claude Code dans l'éditeur : il lit et modifie le projet sous vos yeux, vous autorisez ses commandes d'un clic, et vous suivez les tokens, le coût, le contexte et les limites de votre abonnement.",
        // Looked for in the installer's folders too (see `claude::find_claude`): the tab explains how to install it.
        needs: None,
        icon: || Icon::empty().path("icons/claude-code.svg"),
        accent: |_| gpui::rgb(claude::ORANGE).into(),
        build: |ws, root, window, cx| cx.new(|cx| claude::ClaudeView::new(ws, root, window, cx)).into(),
    },
    Tool {
        id: "mux",
        name: "Multiplexeur",
        meta: "sessions Claude Code en parallèle",
        about: "Plusieurs sessions Claude Code côte à côte : sur des projets différents, chacune dans son dossier, ou sur un même projet, chacune dans son worktree, avec des baux sur les fichiers, une messagerie et une file d'intégration qui teste avant de fusionner.",
        needs: None,
        icon: || Icon::new(IconName::LayoutDashboard),
        accent: |_| gpui::rgb(claude::ORANGE).into(),
        build: |ws, _, _, cx| cx.new(|_| mux::MuxView::new(ws)).into(),
    },
    Tool {
        id: "http",
        name: "Client HTTP",
        meta: "GET, POST, PUT, PATCH, DELETE",
        about: "Testez une API sans quitter l'éditeur : méthode, en-têtes, corps, puis la réponse avec son code, sa durée et le JSON mis en forme.",
        needs: Some(("curl", "fourni avec macOS")),
        icon: || Icon::new(IconName::Globe),
        accent: |t| t.blue,
        build: |ws, root, window, cx| cx.new(|cx| http::HttpView::new(ws, root, window, cx)).into(),
    },
    Tool {
        id: "docker",
        name: "Docker",
        meta: "conteneurs",
        about: "Vos conteneurs d'un coup d'œil : démarrer, arrêter, redémarrer, supprimer, voir les logs ou ouvrir un shell dans le terminal intégré.",
        needs: Some(("docker", "Docker Desktop : docker.com")),
        icon: || Icon::empty().path("icons/container.svg"),
        accent: |t| t.cyan,
        build: |ws, _, _, cx| cx.new(|cx| docker::DockerView::new(ws, cx)).into(),
    },
    Tool {
        id: "ports",
        name: "Ports ouverts",
        meta: "serveurs qui tournent",
        about: "Les programmes qui écoutent sur un port de votre Mac : serveurs de dev, bases de données… Ouvrez-les dans le navigateur ou arrêtez-les.",
        needs: Some(("lsof", "fourni avec macOS")),
        icon: || Icon::empty().path("icons/radio.svg"),
        accent: |t| t.magenta,
        build: |_, _, _, cx| cx.new(ports::PortsView::new).into(),
    },
    Tool {
        id: "devtools",
        name: "Boîte à outils",
        meta: "JSON, Base64, JWT, URL, SHA-256…",
        about: "Les petites conversions du quotidien : JSON lisible ou compact, Base64, URL, décodage de JWT, SHA-256, horodatages, UUID.",
        needs: None,
        icon: || Icon::empty().path("icons/wrench.svg"),
        accent: |t| t.yellow,
        build: |_, _, window, cx| cx.new(|cx| devtools::DevtoolsView::new(window, cx)).into(),
    },
    Tool {
        id: "forge",
        name: "Pull requests",
        meta: "GitHub et GitLab",
        about: "Les pull requests du dépôt : leur description, fichiers et commentaires, l'état des tests et des relectures. Récupérez une branche ou créez une PR sans quitter l'éditeur.",
        needs: None,
        icon: || Icon::empty().path("icons/git-pull-request.svg"),
        accent: |t| t.magenta,
        build: |ws, root, _, cx| cx.new(|cx| forge::ForgeView::new(ws, root, cx)).into(),
    },
    Tool {
        id: "tests",
        name: "Tests",
        meta: "cargo, go, pytest, jest, vitest",
        about: "Lancez les tests du projet sans quitter l'éditeur : ce qui passe, ce qui casse, le message de chaque échec et un clic pour ouvrir le test.",
        needs: None,
        icon: || Icon::empty().path("icons/flask.svg"),
        accent: |t| t.green,
        build: |ws, root, _, cx| cx.new(|_| tests::TestsView::new(ws, root)).into(),
    },
    Tool {
        id: "database",
        name: "Base de données",
        meta: "SQLite : tables et requêtes",
        about: "Les bases SQLite du projet : leurs tables, leurs lignes, et un éditeur de requêtes. Lecture seule, sauf si vous confirmez une modification.",
        needs: Some(("sqlite3", "fourni avec macOS")),
        icon: || Icon::empty().path("icons/database.svg"),
        accent: |t| t.cyan,
        build: |ws, root, window, cx| cx.new(|cx| database::DatabaseView::new(ws, root, window, cx)).into(),
    },
    Tool {
        id: "env",
        name: "Variables d'environnement",
        meta: ".env, .env.local, .env.example…",
        about: "Les fichiers .env du projet en tableau : secrets masqués, valeurs modifiables, et les variables qui manquent par rapport à .env.example.",
        needs: None,
        icon: || Icon::empty().path("icons/key-round.svg"),
        accent: |t| t.yellow,
        build: |ws, root, window, cx| cx.new(|cx| env::EnvView::new(ws, root, window, cx)).into(),
    },
    Tool {
        id: "preview",
        name: "Aperçu en direct",
        meta: "HTML, CSS, JS · rechargement auto",
        about: "Un petit serveur local pour vos pages HTML : ouvrez-les dans le navigateur, elles se rechargent toutes seules à chaque enregistrement.",
        needs: None,
        icon: || Icon::new(IconName::Eye),
        accent: |t| t.green,
        build: |ws, root, _, cx| cx.new(|cx| preview::PreviewView::new(ws, root, cx)).into(),
    },
    Tool {
        id: "kubernetes",
        name: "Kubernetes",
        meta: "pods, déploiements, services",
        about: "Votre cluster d'un coup d'œil : pods, déploiements et services par namespace. Logs, shell, redémarrage, mise à l'échelle et redirection de port.",
        needs: Some(("kubectl", "brew install kubectl")),
        icon: || Icon::empty().path("icons/ship-wheel.svg"),
        accent: |t| t.blue,
        build: |ws, _, window, cx| cx.new(|cx| kubernetes::KubernetesView::new(ws, window, cx)).into(),
    },
    Tool {
        id: "compare",
        name: "Comparateur",
        meta: "deux textes ou deux fichiers",
        about: "Collez deux textes ou choisissez deux fichiers du projet : leurs différences ligne par ligne, en une colonne ou côte à côte, espaces ignorés si besoin.",
        needs: Some(("git", "xcode-select --install")),
        icon: || Icon::empty().path("icons/diff.svg"),
        accent: |t| t.cyan,
        build: |ws, root, window, cx| cx.new(|cx| compare::CompareView::new(ws, root, window, cx)).into(),
    },
    Tool {
        id: "deps",
        name: "Dépendances",
        meta: "npm, Cargo, Python, Go",
        about: "Les paquets dont dépend le projet, avec leur version installée. Sur demande : les mises à jour disponibles et l'audit de sécurité. Rien n'est jamais installé.",
        needs: None,
        icon: || Icon::empty().path("icons/package.svg"),
        accent: |t| t.blue,
        build: |ws, root, _, cx| cx.new(|cx| deps::DepsView::new(ws, root, cx)).into(),
    },
    Tool {
        id: "stats",
        name: "Statistiques",
        meta: "lignes, langages, contributeurs",
        about: "Le projet en chiffres : lignes de code et de commentaires par langage, les plus gros fichiers, et avec Git les fichiers les plus modifiés et les contributeurs.",
        needs: None,
        icon: || Icon::new(IconName::ChartPie),
        accent: |t| t.magenta,
        build: |ws, root, _, cx| cx.new(|_| stats::StatsView::new(ws, root)).into(),
    },
];
pub fn find(id: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|t| t.id == id)
}

impl Tool {
    /// Its program is installed (or it needs none).
    pub fn available(&self) -> bool {
        self.needs.is_none_or(|(program, _)| lsp::find_program(program).is_some())
    }
}

/// An icon or a logo on a tinted square (tool tabs, store cards).
pub fn tile(color: Hsla, size: f32, child: impl IntoElement) -> Div {
    div().size(r(size)).flex_shrink_0().flex().items_center().justify_center().rounded(r(size / 4.)).bg(color.opacity(0.14)).child(child)
}

/// The top of a tool's tab: icon, name, one line, and its buttons.
pub fn header(t: &Tool, subtitle: impl Into<String>, actions: Vec<AnyElement>, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .flex_shrink_0()
        .px(r(24.))
        .py(r(16.))
        .gap(r(12.))
        .border_b_1()
        .border_color(theme.border)
        .child(tile((t.accent)(theme), 36., (t.icon)().size(r(18.)).text_color((t.accent)(theme))))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(div().text_size(r(16.)).font_weight(FontWeight::SEMIBOLD).child(t.name))
                .child(div().truncate().text_size(r(12.5)).text_color(theme.muted_foreground).child(subtitle.into())),
        )
        .children(actions)
}

/// Typed in the integrated terminal (logs, shells, fixes).
pub fn in_terminal(workspace: &WeakEntity<Workspace>, command: &str, window: &mut Window, cx: &mut App) {
    if let Some(ws) = workspace.upgrade() {
        ws.update(cx, |ws, cx| ws.run_in_terminal(command, window, cx));
    }
}

/// A tool's own entry, for its header.
pub fn tool(id: &str) -> &'static Tool {
    find(id).expect("a tool of TOOLS")
}
