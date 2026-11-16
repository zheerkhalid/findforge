//! Modal dialogs: rename, delete confirmation, the search syntax help, and
//! the editors for filters and bookmarks.

use super::app::App;
use super::theme::{pal, pal_ctx, regular, semibold, Icon, Palette};
use super::widgets;
use crate::query::{self, FilterDef, Now, SearchOptions};
use crate::settings::Bookmark;
use crate::{shell, sys};
use eframe::egui::{
    self, vec2, Align, CornerRadius, FontFamily, FontId, Frame, Layout, Margin, RichText, Sense,
    Stroke, Ui,
};

fn modal_frame(p: &Palette) -> Frame {
    Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::same(22))
}

/// A right-aligned row of buttons with a fixed height (see the note in
/// `app.rs` about unbounded layouts inside modals).
fn button_row(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 34.0),
        Layout::right_to_left(Align::Center),
        add,
    );
}

fn text_field(ui: &mut Ui, text: &mut String, hint: &str, mono: bool) -> egui::Response {
    let p = pal(ui);
    Frame::new()
        .fill(p.bg)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(text)
                    .hint_text(RichText::new(hint).color(p.faint))
                    .frame(false)
                    .desired_width(ui.available_width())
                    .font(if mono {
                        FontId::new(14.0, FontFamily::Monospace)
                    } else {
                        regular(15.0)
                    }),
            )
        })
        .inner
}

