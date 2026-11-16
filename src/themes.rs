//! Color themes. "Minuit" is `assets/theme.json` as is; the others are built
//! from a palette of a few colors onto the same template.

use std::rc::Rc;

use gpui::{App, Hsla, Rgba, hsla, rgb};
use gpui_component::theme::{Theme, ThemeConfig};
use serde_json::{Value, json};

const TEMPLATE: &str = include_str!("../assets/theme.json");
pub const DEFAULT: &str = "Minuit";

pub struct Palette {
    pub name: &'static str,
    pub dark: bool,
    /// Imported by the user (a VS Code theme): it can be removed.
    pub custom: bool,
    /// Window chrome: title bar, sidebar, status bar.
    pub chrome: u32,
    /// The editor card.
    pub card: u32,
    /// Popovers, menus, plugin cards.
    pub surface: u32,
    pub border: u32,
    pub fg: u32,
    pub muted: u32,
    pub primary: u32,
    pub selection: u32,
    // Syntax.
    pub keyword: u32,
    pub function: u32,
    pub types: u32,
    pub string: u32,
    pub number: u32,
    pub comment: u32,
    pub operator: u32,
    pub property: u32,
    pub tag: u32,
}

/// Minuit's colors, for its swatches (its config is the template itself).
const MINUIT: Palette = Palette {
    name: DEFAULT, dark: true, custom: false, chrome: 0x0f1014, card: 0x16181d, surface: 0x1d2026, border: 0x24272e, fg: 0xe4e6eb, muted: 0x868c97,
    primary: 0x8b9cff, selection: 0x3a4670, keyword: 0xc792ea, function: 0x82aaff, types: 0xffcb6b, string: 0xa8d68a, number: 0xf78c6c,
    comment: 0x5c6773, operator: 0x89ddff, property: 0x9cdcfe, tag: 0xf07178,
};

