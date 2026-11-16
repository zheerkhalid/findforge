//! The search view: filter chips, the virtualised results table, the status
//! bar, selection, keyboard handling and the actions on results.

use super::app::{index_summary, App, DemoSelect, FilterDialog, Msg, RenameDialog, RowCache, Tab};
use super::icons;
use super::theme::{pal, pal_ctx, regular, semibold, Icon, Palette};
use super::widgets;
use crate::engine::Row;
use crate::index::Hit;
use crate::query::SortKey;
use crate::settings::default_columns;
use crate::{export, shell, util};
use eframe::egui::{
    self, pos2, text::LayoutJob, text::TextFormat, text::TextWrapping, vec2, Align, Align2,
    Color32, CornerRadius, CursorIcon, Frame, Key, Layout, Margin, Rect, RichText, Sense, Stroke,
    Ui,
};
use std::ops::Range;

pub(super) const ROW_H: f32 = 30.0;
const HEADER_H: f32 = 34.0;
/// Opening more files than this at once is almost certainly a slip.
const OPEN_LIMIT: usize = 12;

fn column_title(id: &str) -> &'static str {
    match id {
        "name" => "NAME",
        "path" => "PATH",
        "size" => "SIZE",
        "modified" => "DATE MODIFIED",
        "created" => "DATE CREATED",
        _ => "TYPE",
    }
}

fn column_label(id: &str) -> &'static str {
    match id {
        "name" => "Name",
        "path" => "Path",
        "size" => "Size",
        "modified" => "Date modified",
        "created" => "Date created",
        _ => "Type",
    }
}

/// A visible column: index into the settings and its place on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ColumnBox {
    pub index: usize,
    pub x: f32,
    pub width: f32,
}

/// Lays the visible columns out from `left`. Spare room goes to the path
/// column (or the name column when the path is hidden); when there is too
/// little room the path column gives way first, then the name column.
pub(super) fn layout_columns(
    columns: &[crate::settings::ColumnSetting],
    left: f32,
    available: f32,
) -> Vec<ColumnBox> {
    const MIN_PATH: f32 = 110.0;
    const MIN_NAME: f32 = 170.0;
    let visible: Vec<usize> = (0..columns.len()).filter(|i| columns[*i].visible).collect();
    let mut widths: Vec<f32> = visible.iter().map(|i| columns[*i].width).collect();
    let find = |id: &str| visible.iter().position(|i| columns[*i].id == id);
    let (path, name) = (find("path"), find("name"));
    let total: f32 = widths.iter().sum();
    if total < available {
        if let Some(k) = path.or(name) {
            widths[k] += available - total;
        }
    } else {
        // Both give way in proportion to how much they can spare.
        let deficit = total - available;
        let slack =
            |k: Option<usize>, min: f32| k.map(|k| (widths[k] - min).max(0.0)).unwrap_or(0.0);
        let (sp, sn) = (slack(path, MIN_PATH), slack(name, MIN_NAME));
        let share = if sp + sn > 0.0 {
            (deficit / (sp + sn)).min(1.0)
        } else {
            0.0
        };
        if let Some(k) = path {
            widths[k] -= sp * share;
        }
        if let Some(k) = name {
            widths[k] -= sn * share;
        }
    }
    let mut x = left;
    visible
        .into_iter()
        .zip(widths)
        .map(|(index, width)| {
            let b = ColumnBox { index, x, width };
            x += width;
            b
        })
        .collect()
}