impl App {
    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        self.rename_dialog(ctx);
        self.delete_dialog(ctx);
        self.help_dialog(ctx);
        self.filter_dialog_ui(ctx);
        self.bookmark_dialog_ui(ctx);
    }

    fn rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.rename else {
            return;
        };
        let p = pal_ctx(ctx);
        let mut decision: Option<bool> = None;
        let modal = egui::Modal::new(egui::Id::new("rename"))
            .frame(modal_frame(&p))
            .show(ctx, |ui| {
                ui.set_width(460.0);
                ui.label(RichText::new("Rename").font(semibold(19.0)));
                ui.add_space(2.0);
                ui.add(
                    egui::Label::new(RichText::new(&dialog.path).size(12.5).color(p.muted))
                        .truncate(),
                );
                ui.add_space(6.0);
                let resp = text_field(ui, &mut dialog.text, "New name", false);
                if dialog.focus {
                    dialog.focus = false;
                    resp.request_focus();
                    // Select the name without its extension, as Explorer does.
                    let stem = dialog
                        .text
                        .rfind('.')
                        .filter(|i| *i > 0)
                        .map(|i| dialog.text[..i].chars().count())
                        .unwrap_or_else(|| dialog.text.chars().count());
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), resp.id) {
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(stem),
                            )));
                        state.store(ui.ctx(), resp.id);
                    }
                }
                if resp.changed() {
                    dialog.error = shell::check_new_name(&dialog.text).err();
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                match &dialog.error {
                    Some(e) => {
                        ui.label(RichText::new(e).size(12.5).color(p.danger));
                    }
                    None => {
                        ui.label(RichText::new(" ").size(12.5));
                    }
                }
                ui.add_space(8.0);
                button_row(ui, |ui| {
                    let ok = dialog.error.is_none() && !dialog.text.is_empty();
                    if widgets::primary_button(ui, "Rename", ok).clicked() || (enter && ok) {
                        decision = Some(true);
                    }
                    if widgets::ghost_button(ui, "Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                let Some(d) = self.rename.take() else { return };
                if self.demo {
                    self.toast("Demo: nothing on this PC is changed.", false);
                    return;
                }
                match shell::rename(&d.path, &d.text) {
                    Ok(_) => {
                        self.engine.note_removed(&d.path);
                        self.toast(format!("Renamed to {}.", d.text), false);
                    }
                    Err(e) => {
                        self.rename = Some(super::app::RenameDialog {
                            error: Some(e),
                            focus: true,
                            ..d
                        });
                    }
                }
            }
            Some(false) => self.rename = None,
            None => {}
        }
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(paths) = &self.confirm_delete else {
            return;
        };
        let p = pal_ctx(ctx);
        let mut decision: Option<bool> = None;
        let mut ask = self.settings.confirm_delete;
        let modal = egui::Modal::new(egui::Id::new("confirm-delete"))
            .frame(modal_frame(&p))
            .show(ctx, |ui| {
                ui.set_width(500.0);
                let n = paths.len();
                ui.label(
                    RichText::new(if n == 1 {
                        "Move this item to the Recycle Bin?".to_string()
                    } else {
                        format!("Move {n} items to the Recycle Bin?")
                    })
                    .font(semibold(19.0)),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new("You can restore them from the Recycle Bin later.")
                        .color(p.muted),
                );
                ui.add_space(6.0);
                Frame::new()
                    .fill(p.bg)
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 4.0;
                        for path in paths.iter().take(8) {
                            let (rect, _) = ui.allocate_exact_size(
                                vec2(ui.available_width(), 20.0),
                                Sense::hover(),
                            );
                            ui.painter().circle_filled(
                                rect.left_center() + vec2(4.0, 0.0),
                                2.5,
                                p.danger,
                            );
                            widgets::text_in(
                                ui,
                                egui::Rect::from_min_max(rect.min + vec2(16.0, 0.0), rect.max),
                                path,
                                regular(13.5),
                                p.text,
                            );
                        }
                        if n > 8 {
                            ui.label(
                                RichText::new(format!("and {} more", n - 8))
                                    .size(12.5)
                                    .color(p.muted),
                            );
                        }
                    });
                ui.add_space(8.0);
                widgets::checkbox(ui, &mut ask, "Ask every time");
                ui.add_space(8.0);
                button_row(ui, |ui| {
                    if widgets::danger_button(ui, "Move to Recycle Bin", true).clicked() {
                        decision = Some(true);
                    }
                    if widgets::ghost_button(ui, "Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        if ask != self.settings.confirm_delete {
            self.settings.confirm_delete = ask;
            self.save_settings();
        }
        if modal.should_close() && decision.is_none() {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                if let Some(paths) = self.confirm_delete.take() {
                    self.delete_paths(paths);
                }
            }
            Some(false) => self.confirm_delete = None,
            None => {}
        }
    }

    fn help_dialog(&mut self, ctx: &egui::Context) {
        if !self.help_open {
            return;
        }
        let p = pal_ctx(ctx);
        let mut close = false;
        let mut pick: Option<String> = None;
        let modal = egui::Modal::new(egui::Id::new("syntax-help"))
            .frame(modal_frame(&p))
            .show(ctx, |ui| {
                ui.set_width(760.0);
                ui.label(RichText::new("Search syntax").font(semibold(19.0)));
                ui.add_space(2.0);
                ui.label(
                    RichText::new("Type words to match names. Click an example to try it.")
                        .color(p.muted),
                );
                ui.add_space(8.0);
                egui::ScrollArea::vertical()
                    .max_height(470.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for (example, text) in query::syntax_help() {
                            let (rect, resp) = ui.allocate_exact_size(
                                vec2(ui.available_width() - 12.0, 30.0),
                                Sense::click(),
                            );
                            if resp.hovered() {
                                ui.painter()
                                    .rect_filled(rect, CornerRadius::same(7), p.hover);
                            }
                            let left = egui::Rect::from_min_max(
                                rect.min + vec2(10.0, 0.0),
                                egui::pos2(rect.left() + 282.0, rect.bottom()),
                            );
                            widgets::text_in(
                                ui,
                                left,
                                example,
                                FontId::new(13.0, FontFamily::Monospace),
                                p.accent,
                            );
                            let right = egui::Rect::from_min_max(
                                egui::pos2(rect.left() + 296.0, rect.top()),
                                rect.max - vec2(8.0, 0.0),
                            );
                            let cut = widgets::text_in(ui, right, text, regular(13.5), p.text);
                            let resp = if cut { resp.on_hover_text(text) } else { resp };
                            if resp.clicked() {
                                pick = example.split("   ").next().map(|s| s.to_string());
                            }
                        }
                    });
                ui.add_space(10.0);
                button_row(ui, |ui| {
                    if widgets::primary_button(ui, "Close", true).clicked() {
                        close = true;
                    }
                });
            });
        if let Some(q) = pick {
            self.search = q;
            self.filter = None;
            self.tab = super::app::Tab::Search;
            self.focus_search = true;
            close = true;
        }
        if close || modal.should_close() {
            self.help_open = false;
        }
    }

    fn filter_dialog_ui(&mut self, ctx: &egui::Context) {
        let builtin: Vec<String> = query::builtin_filters()
            .into_iter()
            .map(|f| f.name.to_lowercase())
            .collect();
        let others: Vec<(usize, String)> = self
            .settings
            .user_filters
            .iter()
            .enumerate()
            .map(|(i, f)| (i, f.name.to_lowercase()))
            .collect();
        let Some(dialog) = &mut self.filter_dialog else {
            return;
        };
        let p = pal_ctx(ctx);
        // 0 = cancel, 1 = save, 2 = delete
        let mut decision: Option<u8> = None;
        let modal = egui::Modal::new(egui::Id::new("filter-edit"))
            .frame(modal_frame(&p))
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(
                    RichText::new(if dialog.index.is_some() {
                        "Edit filter"
                    } else {
                        "New filter"
                    })
                    .font(semibold(19.0)),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(
                        "A filter is a saved piece of search that you switch on with one click, for example ext:psd;ai;svg or size:>1gb.",
                    )
                    .color(p.muted),
                );
                ui.add_space(8.0);
                ui.label(RichText::new("Name").size(12.0).color(p.faint));
                text_field(ui, &mut dialog.name, "Design files", false);
                ui.add_space(4.0);
                ui.label(RichText::new("Search").size(12.0).color(p.faint));
                text_field(ui, &mut dialog.query, "ext:psd;ai;svg", true);
                // Validate as the user types.
                let name = dialog.name.trim().to_lowercase();
                let now = Now {
                    unix: sys::now_unix() as i64,
                    utc_offset_secs: 0,
                };
                dialog.error = if name.is_empty() {
                    Some("Give the filter a name.".to_string())
                } else if builtin.contains(&name)
                    || others
                        .iter()
                        .any(|(i, n)| *n == name && Some(*i) != dialog.index)
                {
                    Some("There is already a filter with this name.".to_string())
                } else if dialog.query.trim().is_empty() {
                    Some("Type what the filter should search for.".to_string())
                } else {
                    query::parse(dialog.query.trim(), None, &SearchOptions::default(), now).err()
                };
                ui.label(
                    RichText::new(dialog.error.as_deref().unwrap_or(" "))
                        .size(12.5)
                        .color(p.danger),
                );
                ui.add_space(8.0);
                button_row(ui, |ui| {
                    if widgets::primary_button(ui, "Save", dialog.error.is_none()).clicked() {
                        decision = Some(1);
                    }
                    if widgets::ghost_button(ui, "Cancel").clicked() {
                        decision = Some(0);
                    }
                    if dialog.index.is_some()
                        && widgets::ghost_button(ui, &Icon::DELETE.with("Delete")).clicked()
                    {
                        decision = Some(2);
                    }
                });
            });
        if modal.should_close() && decision.is_none() {
            decision = Some(0);
        }
        let Some(decision) = decision else { return };
        let Some(d) = self.filter_dialog.take() else {
            return;
        };
        match decision {
            1 => {
                let def = FilterDef {
                    name: d.name.trim().to_string(),
                    query: d.query.trim().to_string(),
                };
                match d.index {
                    Some(i) if i < self.settings.user_filters.len() => {
                        if self.filter.as_deref()
                            == Some(self.settings.user_filters[i].name.as_str())
                        {
                            self.filter = Some(def.name.clone());
                        }
                        self.settings.user_filters[i] = def;
                    }
                    _ => {
                        self.filter = Some(def.name.clone());
                        self.settings.user_filters.push(def);
                    }
                }
                self.save_settings();
            }
            2 => {
                if let Some(i) = d.index.filter(|i| *i < self.settings.user_filters.len()) {
                    let gone = self.settings.user_filters.remove(i);
                    if self.filter.as_deref() == Some(gone.name.as_str()) {
                        self.filter = None;
                    }
                    self.save_settings();
                    self.toast(format!("Filter \"{}\" deleted.", gone.name), false);
                }
            }
            _ => {}
        }
    }

    fn bookmark_dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.bookmark_dialog else {
            return;
        };
        let p = pal_ctx(ctx);
        let mut decision: Option<u8> = None;
        let modal = egui::Modal::new(egui::Id::new("bookmark-edit"))
            .frame(modal_frame(&p))
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.label(
                    RichText::new(if dialog.index.is_some() {
                        "Edit bookmark"
                    } else {
                        "Bookmark this search"
                    })
                    .font(semibold(19.0)),
                );
                ui.add_space(8.0);
                ui.label(RichText::new("Name").size(12.0).color(p.faint));
                text_field(ui, &mut dialog.name, "Name", false);
                ui.add_space(4.0);
                ui.label(RichText::new("Search").size(12.0).color(p.faint));
                text_field(ui, &mut dialog.query, "What to search for", true);
                if let Some(f) = &dialog.filter {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Filter").size(12.0).color(p.faint));
                        widgets::badge(ui, f, p.accent);
                    });
                }
                ui.add_space(12.0);
                button_row(ui, |ui| {
                    let ok = !dialog.name.trim().is_empty();
                    if widgets::primary_button(ui, "Save", ok).clicked() {
                        decision = Some(1);
                    }
                    if widgets::ghost_button(ui, "Cancel").clicked() {
                        decision = Some(0);
                    }
                    if dialog.index.is_some()
                        && widgets::ghost_button(ui, &Icon::DELETE.with("Delete")).clicked()
                    {
                        decision = Some(2);
                    }
                });
            });
        if modal.should_close() && decision.is_none() {
            decision = Some(0);
        }
        let Some(decision) = decision else { return };
        let Some(d) = self.bookmark_dialog.take() else {
            return;
        };
        match decision {
            1 => {
                let b = Bookmark {
                    name: d.name.trim().to_string(),
                    query: d.query.trim().to_string(),
                    filter: d.filter,
                };
                match d.index {
                    Some(i) if i < self.settings.bookmarks.len() => self.settings.bookmarks[i] = b,
                    _ => {
                        self.toast(format!("Bookmarked as \"{}\".", b.name), false);
                        self.settings.bookmarks.push(b);
                    }
                }
                self.save_settings();
            }
            2 => {
                if let Some(i) = d.index.filter(|i| *i < self.settings.bookmarks.len()) {
                    self.settings.bookmarks.remove(i);
                    self.save_settings();
                }
            }
            _ => {}
        }
    }
}