pub static THEMES: &[Palette] = &[
    MINUIT,
    Palette {
        name: "Abysse", dark: true, custom: false, chrome: 0x0b1220, card: 0x0f172a, surface: 0x16203a, border: 0x1e2a44, fg: 0xdbe4f0, muted: 0x7d8ba3,
        primary: 0x38bdf8, selection: 0x1d3b5c, keyword: 0xc4a7ff, function: 0x5fd4f4, types: 0xfcd581, string: 0x9ee6a9, number: 0xffa37f,
        comment: 0x52607a, operator: 0x7dd3fc, property: 0xa5c8ff, tag: 0xff8fa3,
    },
    Palette {
        name: "Forêt", dark: true, custom: false, chrome: 0x0d1310, card: 0x121a16, surface: 0x18231d, border: 0x223029, fg: 0xdde8e1, muted: 0x80948a,
        primary: 0x4ade80, selection: 0x24493a, keyword: 0xf2b880, function: 0x7fd1b9, types: 0xe8d27c, string: 0xb5e48c, number: 0xff9e7a,
        comment: 0x5a6e63, operator: 0x8fd3c8, property: 0xa8d5c2, tag: 0xf28b82,
    },
    Palette {
        name: "Braise", dark: true, custom: false, chrome: 0x14100e, card: 0x1b1613, surface: 0x241d19, border: 0x30261f, fg: 0xeee3d9, muted: 0x9a8b7e,
        primary: 0xfb923c, selection: 0x5a3a22, keyword: 0xff8f6b, function: 0xf6c177, types: 0x9ccfd8, string: 0xc3d88a, number: 0xe6a5c7,
        comment: 0x6b5d52, operator: 0xe9b18a, property: 0xd8c1a8, tag: 0xee6f6f,
    },
    Palette {
        name: "Contraste", dark: true, custom: false, chrome: 0x000000, card: 0x0a0a0a, surface: 0x141414, border: 0x3a3a3a, fg: 0xffffff, muted: 0xb8b8b8,
        primary: 0xffd60a, selection: 0x264f78, keyword: 0xff7edb, function: 0x5cc8ff, types: 0xffd60a, string: 0x7ee787, number: 0xffa657,
        comment: 0x9da5b4, operator: 0x79c0ff, property: 0xd2a8ff, tag: 0xff6b6b,
    },
    Palette {
        name: "Aube", dark: false, custom: false, chrome: 0xeef0f4, card: 0xffffff, surface: 0xf6f7f9, border: 0xe1e4ea, fg: 0x1f2328, muted: 0x6a7280,
        primary: 0x5b6cf0, selection: 0xd4dbff, keyword: 0x8839ef, function: 0x1e66f5, types: 0xb35900, string: 0x2f8a2f, number: 0xd2491d,
        comment: 0x8c95a3, operator: 0x0b7a8a, property: 0x2a5db0, tag: 0xd20f39,
    },
    Palette {
        name: "Sable", dark: false, custom: false, chrome: 0xf1ebe1, card: 0xfbf8f2, surface: 0xf5efe5, border: 0xe4dccd, fg: 0x2b2620, muted: 0x7d7366,
        primary: 0xc2611f, selection: 0xf3d9bd, keyword: 0xa23b72, function: 0x2c6e9b, types: 0x8a5f00, string: 0x4f7a28, number: 0xc24e1c,
        comment: 0x9a8f80, operator: 0x2a7f7f, property: 0x5a4fa0, tag: 0xb83232,
    },
    Palette {
        name: "Lavande", dark: true, custom: false, chrome: 0x13111c, card: 0x1a1726, surface: 0x221e31, border: 0x2d283f, fg: 0xe6e1f5, muted: 0x8f88a8,
        primary: 0xb794f6, selection: 0x3d3260, keyword: 0xf19bd4, function: 0x9cc4ff, types: 0xf5d68a, string: 0xb4e4a3, number: 0xffa98a,
        comment: 0x625b7a, operator: 0x9ee7e3, property: 0xc9b8ff, tag: 0xff8fb1,
    },
    Palette {
        name: "Océan", dark: true, custom: false, chrome: 0x081419, card: 0x0c1b22, surface: 0x11252e, border: 0x1a333e, fg: 0xd6e9ef, muted: 0x7a96a1,
        primary: 0x2dd4bf, selection: 0x174450, keyword: 0x7fb7ff, function: 0x5ee6c9, types: 0xf2d479, string: 0xa9e08f, number: 0xff9f80,
        comment: 0x4f6b75, operator: 0x89d9f0, property: 0x9fd3e8, tag: 0xff8c8c,
    },
    Palette {
        name: "Ardoise", dark: true, custom: false, chrome: 0x1b1d21, card: 0x212328, surface: 0x292c32, border: 0x33363d, fg: 0xd8dade, muted: 0x8b9099,
        primary: 0x7aa2f7, selection: 0x3a4255, keyword: 0xbb9af7, function: 0x7dcfff, types: 0xe0af68, string: 0x9ece6a, number: 0xff9e64,
        comment: 0x5f6570, operator: 0x89ddff, property: 0x73daca, tag: 0xf7768e,
    },
    Palette {
        name: "Menthe", dark: false, custom: false, chrome: 0xe9f2ee, card: 0xfbfefc, surface: 0xf1f7f4, border: 0xd8e6df, fg: 0x1d2a24, muted: 0x63766d,
        primary: 0x0f9d74, selection: 0xc8ecdf, keyword: 0x7c3aed, function: 0x0b6fb5, types: 0xa15c00, string: 0x2d7d2d, number: 0xc2410c,
        comment: 0x8a9a92, operator: 0x0e7c7b, property: 0x2563a8, tag: 0xbe123c,
    },
    Palette {
        name: "Pêche", dark: false, custom: false, chrome: 0xf7ece6, card: 0xfffaf7, surface: 0xfaf1ec, border: 0xeddcd2, fg: 0x2e2420, muted: 0x86726a,
        primary: 0xe8603c, selection: 0xfad6c8, keyword: 0xb1327a, function: 0x2f6fae, types: 0x9a6200, string: 0x4c7a1f, number: 0xc2410c,
        comment: 0xa39188, operator: 0x1f7f86, property: 0x6b4fb0, tag: 0xc0262d,
    },
    // Dark.
    Palette {
        name: "Encre", dark: true, custom: false, chrome: 0x0a0a0b, card: 0x111113, surface: 0x18181b, border: 0x24242a, fg: 0xe8e8ea, muted: 0x8a8a93,
        primary: 0x8ab4ff, selection: 0x2a3550, keyword: 0xc792ea, function: 0x82aaff, types: 0xffcb6b, string: 0xc3e88d, number: 0xf78c6c,
        comment: 0x5c5c66, operator: 0x89ddff, property: 0xb2ccd6, tag: 0xff6f86,
    },
    Palette {
        name: "Aurore", dark: true, custom: false, chrome: 0x140f1a, card: 0x1b1424, surface: 0x231a2f, border: 0x30243f, fg: 0xf0e6f7, muted: 0x9a8aab,
        primary: 0xff7ab8, selection: 0x4a2a52, keyword: 0xf28fd0, function: 0x8fb8ff, types: 0xffd479, string: 0xa6e3a1, number: 0xffab70,
        comment: 0x6e5c80, operator: 0x7fe0e6, property: 0xc7a6ff, tag: 0xff6f91,
    },
    Palette {
        name: "Cerise", dark: true, custom: false, chrome: 0x170c10, card: 0x1f1015, surface: 0x28151c, border: 0x381e27, fg: 0xf3e4e8, muted: 0xa28890,
        primary: 0xff5c7a, selection: 0x55222f, keyword: 0xff7f9e, function: 0x8ec5ff, types: 0xf6d58e, string: 0xb7df8f, number: 0xffa07a,
        comment: 0x75545e, operator: 0x8fd3e8, property: 0xe6a5c0, tag: 0xff6b6b,
    },
    Palette {
        name: "Ambre", dark: true, custom: false, chrome: 0x15110c, card: 0x1c1712, surface: 0x241e17, border: 0x332a20, fg: 0xf0e6d8, muted: 0xa39581,
        primary: 0xf5a524, selection: 0x4a3a20, keyword: 0xe58f65, function: 0xf2c36b, types: 0x9fc6a8, string: 0xc2d68c, number: 0xf08a5d,
        comment: 0x6f6253, operator: 0x8fc1c7, property: 0xd9b38c, tag: 0xe5786d,
    },
    Palette {
        name: "Cobalt", dark: true, custom: false, chrome: 0x0a1630, card: 0x0d1d3d, surface: 0x13264d, border: 0x1c3460, fg: 0xe6eeff, muted: 0x8fa3c7,
        primary: 0x4f8cff, selection: 0x1f4a8a, keyword: 0xff9d3d, function: 0xffd84a, types: 0x7ee0ff, string: 0x8fe08a, number: 0xff7ac6,
        comment: 0x5a72a0, operator: 0x9fd8ff, property: 0xb7c9ff, tag: 0xff6b8a,
    },
    // Light.
    Palette {
        name: "Papier", dark: false, custom: false, chrome: 0xf3f3f2, card: 0xffffff, surface: 0xf7f7f6, border: 0xe3e3e0, fg: 0x1f1f1f, muted: 0x6e6e6a,
        primary: 0x2f6fed, selection: 0xd3e2fd, keyword: 0xa626a4, function: 0x1f5fbf, types: 0x9a6400, string: 0x2f7d32, number: 0xb5530c,
        comment: 0x9a9a95, operator: 0x0f7a7a, property: 0x3955a3, tag: 0xc0262d,
    },
    Palette {
        name: "Glacier", dark: false, custom: false, chrome: 0xe8eef5, card: 0xf8fbfe, surface: 0xeef3f9, border: 0xd6e0ec, fg: 0x1b2633, muted: 0x627287,
        primary: 0x2b8ad6, selection: 0xcfe3f6, keyword: 0x6d4fc2, function: 0x0b6fb5, types: 0x9a5b00, string: 0x2e7d4f, number: 0xc2410c,
        comment: 0x8a99ab, operator: 0x0e7c86, property: 0x2f5aa8, tag: 0xc0264d,
    },
    Palette {
        name: "Lin", dark: false, custom: false, chrome: 0xefe9de, card: 0xfbf8f2, surface: 0xf4efe5, border: 0xe2d9c8, fg: 0x2b2620, muted: 0x7b7062,
        primary: 0x5f8a3a, selection: 0xdfe9cc, keyword: 0x8a3f7a, function: 0x2f5f8a, types: 0x8a5a00, string: 0x4f7a26, number: 0xb4501a,
        comment: 0xa39785, operator: 0x2a7a72, property: 0x5a4a9a, tag: 0xb3302c,
    },
];

