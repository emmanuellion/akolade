//! Built-in image viewer: fit / zoom, and the image's details.

use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result};
use gpui::{
    Context, EventEmitter, FontWeight, ImageSource, InteractiveElement, IntoElement, ObjectFit,
    ParentElement, Render, RenderImage, StatefulInteractiveElement, Styled, StyledImage, Task,
    Window, div, img, prelude::FluentBuilder, px,
};
use gpui_component::{
    ActiveTheme, Sizable,
    button::ButtonVariants,
    h_flex, v_flex,
};
use image::{AnimationDecoder, Frame, ImageFormat, codecs::gif::GifDecoder, imageops};

use crate::{
    icons,
    workspace::{button, r},
};

/// Extensions opened in the viewer instead of the code editor.
pub const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "svg"];

/// Bigger images are shown as a reduced preview (GPU texture limits).
const MAX_SIDE: u32 = 4096;

/// Memory for the decoded frames of an animation: a long screen-recording GIF
/// would need gigabytes. Past it, only the first frames are played.
const MAX_ANIMATION_BYTES: usize = 256 << 20;

/// The first frames of a GIF that fit in `budget` bytes (at least one), and
/// whether some were left out.
fn gif_frames(bytes: &[u8], budget: usize) -> Result<(Vec<Frame>, bool)> {
    let mut frames = vec![];
    let mut used = 0;
    for frame in GifDecoder::new(Cursor::new(bytes))?.into_frames() {
        let frame = frame?;
        let size = frame.buffer().len();
        if used + size > budget && !frames.is_empty() {
            return Ok((frames, true));
        }
        used += size;
        frames.push(frame);
    }
    Ok((frames, false))
}

pub fn is_image(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    EXTENSIONS.contains(&ext.as_str())
}

pub struct Decoded {
    pub image: Arc<RenderImage>,
    pub format: String,
    /// Real size, even when the preview is reduced.
    pub width: u32,
    pub height: u32,
    pub reduced: bool,
    pub frames: usize,
    /// An animation cut to its first frames (memory budget).
    pub truncated: bool,
}

pub fn decode(path: &Path) -> Result<Decoded> {
    let bytes = fs::read(path)?;
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg")) {
        let tree = resvg::usvg::Tree::from_data(&bytes, &Default::default())?;
        let size = tree.size();
        let (width, height) = (size.width().ceil() as u32, size.height().ceil() as u32);
        // 2x for retina, but never above MAX_SIDE.
        let scale = (MAX_SIDE as f32 / size.width().max(size.height())).min(2.0);
        let pixmap = icons::render_pixmap(&bytes, scale).context("SVG vide ou trop grand")?;
        let image = Arc::new(RenderImage::new(vec![icons::pixmap_frame(pixmap)]));
        return Ok(Decoded { image, format: "SVG".into(), width, height, reduced: false, frames: 1, truncated: false });
    }

    let format = image::guess_format(&bytes).context("format d'image inconnu")?;
    let (frames, truncated) = if format == ImageFormat::Gif {
        gif_frames(&bytes, MAX_ANIMATION_BYTES)?
    } else {
        (vec![Frame::new(image::load_from_memory_with_format(&bytes, format)?.into_rgba8())], false)
    };
    let (width, height) = frames.first().context("image sans contenu")?.buffer().dimensions();
    let reduced = width.max(height) > MAX_SIDE;
    let count = frames.len();
    let frames: Vec<Frame> = frames
        .into_iter()
        .map(|f| {
            let (left, top, delay) = (f.left(), f.top(), f.delay());
            let mut buf = f.into_buffer();
            if reduced {
                let ratio = MAX_SIDE as f64 / width.max(height) as f64;
                let (w, h) = ((width as f64 * ratio) as u32, (height as f64 * ratio) as u32);
                buf = imageops::thumbnail(&buf, w.max(1), h.max(1));
            }
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2); // gpui wants BGRA
            }
            Frame::from_parts(buf, left, top, delay)
        })
        .collect();
    let format = format!("{format:?}").to_uppercase();
    Ok(Decoded { image: Arc::new(RenderImage::new(frames)), format, width, height, reduced, frames: count, truncated })
}

