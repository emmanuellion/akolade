//! Claude Code's mods, drawn here as its desktop app draws them. Akolade attaches
//! to the session as a `desktop` surface (`ui_attach`), asks for the band above
//! the prompt and the plugins' panes (`ui_render`) and sends presses back
//! (`ui_press`); toasts, status lines and "redraw" come as pushes.
//!
//! shortcut: Claude Code marks this protocol internal, so it may change with a version.
//! What is not known here is drawn as nothing, never an error. Not drawn: text fields,
//! pickers and `Client` frames (not named in `ui_attach`, so the engine leaves them
//! out), pictures, themes, and mods' changes to the transcript's own rows.

use std::sync::Arc;

use gpui::{AnyElement, Context, Hsla, Window, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div, img, prelude::FluentBuilder, relative, rgb};
use gpui_component::{ActiveTheme, Sizable, button::ButtonVariants, h_flex, v_flex};
use serde_json::{Value, json};

use super::{ClaudeSession, ORANGE, markdown};
use crate::workspace::{button, r};

pub const CLIENT: &str = "akolade";
/// The band's width in character cells, as the terminal would give it (the box is ~800 px).
pub const BAND_COLUMNS: u64 = 100;
/// A pane's, in the side panel.
pub const PANE_COLUMNS: u64 = 40;

fn control(id: &str, request: Value) -> Value {
    json!({ "type": "control_request", "request_id": id, "request": request })
}

pub fn attach() -> Value {
    control("ui-attach", json!({ "subtype": "ui_attach", "surface": "desktop", "client_id": CLIENT }))
}

/// `id` comes back on the answer: `ui-band-…` or `ui-pane-<pane>-…`.
pub fn render(id: &str, component: &str, instance: &str, columns: u64) -> Value {
    let props = json!({ "bodyColumns": columns, "placement": "dock" });
    control(id, json!({ "subtype": "ui_render", "surface": "desktop", "client_id": CLIENT, "component": component, "instance_id": instance, "props": props }))
}

pub fn press(id: &str, press: &Value, key: Option<&str>) -> Value {
    let mut request = json!({ "subtype": "ui_press", "plugin": press["plugin"], "handle": press["handle"], "surface": "desktop", "client_id": CLIENT });
    if let Some(key) = key {
        request["key"] = json!(key);
    }
    control(id, request)
}

pub fn close(id: &str, pane: &str) -> Value {
    control(id, json!({ "subtype": "ui_close", "id": pane }))
}

/// A color a mod names: one of Claude Code's theme keys, or `#rrggbb`.
fn color(name: &str, cx: &gpui::App) -> Option<Hsla> {
    let t = cx.theme();
    Some(match name {
        "success" => t.green,
        "error" => t.red,
        "warning" => t.yellow,
        "inactive" | "subtle" | "secondaryText" | "secondaryBorder" => t.muted_foreground,
        "text" => t.foreground,
        "claude" => rgb(ORANGE).into(),
        "suggestion" | "permission" | "planMode" | "autoAccept" | "ide" | "remember" => t.primary,
        hex if hex.len() == 7 && hex.starts_with('#') => rgb(u32::from_str_radix(&hex[1..], 16).ok()?).into(),
        _ => return None,
    })
}

fn cells(v: &Value) -> Option<f32> {
    v.as_f64().map(|n| n as f32)
}

