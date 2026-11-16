//! File icons: the real shell icons, loaded on a background thread and
//! cached as textures, plus small drawn glyphs that stand in until an icon
//! has arrived (and always in demo mode, which never asks Windows anything).

use super::app::Msg;
use super::theme::pal;
use crate::shell::{self, IconImage};
use eframe::egui::{self, pos2, vec2, Color32, CornerRadius, Rect, Stroke, TextureHandle, Ui};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Sender};

enum Request {
    Type {
        key: String,
        ext: String,
        is_dir: bool,
    },
    Path {
        key: String,
        path: String,
    },
}

/// How many per-file icons (programs, shortcuts) are kept.
const PATH_ICON_LIMIT: usize = 400;

pub struct IconCache {
    textures: HashMap<String, Option<TextureHandle>>,
    /// Keys of per-file icons, oldest first.
    path_keys: VecDeque<String>,
    requests: Option<Sender<Request>>,
    pending: usize,
}

impl IconCache {
    pub fn new(tx: Sender<Msg>, ctx: egui::Context, demo: bool) -> IconCache {
        let requests = if demo {
            None
        } else {
            let (req_tx, req_rx) = channel::<Request>();
            let spawned = std::thread::Builder::new()
                .name("findforge-icons".into())
                .spawn(move || {
                    while let Ok(req) = req_rx.recv() {
                        let (key, image) = match req {
                            Request::Type { key, ext, is_dir } => {
                                (key, shell::icon_for_type(&ext, is_dir, true))
                            }
                            Request::Path { key, path } => (key, shell::icon_for_path(&path, true)),
                        };
                        if tx.send(Msg::Icon(key, image)).is_err() {
                            return;
                        }
                        ctx.request_repaint();
                    }
                });
            spawned.ok().map(|_| req_tx)
        };
        IconCache {
            textures: HashMap::new(),
            path_keys: VecDeque::new(),
            requests,
            pending: 0,
        }
    }

    /// Icons asked for but not yet delivered.
    pub fn pending(&self) -> usize {
        self.pending
    }

    fn request(&mut self, key: &str, make: impl FnOnce(String) -> Request) {
        if self.textures.contains_key(key) {
            return;
        }
        if let Some(tx) = &self.requests {
            self.textures.insert(key.to_string(), None);
            if tx.send(make(key.to_string())).is_ok() {
                self.pending += 1;
            }
        }
    }

    /// The icon for a row, if it has been loaded. Asks for it otherwise.
    /// `path` is given for files that carry their own icon.
    pub fn get(&mut self, ext: &str, is_dir: bool, path: Option<&str>) -> Option<TextureHandle> {
        if let Some(path) = path.filter(|_| !is_dir && shell::has_own_icon(ext)) {
            let key = format!("p:{path}");
            self.request(&key, |key| Request::Path {
                key,
                path: path.to_string(),
            });
            if let Some(Some(tex)) = self.textures.get(&key) {
                return Some(tex.clone());
            }
        }
        let key = if is_dir {
            "d:".to_string()
        } else {
            format!("t:{ext}")
        };
        self.request(&key, |key| Request::Type {
            key,
            ext: ext.to_string(),
            is_dir,
        });
        self.textures.get(&key).cloned().flatten()
    }

    /// Stores an icon delivered by the background thread.
    pub fn loaded(&mut self, ctx: &egui::Context, key: String, image: Option<IconImage>) {
        self.pending = self.pending.saturating_sub(1);
        let texture = image.map(|img| {
            let color = egui::ColorImage::from_rgba_unmultiplied(
                [img.width as usize, img.height as usize],
                &img.rgba,
            );
            ctx.load_texture(format!("icon:{key}"), color, egui::TextureOptions::LINEAR)
        });
        if key.starts_with("p:") {
            self.path_keys.push_back(key.clone());
            while self.path_keys.len() > PATH_ICON_LIMIT {
                if let Some(old) = self.path_keys.pop_front() {
                    self.textures.remove(&old);
                }
            }
        }
        self.textures.insert(key, texture);
    }
}

/// Broad kinds of files, for the drawn glyphs and the demo type names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Folder,
    Picture,
    Audio,
    Video,
    Document,
    Sheet,
    Archive,
    Program,
    Code,
    Other,
}

pub fn kind_of(ext: &str, is_dir: bool) -> Kind {
    if is_dir {
        return Kind::Folder;
    }
    match ext {
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "svg" | "ico"
        | "heic" | "cr3" | "cr2" | "nef" | "raw" | "psd" => Kind::Picture,
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "wma" | "opus" => Kind::Audio,
        "mp4" | "mkv" | "mov" | "avi" | "wmv" | "webm" | "m4v" | "mpg" => Kind::Video,
        "xlsx" | "xls" | "csv" | "ods" => Kind::Sheet,
        "pdf" | "doc" | "docx" | "dotx" | "txt" | "md" | "rtf" | "odt" | "ppt" | "pptx"
        | "epub" | "mobi" | "log" => Kind::Document,
        "zip" | "7z" | "rar" | "gz" | "tar" | "cab" | "iso" | "vhdx" | "pak" => Kind::Archive,
        "exe" | "msi" | "dll" | "sys" | "bat" | "cmd" | "lnk" | "com" | "scr" => Kind::Program,
        "rs" | "ts" | "tsx" | "js" | "py" | "css" | "html" | "json" | "toml" | "yaml" | "yml"
        | "xml" | "c" | "h" | "cpp" | "cs" | "java" | "ps1" | "sh" => Kind::Code,
        _ => Kind::Other,
    }
}