/// "512 o", "245,3 Ko", "1,2 Mo".
pub fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} o"),
        b if b < 1024 * 1024 => format!("{:.1} Ko", b as f64 / 1024.0).replace('.', ","),
        b => format!("{:.1} Mo", b as f64 / (1024.0 * 1024.0)).replace('.', ","),
    }
}

enum State {
    Loading,
    Ready(Decoded),
    Failed(String),
}

pub enum ImageViewerEvent {
    /// SVG: the user wants to edit the source.
    ShowSource,
}

pub struct ImageViewer {
    path: PathBuf,
    state: State,
    file_size: u64,
    /// None = fit the panel (never upscaled); Some(1.0) = 100 %.
    zoom: Option<f32>,
    _task: Task<()>,
}

impl EventEmitter<ImageViewerEvent> for ImageViewer {}

impl ImageViewer {
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let p = path.clone();
        // Decoding a big photo takes a while: keep the UI responsive.
        let task = cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { decode(&p) }).await;
            let _ = this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(d) => State::Ready(d),
                    Err(e) => State::Failed(e.to_string()),
                };
                cx.notify();
            });
        });
        let file_size = fs::metadata(&path).map_or(0, |m| m.len());
        Self { path, state: State::Loading, file_size, zoom: None, _task: task }
    }

    /// "1280 × 720" once decoded.
    pub fn dimensions(&self) -> Option<String> {
        match &self.state {
            State::Ready(d) => Some(format!("{} × {}", d.width, d.height)),
            _ => None,
        }
    }

    fn set_zoom(&mut self, zoom: Option<f32>, cx: &mut Context<Self>) {
        self.zoom = zoom.map(|z| z.clamp(0.05, 32.0));
        cx.notify();
    }
}