/// Themes imported by the user, loaded from `<app dir>/themes` at first use.
static CUSTOM: std::sync::Mutex<Option<Vec<&'static Palette>>> = std::sync::Mutex::new(None);

fn themes_dir() -> std::path::PathBuf {
    crate::settings::app_dir().join("themes")
}

fn custom() -> std::sync::MutexGuard<'static, Option<Vec<&'static Palette>>> {
    let mut guard = CUSTOM.lock().unwrap();
    if guard.is_none() {
        let mut list: Vec<&'static Palette> = std::fs::read_dir(themes_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| from_saved(&std::fs::read_to_string(e.path()).ok()?))
            .map(|p| &*Box::leak(Box::new(p)))
            .collect();
        list.sort_by_key(|p| p.name);
        *guard = Some(list);
    }
    guard
}

/// The built-in themes and the imported ones, by name.
pub fn all() -> Vec<&'static Palette> {
    let mut all: Vec<&'static Palette> = THEMES.iter().chain(custom().as_ref().into_iter().flatten().copied()).collect();
    all.sort_by_cached_key(|t| crate::ui::sort_key(t.name));
    all
}

/// A theme file's name: letters and digits of the theme's name.
fn file_of(name: &str) -> std::path::PathBuf {
    file_in(&themes_dir(), name)
}

