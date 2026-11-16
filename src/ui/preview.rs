//! The preview pane: facts about the focused result and, for pictures and
//! text files, a look inside. Files are read on a background thread; in demo
//! mode nothing is read at all and invented content is shown instead.

use super::app::{App, Msg};
use super::icons;
use super::theme::{pal, regular, semibold, Icon};
use super::widgets;
use crate::engine::Row;
use crate::util;
use eframe::egui::{
    self, vec2, Color32, CornerRadius, FontFamily, FontId, Frame, Margin, Rect, RichText, Sense,
    Stroke, TextureHandle, Ui,
};
use std::sync::mpsc::Sender;

const MAX_IMAGE_BYTES: u64 = 40 << 20;
const MAX_TEXT_BYTES: usize = 64 << 10;
const MAX_SIDE: u32 = 1024;

/// What a background load produced.
pub struct Loaded {
    path: String,
    content: Content,
}

enum Content {
    Image {
        size: [usize; 2],
        rgba: Vec<u8>,
        original: (u32, u32),
    },
    Text {
        text: String,
        truncated: bool,
    },
    Nothing(String),
}

#[derive(Default)]
pub struct Preview {
    /// The path the pane currently shows (or is loading).
    path: String,
    loading: bool,
    texture: Option<(TextureHandle, (u32, u32))>,
    text: Option<(String, bool)>,
    note: String,
}

pub fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "jpe" | "jfif" | "bmp" | "gif" | "ico"
    )
}

pub fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "md"
            | "log"
            | "csv"
            | "json"
            | "xml"
            | "ini"
            | "cfg"
            | "conf"
            | "toml"
            | "yaml"
            | "yml"
            | "rs"
            | "py"
            | "js"
            | "ts"
            | "tsx"
            | "css"
            | "html"
            | "htm"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cs"
            | "java"
            | "bat"
            | "cmd"
            | "ps1"
            | "sh"
            | "sql"
            | "gitignore"
            | "srt"
            | "tex"
            | "lua"
            | "go"
            | "php"
            | "rb"
            | "kt"
            | "swift"
    )
}

fn load(path: &str, ext: &str, size: u64) -> Content {
    if is_image_ext(ext) {
        if size > MAX_IMAGE_BYTES {
            return Content::Nothing("This picture is too large to preview.".into());
        }
        return match image::open(path) {
            Ok(img) => {
                let original = (img.width(), img.height());
                let img = if img.width() > MAX_SIDE || img.height() > MAX_SIDE {
                    img.thumbnail(MAX_SIDE, MAX_SIDE)
                } else {
                    img
                };
                let rgba = img.into_rgba8();
                Content::Image {
                    size: [rgba.width() as usize, rgba.height() as usize],
                    rgba: rgba.into_raw(),
                    original,
                }
            }
            Err(_) => Content::Nothing("This picture could not be read.".into()),
        };
    }
    if is_text_ext(ext) {
        use std::io::Read;
        let mut buf = vec![0u8; MAX_TEXT_BYTES + 1];
        let read = std::fs::File::open(path).and_then(|mut f| {
            let mut n = 0;
            loop {
                let got = f.read(&mut buf[n..])?;
                if got == 0 {
                    break;
                }
                n += got;
                if n == buf.len() {
                    break;
                }
            }
            Ok(n)
        });
        return match read {
            Ok(n) => {
                let truncated = n > MAX_TEXT_BYTES;
                let bytes = &buf[..n.min(MAX_TEXT_BYTES)];
                if bytes.contains(&0) {
                    Content::Nothing("This file does not contain plain text.".into())
                } else {
                    Content::Text {
                        text: String::from_utf8_lossy(bytes).replace('\r', ""),
                        truncated,
                    }
                }
            }
            Err(_) => Content::Nothing("This file could not be read.".into()),
        };
    }
    Content::Nothing(String::new())
}

