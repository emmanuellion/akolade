//! "Boîte à outils": everyday conversions of a text (JSON, Base64, URL, JWT,
//! SHA-256, timestamps, UUID).

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use std::time::Duration;

use gpui::{AppContext, ClipboardItem, Context, Entity, FontWeight, IntoElement, Subscription, Task, ParentElement, Render, SharedString, Styled, Window, div, prelude::FluentBuilder};
use gpui_component::{
    ActiveTheme, IconName, Sizable,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use sha2::{Digest, Sha256};

use crate::{
    tools::{header, tool},
    workspace::{button, r},
};

/// One conversion: its button and what it does.
pub struct Op {
    pub label: &'static str,
    pub run: fn(&str) -> Result<String, String>,
    /// Runs again as the text changes (not "Nouvel UUID": it ignores the text).
    pub live: bool,
}

pub static OPS: &[Op] = &[
    Op { label: "JSON lisible", run: json_pretty, live: true },
    Op { label: "JSON compact", run: json_compact, live: true },
    Op { label: "Texte → Base64", run: |s| Ok(STANDARD.encode(s)), live: true },
    Op { label: "Base64 → texte", run: base64_decode, live: true },
    Op { label: "Encoder URL", run: |s| Ok(utf8_percent_encode(s, URL_SET).to_string()), live: true },
    Op { label: "Décoder URL", run: url_decode, live: true },
    Op { label: "Décoder un JWT", run: jwt, live: true },
    Op { label: "SHA-256", run: |s| Ok(Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()), live: true },
    Op { label: "Horodatage ↔ date", run: timestamp, live: true },
    Op { label: "Nouvel UUID", run: |_| Ok(uuid_v4()), live: false },
];

/// Like JavaScript's encodeURIComponent: letters, digits and -_.~ stay (also for URL path segments).
pub const URL_SET: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

fn json(s: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(s.trim()).map_err(|e| format!("JSON invalide : {e}"))
}

pub fn json_pretty(s: &str) -> Result<String, String> {
    serde_json::to_string_pretty(&json(s)?).map_err(|e| e.to_string())
}

pub fn json_compact(s: &str) -> Result<String, String> {
    serde_json::to_string(&json(s)?).map_err(|e| e.to_string())
}

/// Standard or URL-safe alphabet, padded or not.
fn base64_bytes(s: &str) -> Result<Vec<u8>, String> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD].iter().find_map(|e| e.decode(&s).ok()).ok_or_else(|| "Ce n'est pas du Base64 valide.".into())
}

pub fn base64_decode(s: &str) -> Result<String, String> {
    String::from_utf8(base64_bytes(s)?).map_err(|_| "Décodé, mais ce n'est pas du texte (données binaires).".into())
}

pub fn url_decode(s: &str) -> Result<String, String> {
    percent_decode_str(&s.replace('+', " ")).decode_utf8().map(|s| s.into_owned()).map_err(|_| "Le résultat n'est pas du texte UTF-8.".into())
}

/// "2026-11-14 13:49:57 (heure locale)".
fn local_date(seconds: i64) -> Option<String> {
    Local.timestamp_opt(seconds, 0).single().map(|d| d.format("%Y-%m-%d %H:%M:%S (heure locale)").to_string())
}

/// The header and the payload of a JWT, readable; `exp`, `iat` and `nbf` as dates.
/// The signature is not checked (that needs the key).
pub fn jwt(s: &str) -> Result<String, String> {
    let parts: Vec<&str> = s.trim().trim_start_matches("Bearer ").split('.').collect();
    if parts.len() != 3 {
        return Err("Un JWT a trois parties séparées par des points.".into());
    }
    let part = |p: &str| -> Result<serde_json::Value, String> { json(&base64_decode(p)?) };
    let (head, payload) = (part(parts[0])?, part(parts[1])?);
    let mut out = format!(
        "En-tête :\n{}\n\nContenu :\n{}",
        serde_json::to_string_pretty(&head).unwrap_or_default(),
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    );
    for (key, label) in [("iat", "Émis le"), ("nbf", "Valide dès"), ("exp", "Expire le")] {
        if let Some(date) = payload.get(key).and_then(|v| v.as_i64()).and_then(local_date) {
            out.push_str(&format!("\n{label} : {date}"));
        }
    }
    Ok(out + "\n\nSignature non vérifiée.")
}