/// The file holding `name` in `dir`, else the first free one: "One Dark" and
/// "One-Dark" share a slug, so the second gets "-2".
fn file_in(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let slug: String = name.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    (1..)
        .map(|n| dir.join(if n == 1 { format!("{slug}.json") } else { format!("{slug}-{n}.json") }))
        .find(|f| std::fs::read_to_string(f).map_or(true, |t| from_saved(&t).is_some_and(|p| p.name == name)))
        .expect("endless")
}

/// Keeps an imported theme (it replaces one of the same name) and makes it available.
pub fn save_custom(p: Palette) -> Result<&'static str, String> {
    if THEMES.iter().any(|t| t.name == p.name) {
        return Err(format!("« {} » est déjà un thème d'Akolade.", p.name));
    }
    std::fs::create_dir_all(themes_dir()).map_err(|e| e.to_string())?;
    crate::workspace::write_file(&file_of(p.name), &saved(&p).to_string()).map_err(|e| e.to_string())?;
    let mut guard = custom();
    let list = guard.as_mut().expect("loaded");
    list.retain(|t| t.name != p.name);
    let p: &'static Palette = Box::leak(Box::new(p));
    list.push(p);
    list.sort_by_key(|t| t.name);
    Ok(p.name)
}

pub fn remove_custom(name: &str) {
    let _ = std::fs::remove_file(file_of(name));
    if let Some(list) = custom().as_mut() {
        list.retain(|t| t.name != name);
    }
}

const FIELDS: [&str; 17] = ["chrome", "card", "surface", "border", "fg", "muted", "primary", "selection", "keyword", "function", "types", "string", "number", "comment", "operator", "property", "tag"];

fn colors(p: &Palette) -> [u32; 17] {
    [p.chrome, p.card, p.surface, p.border, p.fg, p.muted, p.primary, p.selection, p.keyword, p.function, p.types, p.string, p.number, p.comment, p.operator, p.property, p.tag]
}

/// An imported theme as kept on disk (our own small format).
fn saved(p: &Palette) -> Value {
    let mut v = json!({ "name": p.name, "dark": p.dark });
    for (k, c) in FIELDS.iter().zip(colors(p)) {
        v[k] = json!(hex(c));
    }
    v
}

fn from_saved(text: &str) -> Option<Palette> {
    let v: Value = serde_json::from_str(text).ok()?;
    let c = |k: &str| v[k].as_str().and_then(parse_hex);
    let name: &'static str = Box::leak(v["name"].as_str()?.to_string().into_boxed_str());
    Some(Palette {
        name,
        dark: v["dark"].as_bool()?,
        custom: true,
        chrome: c("chrome")?, card: c("card")?, surface: c("surface")?, border: c("border")?, fg: c("fg")?, muted: c("muted")?,
        primary: c("primary")?, selection: c("selection")?, keyword: c("keyword")?, function: c("function")?, types: c("types")?,
        string: c("string")?, number: c("number")?, comment: c("comment")?, operator: c("operator")?, property: c("property")?, tag: c("tag")?,
    })
}

/// "#a1b2c3", "#a1b2c3ff" (alpha dropped) or "#abc".
fn parse_hex(s: &str) -> Option<u32> {
    let h = s.trim().strip_prefix('#')?;
    // Byte slicing below: "#😀" would panic.
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        6 | 8 => u32::from_str_radix(&h[..6], 16).ok(),
        3 | 4 => {
            let d: Vec<u32> = h[..3].chars().map(|c| c.to_digit(16)).collect::<Option<_>>()?;
            Some((d[0] * 17) << 16 | (d[1] * 17) << 8 | d[2] * 17)
        }
        _ => None,
    }
}