impl ClaudeSession {
    /// One element of a mod's tree; `path` makes the ids of its parts unique.
    pub(super) fn draw(&self, node: &Value, path: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(s) = node.as_str() {
            return div().child(s.to_string()).into_any_element();
        }
        let p = &node["props"];
        let children = |this: &Self, window: &mut Window, cx: &mut Context<Self>| -> Vec<AnyElement> {
            node["children"].as_array().into_iter().flatten().enumerate().map(|(i, c)| this.draw(c, &format!("{path}.{i}"), window, cx)).collect()
        };
        let theme = cx.theme();
        let (fg, muted, border, primary, mono) = (theme.foreground, theme.muted_foreground, theme.border, theme.primary, theme.mono_font_family.clone());
        match node["type"].as_str().unwrap_or_default() {
            "Box" => {
                if p["display"] == "none" {
                    return div().into_any_element();
                }
                let column = p["flexDirection"].as_str().is_some_and(|d| d.starts_with("column"));
                let gap = cells(&p["gap"]).unwrap_or(0.);
                let pad = |k: &str| cells(&p[k]).or(cells(&p[if k.ends_with("Left") || k.ends_with("Right") { "paddingX" } else { "paddingY" }])).or(cells(&p["padding"])).unwrap_or(0.);
                let bordered = p["borderStyle"].is_string();
                let mut d = div().flex().min_w_0().map(|d| if column { d.flex_col().gap(r(gap * 14.)) } else { d.flex_row().gap(r(gap * 7.)) });
                d = match p["alignItems"].as_str() {
                    Some("center") => d.items_center(),
                    Some("flex-end") => d.items_end(),
                    Some("flex-start") => d.items_start(),
                    _ => d,
                };
                d = match p["justifyContent"].as_str() {
                    Some("center") => d.justify_center(),
                    Some("flex-end") => d.justify_end(),
                    Some("space-between") => d.justify_between(),
                    Some("space-around") => d.justify_around(),
                    _ => d,
                };
                if cells(&p["flexGrow"]).is_some_and(|g| g > 0.) {
                    d = d.flex_grow();
                }
                if p["flexWrap"] == "wrap" {
                    d = d.flex_wrap();
                }
                if let Some(w) = cells(&p["width"]) {
                    d = d.w(r(w * 7.));
                } else if p["width"] == "100%" {
                    d = d.w_full();
                }
                d = d.pl(r(pad("paddingLeft") * 7.)).pr(r(pad("paddingRight") * 7.)).pt(r(pad("paddingTop") * 14.)).pb(r(pad("paddingBottom") * 14.));
                if bordered {
                    let c = p["borderColor"].as_str().and_then(|c| color(c, cx)).unwrap_or(border);
                    d = d.border_1().border_color(c).rounded(r(6.)).p(r(6.));
                }
                if let Some(bg) = p["backgroundColor"].as_str().and_then(|c| color(c, cx)) {
                    d = d.bg(bg.opacity(0.15)).rounded(r(4.));
                }
                d.children(children(self, window, cx)).into_any_element()
            }
            "Text" => {
                let c = p["color"].as_str().and_then(|c| color(c, cx));
                let mut d = div().flex().flex_wrap().map(|d| match c {
                    Some(c) => d.text_color(c),
                    None if p["dimColor"] == true => d.text_color(muted),
                    None => d,
                });
                if p["bold"] == true {
                    d = d.font_weight(gpui::FontWeight::SEMIBOLD);
                }
                if p["italic"] == true {
                    d = d.italic();
                }
                if p["underline"] == true {
                    d = d.underline();
                }
                if p["strikethrough"] == true {
                    d = d.line_through();
                }
                if p["wrap"].as_str().is_some_and(|w| w.starts_with("truncate")) {
                    d = d.truncate();
                }
                d.children(children(self, window, cx)).into_any_element()
            }
            "Button" => {
                let pressed = node["press"].clone();
                let key = p["key"].as_str().map(str::to_string);
                let id = SharedString::from(format!("mod-{path}"));
                let label = p["label"].as_str().unwrap_or_default().to_string();
                let on_press = cx.listener(move |this, _, _, cx| this.ui_press(&pressed, key.as_deref(), cx));
                let has_children = node["children"].as_array().is_some_and(|c| !c.is_empty());
                if p["plain"] == true || has_children {
                    let inner: Vec<AnyElement> = if has_children { children(self, window, cx) } else { vec![div().child(label).into_any_element()] };
                    h_flex()
                        .id(id)
                        .gap(r(3.))
                        .px(r(4.))
                        .rounded(r(4.))
                        .cursor_pointer()
                        .when(p["dimColor"] == true, |d| d.text_color(muted))
                        .hover(|d| d.bg(fg.opacity(0.08)))
                        .children(inner)
                        .on_click(on_press)
                        .into_any_element()
                } else {
                    let b = button(id).small().label(label);
                    let b = if p["variant"] == "primary" { b.primary() } else { b.outline() };
                    b.on_click(on_press).into_any_element()
                }
            }
            "Markdown" => div()
                .text_size(r(13.))
                .when(p["dimColor"] == true, |d| d.text_color(muted))
                .child(markdown(SharedString::from(format!("mod-md-{path}")), p["text"].as_str().unwrap_or_default().to_string(), window, cx))
                .into_any_element(),
            "Svg" => {
                let source = p["source"].as_str().unwrap_or_default().as_bytes().to_vec();
                div().max_w_full().child(img(Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Svg, source))).max_w_full()).into_any_element()
            }
            "Progress" => {
                let max = p["max"].as_f64().unwrap_or(1.).max(f64::EPSILON);
                let part = p["value"].as_f64().map(|v| (v / max).clamp(0., 1.));
                let bar_color = p["color"].as_str().and_then(|c| color(c, cx)).unwrap_or(primary);
                v_flex()
                    .gap(r(3.))
                    .child(h_flex().gap(r(6.)).child(div().flex_1().child(p["label"].as_str().unwrap_or_default().to_string())).child(div().text_color(muted).child(p["valueText"].as_str().unwrap_or_default().to_string())))
                    .child(div().h(r(5.)).w_full().rounded_full().bg(fg.opacity(0.08)).child(div().h_full().rounded_full().bg(bar_color).w(relative(part.unwrap_or(0.) as f32))))
                    .into_any_element()
            }
            "Code" => v_flex()
                .p(r(8.))
                .rounded(r(6.))
                .bg(fg.opacity(0.04))
                .font_family(mono)
                .text_size(r(12.))
                .children(p["source"].as_str().unwrap_or_default().lines().map(|l| div().child(if l.is_empty() { " ".to_string() } else { l.to_string() })))
                .into_any_element(),
            "Link" => {
                let url = p["url"].as_str().or(p["href"].as_str()).unwrap_or_default().to_string();
                let safe = ["https:", "http:", "file:"].iter().any(|s| url.starts_with(s));
                let mut inner = children(self, window, cx);
                if inner.is_empty() {
                    inner.push(div().child(url.clone()).into_any_element());
                }
                h_flex()
                    .id(SharedString::from(format!("mod-link-{path}")))
                    .text_color(primary)
                    .when(safe, |d| d.cursor_pointer().on_click(move |_, _, _| _ = crate::platform::open(&url)))
                    .children(inner)
                    .into_any_element()
            }
            // The engine's own node (what Claude Code draws there): nothing of ours.
            "engine" => div().into_any_element(),
            // Text fields, pickers, pictures, frames: their words, if any.
            "Input" | "Select" | "Image" | "Raster" | "Client" => div().text_color(muted).child(p["alt"].as_str().unwrap_or_default().to_string()).into_any_element(),
            _ => div().flex().flex_col().children(children(self, window, cx)).into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_name_this_client_and_the_element() {
        assert_eq!(attach()["request"]["surface"], "desktop");
        let r = render("ui-band-1", "AbovePrompt", "above-prompt", 100);
        assert_eq!((r["request_id"].as_str(), r["request"]["component"].as_str(), r["request"]["props"]["bodyColumns"].as_u64()), (Some("ui-band-1"), Some("AbovePrompt"), Some(100)));
        let p = press("ui-press-1", &json!({ "plugin": "ponytail", "handle": 7 }), Some("review"));
        assert_eq!((p["request"]["subtype"].as_str(), p["request"]["handle"].as_i64(), p["request"]["key"].as_str()), (Some("ui_press"), Some(7), Some("review")));
    }
}
