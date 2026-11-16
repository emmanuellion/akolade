use std::{borrow::Cow, cell::RefCell, collections::HashMap, path::Path, sync::Arc};

use gpui::{AssetSource, Img, ImageSource, RenderImage, SharedString, img};
use image::{Frame, ImageBuffer};
use resvg::{tiny_skia, usvg};
use rust_embed::RustEmbed;

/// Our icons (`assets/icons`), then gpui-component's icon set.
#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        match Self::get(path) {
            Some(f) => Ok(Some(f.data)),
            None => gpui_component_assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut all: Vec<SharedString> = Self::iter().filter(|p| p.starts_with(path)).map(|p| p.into_owned().into()).collect();
        all.extend(gpui_component_assets::Assets.list(path)?);
        Ok(all)
    }
}

/// Image of an embedded SVG, rasterized here (synchronously, cached) rather than
/// by gpui's `img("path")`: that one loads in the background and only redraws
/// "on the next frame", which never comes while the window is idle, so icons
/// stayed blank until the mouse moved.
pub fn asset(path: &'static str) -> Img {
    thread_local! {
        static CACHE: RefCell<HashMap<&'static str, Arc<RenderImage>>> = RefCell::default();
    }
    let image = CACHE.with_borrow_mut(|cache| cache.entry(path).or_insert_with(|| rasterize(path)).clone());
    img(ImageSource::Render(image))
}

/// At 2x the SVG's declared size, for retina screens.
fn rasterize(path: &str) -> Arc<RenderImage> {
    let svg = Assets::get(path).unwrap_or_else(|| panic!("missing icon {path}"));
    let pixmap = render_pixmap(&svg.data, 2.0).unwrap_or_else(|| panic!("invalid icon {path}"));
    Arc::new(RenderImage::new(vec![pixmap_frame(pixmap)]))
}

/// tiny-skia gives premultiplied RGBA, gpui wants straight BGRA.
pub fn pixmap_frame(pixmap: tiny_skia::Pixmap) -> Frame {
    let (w, h) = (pixmap.width(), pixmap.height());
    let mut bgra = pixmap.take();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
        if px[3] > 0 {
            let a = px[3] as f32 / 255.0;
            for c in &mut px[..3] {
                *c = (*c as f32 / a).min(255.0) as u8;
            }
        }
    }
    Frame::new(ImageBuffer::from_raw(w, h, bgra).expect("pixmap size"))
}

/// Dock icon only (never drawn with `img()`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const APP_ICON: &str = "icons/app-icon.svg";
pub const LOGO: &str = "icons/logo.svg";
pub const FOLDER: &str = "icons/files/folder.svg";
pub const FOLDER_OPEN: &str = "icons/files/folder-open.svg";

/// Colored icon for a file: special names, images, then the language's icon.
pub fn file_icon(path: &Path) -> &'static str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    match (name.as_str(), ext.as_str()) {
        ("akolade.json", _) => LOGO,
        ("package.json" | "package-lock.json", _) => "icons/files/npm.svg",
        ("cargo.toml" | "cargo.lock", _) => "icons/files/rust.svg",
        ("makefile" | ".env", _) => "icons/files/config.svg",
        (_, "ini" | "lock" | "env" | "conf" | "cfg") => "icons/files/config.svg",
        _ if crate::image_view::is_image(path) => "icons/files/image.svg",
        _ => crate::languages::for_path(path).icon,
    }
}

/// The icon packs (Settings › Apparence, Store › Icônes): id, name, what it looks like.
pub const PACKS: [(&str, &str, &str); 8] = [
    ("couleurs", "Couleurs", "Les logos des langages, en couleur."),
    ("pastilles", "Pastilles", "Une pastille de la couleur du langage, avec son extension."),
    ("categories", "Catégories", "Une icône au trait par type de fichier (code, données, config…), dans la couleur du langage."),
    ("lignes", "Lignes", "Les icônes au trait des catégories, d'une seule couleur sobre."),
    ("contours", "Contours", "L'extension dans un cadre fin de la couleur du langage."),
    ("lettres", "Lettres", "Juste l'extension, en couleur : le plus léger."),
    ("points", "Points", "Un point de la couleur du langage, rien d'autre."),
    ("minimal", "Minimal", "Une icône de fichier sobre, d'une seule couleur."),
];