/// VS Code theme files are JSON with comments and trailing commas: plain JSON.
fn strip_jsonc(text: &str) -> String {
    let (mut out, mut chars) = (String::with_capacity(text.len()), text.chars().peekable());
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                while chars.next().is_some_and(|c| c != '\n') {}
                out.push('\n');
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
            }
            _ => out.push(c),
        }
    }
    drop_trailing_commas(&out)
}

/// A comma right before } or ] is dropped. On comment-free text: a comment
/// between them (`"a": 1, // "b": 2` then `}`) hides nothing.
fn drop_trailing_commas(text: &str) -> String {
    let (mut out, mut chars) = (String::with_capacity(text.len()), text.chars().peekable());
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        in_string = c == '"';
        if c != ',' || !matches!(chars.clone().find(|c| !c.is_whitespace()), Some('}' | ']')) {
            out.push(c);
        }
    }
    out
}

/// A VS Code color theme (`.json`) as an Akolade palette: its UI colors, then
/// its token colors for the syntax. Missing colors are made up from the others.
pub fn from_vscode(text: &str, fallback_name: &str) -> Result<Palette, String> {
    let v: Value = serde_json::from_str(&strip_jsonc(text)).map_err(|e| format!("Ce fichier n'est pas un thème VS Code lisible : {e}"))?;
    let ui = &v["colors"];
    let color = |keys: &[&str]| keys.iter().find_map(|k| ui[*k].as_str().and_then(parse_hex));
    let dark = match v["type"].as_str() {
        Some(t) => !t.contains("light"),
        None => color(&["editor.background"]).is_none_or(|bg| Hsla::from(rgb(bg)).l < 0.5),
    };
    // Token colors: the first rule whose scope starts with one of `scopes`.
    let tokens = v["tokenColors"].as_array().cloned().unwrap_or_default();
    let token = |scopes: &[&str]| {
        tokens.iter().find_map(|t| {
            let names: Vec<String> = match &t["scope"] {
                Value::String(s) => s.split(',').map(|x| x.trim().to_string()).collect(),
                Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                _ => vec![],
            };
            names.iter().any(|n| scopes.iter().any(|s| n.starts_with(s))).then(|| t["settings"]["foreground"].as_str().and_then(parse_hex)).flatten()
        })
    };
    let card = color(&["editor.background"]).ok_or("Ce thème n'a pas de couleur de fond (editor.background).")?;
    let fg = color(&["editor.foreground", "foreground"]).unwrap_or(if dark { 0xd4d4d4 } else { 0x1f1f1f });
    let near = |c: u32, dl: f32| parse_hex(&shade(c, dl)).unwrap_or(c);
    let step = if dark { -0.03 } else { -0.04 };
    let primary = color(&["focusBorder", "button.background", "activityBarBadge.background", "textLink.foreground"]).unwrap_or(0x3794ff);
    let keyword = token(&["keyword", "storage"]).unwrap_or(primary);
    let name = v["name"].as_str().filter(|n| !n.trim().is_empty()).unwrap_or(fallback_name).trim().to_string();
    Ok(Palette {
        name: Box::leak(name.into_boxed_str()),
        dark,
        custom: true,
        chrome: color(&["sideBar.background", "activityBar.background", "titleBar.activeBackground"]).unwrap_or(near(card, step)),
        card,
        surface: color(&["editorWidget.background", "dropdown.background", "input.background"]).unwrap_or(near(card, -step)),
        border: color(&["editorGroup.border", "panel.border", "sideBar.border", "widget.border"]).unwrap_or(near(card, -step * 3.)),
        fg,
        muted: color(&["descriptionForeground", "editorLineNumber.foreground", "tab.inactiveForeground"]).unwrap_or(near(fg, if dark { -0.3 } else { 0.3 })),
        primary,
        selection: color(&["editor.selectionBackground"]).unwrap_or(near(card, -step * 4.)),
        keyword,
        function: token(&["entity.name.function", "support.function"]).unwrap_or(keyword),
        types: token(&["entity.name.type", "support.type", "storage.type", "entity.name.class"]).unwrap_or(keyword),
        string: token(&["string"]).unwrap_or(fg),
        number: token(&["constant.numeric", "constant.language", "constant"]).unwrap_or(fg),
        comment: token(&["comment"]).unwrap_or(near(fg, if dark { -0.4 } else { 0.4 })),
        operator: token(&["keyword.operator", "punctuation"]).unwrap_or(fg),
        property: token(&["variable.other.property", "support.type.property-name", "variable.other.object.property", "meta.object-literal.key"]).unwrap_or(fg),
        tag: token(&["entity.name.tag"]).unwrap_or(keyword),
    })
}

