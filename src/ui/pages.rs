//! The pages behind the navigation rail: Bookmarks, History, Index, Settings.

use super::app::{App, BookmarkDialog, Tab};
use super::theme::{self, pal, regular, semibold, Icon, Palette};
use super::widgets;
use crate::desktop;
use crate::engine::{VolState, VolStatus};
use crate::index::VolumeKind;
use crate::settings::ThemeChoice;
use crate::{sys, util, volumes};
use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, FontFamily, FontId, Frame, Layout, Margin,
    Rect, RichText, Sense, Stroke, Ui,
};

/// A settings row: title and explanation on the left, a control on the right.
fn setting(ui: &mut Ui, title: &str, description: &str, control: impl FnOnce(&mut Ui)) {
    let p = pal(ui);
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 46.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.set_max_width((ui.available_width() - 250.0).max(200.0));
                ui.label(RichText::new(title).size(14.5));
                if !description.is_empty() {
                    ui.add(
                        egui::Label::new(RichText::new(description).size(12.5).color(p.muted))
                            .wrap(),
                    );
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), control);
        },
    );
}

fn switch(ui: &mut Ui, value: &mut bool) -> bool {
    if widgets::toggle(ui, *value, true).clicked() {
        *value = !*value;
        true
    } else {
        false
    }
}

fn small_field(ui: &mut Ui, text: &mut String, hint: &str, width: f32) -> egui::Response {
    let p = pal(ui);
    Frame::new()
        .fill(p.bg)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            super::bidi::show_edit(ui, text, 14.0, p.text, |edit| {
                edit.hint_text(RichText::new(hint).color(p.faint))
                    .frame(false)
                    .desired_width(width)
                    .font(regular(14.0))
            })
            .response
        })
        .inner
}

/// A removable list of strings with an "add" line. Returns true when changed.
fn string_list(
    ui: &mut Ui,
    items: &mut Vec<String>,
    draft: &mut String,
    hint: &str,
    browse: Option<&mut dyn FnMut() -> Option<String>>,
) -> bool {
    let p = pal(ui);
    let mut changed = false;
    let mut remove: Option<usize> = None;
    for (i, item) in items.iter().enumerate() {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(7), p.bg);
        widgets::text_in(
            ui,
            Rect::from_min_max(rect.min + vec2(12.0, 0.0), rect.max - vec2(40.0, 0.0)),
            item,
            FontId::new(13.0, FontFamily::Monospace),
            p.text,
        );
        let x = Rect::from_center_size(rect.right_center() - vec2(18.0, 0.0), vec2(24.0, 24.0));
        let resp = ui
            .interact(x, ui.id().with(("remove", hint, i)), Sense::click())
            .on_hover_text("Remove");
        ui.painter().text(
            x.center(),
            Align2::CENTER_CENTER,
            Icon::CLOSE.glyph(),
            regular(11.0),
            if resp.hovered() { p.danger } else { p.faint },
        );
        if resp.clicked() {
            remove = Some(i);
        }
    }
    if let Some(i) = remove {
        items.remove(i);
        changed = true;
    }
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 36.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            let has_browse = browse.is_some();
            let width = ui.available_width() - if has_browse { 220.0 } else { 110.0 };
            let resp = small_field(ui, draft, hint, width.max(120.0));
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let add = widgets::ghost_button_enabled(ui, "Add", !draft.trim().is_empty()).clicked();
            if (add || enter) && !draft.trim().is_empty() {
                let v = draft.trim().to_string();
                if !items.iter().any(|x| x.eq_ignore_ascii_case(&v)) {
                    items.push(v);
                    changed = true;
                }
                draft.clear();
            }
            if let Some(browse) = browse {
                if widgets::ghost_button(ui, &Icon::FOLDER_OPEN.with("Browse…")).clicked() {
                    if let Some(v) = browse() {
                        if !items.iter().any(|x| x.eq_ignore_ascii_case(&v)) {
                            items.push(v);
                            changed = true;
                        }
                    }
                }
            }
        },
    );
    changed
}