/// An invented picture for the demo: a sky, a sun and two rows of hills.
fn demo_picture(seed: u32) -> Content {
    let (w, h) = (640usize, 400usize);
    let mut rgba = vec![0u8; w * h * 4];
    let hue = (seed % 5) as f32;
    let sun = (
        120.0 + (seed % 7) as f32 * 60.0,
        90.0 + (seed % 3) as f32 * 30.0,
    );
    for y in 0..h {
        for x in 0..w {
            let t = y as f32 / h as f32;
            // Sky gradient.
            let mut c = [
                70.0 + 150.0 * t + hue * 6.0,
                130.0 + 80.0 * t,
                210.0 - 40.0 * t - hue * 8.0,
            ];
            let d = ((x as f32 - sun.0).powi(2) + (y as f32 - sun.1).powi(2)).sqrt();
            if d < 34.0 {
                c = [255.0, 236.0, 170.0];
            } else if d < 90.0 {
                let g = 1.0 - (d - 34.0) / 56.0;
                c = [c[0] + 90.0 * g, c[1] + 60.0 * g, c[2] + 10.0 * g];
            }
            let xf = x as f32 / w as f32;
            let far = 0.62 + 0.07 * (xf * 7.0 + hue).sin() + 0.03 * (xf * 19.0).sin();
            let near = 0.78 + 0.06 * (xf * 4.0 + 1.7 + hue).sin() + 0.02 * (xf * 23.0).cos();
            if t > near {
                c = [34.0, 92.0 - 20.0 * (t - near), 70.0];
            } else if t > far {
                c = [62.0, 120.0, 128.0];
            }
            let i = (y * w + x) * 4;
            rgba[i] = c[0].clamp(0.0, 255.0) as u8;
            rgba[i + 1] = c[1].clamp(0.0, 255.0) as u8;
            rgba[i + 2] = c[2].clamp(0.0, 255.0) as u8;
            rgba[i + 3] = 255;
        }
    }
    Content::Image {
        size: [w, h],
        rgba,
        original: (4032, 2520),
    }
}

fn demo_text(name: &str) -> Content {
    let title = name.rsplit_once('.').map(|(n, _)| n).unwrap_or(name);
    let text = format!(
        "{title}\n{}\n\nSummary\n  Revenue grew in every region, led by\n  the northern stores. Costs stayed\n  within the plan.\n\nNext steps\n  1. Review the figures.\n  2. Publish the summary.\n  3. Archive last year's drafts.\n\nNotes\n  Invented sample text for the demo.\n  Nothing was read from a disk.\n",
        "=".repeat(title.chars().count().min(40))
    );
    Content::Text {
        text,
        truncated: false,
    }
}

impl Preview {
    pub fn loading(&self) -> bool {
        self.loading
    }

    /// Makes the pane show `row` (starts loading if it is a different file).
    pub fn show(&mut self, row: &Row, demo: bool, tx: &Sender<Msg>, ctx: &egui::Context) {
        let path = row.full_path();
        if path == self.path {
            return;
        }
        self.path = path.clone();
        self.texture = None;
        self.text = None;
        self.note.clear();
        self.loading = false;
        if row.is_dir {
            return;
        }
        let ext = row.ext.clone();
        if demo {
            // Invented content, made right here: nothing touches the disk.
            let content = if is_image_ext(&ext) {
                demo_picture(row.name.len() as u32 + row.size as u32 % 97)
            } else if is_text_ext(&ext) {
                demo_text(&row.name)
            } else {
                Content::Nothing(String::new())
            };
            self.loaded(ctx, Loaded { path, content });
            return;
        }
        if !is_image_ext(&ext) && !is_text_ext(&ext) {
            return;
        }
        self.loading = true;
        let (tx, ctx, size) = (tx.clone(), ctx.clone(), row.size);
        std::thread::spawn(move || {
            let content = load(&path, &ext, size);
            let _ = tx.send(Msg::Preview(Loaded { path, content }));
            ctx.request_repaint();
        });
    }

    pub fn clear(&mut self) {
        *self = Preview::default();
    }