pub fn find(name: &str) -> &'static Palette {
    all().into_iter().find(|t| t.name == name).unwrap_or(&THEMES[0])
}

fn hex(c: u32) -> String {
    format!("#{c:06x}")
}

fn alpha(c: u32, a: u8) -> String {
    format!("#{c:06x}{a:02x}")
}

pub fn hsla_hex(c: Hsla) -> String {
    let c = Rgba::from(c);
    let byte = |v: f32| (v.clamp(0., 1.) * 255.).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(c.r), byte(c.g), byte(c.b))
}

/// A syntax color with more punch: more saturated, lightness kept in the band
/// where colors read best on the theme's background (pastels were washed out).
fn vivid(c: u32, dark: bool) -> String {
    let h = Hsla::from(rgb(c));
    if h.s < 0.12 {
        return hex(c); // a grey stays a grey
    }
    let s = (h.s * 1.2 + 0.1).min(1.);
    let l = if dark { (h.l - 0.04).clamp(0.56, 0.74) } else { (h.l - 0.02).clamp(0.28, 0.46) };
    hsla_hex(hsla(h.h, s, l, 1.))
}

/// Lighter (`dl` > 0) or darker.
fn shade(c: u32, dl: f32) -> String {
    let h = Hsla::from(rgb(c));
    hsla_hex(hsla(h.h, h.s, (h.l + dl).clamp(0., 1.), 1.))
}