/// A Unix timestamp (seconds or milliseconds) → its date, or a date → its timestamp.
pub fn timestamp(s: &str) -> Result<String, String> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        // Past year 33658 in seconds: it must be milliseconds.
        let (seconds, unit) = if n.abs() >= 1_000_000_000_000 { (n / 1000, "millisecondes") } else { (n, "secondes") };
        let utc = Utc.timestamp_opt(seconds, 0).single().ok_or("Horodatage hors limites.")?;
        return Ok(format!("{} UTC\n{}\n(lu en {unit})", utc.format("%Y-%m-%d %H:%M:%S"), local_date(seconds).unwrap_or_default()));
    }
    let date = DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp())
        .or_else(|_| ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"].iter().find_map(|f| NaiveDateTime::parse_from_str(s, f).ok()).and_then(|d| d.and_local_timezone(Local).single()).map(|d| d.timestamp()).ok_or(()))
        .or_else(|_| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)).and_then(|d| d.and_local_timezone(Local).single()).map(|d| d.timestamp()).ok_or(()))
        .map_err(|_| "Entrez un horodatage (1731590000) ou une date (2026-11-14 13:49:57).".to_string())?;
    Ok(format!("{date}\n{}000 en millisecondes", date))
}

/// A random version-4 UUID.
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    // The system's generator (getrandom on Linux, SecRandom on macOS, BCrypt on Windows).
    let _ = getrandom::fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

pub struct DevtoolsView {
    input: Entity<InputState>,
    output: Entity<InputState>,
    /// The chosen conversion (it runs again as the text changes), and its error.
    last: Option<(&'static Op, Option<String>)>,
    _live: Option<Task<()>>,
    _sub: Subscription,
}

/// Typing pauses this long before the conversion runs again.
const LIVE_DELAY: Duration = Duration::from_millis(150);

impl DevtoolsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).code_editor("text").placeholder("Collez un texte, un JSON, un JWT, un horodatage…"));
        let output = cx.new(|cx| InputState::new(window, cx).code_editor("json"));
        let _sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) {
                this.schedule(window, cx);
            }
        });
        Self { input, output, last: None, _live: None, _sub }
    }

    /// The chosen conversion runs again once typing pauses (a new keystroke cancels the wait).
    fn schedule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(op) = self.last.as_ref().map(|(op, _)| *op).filter(|op| op.live) else { return };
        self._live = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(LIVE_DELAY).await;
            let _ = this.update_in(cx, |this, window, cx| this.apply(op, window, cx));
        }));
    }

    fn apply(&mut self, op: &'static Op, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().to_string();
        match (op.run)(&text) {
            Ok(result) => {
                // Colors only for JSON (a Base64 or a hash in JSON colors looked like noise).
                let json = result.trim_start().starts_with(['{', '[']);
                self.output.update(cx, |o, cx| {
                    o.set_highlighter(if json { "json" } else { "text" }, cx);
                    o.set_value(result, window, cx)
                });
                self.last = Some((op, None));
            }
            Err(e) => self.last = Some((op, Some(e))),
        }
        cx.notify();
    }
}