fn kind_color(kind: Kind) -> Color32 {
    match kind {
        Kind::Folder => Color32::from_rgb(0xf2, 0xb2, 0x3a),
        Kind::Picture => Color32::from_rgb(0x3c, 0xb3, 0x8a),
        Kind::Audio => Color32::from_rgb(0x9a, 0x6c, 0xe0),
        Kind::Video => Color32::from_rgb(0xe0, 0x5a, 0x6a),
        Kind::Document => Color32::from_rgb(0x4a, 0x90, 0xe2),
        Kind::Sheet => Color32::from_rgb(0x2f, 0xa5, 0x5f),
        Kind::Archive => Color32::from_rgb(0xc9, 0x86, 0x3a),
        Kind::Program => Color32::from_rgb(0x6f, 0x86, 0x9c),
        Kind::Code => Color32::from_rgb(0x2f, 0xa9, 0xc9),
        Kind::Other => Color32::from_rgb(0x8a, 0x98, 0xa6),
    }
}

/// Draws a small folder or page glyph in `rect`.
pub fn paint_glyph(ui: &Ui, rect: Rect, ext: &str, is_dir: bool) {
    let kind = kind_of(ext, is_dir);
    let color = kind_color(kind);
    let p = pal(ui);
    let s = rect.width().min(rect.height());
    let r = Rect::from_center_size(rect.center(), vec2(s, s));
    let painter = ui.painter();
    if kind == Kind::Folder {
        // A tab and a body.
        let tab = Rect::from_min_max(
            pos2(r.left() + s * 0.06, r.top() + s * 0.16),
            pos2(r.left() + s * 0.48, r.top() + s * 0.40),
        );
        painter.rect_filled(
            tab,
            CornerRadius::same((s * 0.10) as u8),
            color.gamma_multiply(0.8),
        );
        let body = Rect::from_min_max(
            pos2(r.left() + s * 0.06, r.top() + s * 0.28),
            pos2(r.right() - s * 0.06, r.bottom() - s * 0.14),
        );
        painter.rect_filled(body, CornerRadius::same((s * 0.12) as u8), color);
        return;
    }
    // A page with a coloured band.
    let page = Rect::from_min_max(
        pos2(r.left() + s * 0.18, r.top() + s * 0.06),
        pos2(r.right() - s * 0.18, r.bottom() - s * 0.06),
    );
    let paper = if p.dark {
        Color32::from_rgb(0xdd, 0xe4, 0xea)
    } else {
        Color32::WHITE
    };
    painter.rect_filled(page, CornerRadius::same((s * 0.10) as u8), paper);
    if !p.dark {
        painter.rect_stroke(
            page,
            CornerRadius::same((s * 0.10) as u8),
            Stroke::new(1.0_f32, p.border),
            egui::StrokeKind::Inside,
        );
    }
    let band = Rect::from_min_max(
        pos2(page.left(), page.bottom() - s * 0.38),
        pos2(page.right(), page.bottom() - s * 0.12),
    );
    painter.rect_filled(band, CornerRadius::ZERO, color);
    // Two "text" lines.
    for (i, w) in [0.44f32, 0.30].iter().enumerate() {
        let y = page.top() + s * (0.20 + 0.13 * i as f32);
        painter.line_segment(
            [
                pos2(page.left() + s * 0.10, y),
                pos2(page.left() + s * (0.10 + w), y),
            ],
            Stroke::new((s * 0.06).max(1.0), Color32::from_rgb(0xa9, 0xb4, 0xbe)),
        );
    }
}

/// A type name for the demo, where Windows is not asked.
pub fn demo_type_name(ext: &str, is_dir: bool) -> String {
    if is_dir {
        return "File folder".into();
    }
    let known = match ext {
        "pdf" => "PDF document",
        "docx" | "doc" => "Text document",
        "dotx" => "Document template",
        "xlsx" | "xls" => "Spreadsheet",
        "pptx" => "Presentation",
        "txt" => "Plain text",
        "md" => "Markdown text",
        "csv" => "Comma-separated values",
        "jpg" | "jpeg" => "JPEG image",
        "png" => "PNG image",
        "cr3" => "Raw photo",
        "mp3" => "MP3 audio",
        "flac" => "FLAC audio",
        "mp4" => "MP4 video",
        "mkv" => "Matroska video",
        "mov" => "QuickTime video",
        "zip" => "ZIP archive",
        "7z" => "7z archive",
        "rar" => "RAR archive",
        "iso" => "Disc image",
        "exe" => "Application",
        "msi" => "Installer package",
        "dll" => "Application extension",
        "lnk" => "Shortcut",
        "json" => "JSON file",
        "epub" | "mobi" => "E-book",
        "" => "File",
        _ => "",
    };
    if known.is_empty() {
        format!("{} file", ext.to_uppercase())
    } else {
        known.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_demo_names() {
        assert_eq!(kind_of("", true), Kind::Folder);
        assert_eq!(kind_of("jpg", false), Kind::Picture);
        assert_eq!(kind_of("flac", false), Kind::Audio);
        assert_eq!(kind_of("zzz", false), Kind::Other);
        assert_eq!(demo_type_name("pdf", false), "PDF document");
        assert_eq!(demo_type_name("zzz", false), "ZZZ file");
        assert_eq!(demo_type_name("", false), "File");
        assert_eq!(demo_type_name("pdf", true), "File folder");
    }
}