/// The full theme config of a palette, on the template's keys.
pub fn config(p: &Palette) -> Value {
    let mut cfg: Value = serde_json::from_str(TEMPLATE).expect("assets/theme.json");
    if p.name == DEFAULT {
        return cfg;
    }
    let ink = if p.dark { 0xffffff } else { 0x000000 };
    let on_primary = if Hsla::from(rgb(p.primary)).l > 0.6 { "#0c0d12" } else { "#ffffff" };
    let step = if p.dark { 0.06 } else { -0.06 };
    let (info, success, warning, danger) = if p.dark {
        (("#1e3a5f", "#dbeafe"), ("#1f4d2e", "#dcfce7"), ("#5c4410", "#fef9c3"), ("#6b1f24", "#fee2e2"))
    } else {
        (("#dbeafe", "#1e3a8a"), ("#dcfce7", "#14532d"), ("#fef3c7", "#78350f"), ("#fee2e2", "#7f1d1d"))
    };
    cfg["name"] = json!(p.name);
    cfg["mode"] = json!(if p.dark { "dark" } else { "light" });
    cfg["colors"] = json!({
        "background": hex(p.card), "foreground": hex(p.fg), "border": hex(p.border),
        "accent.background": alpha(ink, 0x0d), "accent.foreground": hex(p.fg),
        "muted.background": hex(p.surface), "muted.foreground": hex(p.muted),
        "caret": hex(p.primary), "ring": hex(p.primary),
        // The accent at 30%: gpui-component caps selection opacity at 0.3, and a dull
        // solid color at 30% was almost invisible (the rename dialog's selected name).
        "selection.background": alpha(p.primary, 0x4d),
        "primary.background": hex(p.primary), "primary.hover.background": shade(p.primary, step), "primary.active.background": shade(p.primary, -step),
        "primary.foreground": on_primary,
        "secondary.background": alpha(ink, 0x0a), "secondary.hover.background": alpha(ink, 0x12), "secondary.active.background": alpha(ink, 0x1a),
        "secondary.foreground": hex(p.fg), "input.border": shade(p.border, step / 2.),
        "popover.background": hex(p.surface), "popover.foreground": hex(p.fg),
        "list.background": hex(p.surface), "list.hover.background": alpha(ink, 0x0d), "list.active.background": alpha(p.primary, 0x26), "list.active.border": alpha(p.primary, 0),
        "sidebar.background": hex(p.chrome), "sidebar.foreground": hex(p.fg), "sidebar.border": hex(p.chrome),
        "sidebar.accent.background": alpha(p.primary, 0x1f), "sidebar.accent.foreground": hex(p.fg),
        "sidebar.primary.background": hex(p.primary), "sidebar.primary.foreground": on_primary,
        "tab_bar.background": hex(p.chrome), "tab.background": "#00000000", "tab.foreground": hex(p.muted),
        "tab.active.background": hex(p.card), "tab.active.foreground": hex(p.fg),
        "title_bar.background": hex(p.chrome), "title_bar.border": hex(p.chrome), "window.border": hex(p.border),
        "scrollbar.background": "#00000000", "scrollbar.thumb.background": alpha(ink, 0x1f), "scrollbar.thumb.hover.background": alpha(ink, 0x33),
        "link.foreground": hex(p.function),
        "info.background": info.0, "info.foreground": info.1, "success.background": success.0, "success.foreground": success.1,
        "warning.background": warning.0, "warning.foreground": warning.1, "danger.background": danger.0, "danger.foreground": danger.1,
        "switch.background": shade(p.surface, step * 2.),
    });
    // Built-in themes get vivid syntax colors; an imported one keeps its own.
    let paint = |color: u32| if p.custom { hex(color) } else { vivid(color, p.dark) };
    let c = |color: u32| json!({ "color": paint(color) });
    let italic = |color: u32| json!({ "color": paint(color), "font_style": "italic" });
    // Comments and punctuation stay quiet: as the theme made them.
    let quiet = |color: u32| json!({ "color": hex(color) });
    let quiet_italic = |color: u32| json!({ "color": hex(color), "font_style": "italic" });
    cfg["highlight"] = json!({
        "editor.background": hex(p.card), "editor.foreground": hex(p.fg), "editor.active_line.background": alpha(ink, if p.dark { 0x05 } else { 0x06 }),
        "editor.line_number": alpha(p.muted, 0x80), "editor.active_line_number": hex(p.muted), "hint": hex(p.muted), "predictive": alpha(p.muted, 0x99),
        "syntax": {
            "attribute": c(p.types), "boolean": c(p.number), "comment": quiet_italic(p.comment), "comment.doc": quiet_italic(p.comment),
            "constant": c(p.number), "constructor": c(p.types), "embedded": c(p.fg), "emphasis": { "font_style": "italic" },
            "emphasis.strong": { "font_weight": 700 }, "enum": c(p.types), "function": c(p.function), "keyword": c(p.keyword),
            "label": c(p.operator), "link_text": c(p.function), "link_uri": italic(p.operator), "number": c(p.number),
            "operator": c(p.operator), "preproc": c(p.keyword), "property": c(p.property), "punctuation": quiet(p.muted),
            "punctuation.bracket": quiet(p.muted), "punctuation.delimiter": quiet(p.muted), "punctuation.special": c(p.operator),
            "string": c(p.string), "string.escape": c(p.operator), "string.regex": c(p.operator), "string.special": c(p.number),
            "string.special.symbol": c(p.number), "tag": c(p.tag), "tag.doctype": c(p.keyword), "text.literal": c(p.string),
            "title": { "color": paint(p.function), "font_weight": 700 }, "type": c(p.types), "variable": quiet(p.fg),
            "variable.special": italic(p.tag), "variant": c(p.number),
        },
    });
    cfg
}