fn method(kind: VolumeKind) -> &'static str {
    match kind {
        VolumeKind::NtfsMft => "Master file table and change journal",
        VolumeKind::NtfsWalk => "Folder walk and change notifications",
        VolumeKind::Walk => "Folder walk, rescanned periodically",
    }
}

fn ago(now: u64, then: u64) -> String {
    if then == 0 {
        return "Never".into();
    }
    let d = now.saturating_sub(then);
    match d {
        0..=4 => "Just now".into(),
        5..=59 => format!("{d} seconds ago"),
        60..=119 => "A minute ago".into(),
        120..=3599 => format!("{} minutes ago", d / 60),
        3600..=7199 => "An hour ago".into(),
        7200..=86_399 => format!("{} hours ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

fn tile(ui: &mut Ui, p: &Palette, width: f32, label: &str, value: &str, sub: &str) {
    Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.set_width(width - 34.0);
                ui.set_height(58.0);
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(RichText::new(label).size(12.0).color(p.faint));
                ui.label(RichText::new(value).font(semibold(20.0)));
                ui.add(egui::Label::new(RichText::new(sub).size(12.0).color(p.muted)).truncate());
            });
        });
}

impl App {
    // ----------------------------------------------------------------------------------
    // Bookmarks
    // ----------------------------------------------------------------------------------