/// What a file is, for the line-icon packs: `icons/kinds/<kind>.svg`.
pub fn kind(path: &Path) -> &'static str {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    match (name.as_str(), ext.as_str()) {
        _ if crate::image_view::is_image(path) => "image",
        (_, "lock") | ("package-lock.json" | "pnpm-lock.yaml" | "go.sum", _) => "lock",
        (_, "json" | "jsonc" | "json5") => "json",
        (_, "md" | "mdx" | "txt" | "rst" | "adoc") | ("readme" | "license" | "changelog", _) => "text",
        (_, "csv" | "tsv" | "sql" | "db" | "sqlite" | "sqlite3" | "parquet" | "xlsx") => "table",
        (_, "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar") => "archive",
        (_, "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat") | ("makefile" | "dockerfile" | "containerfile" | "justfile" | "procfile", _) => "terminal",
        (_, "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "env" | "properties" | "editorconfig")
        | (".env" | ".gitignore" | ".gitattributes" | ".dockerignore" | ".npmrc" | ".prettierrc" | ".eslintrc", _) => "config",
        _ if name.starts_with(".env") => "config",
        _ if !std::ptr::eq(crate::languages::for_path(path), &crate::languages::PLAIN) => "code",
        _ => "file",
    }
}

/// "JS", "PY", "TSX": a file's short label for the "Pastilles" pack.
pub fn label(path: &Path) -> String {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
    match name.as_str() {
        "package.json" | "package-lock.json" => "NPM".into(),
        "dockerfile" | "containerfile" => "DK".into(),
        "makefile" => "MK".into(),
        _ if !ext.is_empty() => ext.chars().take(3).collect::<String>().to_uppercase(),
        _ => name.chars().take(2).collect::<String>().to_uppercase(),
    }
}

/// A file's icon in the pack chosen in the settings.
pub fn file(path: &Path, size: f32, cx: &gpui::App) -> gpui::AnyElement {
    let pack = cx.try_global::<crate::settings::AppSettings>().map_or("couleurs", |s| s.icon_pack.as_str());
    file_in(pack, path, size, cx)
}

/// A file's icon in a given pack (the store previews each one).
pub fn file_in(pack: &str, path: &Path, size: f32, cx: &gpui::App) -> gpui::AnyElement {
    use gpui::{FontWeight, IntoElement, ParentElement, Styled, div, prelude::FluentBuilder, rgb};
    use gpui_component::{ActiveTheme, Icon, IconName};

    use crate::workspace::r;
    match pack {
        "pastilles" => {
            let lang = crate::languages::for_path(path);
            let color = if crate::image_view::is_image(path) { 0x8a63d2 } else { lang.color };
            // Dark text on light colors (JavaScript's yellow), white otherwise.
            let light = gpui::Hsla::from(rgb(color)).l > 0.6;
            let text = label(path);
            let font = if text.len() > 2 { size * 0.36 } else { size * 0.44 };
            div()
                .size(r(size))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(r(size * 0.22))
                .bg(rgb(color))
                .text_color(if light { rgb(0x1a1a1a) } else { rgb(0xffffff) })
                .text_size(r(font))
                .font_weight(FontWeight::BOLD)
                .child(text)
                .into_any_element()
        }
        "categories" | "lignes" => {
            let color = if pack == "lignes" { cx.theme().muted_foreground } else { lang_color(path, cx) };
            Icon::empty().path(format!("icons/kinds/{}.svg", kind(path))).size(r(size)).flex_shrink_0().text_color(color).into_any_element()
        }
        "contours" | "lettres" => {
            let color = lang_color(path, cx);
            let text = label(path);
            let font = if text.len() > 2 { size * 0.4 } else { size * 0.5 };
            div()
                .min_w(r(size))
                .h(r(size))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .when(pack == "contours", |d| d.px(r(1.5)).rounded(r(size * 0.22)).border_1().border_color(color.opacity(0.7)))
                .text_color(color)
                .text_size(r(font))
                .font_weight(FontWeight::BOLD)
                .child(text)
                .into_any_element()
        }
        "points" => div()
            .size(r(size))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .child(div().size(r(size * 0.45)).rounded_full().bg(lang_color(path, cx)))
            .into_any_element(),
        "minimal" => Icon::new(IconName::File).size(r(size)).text_color(cx.theme().muted_foreground).into_any_element(),
        _ => asset(file_icon(path)).size(r(size)).flex_shrink_0().into_any_element(),
    }
}