    /// Takes what the background thread produced (ignored if the pane has
    /// moved on to another file meanwhile).
    pub fn loaded(&mut self, ctx: &egui::Context, l: Loaded) {
        if l.path != self.path {
            return;
        }
        self.loading = false;
        match l.content {
            Content::Image {
                size,
                rgba,
                original,
            } => {
                let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                let tex = ctx.load_texture("preview", image, egui::TextureOptions::LINEAR);
                self.texture = Some((tex, original));
            }
            Content::Text { text, truncated } => self.text = Some((text, truncated)),
            Content::Nothing(note) => self.note = note,
        }
    }
}

impl App {
    /// The focused result, if any.
    pub(super) fn focused_row(&mut self) -> Option<Row> {
        let idx = self.cursor?;
        let hit = *self.results.as_ref()?.hits.get(idx)?;
        self.engine.rows(&[hit]).pop().filter(|r| r.exists)
    }

    pub(super) fn preview_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let Some(row) = self.focused_row() else {
            self.preview.clear();
            widgets::empty_state(
                ui,
                Icon::PREVIEW,
                p.muted,
                "Nothing selected",
                "Select a result to see its details and a preview here.",
            );
            return;
        };
        let ctx = ui.ctx().clone();
        self.preview.show(&row, self.demo, &self.tx, &ctx);
        let path = row.full_path();

        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
            match self
                .icons
                .get(&row.ext, row.is_dir, (!self.demo).then_some(path.as_str()))
            {
                Some(tex) => {
                    ui.painter().image(
                        tex.id(),
                        rect,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                None => icons::paint_glyph(ui, rect, &row.ext, row.is_dir),
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add(egui::Label::new(RichText::new(&row.name).font(semibold(15.5))).truncate());
                ui.label(
                    RichText::new(self.type_name(&row.ext, row.is_dir))
                        .size(12.5)
                        .color(p.muted),
                );
            });
        });
        ui.add_space(6.0);

        // The preview itself.
        let avail = ui.available_width();
        if let Some((tex, original)) = &self.preview.texture {
            let size = tex.size_vec2();
            let scale = (avail / size.x).min(260.0 / size.y).min(1.0);
            let shown = size * scale;
            Frame::new()
                .fill(p.bg)
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(6))
                .show(ui, |ui| {
                    ui.set_width(avail - 12.0);
                    ui.vertical_centered(|ui| {
                        let (rect, _) = ui
                            .allocate_exact_size(shown - vec2(12.0, 12.0) * scale, Sense::hover());
                        ui.painter().image(
                            tex.id(),
                            rect,
                            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    });
                });
            ui.label(
                RichText::new(format!("{} × {} pixels", original.0, original.1))
                    .size(12.0)
                    .color(p.faint),
            );
        } else if let Some((text, truncated)) = &self.preview.text {
            Frame::new()
                .fill(p.bg)
                .stroke(Stroke::new(1.0_f32, p.border))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(avail - 20.0);
                    egui::ScrollArea::vertical()
                        .id_salt("preview-text")
                        .max_height(270.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(text.as_str())
                                        .font(FontId::new(12.5, FontFamily::Monospace))
                                        .color(p.text),
                                )
                                .wrap(),
                            );
                        });
                });
            if *truncated {
                ui.label(
                    RichText::new("Only the beginning of the file is shown.")
                        .size(12.0)
                        .color(p.faint),
                );
            }
        } else if self.preview.loading {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0).color(p.accent));
                ui.label(RichText::new("Loading the preview…").color(p.muted));
            });
        } else if !self.preview.note.is_empty() {
            ui.label(RichText::new(&self.preview.note).color(p.muted));
        }
        ui.add_space(8.0);
        widgets::divider(ui);
        ui.add_space(8.0);

        egui::ScrollArea::vertical()
            .id_salt("preview-facts")
            .show(ui, |ui| {
                if !row.is_dir {
                    widgets::field(
                        ui,
                        "Size",
                        &format!(
                            "{} ({} bytes)",
                            util::fmt_size(row.size),
                            util::fmt_count(row.size)
                        ),
                    );
                }
                widgets::field(
                    ui,
                    "Date modified",
                    &util::fmt_time(row.mtime, self.utc_offset),
                );
                widgets::field(
                    ui,
                    "Date created",
                    &util::fmt_time(row.ctime, self.utc_offset),
                );
                widgets::field(ui, "Folder", &row.dir);
                let mut attrs = Vec::new();
                use crate::index::flags;
                if row.flags & flags::HIDDEN != 0 {
                    attrs.push("Hidden");
                }
                if row.flags & flags::SYSTEM != 0 {
                    attrs.push("System");
                }
                if row.flags & flags::READONLY != 0 {
                    attrs.push("Read-only");
                }
                if row.flags & flags::REPARSE != 0 {
                    attrs.push("Link");
                }
                if !attrs.is_empty() {
                    widgets::field(ui, "Attributes", &attrs.join(", "));
                }
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    if widgets::ghost_button(ui, &Icon::OPEN.with("Open")).clicked() {
                        self.open_selected();
                    }
                    if widgets::ghost_button(ui, &Icon::FOLDER_OPEN.with("Open folder")).clicked() {
                        self.reveal_selected();
                    }
                    if widgets::ghost_button(ui, &Icon::COPY.with("Copy path")).clicked() {
                        self.copy_paths();
                    }
                });
            });
        let _ = regular(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previewable_kinds() {
        assert!(is_image_ext("png") && is_image_ext("jpg") && !is_image_ext("cr3"));
        assert!(is_text_ext("txt") && is_text_ext("rs") && !is_text_ext("pdf"));
    }

    #[test]
    fn loads_text_and_refuses_binary_from_a_temp_folder() {
        let dir =
            std::env::temp_dir().join(format!("findforge-preview-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("a.txt");
        std::fs::write(&text, "line one\r\nline two\r\n").unwrap();
        match load(&text.to_string_lossy(), "txt", 20) {
            Content::Text { text, truncated } => {
                assert_eq!(text, "line one\nline two\n");
                assert!(!truncated);
            }
            _ => panic!("expected text"),
        }
        let big = dir.join("big.log");
        std::fs::write(&big, vec![b'a'; MAX_TEXT_BYTES + 500]).unwrap();
        match load(&big.to_string_lossy(), "log", 0) {
            Content::Text { text, truncated } => {
                assert_eq!(text.len(), MAX_TEXT_BYTES);
                assert!(truncated);
            }
            _ => panic!("expected text"),
        }
        let bin = dir.join("b.txt");
        std::fs::write(&bin, [1u8, 0, 2, 0]).unwrap();
        assert!(
            matches!(load(&bin.to_string_lossy(), "txt", 4), Content::Nothing(n) if !n.is_empty())
        );
        // A tiny PNG made on the spot.
        let png = dir.join("p.png");
        image::save_buffer(
            &png,
            &[255u8, 0, 0, 255, 0, 255, 0, 255],
            2,
            1,
            image::ColorType::Rgba8,
        )
        .unwrap();
        match load(&png.to_string_lossy(), "png", 100) {
            Content::Image {
                size,
                rgba,
                original,
            } => {
                assert_eq!(size, [2, 1]);
                assert_eq!(original, (2, 1));
                assert_eq!(rgba.len(), 8);
            }
            _ => panic!("expected an image"),
        }
        assert!(matches!(
            load(&png.to_string_lossy(), "png", MAX_IMAGE_BYTES + 1),
            Content::Nothing(_)
        ));
        assert!(matches!(
            load(&dir.join("missing.txt").to_string_lossy(), "txt", 1),
            Content::Nothing(_)
        ));
        assert!(
            matches!(load(&text.to_string_lossy(), "zip", 1), Content::Nothing(n) if n.is_empty())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn demo_content_is_made_up_without_a_disk() {
        assert!(matches!(
            demo_picture(3),
            Content::Image {
                size: [640, 400],
                ..
            }
        ));
        match demo_text("Report notes.txt") {
            Content::Text { text, .. } => assert!(text.starts_with("Report notes\n")),
            _ => panic!(),
        }
    }
}
