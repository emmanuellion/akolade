//! The few shared looks every panel and tool tab uses, so they all read the
//! same: empty, loading and error states, row hover, section titles.

use gpui::{App, Div, FontWeight, Hsla, ParentElement, Styled, div, prelude::FluentBuilder};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable, spinner::Spinner, v_flex};

use crate::workspace::r;

/// "1 fichier", "3 fichiers".
pub fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n > 1 { "s" } else { "" })
}

/// A name as lists sort it: case and accents aside ("Éclat" between "Ébène"… and "Encre").
pub fn sort_key(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' | 'á' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' => 'i',
            'ô' | 'ö' | 'ó' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ç' => 'c',
            c => c,
        })
        .collect()
}

#[test]
fn names_sort_without_case_or_accents() {
    let mut names = ["Océan", "encre", "Éclat", "Aube", "Pêche", "Papier"];
    names.sort_by_key(|n| sort_key(n));
    assert_eq!(names, ["Aube", "Éclat", "encre", "Océan", "Papier", "Pêche"]);
}

/// The hover background of a clickable row or card, everywhere.
pub fn hover_bg(cx: &App) -> Hsla {
    cx.theme().foreground.opacity(0.05)
}

/// Nothing to show yet: an icon in a soft circle, a title, a line of help.
pub fn empty_state(icon: Icon, color: Hsla, title: &str, text: &str, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(r(8.))
        .p(r(24.))
        .child(div().size(r(56.)).mb(r(6.)).flex().items_center().justify_center().rounded_full().bg(color.opacity(0.12)).child(icon.size(r(26.)).text_color(color)))
        .child(div().text_size(r(15.)).font_weight(FontWeight::SEMIBOLD).text_center().child(title.to_string()))
        .when(!text.is_empty(), |d| d.child(div().max_w(r(380.)).text_center().text_size(r(13.)).text_color(muted).child(text.to_string())))
}

/// Something failed: the same layout, in red.
pub fn error_state(title: &str, text: &str, cx: &App) -> Div {
    empty_state(Icon::new(IconName::TriangleAlert), cx.theme().red, title, text, cx)
}

/// Waiting for something slow: a spinner and what is happening.
pub fn loading(text: &str, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(r(10.))
        .child(Spinner::new().large().color(muted))
        .child(div().text_size(r(13.)).text_color(muted).child(text.to_string()))
}

/// A small uppercase title over a group of rows (sidebar, tool panes).
pub fn section_title(text: &str, cx: &App) -> Div {
    div().text_size(r(11.)).font_weight(FontWeight::SEMIBOLD).text_color(cx.theme().muted_foreground).child(text.to_uppercase())
}
