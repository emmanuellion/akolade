mod format;
mod inline;
mod node;
mod style;
mod text_view;
mod utils;

use gpui::App;
pub use style::*;
pub use text_view::*;

/// akolade: highlights a text can take: on char boundaries, in order, without
/// overlap, within the text. gpui aborts the app on a run that cuts a character
/// (a "\u{a0}" in a hover or a PR body did).
pub fn char_safe_highlights<H: Copy>(text: &str, highlights: impl IntoIterator<Item = (std::ops::Range<usize>, H)>) -> Vec<(std::ops::Range<usize>, H)> {
    let snap = |i: usize| {
        let mut i = i.min(text.len());
        while !text.is_char_boundary(i) {
            i += 1;
        }
        i
    };
    let mut ix = 0;
    highlights
        .into_iter()
        .filter_map(|(r, h)| {
            let (start, end) = (snap(r.start).max(ix), snap(r.end));
            (start < end).then(|| {
                ix = end;
                (start..end, h)
            })
        })
        .collect()
}

pub(crate) fn init(cx: &mut App) {
    text_view::init(cx);
}