/// A file's language color, readable on the theme: dark colors (C's navy, Rust's
/// near-black logo) lightened on dark themes, light ones darkened on light themes.
fn lang_color(path: &Path, cx: &gpui::App) -> gpui::Hsla {
    use gpui_component::ActiveTheme;
    let raw = if crate::image_view::is_image(path) { 0x8a63d2 } else { crate::languages::for_path(path).color };
    let mut c = gpui::Hsla::from(gpui::rgb(raw));
    let dark_theme = cx.theme().is_dark();
    if crate::languages::for_path(path).name == crate::languages::PLAIN.name && !crate::image_view::is_image(path) {
        return cx.theme().muted_foreground;
    }
    c.l = if dark_theme { c.l.max(0.62) } else { c.l.min(0.42) };
    c
}

/// Dock icon, rendered from `assets/icons/app-icon.svg` (no .app bundle needed).
#[cfg(target_os = "macos")]
pub fn set_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(png) = render_png(&Assets::get(APP_ICON).expect("app icon").data) else { return };
    let Some(mtm) = MainThreadMarker::new() else { return };
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&png)) {
        // Safety: called on the main thread, after gpui created the NSApplication.
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_icon() {}

pub fn render_pixmap(svg: &[u8], scale: f32) -> Option<tiny_skia::Pixmap> {
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
    let size = tree.size().to_int_size().scale_by(scale)?;
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height())?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    Some(pixmap)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn render_png(svg: &[u8]) -> Option<Vec<u8>> {
    render_pixmap(svg, 1.0)?.encode_png().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_exists_and_renders() {
        let paths = ["a.png", "a.lock", "package.json", "akolade.json", "Cargo.toml", "README"];
        for p in paths.map(|p| file_icon(Path::new(p))).into_iter().chain([FOLDER, FOLDER_OPEN, APP_ICON]) {
            let svg = Assets::get(p).unwrap_or_else(|| panic!("missing asset {p}"));
            assert!(render_png(&svg.data).is_some(), "{p} does not render");
            assert!(rasterize(p).size(0).width.0 > 0);
        }
        assert_eq!(file_icon(Path::new("src/App.TSX")), "icons/files/react-ts.svg");
        assert_eq!(file_icon(Path::new("notes.txt")), "icons/files/text.svg");
        assert_eq!([label(Path::new("a.js")), label(Path::new("App.tsx")), label(Path::new("package.json")), label(Path::new("README"))], ["JS", "TSX", "NPM", "RE"]);
        // The line packs: every kind has its icon, and files land in the right one.
        for k in ["image", "lock", "json", "text", "table", "archive", "terminal", "config", "code", "file"] {
            let p = format!("icons/kinds/{k}.svg");
            assert!(render_png(&Assets::get(&p).unwrap_or_else(|| panic!("missing {p}")).data).is_some(), "{p}");
        }
        let kinds = ["logo.png", "Cargo.lock", "package-lock.json", "tsconfig.json", "README.md", "data.csv", "x.tar.gz", "run.sh", "Makefile", "Cargo.toml", ".env.local", "main.rs", "notes"]
            .map(|p| kind(Path::new(p)));
        assert_eq!(kinds, ["image", "lock", "lock", "json", "text", "table", "archive", "terminal", "terminal", "config", "config", "code", "file"]);
    }
}