impl Render for DevtoolsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let pane = |title: &'static str, right: Option<gpui::AnyElement>, editor: &Entity<InputState>| {
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(h_flex().flex_shrink_0().h(r(40.)).px(r(16.)).border_b_1().border_color(border).child(div().flex_1().font_weight(FontWeight::SEMIBOLD).text_size(r(13.)).child(title)).children(right))
                .child(div().flex_1().min_h_0().child(Input::new(editor).h_full().appearance(false).font_family(theme.mono_font_family.clone()).text_size(theme.mono_font_size)))
        };
        let output = self.output.clone();
        let copy = button("copy-result").ghost().xsmall().icon(IconName::Copy).label("Copier").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(output.read(cx).value().to_string()));
        });
        let status = match &self.last {
            None => div().text_color(muted).child("Choisissez une conversion : elle s'applique au texte de gauche, puis suit chaque modification."),
            Some((op, None)) if op.live => div().text_color(theme.green).child(format!("✓ {} · mis à jour pendant la frappe", op.label)),
            Some((op, None)) => div().text_color(theme.green).child(format!("✓ {}", op.label)),
            Some((op, Some(e))) => div().text_color(theme.red).child(format!("{} : {e}", op.label)),
        };
        v_flex()
            .size_full()
            .child(header(tool("devtools"), "Collez un texte à gauche, choisissez une conversion.", vec![], cx))
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(r(24.))
                    .py(r(14.))
                    .gap(r(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(h_flex().flex_wrap().gap(r(8.)).children(OPS.iter().map(|op| {
                        button(SharedString::from(format!("op-{}", op.label)))
                            .outline()
                            .small()
                            .label(op.label)
                            .when(self.last.as_ref().is_some_and(|(l, _)| std::ptr::eq(*l, op)), |b| b.primary())
                            .on_click(cx.listener(move |this, _, window, cx| this.apply(op, window, cx)))
                    })))
                    .child(div().text_size(r(12.5)).child(status)),
            )
            .child(h_flex().flex_1().min_h_0().child(pane("Texte", None, &self.input)).child(div().w(gpui::px(1.)).h_full().bg(border)).child(pane("Résultat", Some(copy.into_any_element()), &self.output)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trips_and_keeps_key_order() {
        assert_eq!(json_pretty(r#"{"b":1,"a":[1,2]}"#).unwrap(), "{\n  \"b\": 1,\n  \"a\": [\n    1,\n    2\n  ]\n}");
        assert_eq!(json_compact("{ \"b\" : 1 ,\n \"a\": true }").unwrap(), r#"{"b":1,"a":true}"#);
        assert!(json_pretty("{oops}").unwrap_err().starts_with("JSON invalide"));
    }

    #[test]
    fn base64_and_url_decode_what_they_encode() {
        let text = "Élan & co/ü?";
        let b64 = (OPS[2].run)(text).unwrap();
        assert_eq!(base64_decode(&b64).unwrap(), text);
        assert_eq!(base64_decode("w4lsYW4").unwrap(), "Élan", "unpadded");
        assert!(base64_decode("@@@").is_err());
        let url = (OPS[4].run)(text).unwrap();
        assert_eq!(url, "%C3%89lan%20%26%20co%2F%C3%BC%3F");
        assert_eq!(url_decode(&url).unwrap(), text);
        assert_eq!(url_decode("a+b").unwrap(), "a b");
    }

    #[test]
    fn jwt_shows_header_payload_and_dates() {
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhZGEiLCJleHAiOjE3MzE1OTAwMDB9.c2ln";
        let out = jwt(&format!("Bearer {token}")).unwrap();
        assert!(out.contains("\"alg\": \"HS256\"") && out.contains("\"sub\": \"ada\"") && out.contains("Expire le : 2024-11-14"), "{out}");
        assert!(jwt("a.b").is_err());
    }

    #[test]
    fn timestamps_and_dates_convert_both_ways() {
        assert!(timestamp("0").unwrap().starts_with("1970-01-01 00:00:00 UTC"));
        assert!(timestamp("1731590000000").unwrap().contains("millisecondes"));
        assert!(timestamp("2024-11-14T13:13:20Z").unwrap().starts_with("1731590000\n"));
        let local = timestamp("2024-11-14 13:13:20").unwrap();
        let back: i64 = local.lines().next().unwrap().parse().unwrap();
        assert!(timestamp(&back.to_string()).unwrap().contains("2024-11-14 13:13:20 (heure locale)"));
        assert!(timestamp("demain").is_err());
    }

    #[test]
    fn sha256_and_uuid_have_the_right_shape() {
        assert_eq!((OPS[7].run)("abc").unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let id = uuid_v4();
        assert_eq!((id.len(), &id[14..15]), (36, "4"));
        assert_ne!(id, uuid_v4());
    }
}