impl Render for ImageViewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let message = |text: String| div().size_full().flex().items_center().justify_center().text_color(theme.muted_foreground).child(text);
        let body = match &self.state {
            State::Loading => message("Chargement…".into()).into_any_element(),
            State::Failed(e) => message(format!("Impossible d'afficher l'image : {e}")).into_any_element(),
            State::Ready(d) => match self.zoom {
                // gpui's img forces its aspect ratio and intrinsic size, so it is
                // capped with max sizes inside a box pinned to the panel.
                // The id lets gpui keep the current frame of animated GIFs.
                None => div()
                    .absolute()
                    .inset_0()
                    .p_6()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        img(ImageSource::Render(d.image.clone()))
                            .object_fit(ObjectFit::Contain)
                            .max_w_full()
                            .max_h_full()
                            .id("image"),
                    )
                    .into_any_element(),
                Some(z) => div()
                    .id("image-scroll")
                    .size_full()
                    .flex()
                    .overflow_scroll()
                    .child(
                        img(ImageSource::Render(d.image.clone()))
                            .flex_shrink_0()
                            .m_auto()
                            .w(px(d.width as f32 * z))
                            .h(px(d.height as f32 * z))
                            .id("image"),
                    )
                    .into_any_element(),
            },
        };

        let mut details = vec![];
        if let State::Ready(d) = &self.state {
            details.push(d.format.clone());
            details.push(format!("{} × {} px", d.width, d.height));
            if d.frames > 1 {
                details.push(format!("{} images", d.frames));
            }
        }
        details.push(human_size(self.file_size));
        let reduced = matches!(&self.state, State::Ready(d) if d.reduced);
        let truncated = matches!(&self.state, State::Ready(d) if d.truncated);
        let is_svg = self.path.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg"));
        let zoom_label = self.zoom.map_or("Ajusté".to_string(), |z| format!("{:.0} %", z * 100.0));
        let zoom = self.zoom;

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(div().flex_1().min_h_0().relative().child(body))
            .child(
                h_flex()
                    .h(r(36.))
                    .flex_shrink_0()
                    .px(r(12.))
                    .gap(r(4.))
                    .text_size(r(12.))
                    .bg(theme.title_bar)
                    .border_t_1()
                    .border_color(theme.border)
                    .text_color(theme.muted_foreground)
                    .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.foreground).child(details.join("  ·  ")))
                    .when(reduced, |this| this.child(div().pl_2().child("(aperçu réduit)")))
                    .when(truncated, |this| this.child(div().pl_2().child("(animation tronquée)")))
                    .child(div().flex_1())
                    .when(is_svg, |this| {
                        this.child(button("svg-source").ghost().small().label("Code source").mr_2()
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ImageViewerEvent::ShowSource))))
                    })
                    .child(button("zoom-out").ghost().small().label("−").tooltip("Dézoomer")
                        .on_click(cx.listener(move |this, _, _, cx| this.set_zoom(Some(zoom.unwrap_or(1.0) / 1.25), cx))))
                    .child(div().w(r(52.)).text_center().child(zoom_label))
                    .child(button("zoom-in").ghost().small().label("+").tooltip("Zoomer")
                        .on_click(cx.listener(move |this, _, _, cx| this.set_zoom(Some(zoom.unwrap_or(1.0) * 1.25), cx))))
                    .child(button("zoom-100").ghost().small().label("100 %").tooltip("Taille réelle")
                        .on_click(cx.listener(|this, _, _, cx| this.set_zoom(Some(1.0), cx))))
                    .child(button("zoom-fit").ghost().small().label("Ajuster").tooltip("Ajuster à la fenêtre")
                        .on_click(cx.listener(|this, _, _, cx| this.set_zoom(None, cx)))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("akolade-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn decodes_png_and_reports_real_size() {
        let path = tmp("a.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([255, 0, 0, 255])).save(&path).unwrap();
        let d = decode(&path).unwrap();
        assert_eq!((d.format.as_str(), d.width, d.height, d.reduced), ("PNG", 3, 2, false));
        // Red in RGBA must become BGRA for gpui.
        assert_eq!(&d.image.as_bytes(0).unwrap()[..4], &[0, 0, 255, 255]);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn huge_images_are_reduced_but_keep_their_size() {
        let path = tmp("big.png");
        image::RgbaImage::new(MAX_SIDE * 2, 10).save(&path).unwrap();
        let d = decode(&path).unwrap();
        assert!(d.reduced);
        assert_eq!((d.width, d.height), (MAX_SIDE * 2, 10));
        assert_eq!(d.image.size(0).width.0 as u32, MAX_SIDE);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn decodes_svg_and_rejects_garbage() {
        let path = tmp("a.svg");
        fs::write(&path, r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="20"><rect width="10" height="20"/></svg>"#).unwrap();
        let d = decode(&path).unwrap();
        assert_eq!((d.format.as_str(), d.width, d.height), ("SVG", 10, 20));
        fs::write(&path, "pas une image").unwrap();
        assert!(decode(&path).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn long_animations_stop_at_the_memory_budget() {
        let mut gif = vec![];
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            for i in 0..5u8 {
                encoder.encode_frame(Frame::new(image::RgbaImage::from_pixel(4, 4, image::Rgba([i * 40, 0, 0, 255])))).unwrap();
            }
        }
        let frame_bytes = 4 * 4 * 4;
        let (frames, truncated) = gif_frames(&gif, 2 * frame_bytes).unwrap();
        assert_eq!((frames.len(), truncated), (2, true));
        let (frames, truncated) = gif_frames(&gif, 1).unwrap();
        assert_eq!((frames.len(), truncated), (1, true), "always at least one frame");
        let (frames, truncated) = gif_frames(&gif, MAX_ANIMATION_BYTES).unwrap();
        assert_eq!((frames.len(), truncated), (5, false));
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(512), "512 o");
        assert_eq!(human_size(2048 + 307), "2,3 Ko");
        assert_eq!(human_size(5 * 1024 * 1024), "5,0 Mo");
        assert!(is_image(Path::new("photo.JPG")) && !is_image(Path::new("a.ts")));
    }
}
