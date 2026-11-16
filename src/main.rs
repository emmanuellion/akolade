// Windows: no console window next to the app's (debug builds keep it, for the logs).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// serde_json::json! of a whole theme (themes.rs) nests deeper than the default.
#![recursion_limit = "256"]

mod completion;
mod explorer;
mod files;
mod format;
mod git;
mod git_view;
mod icons;
mod image_view;
mod languages;
mod lint;
mod lsp;
mod platform;
#[cfg(test)]
mod perf;
mod plugins;
mod problems;
mod search;
mod session;
mod settings;
mod shell_path;
mod tasks;
mod terminal;
mod themes;
mod tools;
mod trust;
mod ui;
mod workspace;

use std::path::PathBuf;

use gpui::{
    App, AppContext, Application, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use gpui_component::{
    Root, TitleBar, WindowExt,
};

use settings::AppSettings;
use workspace::*;

fn init_theme(cx: &mut App) {
    cx.set_global(AppSettings::load());
    settings::apply_appearance(cx);
}

fn main() {
    // Before anything else: it sets the environment (opened from the Finder: the shell's PATH).
    shell_path::import();
    // Optional arguments: a folder (the project) and/or files to open.
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).map(|p| platform::canonical(&p).unwrap_or(p)).collect();

    Application::new().with_assets(icons::Assets).run(move |cx: &mut App| {
        gpui_component::init(cx);
        // Menus and placeholders of the components in French (English for what has no translation).
        gpui_component::set_locale("fr");
        init_theme(cx);
        languages::register_grammars();
        icons::set_dock_icon();
        cx.bind_keys([
            KeyBinding::new("secondary-s", Save, Some("Workspace")),
            KeyBinding::new("secondary-r", Run, Some("Workspace")),
            KeyBinding::new("secondary-w", CloseTab, Some("Workspace")),
            KeyBinding::new("secondary-n", NewFile, Some("Workspace")),
            KeyBinding::new("secondary-o", OpenFile, Some("Workspace")),
            KeyBinding::new("secondary-shift-o", OpenFolder, Some("Workspace")),
            KeyBinding::new("secondary-alt-w", CloseFolder, Some("Workspace")),
            KeyBinding::new("secondary-b", ToggleSidebar, Some("Workspace")),
            KeyBinding::new("ctrl-`", ToggleTerminal, Some("Workspace")),
            KeyBinding::new("secondary-j", ToggleTerminal, Some("Workspace")),
            KeyBinding::new("secondary-,", OpenSettings, Some("Workspace")),
            KeyBinding::new("secondary-t", NewFile, Some("Workspace")),
            KeyBinding::new("secondary-shift-t", ReopenTab, Some("Workspace")),
            KeyBinding::new("ctrl-tab", NextTab, Some("Workspace")),
            KeyBinding::new("ctrl-shift-tab", PrevTab, Some("Workspace")),
            KeyBinding::new("secondary-}", NextTab, Some("Workspace")),
            KeyBinding::new("secondary-{", PrevTab, Some("Workspace")),
            KeyBinding::new("secondary-alt-right", NextTab, Some("Workspace")),
            KeyBinding::new("secondary-alt-left", PrevTab, Some("Workspace")),
            KeyBinding::new("secondary-=", ZoomIn, Some("Workspace")),
            KeyBinding::new("secondary-+", ZoomIn, Some("Workspace")),
            KeyBinding::new("secondary--", ZoomOut, Some("Workspace")),
            KeyBinding::new("secondary-0", ZoomReset, Some("Workspace")),
            KeyBinding::new("alt-shift-f", Format, Some("Workspace")),
            KeyBinding::new("secondary-shift-g", OpenGit, Some("Workspace")),
            KeyBinding::new("secondary-shift-f", OpenSearch, Some("Workspace")),
            KeyBinding::new("f12", GoToDefinitionKey, Some("Workspace")),
            KeyBinding::new("shift-f12", FindReferences, Some("Workspace")),
            KeyBinding::new("f2", RenameSymbol, Some("Workspace")),
            KeyBinding::new("secondary-.", QuickFix, Some("Workspace")),
            KeyBinding::new("secondary-\\", SplitEditor, Some("Workspace")),
            // French keyboards: "\\" needs ⌥⇧, so also ⌘⇧D ("dédoubler").
            KeyBinding::new("secondary-shift-d", SplitEditor, Some("Workspace")),
            // ⌘F is gpui-component's; Ctrl+F as well on macOS.
            KeyBinding::new("ctrl-f", gpui_component::input::Search, Some("Input")),
            // Claude Code's box: ⏎ sends, ⇧⏎ goes to the line (as ⌘⏎).
            KeyBinding::new("shift-enter", gpui_component::input::Enter { secondary: true }, Some("ClaudeComposer > Input")),
        ]);
        cx.bind_keys((1..=9).map(|n| KeyBinding::new(&format!("secondary-{n}"), SelectTab(n), Some("Workspace"))));
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        // Never larger than the screen: at 150 % scaling, 1280×820 is taller than a 1080p display.
        let wanted = size(px(1280.), px(820.));
        let fit = cx.primary_display().map_or(wanted, |d| {
            let screen = d.bounds().size;
            wanted.min(&size(screen.width * 0.9, screen.height * 0.85))
        });
        let options = WindowOptions {
            // Taller title bar: traffic lights centered in it.
            titlebar: Some(TitlebarOptions { traffic_light_position: Some(point(px(16.), px(16.))), ..TitleBar::title_bar_options() }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, fit, cx))),
            window_min_size: Some(size(px(720.), px(480.))),
            // Linux: our own title bar and window buttons (Wayland compositors like GNOME draw none).
            window_decorations: cfg!(target_os = "linux").then_some(gpui::WindowDecorations::Client),
            app_id: Some("akolade".into()),
            ..Default::default()
        };
        cx.spawn(async move |cx| {
            cx.open_window(options, |window, cx| {
                // The taskbar's name on Windows and Linux.
                window.set_window_title("Akolade");
                let ws = cx.new(|cx| Workspace::new(window, cx));
                // The folder and files once the window's Root exists: opening them may show a
                // notification ("file not found"), which needs it.
                let opening = ws.clone();
                window.defer(cx, move |window, cx| {
                    opening.update(cx, |ws, cx| {
                        // No folder given: the last one, if wanted and still there.
                        let settings = AppSettings::get(cx);
                        let last = settings.last_folder.clone().filter(|p| settings.reopen_last_folder && p.is_dir());
                        // Its tabs from last time come back with it (set_root), then the files given.
                        if let Some(last) = last.filter(|_| !args.iter().any(|p| p.is_dir())) {
                            ws.set_root(Some(last), window, cx);
                        }
                        for p in args {
                            if p.is_dir() {
                                ws.set_root(Some(p), window, cx);
                            } else {
                                ws.open_path(p, window, cx);
                            }
                        }
                        if window.focused(cx).is_none() {
                            ws.focus.focus(window);
                        }
                    });
                });
                let weak = ws.downgrade();
                window.on_window_should_close(cx, move |window, cx| {
                    let warn = !weak.update(cx, |ws, _| ws.confirm_window_close()).unwrap_or(true);
                    if warn {
                        window.push_notification("Fichiers non enregistrés : fermez à nouveau pour abandonner les modifications.", cx);
                    }
                    !warn
                });
                // Launched from a terminal, the window would open behind it.
                cx.activate(true);
                cx.new(|cx| Root::new(ws, window, cx))
            })
            // No window, no app: say why (Linux without a Vulkan driver…) rather than run invisibly.
            .unwrap_or_else(|e| {
                eprintln!("Akolade : impossible d'ouvrir la fenêtre : {e:#}");
                std::process::exit(1);
            });
        })
        .detach();
    });
}