/// A single-line layout of `name` with the matched ranges in the accent colour.
fn name_job(
    name: &str,
    ranges: &[Range<usize>],
    color: Color32,
    mark: Color32,
    max_width: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let plain = TextFormat {
        font_id: regular(14.0),
        color,
        ..Default::default()
    };
    let hot = TextFormat {
        font_id: semibold(14.0),
        color: mark,
        ..Default::default()
    };
    let mut at = 0;
    for r in ranges {
        if r.start < at || r.end > name.len() || name.get(r.clone()).is_none() {
            continue;
        }
        if r.start > at {
            job.append(&name[at..r.start], 0.0, plain.clone());
        }
        job.append(&name[r.clone()], 0.0, hot.clone());
        at = r.end;
    }
    if at < name.len() {
        job.append(&name[at..], 0.0, plain);
    }
    job.wrap = TextWrapping {
        max_width: max_width.max(8.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    job
}

enum RowEvent {
    Click {
        index: usize,
        ctrl: bool,
        shift: bool,
    },
    Open(usize),
    Menu(usize),
    Drag(usize),
}

impl App {
    pub(super) fn type_name(&mut self, ext: &str, is_dir: bool) -> String {
        let key = (ext.to_string(), is_dir);
        if let Some(n) = self.type_names.get(&key) {
            return n.clone();
        }
        let name = if self.demo {
            icons::demo_type_name(ext, is_dir)
        } else {
            shell::type_name(ext, is_dir)
        };
        self.type_names.insert(key, name.clone());
        name
    }

    fn hit_count(&self) -> usize {
        self.results.as_ref().map(|r| r.hits.len()).unwrap_or(0)
    }

    // ----------------------------------------------------------------------------------
    // Selection
    // ----------------------------------------------------------------------------------

    fn hit_at(&self, index: usize) -> Option<Hit> {
        self.results.as_ref()?.hits.get(index).copied()
    }

    fn select_only(&mut self, index: usize) {
        self.selected.clear();
        if let Some(h) = self.hit_at(index) {
            self.selected.insert(h);
        }
        self.cursor = Some(index);
        self.anchor = Some(index);
    }

    fn select_range(&mut self, to: usize, keep: bool) {
        let from = self.anchor.or(self.cursor).unwrap_or(to);
        if !keep {
            self.selected.clear();
        }
        if let Some(r) = &self.results {
            let (a, b) = (
                from.min(to),
                from.max(to).min(r.hits.len().saturating_sub(1)),
            );
            if a < r.hits.len() {
                self.selected.extend(r.hits[a..=b].iter().copied());
            }
        }
        self.cursor = Some(to);
        self.anchor = Some(from);
    }

    fn move_cursor(&mut self, delta: i64, shift: bool) {
        let n = self.hit_count();
        if n == 0 {
            return;
        }
        let next = match self.cursor {
            Some(c) => (c as i64 + delta).clamp(0, n as i64 - 1) as usize,
            None if delta >= 0 => 0,
            None => n - 1,
        };
        if shift {
            self.select_range(next, false);
        } else {
            self.select_only(next);
        }
        self.scroll_to_cursor = true;
    }

    /// Hits the actions apply to: the selection in list order, or the cursor row.
    fn action_hits(&self) -> Vec<Hit> {
        let Some(r) = &self.results else {
            return Vec::new();
        };
        if self.selected.is_empty() {
            return self
                .cursor
                .and_then(|c| r.hits.get(c).copied())
                .into_iter()
                .collect();
        }
        if self.selected.len() <= 2000 {
            // Keep the order of the list for small selections.
            let mut v: Vec<(usize, Hit)> = Vec::new();
            let near = self.cursor.unwrap_or(0);
            let lo = near.saturating_sub(200_000);
            let hi = (near + 200_000).min(r.hits.len());
            for (i, h) in r.hits[lo..hi].iter().enumerate() {
                if self.selected.contains(h) {
                    v.push((lo + i, *h));
                    if v.len() == self.selected.len() {
                        break;
                    }
                }
            }
            if v.len() == self.selected.len() {
                return v.into_iter().map(|(_, h)| h).collect();
            }
        }
        r.hits
            .iter()
            .copied()
            .filter(|h| self.selected.contains(h))
            .collect()
    }

    fn action_paths(&self) -> Vec<String> {
        self.engine.paths(&self.action_hits())
    }

    fn demo_blocked(&mut self) -> bool {
        if self.demo {
            self.toast(
                "Demo: these files are invented, so there is nothing to act on.",
                false,
            );
        }
        self.demo
    }

    // ----------------------------------------------------------------------------------
    // Actions
    // ----------------------------------------------------------------------------------

    pub(super) fn open_selected(&mut self) {
        let paths = self.action_paths();
        if paths.is_empty() {
            return;
        }
        self.note_history();
        if self.demo_blocked() {
            return;
        }
        if paths.len() > OPEN_LIMIT {
            self.toast(
                format!(
                    "{} items are selected. Open at most {OPEN_LIMIT} at a time.",
                    paths.len()
                ),
                true,
            );
            return;
        }
        for p in &paths {
            if let Err(e) = shell::open(p) {
                self.toast(e, true);
                break;
            }
        }
    }

    pub(super) fn reveal_selected(&mut self) {
        let paths = self.action_paths();
        let Some(first) = paths.first() else { return };
        self.note_history();
        if self.demo_blocked() {
            return;
        }
        if let Err(e) = shell::reveal(first) {
            self.toast(e, true);
        }
    }

    pub(super) fn copy_paths(&mut self) {
        let paths = self.action_paths();
        if paths.is_empty() {
            return;
        }
        match shell::copy_text(&paths.join("\r\n")) {
            Ok(()) => self.toast(
                if paths.len() == 1 {
                    "Path copied.".to_string()
                } else {
                    format!("{} paths copied.", paths.len())
                },
                false,
            ),
            Err(e) => self.toast(e, true),
        }
    }

    fn copy_names(&mut self) {
        let hits = self.action_hits();
        let names: Vec<String> = self
            .engine
            .rows(&hits)
            .into_iter()
            .filter(|r| r.exists)
            .map(|r| r.name)
            .collect();
        if names.is_empty() {
            return;
        }
        match shell::copy_text(&names.join("\r\n")) {
            Ok(()) => self.toast(
                if names.len() == 1 {
                    "Name copied.".to_string()
                } else {
                    format!("{} names copied.", names.len())
                },
                false,
            ),
            Err(e) => self.toast(e, true),
        }
    }

    fn copy_files(&mut self) {
        let paths = self.action_paths();
        if paths.is_empty() || self.demo_blocked() {
            return;
        }
        match shell::copy_files(&paths) {
            Ok(()) => self.toast(
                if paths.len() == 1 {
                    "Copied. Paste it in any folder.".to_string()
                } else {
                    format!("{} items copied. Paste them in any folder.", paths.len())
                },
                false,
            ),
            Err(e) => self.toast(e, true),
        }
    }

    fn show_properties(&mut self) {
        let paths = self.action_paths();
        let Some(first) = paths.first() else { return };
        if self.demo_blocked() {
            return;
        }
        if let Err(e) = shell::properties(self.hwnd, first) {
            self.toast(e, true);
        }
    }

    pub(super) fn start_rename(&mut self) {
        let hits = self.action_hits();
        let Some(row) = hits
            .first()
            .and_then(|h| self.engine.rows(&[*h]).pop())
            .filter(|r| r.exists)
        else {
            return;
        };
        self.rename = Some(RenameDialog {
            path: row.full_path(),
            text: row.name.clone(),
            error: None,
            focus: true,
        });
    }

    pub(super) fn start_delete(&mut self) {
        let paths = self.action_paths();
        if paths.is_empty() {
            return;
        }
        if self.settings.confirm_delete || self.demo {
            self.confirm_delete = Some(paths);
        } else {
            self.delete_paths(paths);
        }
    }

    /// Sends files to the Recycle Bin on a background thread.
    pub(super) fn delete_paths(&mut self, paths: Vec<String>) {
        if self.demo_blocked() {
            return;
        }
        let (tx, ctx) = (self.tx.clone(), self.pending_ctx.clone());
        self.selected.clear();
        std::thread::spawn(move || {
            let n = paths.len();
            match shell::recycle(&paths) {
                Ok(()) => {
                    for p in paths {
                        let _ = tx.send(Msg::Removed(p));
                    }
                    let _ = tx.send(Msg::Toast(
                        if n == 1 {
                            "Moved to the Recycle Bin.".into()
                        } else {
                            format!("{n} items moved to the Recycle Bin.")
                        },
                        false,
                    ));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Toast(e, true));
                }
            }
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
    }

    fn export_results(&mut self) {
        let Some(hits) = self.results.as_ref().map(|r| r.hits.clone()) else {
            return;
        };
        if hits.is_empty() {
            self.toast("There are no results to export.", false);
            return;
        }
        if self.demo {
            self.toast("Demo: nothing is written to this PC.", false);
            return;
        }
        let picked = rfd::FileDialog::new()
            .set_title("Export results")
            .set_file_name("FindForge results.csv")
            .add_filter("CSV (name, path, size, dates, type)", &["csv"])
            .add_filter("Text (one full path per line)", &["txt"])
            .save_file();
        self.release_pointer = true;
        let Some(path) = picked else { return };
        let format = export::Format::from_path(&path);
        let offset = self.utc_offset;
        // The engine handle cannot move to another thread, so resolve rows here
        // in pieces; this is quick even for millions of rows.
        match export::export(&self.engine, &hits, &path, format, offset) {
            Ok(n) => self.toast(
                format!("{} results exported.", util::fmt_count(n as u64)),
                false,
            ),
            Err(e) => self.toast(e, true),
        }
    }

    fn shell_menu(&mut self, index: usize) {
        if let Some(h) = self.hit_at(index) {
            if !self.selected.contains(&h) {
                self.select_only(index);
            } else {
                self.cursor = Some(index);
            }
        }
        if self.demo_blocked() {
            return;
        }
        // The clicked item first: the menu is built for its folder.
        let mut paths = self.action_paths();
        if let Some(clicked) = self.hit_at(index).map(|h| self.engine.paths(&[h])) {
            if let Some(c) = clicked.first() {
                paths.retain(|p| p != c);
                paths.insert(0, c.clone());
            }
        }
        let result = shell::context_menu(self.hwnd, &paths);
        self.release_pointer = true;
        match result {
            Ok(shell::MenuChoice::Rename) => self.start_rename(),
            Ok(shell::MenuChoice::Invoked(verb)) if verb == "delete" => {
                for p in paths {
                    self.engine.note_removed(&p);
                }
            }
            Ok(_) => {}
            Err(e) => self.toast(e, true),
        }
    }

    fn drag_out(&mut self, index: usize) {
        if let Some(h) = self.hit_at(index) {
            if !self.selected.contains(&h) {
                self.select_only(index);
            }
        }
        if self.demo {
            return;
        }
        let paths = self.action_paths();
        if paths.is_empty() {
            return;
        }
        if let Err(e) = shell::drag_out(self.hwnd, &paths) {
            self.toast(e, true);
        }
        self.release_pointer = true;
    }

    // ----------------------------------------------------------------------------------
    // Keyboard
    // ----------------------------------------------------------------------------------

    pub(super) fn results_keys(&mut self, ctx: &egui::Context) {
        let in_box = self.search_has_focus;
        let page = ((self.list_view.1 / ROW_H).floor() as i64 - 1).max(1);
        let i = ctx.input(|i| {
            (
                i.modifiers,
                i.key_pressed(Key::ArrowDown),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::PageDown),
                i.key_pressed(Key::PageUp),
                i.key_pressed(Key::Home),
                i.key_pressed(Key::End),
                i.key_pressed(Key::Enter),
                i.key_pressed(Key::F2),
                i.key_pressed(Key::Delete),
                i.key_pressed(Key::A),
                i.key_pressed(Key::B),
                i.key_pressed(Key::P),
            )
        });
        let (m, down, up, pgdn, pgup, home, end, enter, f2, del, a, b, pkey) = i;
        if down {
            self.move_cursor(1, m.shift);
        }
        if up {
            self.move_cursor(-1, m.shift);
        }
        if pgdn {
            self.move_cursor(page, m.shift);
        }
        if pgup {
            self.move_cursor(-page, m.shift);
        }
        if !in_box {
            if home {
                self.move_cursor(-(self.hit_count() as i64), m.shift);
            }
            if end {
                self.move_cursor(self.hit_count() as i64, m.shift);
            }
            if del {
                self.start_delete();
            }
            if m.command && a {
                if let Some(r) = &self.results {
                    self.selected = r.hits.iter().copied().collect();
                }
            }
        }
        if enter {
            if m.alt {
                self.show_properties();
            } else if m.command {
                self.reveal_selected();
            } else {
                if self.cursor.is_none() && self.hit_count() > 0 {
                    self.select_only(0);
                }
                self.open_selected();
                // Keep typing where the user left off.
                self.focus_search = in_box;
            }
        }
        if f2 {
            self.start_rename();
        }
        if m.command && b {
            self.bookmark_current();
        }
        if m.command && pkey {
            self.settings.preview = !self.settings.preview;
            self.save_settings();
        }
        // Ctrl+C copies the files, Ctrl+Shift+C their paths (unless the user
        // is copying text out of the search box).
        let copy = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        let has_selection = !self.selected.is_empty() || self.cursor.is_some();
        if m.command && m.shift && ctx.input(|i| i.key_pressed(Key::C)) && has_selection {
            self.copy_paths();
        } else if copy && has_selection && !in_box {
            self.copy_files();
        }
        // Typing anywhere goes to the search box.
        if !in_box {
            let typed: String = ctx.input(|i| {
                i.events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect()
            });
            if !typed.is_empty()
                && !m.command
                && !m.alt
                && ctx.memory(|mem| mem.focused().is_none())
            {
                self.search.push_str(&typed);
                self.focus_search = true;
            }
        }
    }

    // ----------------------------------------------------------------------------------
    // The view
    // ----------------------------------------------------------------------------------

    pub(super) fn search_view(&mut self, ctx: &egui::Context) {
        let p = pal_ctx(ctx);
        egui::TopBottomPanel::bottom("statusbar")
            .exact_height(32.0)
            .frame(
                Frame::new()
                    .fill(p.rail)
                    .inner_margin(Margin::symmetric(18, 0)),
            )
            .show(ctx, |ui| self.status_bar(ui));
        if self.settings.preview {
            egui::SidePanel::right("preview")
                .exact_width(340.0)
                .resizable(false)
                .frame(Frame::new().fill(p.rail).inner_margin(Margin::same(18)))
                .show(ctx, |ui| {
                    let full = ui.max_rect();
                    ui.painter().vline(
                        full.left() - 18.0,
                        full.y_range().expand(18.0),
                        Stroke::new(1.0_f32, p.border),
                    );
                    self.preview_ui(ui)
                });
        }
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(p.bg)
                    .inner_margin(Margin::symmetric(24, 14)),
            )
            .show(ctx, |ui| {
                self.chips_row(ui);
                ui.add_space(4.0);
                Frame::new()
                    .fill(p.card)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.set_height(ui.available_height());
                        self.table(ui);
                    });
            });
        self.columns_popup(ctx);
    }

    fn chips_row(&mut self, ui: &mut Ui) {
        let filters = self.all_filters();
        let builtin = crate::query::builtin_filters().len();
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), 34.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                // Tools on the right first, so the chips know how much room is left.
                let tools_width = 256.0;
                let chips_width = ui.available_width() - tools_width;
                ui.allocate_ui_with_layout(
                    vec2(chips_width, 34.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("chips")
                            .max_width(chips_width)
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            )
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                if widgets::chip(ui, "All", None, self.filter.is_none()).clicked() {
                                    self.filter = None;
                                    self.focus_search = true;
                                }
                                for (i, f) in filters.iter().enumerate() {
                                    let active = self.filter.as_deref() == Some(f.name.as_str());
                                    let mut resp = widgets::chip(ui, &f.name, None, active);
                                    if i >= builtin {
                                        resp = resp.on_hover_text(format!(
                                            "{}\nRight-click to edit.",
                                            f.query
                                        ));
                                    }
                                    if resp.clicked() {
                                        self.filter =
                                            if active { None } else { Some(f.name.clone()) };
                                        self.focus_search = true;
                                    }
                                    if resp.secondary_clicked() && i >= builtin {
                                        self.filter_dialog = Some(FilterDialog {
                                            index: Some(i - builtin),
                                            name: f.name.clone(),
                                            query: f.query.clone(),
                                            error: None,
                                        });
                                    }
                                }
                            });
                    },
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    self.more_menu(ui);
                    if widgets::icon_button(ui, Icon::EXPORT, "Export the results (CSV or text)")
                        .clicked()
                    {
                        self.export_results();
                    }
                    if widgets::icon_button(ui, Icon::STAR, "Bookmark this search (Ctrl+B)")
                        .clicked()
                    {
                        self.bookmark_current();
                    }
                    if widgets::icon_button(ui, Icon::COLUMNS, "Choose columns").clicked() {
                        // A click on the button while the chooser is open has
                        // already closed it (it closes on any press outside).
                        let now = ui.input(|i| i.time);
                        if now - self.columns_closed_at > 0.4 {
                            self.columns_open = true;
                        }
                    }
                    let pv = widgets::icon_button(ui, Icon::PREVIEW, "Preview pane (Ctrl+P)");
                    if self.settings.preview {
                        let p = pal(ui);
                        ui.painter().rect_stroke(
                            pv.rect.shrink(2.0),
                            CornerRadius::same(8),
                            Stroke::new(1.0_f32, p.accent.gamma_multiply(0.7)),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if pv.clicked() {
                        self.settings.preview = !self.settings.preview;
                        self.save_settings();
                    }
                    if widgets::icon_button(ui, Icon::ADD, "New filter").clicked() {
                        self.filter_dialog = Some(FilterDialog {
                            index: None,
                            name: String::new(),
                            query: self.search.trim().to_string(),
                            error: None,
                        });
                    }
                });
            },
        );
    }

    /// The "more" button: every action on the selection, for mouse users.
    fn more_menu(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let has = !self.selected.is_empty() || self.cursor.is_some();
        let button = egui::Button::new(RichText::new(Icon::MORE.glyph()).size(15.0).color(p.muted))
            .fill(Color32::TRANSPARENT)
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(8))
            .min_size(vec2(32.0, 32.0));
        let resp = ui
            .add(button)
            .on_hover_text("Actions for the selected results");
        let mut act: Option<u8> = None;
        let popup_id = ui.make_persistent_id("more-actions");
        if resp.clicked() {
            ui.memory_mut(|m| m.toggle_popup(popup_id));
        }
        egui::popup::popup_below_widget(
            ui,
            popup_id,
            &resp,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                ui.set_min_width(250.0);
                let mut item = |ui: &mut Ui, n: u8, icon: Icon, label: &str, keys: &str| {
                    let text = icon.with(label);
                    let b = ui.add_enabled(
                        has,
                        egui::Button::new(text).shortcut_text(keys).frame(false),
                    );
                    if b.clicked() {
                        act = Some(n);
                        ui.memory_mut(|m| m.close_popup());
                    }
                };
                item(ui, 1, Icon::OPEN, "Open", "Enter");
                item(
                    ui,
                    2,
                    Icon::FOLDER_OPEN,
                    "Open containing folder",
                    "Ctrl+Enter",
                );
                ui.separator();
                item(ui, 3, Icon::COPY, "Copy", "Ctrl+C");
                item(ui, 4, Icon::LINK, "Copy full path", "Ctrl+Shift+C");
                item(ui, 5, Icon::DOCUMENT, "Copy name", "");
                ui.separator();
                item(ui, 6, Icon::RENAME, "Rename…", "F2");
                item(ui, 7, Icon::DELETE, "Move to Recycle Bin…", "Del");
                item(ui, 8, Icon::INFO, "Properties", "Alt+Enter");
            },
        );
        match act {
            Some(1) => self.open_selected(),
            Some(2) => self.reveal_selected(),
            Some(3) => self.copy_files(),
            Some(4) => self.copy_paths(),
            Some(5) => self.copy_names(),
            Some(6) => self.start_rename(),
            Some(7) => self.start_delete(),
            Some(8) => self.show_properties(),
            _ => {}
        }
    }

    fn columns_popup(&mut self, ctx: &egui::Context) {
        if !self.columns_open {
            return;
        }
        let p = pal_ctx(ctx);
        let mut changed = false;
        let mut close = false;
        let area = egui::Area::new(egui::Id::new("columns-popup"))
            .order(egui::Order::Foreground)
            .anchor(
                Align2::RIGHT_TOP,
                vec2(
                    if self.settings.preview {
                        -470.0
                    } else {
                        -130.0
                    },
                    118.0,
                ),
            )
            .show(ctx, |ui| {
                Frame::new()
                    .fill(p.card)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(14))
                    .show(ui, |ui| {
                        ui.set_width(210.0);
                        ui.label(RichText::new("Columns").font(semibold(15.0)));
                        ui.add_space(2.0);
                        for c in self.settings.columns.iter_mut() {
                            let label = column_label(&c.id);
                            if c.id == "name" {
                                // Always shown.
                                let mut on = true;
                                widgets::checkbox(ui, &mut on, label);
                            } else if widgets::checkbox(ui, &mut c.visible, label).changed() {
                                changed = true;
                            }
                        }
                        ui.add_space(6.0);
                        ui.allocate_ui_with_layout(
                            vec2(ui.available_width(), 34.0),
                            Layout::left_to_right(Align::Center),
                            |ui| {
                                if widgets::ghost_button(ui, "Reset").clicked() {
                                    self.settings.columns = default_columns();
                                    changed = true;
                                }
                                if widgets::primary_button(ui, "Done", true).clicked() {
                                    close = true;
                                }
                            },
                        );
                    });
            });
        if changed {
            self.save_settings();
        }
        let outside_click = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .map(|pos| !area.response.rect.contains(pos))
                    .unwrap_or(false)
        });
        if close || (outside_click && !self.taking_screenshot()) {
            self.columns_open = false;
            self.columns_closed_at = ctx.input(|i| i.time);
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let full = ui.max_rect();
        ui.painter().hline(
            full.x_range().expand(18.0),
            full.top(),
            Stroke::new(1.0_f32, p.border),
        );
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let small = |t: String, c: Color32| RichText::new(t).size(12.5).color(c);
            match &self.results {
                Some(r) if r.error.is_some() => {
                    ui.label(small(
                        format!(
                            "{}  {}",
                            Icon::WARNING.glyph(),
                            r.error.clone().unwrap_or_default()
                        ),
                        p.danger,
                    ));
                }
                Some(r) => {
                    let n = r.hits.len();
                    let what = if n == 1 { "object" } else { "objects" };
                    ui.label(small(
                        format!("{} {what}", util::fmt_count(n as u64)),
                        p.text,
                    ));
                    if !self.selected.is_empty() {
                        let sel = self.selected.len();
                        let size = if sel <= 100_000 {
                            let total = self.engine.total_size(self.selected.iter().copied());
                            format!(" · {}", util::fmt_size(total))
                        } else {
                            String::new()
                        };
                        ui.label(small(
                            format!("{} selected{size}", util::fmt_count(sel as u64)),
                            p.accent,
                        ));
                    } else if n > 0 {
                        ui.label(small(util::fmt_size(r.total_bytes), p.muted));
                    }
                    let took = r.took.as_secs_f64() * 1000.0;
                    let took = if self.taking_screenshot() { 4.0 } else { took };
                    ui.label(small(
                        if took < 10.0 {
                            format!("{took:.1} ms")
                        } else {
                            format!("{took:.0} ms")
                        },
                        p.faint,
                    ));
                }
                None => {
                    ui.label(small("Searching…".into(), p.muted));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, color, spin) = index_summary(&self.status, p.ok, p.warn, p.muted);
                let total = util::fmt_count(self.status.total_entries as u64);
                ui.label(small(format!("{total} indexed"), p.faint));
                ui.label(small(text, p.muted));
                if spin {
                    ui.add(egui::Spinner::new().size(12.0).color(p.accent));
                } else {
                    let (dot, _) = ui.allocate_exact_size(vec2(8.0, 16.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.0, color);
                }
            });
        });
    }

    fn table_header(&mut self, ui: &mut Ui, cols: &[ColumnBox], rect: Rect) {
        let p = pal(ui);
        ui.painter().hline(
            rect.x_range().shrink(4.0),
            rect.bottom() - 1.0,
            Stroke::new(1.0_f32, p.border),
        );
        let sort = SortKey::from_id(&self.settings.sort_key);
        let mut new_sort: Option<(String, bool)> = None;
        let mut resized = false;
        let mut open_chooser = false;
        for c in cols {
            let id = self.settings.columns[c.index].id.clone();
            let right_aligned = id == "size";
            let pad_left = if id == "name" { 36.0 } else { 8.0 };
            let r = Rect::from_min_max(
                pos2(c.x + pad_left, rect.top()),
                pos2(c.x + c.width - 10.0, rect.bottom()),
            );
            let resp = ui
                .interact(r, ui.id().with(("hdr", c.index)), Sense::click())
                .on_hover_text(format!("Sort by {}", column_label(&id).to_lowercase()));
            let active = SortKey::from_id(&id) == sort;
            let color = if active {
                p.text
            } else if resp.hovered() {
                p.muted
            } else {
                p.faint
            };
            let arrow = if !active {
                ""
            } else if self.settings.sort_ascending {
                Icon::SORT_UP.glyph()
            } else {
                Icon::SORT_DOWN.glyph()
            };
            let title = column_title(&id);
            if right_aligned {
                widgets::text_in_right(
                    ui,
                    r,
                    format!("{arrow} {title}").trim(),
                    semibold(12.0),
                    color,
                );
            } else {
                widgets::text_in(
                    ui,
                    r,
                    format!("{title} {arrow}").trim(),
                    semibold(12.0),
                    color,
                );
            }
            if resp.clicked() {
                new_sort = Some(if active {
                    (id.clone(), !self.settings.sort_ascending)
                } else {
                    // Sizes and dates are most useful largest / newest first.
                    (
                        id.clone(),
                        !matches!(id.as_str(), "size" | "modified" | "created"),
                    )
                });
            }
            if resp.secondary_clicked() {
                open_chooser = true;
            }
            // Drag the right edge to resize.
            let grip = Rect::from_min_max(
                pos2(c.x + c.width - 5.0, rect.top() + 4.0),
                pos2(c.x + c.width + 5.0, rect.bottom() - 4.0),
            );
            let g = ui.interact(grip, ui.id().with(("grip", c.index)), Sense::drag());
            if g.hovered() || g.dragged() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                ui.painter().vline(
                    grip.center().x,
                    grip.y_range(),
                    Stroke::new(1.0_f32, p.accent.gamma_multiply(0.8)),
                );
            }
            if g.dragged() {
                let w = &mut self.settings.columns[c.index].width;
                *w = (*w + g.drag_delta().x).clamp(60.0, 1200.0);
            }
            if g.drag_stopped() {
                resized = true;
            }
        }
        if let Some((key, asc)) = new_sort {
            self.settings.sort_key = key;
            self.settings.sort_ascending = asc;
            self.save_settings();
        }
        if resized {
            self.save_settings();
        }
        if open_chooser {
            self.columns_open = true;
        }
    }

    fn table(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        // States without a list.
        let total = self.hit_count();
        if self.status.total_entries == 0 && self.status.busy {
            let (text, _, _) = index_summary(&self.status, p.ok, p.warn, p.muted);
            widgets::busy_state(
                ui,
                "Building the index",
                &format!("{text}. Results appear as soon as the first drive is ready."),
            );
            return;
        }
        match &self.results {
            None => {
                widgets::busy_state(ui, "Searching", "One moment…");
                return;
            }
            Some(r) if r.error.is_some() => {
                let e = r.error.clone().unwrap_or_default();
                widgets::empty_state(ui, Icon::WARNING, p.warn, "This search has a problem", &e);
                ui.vertical_centered(|ui| {
                    if widgets::ghost_button(ui, &Icon::HELP.with("Search syntax")).clicked() {
                        self.help_open = true;
                    }
                });
                return;
            }
            Some(_) if total == 0 => {
                let hint = if self.status.total_entries == 0 {
                    "Nothing is indexed yet. Choose what to index in Settings."
                } else if self.filter.is_some() {
                    "No match with this filter. Try the All filter, or fewer words."
                } else {
                    "Check the spelling, or try fewer words. Terms are matched against names; add a backslash to match folders in the path."
                };
                widgets::empty_state(ui, Icon::SEARCH, p.muted, "Nothing found", hint);
                return;
            }
            Some(_) => {}
        }

        let full = ui.available_rect_before_wrap();
        ui.set_clip_rect(full.intersect(ui.clip_rect()));
        let (header_rect, _) = ui.allocate_exact_size(vec2(full.width(), HEADER_H), Sense::hover());
        let cols = layout_columns(
            &self.settings.columns,
            header_rect.left(),
            header_rect.width() - 10.0,
        );
        self.table_header(ui, &cols, header_rect);

        // Scroll so that the cursor row is visible.
        let mut area = egui::ScrollArea::vertical()
            .id_salt("results")
            .auto_shrink([false, false]);
        if self.scroll_to_cursor {
            self.scroll_to_cursor = false;
            if let Some(c) = self.cursor {
                let (offset, height) = self.list_view;
                let top = c as f32 * ROW_H;
                if top < offset {
                    area = area.vertical_scroll_offset(top);
                } else if top + ROW_H > offset + height && height > 0.0 {
                    area = area.vertical_scroll_offset(top + ROW_H - height);
                }
            }
        }
        let results = self.results.clone().expect("checked above");
        let query = results.query.clone();
        let mut events: Vec<RowEvent> = Vec::new();
        let offset_secs = self.utc_offset;
        ui.spacing_mut().item_spacing.y = 0.0;
        let out = area.show_rows(ui, ROW_H, total, |ui, range| {
            // Fetch what is on screen (plus a margin) in one go.
            let cached = self.row_cache.results_id == results.id
                && self.row_cache.epoch == self.engine.epoch()
                && range.start >= self.row_cache.start
                && range.end <= self.row_cache.start + self.row_cache.rows.len();
            if !cached {
                let start = range.start.saturating_sub(20);
                let end = (range.end + 20).min(total);
                self.row_cache = RowCache {
                    results_id: results.id,
                    epoch: self.engine.epoch(),
                    start,
                    rows: self.engine.rows(&results.hits[start..end]),
                };
            }
            for index in range {
                let row = self.row_cache.rows[index - self.row_cache.start].clone();
                let hit = results.hits[index];
                let (rect, resp) = ui.allocate_exact_size(
                    vec2(ui.available_width(), ROW_H),
                    Sense::click_and_drag(),
                );
                let selected = self.selected.contains(&hit);
                let is_cursor = self.cursor == Some(index);
                let body = rect.shrink2(vec2(0.0, 1.0));
                if selected {
                    ui.painter()
                        .rect_filled(body, CornerRadius::same(7), p.selected);
                } else if resp.hovered() {
                    ui.painter()
                        .rect_filled(body, CornerRadius::same(7), p.hover);
                }
                if is_cursor && !self.search_has_focus {
                    ui.painter().rect_stroke(
                        body,
                        CornerRadius::same(7),
                        Stroke::new(1.0_f32, p.accent.gamma_multiply(0.55)),
                        egui::StrokeKind::Inside,
                    );
                }
                self.paint_row(ui, &p, rect, &cols, &row, query.as_deref(), offset_secs);
                let m = ui.input(|i| i.modifiers);
                if resp.double_clicked() {
                    events.push(RowEvent::Open(index));
                } else if resp.clicked() {
                    events.push(RowEvent::Click {
                        index,
                        ctrl: m.command,
                        shift: m.shift,
                    });
                }
                if resp.secondary_clicked() {
                    events.push(RowEvent::Menu(index));
                }
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    events.push(RowEvent::Drag(index));
                }
            }
        });
        self.list_view = (out.state.offset.y, out.inner_rect.height());

        for ev in events {
            match ev {
                RowEvent::Click { index, ctrl, shift } => {
                    if shift {
                        self.select_range(index, ctrl);
                    } else if ctrl {
                        if let Some(h) = self.hit_at(index) {
                            if !self.selected.remove(&h) {
                                self.selected.insert(h);
                            }
                        }
                        self.cursor = Some(index);
                        self.anchor = Some(index);
                    } else {
                        self.select_only(index);
                    }
                    ui.memory_mut(|m| m.surrender_focus(egui::Id::new("search-box")));
                }
                RowEvent::Open(index) => {
                    self.select_only(index);
                    self.open_selected();
                }
                RowEvent::Menu(index) => self.shell_menu(index),
                RowEvent::Drag(index) => self.drag_out(index),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_row(
        &mut self,
        ui: &Ui,
        p: &Palette,
        rect: Rect,
        cols: &[ColumnBox],
        row: &Row,
        query: Option<&crate::query::Query>,
        offset_secs: i32,
    ) {
        if !row.exists {
            // Deleted since the search ran; the list refreshes within a second.
            return;
        }
        let dim = row.flags & (crate::index::flags::HIDDEN | crate::index::flags::SYSTEM) != 0;
        let name_color = if dim { p.muted } else { p.text };
        for c in cols {
            let id = self.settings.columns[c.index].id.as_str().to_owned();
            let cell = Rect::from_min_max(
                pos2(c.x + 8.0, rect.top()),
                pos2(c.x + c.width - 10.0, rect.bottom()),
            );
            if cell.width() < 6.0 {
                continue;
            }
            match id.as_str() {
                "name" => {
                    let icon =
                        Rect::from_center_size(pos2(c.x + 20.0, rect.center().y), vec2(18.0, 18.0));
                    let tex = if self.demo {
                        None
                    } else if !row.is_dir && shell::has_own_icon(&row.ext) {
                        let path = row.full_path();
                        self.icons.get(&row.ext, false, Some(&path))
                    } else {
                        self.icons.get(&row.ext, row.is_dir, None)
                    };
                    match tex {
                        Some(tex) => {
                            ui.painter().image(
                                tex.id(),
                                icon,
                                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }
                        None => icons::paint_glyph(ui, icon, &row.ext, row.is_dir),
                    }
                    let text_rect = Rect::from_min_max(pos2(c.x + 36.0, rect.top()), cell.max);
                    let ranges = query.map(|q| q.highlights(&row.name)).unwrap_or_default();
                    let job = name_job(&row.name, &ranges, name_color, p.accent, text_rect.width());
                    let galley = ui.fonts(|f| f.layout_job(job));
                    ui.painter().galley(
                        pos2(
                            text_rect.left(),
                            text_rect.center().y - galley.size().y / 2.0,
                        ),
                        galley,
                        name_color,
                    );
                }
                "path" => {
                    widgets::text_in(ui, cell, &row.dir, regular(13.5), p.muted);
                }
                "size" => {
                    if !row.is_dir {
                        widgets::text_in_right(
                            ui,
                            cell,
                            &util::fmt_size(row.size),
                            regular(13.5),
                            p.text,
                        );
                    }
                }
                "modified" => {
                    widgets::text_in(
                        ui,
                        cell,
                        &util::fmt_time(row.mtime, offset_secs),
                        regular(13.5),
                        p.muted,
                    );
                }
                "created" => {
                    widgets::text_in(
                        ui,
                        cell,
                        &util::fmt_time(row.ctime, offset_secs),
                        regular(13.5),
                        p.muted,
                    );
                }
                _ => {
                    let t = self.type_name(&row.ext, row.is_dir);
                    widgets::text_in(ui, cell, &t, regular(13.5), p.muted);
                }
            }
        }
    }

    /// For screenshots: selects rows / opens a dialog once results are in.
    pub(super) fn run_demo_select(&mut self) {
        let Some(sel) = self.pending_demo_select.take() else {
            return;
        };
        if !self.demo || self.hit_count() == 0 {
            return;
        }
        let n = self.hit_count();
        match sel {
            DemoSelect::Rows(rows) => {
                for (k, r) in rows.iter().filter(|r| **r < n).enumerate() {
                    if k == 0 {
                        self.select_only(*r);
                    } else if let Some(h) = self.hit_at(*r) {
                        self.selected.insert(h);
                    }
                }
            }
            DemoSelect::Rename(r) => {
                self.select_only(r.min(n - 1));
                self.start_rename();
            }
            DemoSelect::Delete(rows) => {
                for r in rows.iter().filter(|r| **r < n) {
                    if let Some(h) = self.hit_at(*r) {
                        self.selected.insert(h);
                    }
                }
                self.cursor = rows.first().copied();
                self.start_delete();
            }
        }
        let _ = Tab::Search;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::default_columns;

    #[test]
    fn columns_fill_the_width() {
        let mut cols = default_columns();
        let boxes = layout_columns(&cols, 10.0, 1400.0);
        // "created" is hidden by default.
        assert_eq!(boxes.len(), 5);
        assert_eq!(boxes[0].x, 10.0);
        let total: f32 = boxes.iter().map(|b| b.width).sum();
        assert!((total - 1400.0).abs() < 0.01);
        for w in boxes.windows(2) {
            assert!((w[0].x + w[0].width - w[1].x).abs() < 0.01);
        }
        // The path column takes the spare room.
        let path = boxes.iter().find(|b| cols[b.index].id == "path").unwrap();
        assert!(path.width > cols[path.index].width);
        // Too narrow: the path and name columns give way together.
        let wanted: f32 = boxes.iter().map(|b| cols[b.index].width).sum();
        let tight = layout_columns(&cols, 0.0, wanted - 100.0);
        let total: f32 = tight.iter().map(|b| b.width).sum();
        assert!((total - (wanted - 100.0)).abs() < 0.01);
        assert!(tight[0].width < cols[0].width && tight[1].width < cols[1].width);
        let tighter = layout_columns(&cols, 0.0, 640.0);
        let total: f32 = tighter.iter().map(|b| b.width).sum();
        assert!((total - 640.0).abs() < 0.01, "{total}");
        assert!(tighter[0].width < cols[0].width && tighter[0].width >= 170.0);
        // Far too narrow: the minimum widths are kept (the table is cut off).
        let narrow = layout_columns(&cols, 0.0, 300.0);
        assert_eq!(narrow[0].width, 170.0);
        // Without the path column the name column stretches.
        for c in cols.iter_mut() {
            if c.id == "path" {
                c.visible = false;
            }
        }
        let boxes = layout_columns(&cols, 0.0, 1400.0);
        assert_eq!(boxes.len(), 4);
        assert!(boxes[0].width > cols[0].width);
    }

    #[test]
    fn highlighted_names_keep_their_text() {
        let job = name_job(
            "Annual report.pdf",
            std::slice::from_ref(&(7..13)),
            Color32::WHITE,
            Color32::RED,
            300.0,
        );
        assert_eq!(job.text, "Annual report.pdf");
        assert_eq!(job.sections.len(), 3);
        // Ranges that do not fit the text are ignored rather than trusted.
        let job = name_job(
            "résumé",
            &[1..2, 0..99, 3..3],
            Color32::WHITE,
            Color32::RED,
            300.0,
        );
        assert_eq!(job.text, "résumé");
        let job = name_job("abc", &[], Color32::WHITE, Color32::RED, 300.0);
        assert_eq!(job.sections.len(), 1);
    }

    #[test]
    fn titles() {
        for c in default_columns() {
            assert!(!column_title(&c.id).is_empty());
            assert_eq!(
                column_title(&c.id).to_lowercase(),
                column_label(&c.id).to_lowercase()
            );
        }
    }
}