/// Makes this theme the app's (fonts are applied after, by the settings).
pub fn apply(name: &str, cx: &mut App) {
    let p = find(name);
    let config: ThemeConfig = serde_json::from_value(config(p)).expect("theme config");
    Theme::change(config.mode, None, cx);
    Theme::global_mut(cx).apply_config(&Rc::new(config));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_builds_a_complete_config() {
        let template: Value = serde_json::from_str(TEMPLATE).unwrap();
        for p in THEMES {
            let cfg = config(p);
            let config: ThemeConfig = serde_json::from_value(cfg.clone()).unwrap_or_else(|e| panic!("{}: {e}", p.name));
            assert_eq!(config.mode.is_dark(), p.dark, "{}", p.name);
            // Same keys as the hand-made theme: nothing falls back to gpui-component's defaults.
            for section in ["colors", "highlight"] {
                for key in template[section].as_object().unwrap().keys() {
                    assert!(cfg[section].get(key).is_some(), "{}: {section}.{key} missing", p.name);
                }
            }
            for key in template["highlight"]["syntax"].as_object().unwrap().keys() {
                assert!(cfg["highlight"]["syntax"].get(key).is_some(), "{}: syntax.{key} missing", p.name);
            }
        }
        assert_eq!(config(&THEMES[0]), template, "Minuit is the hand-made theme");
        let names: Vec<&str> = THEMES.iter().map(|t| t.name).collect();
        assert_eq!(names.len(), names.iter().collect::<std::collections::HashSet<_>>().len(), "unique names");
        assert_eq!(find("inconnu").name, DEFAULT);
    }

    #[test]
    fn vscode_themes_with_comments_become_palettes() {
        let text = r##"{
            // A comment, and trailing commas: VS Code's JSON
            "name": "Mon thème",
            "type": "dark",
            "colors": { "editor.background": "#1e1e2e", "editor.foreground": "#cdd6f4", "focusBorder": "#89b4faff", "sideBar.background": "#181825", },
            /* block */
            "tokenColors": [
                { "scope": ["comment", "punctuation.definition.comment"], "settings": { "foreground": "#6c7086", "fontStyle": "italic" } },
                { "scope": "keyword, storage.modifier", "settings": { "foreground": "#cba6f7" } },
                { "scope": "string", "settings": { "foreground": "#a6e3a1" } },
                { "scope": "entity.name.function", "settings": { "foreground": "#89b4fa" } },
            ],
        }"##;
        let p = from_vscode(text, "fichier").unwrap();
        assert_eq!((p.name, p.dark, p.card, p.chrome, p.primary), ("Mon thème", true, 0x1e1e2e, 0x181825, 0x89b4fa));
        assert_eq!((p.keyword, p.string, p.function, p.comment), (0xcba6f7, 0xa6e3a1, 0x89b4fa, 0x6c7086));
        assert_eq!(p.types, p.keyword, "missing: made up from the others");
        let cfg: ThemeConfig = serde_json::from_value(config(&p)).unwrap();
        assert!(cfg.mode.is_dark());
        assert!(from_vscode("{}", "x").is_err(), "no background");
        assert!(from_vscode("pas du json", "x").is_err());
        assert_eq!(parse_hex("#abc"), Some(0xaabbcc));
        assert_eq!(strip_jsonc(r#"{"url": "http://x//y", "a": [1, 2,], }"#), r#"{"url": "http://x//y", "a": [1, 2] }"#);
        let commented = "{\n \"a\": \"#fff\",\n // \"b\": \"#000\"\n}";
        assert_eq!(serde_json::from_str::<Value>(&strip_jsonc(commented)).unwrap()["a"], "#fff", "comma before a commented-out line");
        assert_eq!(strip_jsonc(r#"{"s": ",}", "t": [1, /* x */ ]}"#), r#"{"s": ",}", "t": [1  ]}"#, "commas in strings stay");
    }

    #[test]
    fn imported_themes_round_trip_through_their_file() {
        let p = from_vscode(r##"{"name": "Essai", "type": "light", "colors": {"editor.background": "#ffffff"}}"##, "x").unwrap();
        let back = from_saved(&saved(&p).to_string()).unwrap();
        assert_eq!((back.name, back.dark, colors(&back)), (p.name, p.dark, colors(&p)));
    }

    #[test]
    fn bad_hex_colors_are_refused_not_a_panic() {
        assert_eq!((parse_hex("#a1b2c3"), parse_hex("#abc"), parse_hex("#a1b2c3ff")), (Some(0xa1b2c3), Some(0xaabbcc), Some(0xa1b2c3)));
        for bad in ["#😀", "#abcdeéx", "#abé", "#+abcde", "#ab"] {
            assert_eq!(parse_hex(bad), None, "{bad}");
        }
    }

    #[test]
    fn names_with_the_same_slug_get_their_own_file() {
        let dir = std::env::temp_dir().join(format!("akolade-themes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let theme = |name: &str| from_vscode(&format!(r##"{{"name": "{name}", "type": "dark", "colors": {{"editor.background": "#000000"}}}}"##), "x").unwrap();
        let first = file_in(&dir, "One Dark");
        std::fs::write(&first, saved(&theme("One Dark")).to_string()).unwrap();
        assert_eq!(file_in(&dir, "One Dark"), first, "the same theme: replaced");
        let second = file_in(&dir, "One-Dark");
        assert_eq!(second, dir.join("one-dark-2.json"));
        std::fs::write(&second, saved(&theme("One-Dark")).to_string()).unwrap();
        assert_eq!((file_in(&dir, "One-Dark"), file_in(&dir, "One Dark")), (second, first), "each finds its own (to remove it)");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