    pub(super) fn bookmarks_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let mut add = false;
        let n = self.settings.bookmarks.len();
        App::page_header(
            ui,
            "Bookmarks",
            &format!(
                "{n} saved {}. Click one to run it.",
                if n == 1 { "search" } else { "searches" }
            ),
            |ui| {
                if widgets::primary_button(ui, &Icon::ADD.with("Bookmark current search"), true)
                    .on_hover_text("Ctrl+B")
                    .clicked()
                {
                    add = true;
                }
            },
        );
        if add {
            self.bookmark_current();
        }
        if self.settings.bookmarks.is_empty() {
            widgets::empty_state(
                ui,
                Icon::STAR,
                p.accent,
                "No bookmarks yet",
                "Run a search you use often, then press Ctrl+B to keep it here.",
            );
            return;
        }
        let mut run: Option<usize> = None;
        let mut edit: Option<usize> = None;
        Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0_f32, p.border))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for (i, b) in self.settings.bookmarks.iter().enumerate() {
                            let (rect, resp) = ui.allocate_exact_size(
                                vec2(ui.available_width(), 52.0),
                                Sense::click(),
                            );
                            if resp.hovered() {
                                ui.painter()
                                    .rect_filled(rect, CornerRadius::same(9), p.hover);
                            }
                            ui.painter().text(
                                rect.left_center() + vec2(24.0, 0.0),
                                Align2::CENTER_CENTER,
                                Icon::STAR_FILLED.glyph(),
                                regular(16.0),
                                p.accent,
                            );
                            let text_right = rect.right() - 110.0;
                            let badge_w = match &b.filter {
                                Some(f) => {
                                    // Right-aligned, just before the edit button.
                                    let w = ui
                                        .painter()
                                        .layout_no_wrap(f.clone(), semibold(11.5), p.info)
                                        .size()
                                        .x
                                        + 14.0;
                                    let r = Rect::from_min_max(
                                        egui::pos2(text_right + 40.0 - w, rect.top()),
                                        egui::pos2(text_right + 40.0, rect.bottom()),
                                    );
                                    widgets::paint_badge(ui, r, f, p.info) + 12.0
                                }
                                None => 0.0,
                            };
                            let name_rect = Rect::from_min_max(
                                rect.min + vec2(48.0, 7.0),
                                egui::pos2(text_right - 130.0, rect.top() + 28.0),
                            );
                            widgets::text_in(ui, name_rect, &b.name, semibold(14.5), p.text);
                            let query_rect = Rect::from_min_max(
                                rect.min + vec2(48.0, 27.0),
                                egui::pos2(text_right - 130.0, rect.bottom() - 6.0),
                            );
                            let shown = if b.query.is_empty() {
                                "(everything)"
                            } else {
                                &b.query
                            };
                            widgets::text_in(
                                ui,
                                query_rect,
                                shown,
                                FontId::new(12.5, FontFamily::Monospace),
                                p.muted,
                            );
                            let _ = badge_w;
                            let edit_rect = Rect::from_center_size(
                                rect.right_center() - vec2(30.0, 0.0),
                                vec2(32.0, 32.0),
                            );
                            let e = ui
                                .interact(edit_rect, ui.id().with(("bm-edit", i)), Sense::click())
                                .on_hover_text("Edit or delete");
                            if e.hovered() {
                                ui.painter().rect_filled(
                                    edit_rect,
                                    CornerRadius::same(8),
                                    p.border,
                                );
                            }
                            ui.painter().text(
                                edit_rect.center(),
                                Align2::CENTER_CENTER,
                                Icon::RENAME.glyph(),
                                regular(15.0),
                                p.muted,
                            );
                            if e.clicked() {
                                edit = Some(i);
                            } else if resp.clicked() {
                                run = Some(i);
                            }
                        }
                    });
            });
        if let Some(i) = run {
            let b = self.settings.bookmarks[i].clone();
            self.run_bookmark(&b);
        }
        if let Some(i) = edit {
            let b = &self.settings.bookmarks[i];
            self.bookmark_dialog = Some(BookmarkDialog {
                index: Some(i),
                name: b.name.clone(),
                query: b.query.clone(),
                filter: b.filter.clone(),
            });
        }
    }

    // ----------------------------------------------------------------------------------
    // History
    // ----------------------------------------------------------------------------------

    pub(super) fn history_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let mut clear = false;
        let mut keep = self.settings.keep_history;
        let n = self.settings.history.len();
        App::page_header(
            ui,
            "History",
            &format!(
                "Your last {n} {}, newest first.",
                if n == 1 { "search" } else { "searches" }
            ),
            |ui| {
                if widgets::ghost_button_enabled(ui, &Icon::DELETE.with("Clear history"), n > 0)
                    .clicked()
                {
                    clear = true;
                }
                ui.add_space(8.0);
                if widgets::toggle(ui, keep, true).clicked() {
                    keep = !keep;
                }
                ui.label(RichText::new("Keep history").color(p.muted));
            },
        );
        if keep != self.settings.keep_history {
            self.settings.keep_history = keep;
            self.save_settings();
        }
        if clear {
            self.settings.history.clear();
            self.save_settings();
            self.toast("History cleared.", false);
        }
        if self.settings.history.is_empty() {
            widgets::empty_state(
                ui,
                Icon::HISTORY,
                p.muted,
                if self.settings.keep_history {
                    "No searches yet"
                } else {
                    "History is switched off"
                },
                if self.settings.keep_history {
                    "Searches you run are remembered here so you can repeat them with one click."
                } else {
                    "Switch on \"Keep history\" to remember your searches."
                },
            );
            return;
        }
        let mut run: Option<String> = None;
        let mut remove: Option<usize> = None;
        Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0_f32, p.border))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        for (i, h) in self.settings.history.iter().enumerate() {
                            let (rect, resp) = ui.allocate_exact_size(
                                vec2(ui.available_width(), 38.0),
                                Sense::click(),
                            );
                            if resp.hovered() {
                                ui.painter()
                                    .rect_filled(rect, CornerRadius::same(9), p.hover);
                            }
                            ui.painter().text(
                                rect.left_center() + vec2(24.0, 0.0),
                                Align2::CENTER_CENTER,
                                Icon::CLOCK.glyph(),
                                regular(15.0),
                                p.faint,
                            );
                            widgets::text_in(
                                ui,
                                Rect::from_min_max(
                                    rect.min + vec2(48.0, 0.0),
                                    rect.max - vec2(56.0, 0.0),
                                ),
                                h,
                                FontId::new(13.5, FontFamily::Monospace),
                                p.text,
                            );
                            let x = Rect::from_center_size(
                                rect.right_center() - vec2(26.0, 0.0),
                                vec2(28.0, 28.0),
                            );
                            let xr = ui
                                .interact(x, ui.id().with(("hist-x", i)), Sense::click())
                                .on_hover_text("Remove from history");
                            ui.painter().text(
                                x.center(),
                                Align2::CENTER_CENTER,
                                Icon::CLOSE.glyph(),
                                regular(11.0),
                                if xr.hovered() { p.danger } else { p.faint },
                            );
                            if xr.clicked() {
                                remove = Some(i);
                            } else if resp.clicked() {
                                run = Some(h.clone());
                            }
                        }
                    });
            });
        if let Some(i) = remove {
            self.settings.history.remove(i);
            self.save_settings();
        }
        if let Some(q) = run {
            self.search = q;
            self.tab = Tab::Search;
            self.focus_search = true;
        }
    }

    // ----------------------------------------------------------------------------------
    // Index
    // ----------------------------------------------------------------------------------

    pub(super) fn index_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let st = self.status.clone();
        let now = sys::now_unix();
        let mut rescan_all = false;
        let mut save = false;
        App::page_header(
            ui,
            "Index",
            "What FindForge knows about your drives, and how it stays up to date.",
            |ui| {
                if widgets::primary_button(
                    ui,
                    &Icon::REFRESH.with("Rescan all"),
                    !st.volumes.is_empty(),
                )
                .on_hover_text("F5")
                .clicked()
                {
                    rescan_all = true;
                }
                if widgets::ghost_button(ui, &Icon::SAVE.with("Save now")).clicked() {
                    save = true;
                }
            },
        );
        if rescan_all || save {
            if self.demo {
                self.toast("Demo: the invented index never changes.", false);
            } else if rescan_all {
                self.engine.rescan(None);
                self.toast("Rescanning every drive…", false);
            } else {
                self.engine.save_now();
                self.toast("The index is being saved.", false);
            }
        }

        // Totals.
        let width = (ui.available_width() - 3.0 * 12.0) / 4.0;
        let per_entry = if st.total_entries > 0 {
            format!(
                "{:.0} bytes",
                st.memory_bytes as f64 / st.total_entries as f64
            )
        } else {
            "–".into()
        };
        let process = if self.demo {
            st.memory_bytes as u64 + (38 << 20)
        } else {
            sys::process_memory()
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            tile(
                ui,
                &p,
                width,
                "Objects",
                &util::fmt_count(st.total_entries as u64),
                &format!(
                    "on {} {}",
                    st.volumes.len(),
                    if st.volumes.len() == 1 {
                        "drive"
                    } else {
                        "drives"
                    }
                ),
            );
            tile(
                ui,
                &p,
                width,
                "Index memory",
                &util::fmt_size(st.memory_bytes as u64),
                &format!("{per_entry} per object"),
            );
            tile(
                ui,
                &p,
                width,
                "Program memory",
                &util::fmt_size(process),
                "everything FindForge uses",
            );
            tile(
                ui,
                &p,
                width,
                "Saved to disk",
                &ago(now, st.last_save_unix),
                "so the next start is instant",
            );
        });
        ui.add_space(6.0);

        if st.volumes.is_empty() {
            widgets::empty_state(
                ui,
                Icon::DRIVE,
                p.muted,
                "Nothing is indexed",
                "Choose drives or folders in Settings.",
            );
            return;
        }
        let mut rescan_one: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for v in &st.volumes {
                    if self.volume_card(ui, &p, v, now) {
                        rescan_one = Some(v.root.clone());
                    }
                    ui.add_space(2.0);
                }
                if !self.elevated {
                    ui.add_space(2.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(Icon::INFO.glyph()).color(p.info));
                        ui.label(
                            RichText::new(
                                "FindForge is running with standard rights. NTFS drives are indexed much faster, straight from the master file table, when it runs as administrator.",
                            )
                            .size(13.0)
                            .color(p.muted),
                        );
                    });
                }
            });
        if let Some(root) = rescan_one {
            if self.demo {
                self.toast("Demo: the invented index never changes.", false);
            } else {
                self.toast(format!("Rescanning {root}…"), false);
                self.engine.rescan(Some(root));
            }
        }
    }

    /// One drive on the Index page. Returns true when "Rescan" was clicked.
    fn volume_card(&self, ui: &mut Ui, p: &Palette, v: &VolStatus, now: u64) -> bool {
        let mut rescan = false;
        widgets::card(ui, |ui| {
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), 40.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(10),
                        p.accent.gamma_multiply(0.14),
                    );
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        Icon::DRIVE.glyph(),
                        regular(19.0),
                        p.accent,
                    );
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let title = if v.label.is_empty() {
                            v.root.clone()
                        } else {
                            format!("{}  {}", v.root, v.label)
                        };
                        ui.add(
                            egui::Label::new(RichText::new(title).font(semibold(16.0))).truncate(),
                        );
                        ui.label(
                            RichText::new(format!(
                                "{} · {}",
                                if v.fs.is_empty() { "Unknown" } else { &v.fs },
                                method(v.kind)
                            ))
                            .size(12.5)
                            .color(p.muted),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::ghost_button(ui, &Icon::REFRESH.with("Rescan")).clicked() {
                            rescan = true;
                        }
                        ui.add_space(6.0);
                        match &v.state {
                            VolState::Loading => {
                                widgets::badge(ui, "Loading", p.warn);
                            }
                            VolState::Scanning { found } => {
                                widgets::badge(
                                    ui,
                                    &format!("Indexing  {}", util::fmt_count(*found)),
                                    p.warn,
                                );
                                ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                            }
                            VolState::Failed(_) => {
                                widgets::badge(ui, "Failed", p.danger);
                            }
                            VolState::Ready => {
                                if v.refreshing {
                                    widgets::badge(ui, "Refreshing", p.info);
                                    ui.add(egui::Spinner::new().size(14.0).color(p.accent));
                                } else if v.live {
                                    widgets::badge(ui, "Live", p.ok).on_hover_text(
                                        "Changes on this drive reach the index as they happen.",
                                    );
                                } else {
                                    widgets::badge(ui, "Rescanned on a timer", p.muted);
                                }
                            }
                        }
                    });
                },
            );
            ui.add_space(4.0);
            widgets::divider(ui);
            ui.add_space(4.0);
            let cell = |ui: &mut Ui, label: &str, value: String| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.set_width(150.0);
                    ui.label(RichText::new(label).size(12.0).color(p.faint));
                    ui.label(RichText::new(value).size(14.0));
                });
            };
            ui.horizontal(|ui| {
                cell(ui, "Objects", util::fmt_count(v.entries as u64));
                cell(ui, "Memory", util::fmt_size(v.memory_bytes as u64));
                cell(
                    ui,
                    "Per object",
                    if v.entries > 0 {
                        format!("{:.0} bytes", v.memory_bytes as f64 / v.entries as f64)
                    } else {
                        "–".into()
                    },
                );
                cell(ui, "Last change", ago(now, v.last_update_unix));
                cell(
                    ui,
                    "Last full scan",
                    if v.scanned_unix == 0 {
                        "–".into()
                    } else {
                        format!(
                            "{} ({:.1} s)",
                            ago(now, v.scanned_unix),
                            v.scan_ms as f64 / 1000.0
                        )
                    },
                );
            });
            let note = match &v.state {
                VolState::Failed(e) => e.clone(),
                _ => v.note.clone(),
            };
            if !note.is_empty() {
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(Icon::WARNING.glyph()).color(p.warn));
                    ui.label(RichText::new(note).size(13.0).color(p.muted));
                });
            }
        });
        rescan
    }

    // ----------------------------------------------------------------------------------
    // Settings
    // ----------------------------------------------------------------------------------

    pub(super) fn settings_ui(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        App::page_header(
            ui,
            "Settings",
            "Changes are saved as you make them.",
            |_| {},
        );
        let mut changed = false;
        let mut reindex = false;
        let mut desktop_changed = false;
        let ctx = ui.ctx().clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_width(ui.available_width() - 12.0);

                // ---- Appearance ----
                widgets::card(ui, |ui| {
                    widgets::section(ui, "Appearance");
                    setting(ui, "Theme", "Dark, light, or whatever Windows is set to.", |ui| {
                        let current = match self.settings.theme {
                            ThemeChoice::Dark => 0,
                            ThemeChoice::Light => 1,
                            ThemeChoice::System => 2,
                        };
                        if let Some(i) = widgets::segmented(ui, &["Dark", "Light", "System"], current)
                        {
                            self.settings.theme =
                                [ThemeChoice::Dark, ThemeChoice::Light, ThemeChoice::System][i];
                            theme::apply_choice(&ctx, self.settings.theme);
                            changed = true;
                        }
                    });
                    setting(
                        ui,
                        "Preview pane",
                        "Show details and a preview of the selected result (Ctrl+P).",
                        |ui| changed |= switch(ui, &mut self.settings.preview),
                    );
                });
                ui.add_space(4.0);

                // ---- Searching ----
                widgets::card(ui, |ui| {
                    widgets::section(ui, "Searching");
                    setting(ui, "Match case", "Report and report are different words.", |ui| {
                        changed |= switch(ui, &mut self.settings.match_case)
                    });
                    setting(ui, "Whole words only", "port does not find report.", |ui| {
                        changed |= switch(ui, &mut self.settings.whole_word)
                    });
                    setting(
                        ui,
                        "Match the full path",
                        "Every term may match a folder in the path, not just the name.",
                        |ui| changed |= switch(ui, &mut self.settings.match_path),
                    );
                    setting(
                        ui,
                        "Regular expressions",
                        "Treat what you type as one regular expression.",
                        |ui| changed |= switch(ui, &mut self.settings.regex),
                    );
                    setting(
                        ui,
                        "Accents must match",
                        "When off, resume also finds résumé.",
                        |ui| changed |= switch(ui, &mut self.settings.match_diacritics),
                    );
                    setting(ui, "Show hidden files and folders", "Items with the hidden attribute.", |ui| {
                        changed |= switch(ui, &mut self.settings.include_hidden)
                    });
                    setting(
                        ui,
                        "Show protected system files",
                        "Files that Windows marks as part of the system.",
                        |ui| changed |= switch(ui, &mut self.settings.include_system),
                    );
                });
                ui.add_space(4.0);

                // ---- What is indexed ----
                widgets::card(ui, |ui| {
                    widgets::section(ui, "What is indexed");
                    setting(
                        ui,
                        "Fixed drives",
                        "Index every built-in drive of this PC.",
                        |ui| reindex |= switch(ui, &mut self.settings.index_fixed_drives),
                    );
                    setting(
                        ui,
                        "Removable drives",
                        "Also index USB sticks and memory cards while they are plugged in.",
                        |ui| reindex |= switch(ui, &mut self.settings.index_removable),
                    );
                    ui.add_space(2.0);
                    ui.label(RichText::new("Drives").size(12.0).color(p.faint));
                    let drives = self.drive_list();
                    for (root, label, fs, removable) in drives {
                        let included = if removable {
                            self.settings.index_removable
                        } else {
                            self.settings.index_fixed_drives
                        };
                        let skipped = self
                            .settings
                            .skip_volumes
                            .iter()
                            .any(|s| s.eq_ignore_ascii_case(&root));
                        let mut on = included && !skipped;
                        let text = format!(
                            "{root}  {}{}",
                            if label.is_empty() { "Local disk" } else { &label },
                            if fs.is_empty() {
                                String::new()
                            } else {
                                format!("  ({fs}{})", if removable { ", removable" } else { "" })
                            }
                        );
                        if widgets::checkbox_enabled(ui, &mut on, &text, included).changed() {
                            self.settings
                                .skip_volumes
                                .retain(|s| !s.eq_ignore_ascii_case(&root));
                            if !on {
                                self.settings.skip_volumes.push(root.clone());
                            }
                            reindex = true;
                        }
                    }
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Extra folders (network shares, folders on drives that are not indexed)")
                            .size(12.0)
                            .color(p.faint),
                    );
                    let demo = self.demo;
                    let mut browse = || -> Option<String> {
                        if demo {
                            return None;
                        }
                        rfd::FileDialog::new()
                            .set_title("Choose a folder to index")
                            .pick_folder()
                            .map(|f| f.to_string_lossy().into_owned())
                    };
                    reindex |= string_list(
                        ui,
                        &mut self.settings.folders,
                        &mut self.new_folder_text,
                        "\\\\server\\share\\Projects",
                        Some(&mut browse),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new("Folders to leave out").size(12.0).color(p.faint));
                    let mut browse = || -> Option<String> {
                        if demo {
                            return None;
                        }
                        rfd::FileDialog::new()
                            .set_title("Choose a folder to leave out")
                            .pick_folder()
                            .map(|f| f.to_string_lossy().into_owned())
                    };
                    reindex |= string_list(
                        ui,
                        &mut self.settings.exclude_folders,
                        &mut self.new_exclude_folder,
                        "C:\\Folder to leave out",
                        Some(&mut browse),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Names to leave out (wildcards allowed; a folder name leaves out everything in it)")
                            .size(12.0)
                            .color(p.faint),
                    );
                    reindex |= string_list(
                        ui,
                        &mut self.settings.exclude_names,
                        &mut self.new_exclude_name,
                        "*.tmp",
                        None,
                    );
                    setting(
                        ui,
                        "Rescan interval",
                        "For drives that cannot report changes as they happen (FAT, exFAT, network).",
                        |ui| {
                            ui.label(RichText::new("minutes").color(p.muted));
                            let before = self.settings.rescan_minutes;
                            ui.add(
                                egui::DragValue::new(&mut self.settings.rescan_minutes)
                                    .range(1..=1440)
                                    .speed(1.0),
                            );
                            reindex |= before != self.settings.rescan_minutes;
                        },
                    );
                });
                ui.add_space(4.0);

                // ---- Results and history ----
                widgets::card(ui, |ui| {
                    widgets::section(ui, "Results and history");
                    setting(
                        ui,
                        "Ask before moving to the Recycle Bin",
                        "Show what will be removed and wait for a yes.",
                        |ui| changed |= switch(ui, &mut self.settings.confirm_delete),
                    );
                    setting(
                        ui,
                        "Keep search history",
                        "Remember the last 50 searches on this PC.",
                        |ui| changed |= switch(ui, &mut self.settings.keep_history),
                    );
                });
                ui.add_space(4.0);

                // ---- Desktop ----
                widgets::card(ui, |ui| {
                    widgets::section(ui, "Windows integration");
                    setting(
                        ui,
                        "Global hotkey",
                        "Bring FindForge to the front from anywhere; press again to put it away.",
                        |ui| {
                            if switch(ui, &mut self.settings.hotkey_enabled) {
                                desktop_changed = true;
                            }
                            let valid = desktop::parse_hotkey(&self.hotkey_text).is_some();
                            let resp = small_field(ui, &mut self.hotkey_text, "Ctrl+Alt+F", 120.0);
                            if resp.changed() && valid {
                                self.settings.hotkey = self.hotkey_text.trim().to_string();
                                desktop_changed = true;
                            }
                            if !valid {
                                ui.label(RichText::new("Not a hotkey").size(12.5).color(p.danger))
                                    .on_hover_text(
                                        "Use Ctrl, Alt or Win (optionally Shift) plus a letter, digit, Space or F1 to F24, for example Ctrl+Alt+F.",
                                    );
                            }
                        },
                    );
                    setting(
                        ui,
                        "Tray icon",
                        "Show FindForge in the notification area.",
                        |ui| {
                            if switch(ui, &mut self.settings.tray_icon) {
                                desktop_changed = true;
                            }
                        },
                    );
                    setting(
                        ui,
                        "Keep running when the window is closed",
                        "The close button hides the window; the index stays live. Needs the tray icon.",
                        |ui| {
                            let enabled = self.settings.tray_icon;
                            let mut v = self.settings.close_to_tray && enabled;
                            if widgets::toggle(ui, v, enabled).clicked() {
                                v = !v;
                                self.settings.close_to_tray = v;
                                changed = true;
                            }
                        },
                    );
                    setting(
                        ui,
                        "Start with Windows",
                        "Start hidden in the tray when you sign in.",
                        |ui| {
                            let mut v = self.autostart;
                            if switch(ui, &mut v) {
                                if self.demo {
                                    self.autostart = v;
                                } else {
                                    match desktop::set_autostart(v) {
                                        Ok(()) => {
                                            self.autostart = v;
                                            if v && !self.settings.tray_icon {
                                                self.settings.tray_icon = true;
                                                desktop_changed = true;
                                            }
                                        }
                                        Err(e) => self.toast(e, true),
                                    }
                                }
                            }
                        },
                    );
                    setting(
                        ui,
                        "Always run as administrator",
                        "Ask for administrator rights at every start, for the fastest indexing of NTFS drives.",
                        |ui| changed |= switch(ui, &mut self.settings.always_admin),
                    );
                    ui.add_space(2.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(Icon::INFO.glyph()).color(p.info));
                        ui.label(
                            RichText::new(
                                "An always-on background service that would make administrator rights unnecessary is not part of this version.",
                            )
                            .size(12.5)
                            .color(p.muted),
                        );
                    });
                });
                ui.add_space(12.0);
            });
        if desktop_changed {
            self.apply_desktop_settings();
            changed = true;
        }
        if reindex {
            self.reindex();
            changed = true;
        }
        if changed {
            self.save_settings();
        }
        let _ = Color32::WHITE;
    }

    /// `(root, label, file system, removable)` of every drive worth listing.
    fn drive_list(&self) -> Vec<(String, String, String, bool)> {
        if self.demo {
            return vec![
                ("C:".into(), "System".into(), "NTFS".into(), false),
                ("D:".into(), "Data".into(), "NTFS".into(), false),
                ("E:".into(), "Camera card".into(), "exFAT".into(), true),
            ];
        }
        volumes::list()
            .into_iter()
            .filter(|v| {
                matches!(
                    v.kind,
                    volumes::DriveKind::Fixed | volumes::DriveKind::Removable
                )
            })
            .map(|v| {
                (
                    v.root,
                    v.label,
                    v.fs,
                    v.kind == volumes::DriveKind::Removable,
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        assert_eq!(ago(100, 0), "Never");
        assert_eq!(ago(100, 99), "Just now");
        assert_eq!(ago(100, 70), "30 seconds ago");
        assert_eq!(ago(1000, 910), "A minute ago");
        assert_eq!(ago(10_000, 9_000), "16 minutes ago");
        assert_eq!(ago(10_000, 5_000), "An hour ago");
        assert_eq!(ago(100_000, 50_000), "13 hours ago");
        assert_eq!(ago(1_000_000, 100_000), "10 days ago");
        assert_eq!(ago(5, 50), "Just now", "a clock that went backwards");
    }

    #[test]
    fn methods_are_described() {
        for k in [VolumeKind::NtfsMft, VolumeKind::NtfsWalk, VolumeKind::Walk] {
            assert!(!method(k).is_empty());
        }
    }
}
