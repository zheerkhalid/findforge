//! Application state, background work plumbing and the window frame
//! (header with the search box, navigation rail, dialogs, toasts).

use super::icons::IconCache;
use super::preview::Preview;
use super::theme::{self, pal, pal_ctx, regular, semibold, Icon};
use super::widgets;
use crate::desktop::{self, Desktop, DesktopEvent};
use crate::engine::{self, Engine, EngineStatus, Row, SearchRequest, SearchResults, VolState};
use crate::index::Hit;
use crate::query::{self, FilterDef, SearchOptions, SortKey};
use crate::settings::{Bookmark, Settings, ThemeChoice};
use crate::shell::IconImage;
use crate::{demo, sys};
use eframe::egui::{
    self, vec2, Align, Align2, Color32, CornerRadius, Frame, Key, Layout, Margin, Rect, RichText,
    Sense, Stroke, TextureHandle, Ui,
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// How the app was started.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Show an invented index and never touch the disk.
    pub demo: bool,
    /// Render one frame of the chosen view to this PNG, then exit.
    pub screenshot: Option<PathBuf>,
    /// Which view to open first (`search`, `preview`, `filter`, `syntax`,
    /// `bookmarks`, `history`, `index`, `settings`, `about`, `rename`,
    /// `delete`, `columns`, `empty`).
    pub view: Option<String>,
    /// Force the light theme (for screenshots).
    pub light: bool,
    /// Start hidden in the notification area (`--tray`).
    pub start_hidden: bool,
    /// Text to put in the search box.
    pub initial_query: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Search,
    Bookmarks,
    History,
    Index,
    Settings,
}

/// Results of background work, delivered to the window thread.
pub enum Msg {
    Toast(String, bool),
    /// An icon for a key of the icon cache.
    Icon(String, Option<IconImage>),
    Preview(super::preview::Loaded),
    /// A rename / delete finished: the full path that is gone.
    Removed(String),
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    pub born: Instant,
}

struct Shot {
    path: PathBuf,
    frames: u32,
    requested: bool,
}

/// What was last sent to the search thread.
#[derive(Clone, PartialEq)]
pub(super) struct Submitted {
    pub text: String,
    pub filter: Option<String>,
    pub options: SearchOptions,
    pub sort: SortKey,
    pub ascending: bool,
}

/// Display data of the rows currently on screen.
#[derive(Default)]
pub(super) struct RowCache {
    pub results_id: u64,
    pub epoch: u64,
    pub start: usize,
    pub rows: Vec<Row>,
}

pub(super) struct RenameDialog {
    pub path: String,
    pub text: String,
    pub error: Option<String>,
    pub focus: bool,
}

pub(super) struct FilterDialog {
    /// Index into `settings.user_filters`, or `None` for a new filter.
    pub index: Option<usize>,
    pub name: String,
    pub query: String,
    pub error: Option<String>,
}

pub(super) struct BookmarkDialog {
    pub index: Option<usize>,
    pub name: String,
    pub query: String,
    pub filter: Option<String>,
}

pub struct App {
    pub(super) demo: bool,
    pub(super) elevated: bool,
    pub(super) hwnd: isize,
    pub(super) settings: Settings,
    pub(super) engine: Engine,
    pub(super) status: EngineStatus,
    pub(super) tab: Tab,
    pub(super) utc_offset: i32,

    // Searching
    pub(super) search: String,
    pub(super) focus_search: bool,
    pub(super) select_search_text: bool,
    pub(super) search_has_focus: bool,
    pub(super) filter: Option<String>,
    pub(super) submitted: Option<Submitted>,
    pub(super) submitted_epoch: u64,
    pub(super) last_submit: Instant,
    pub(super) last_typed: Instant,
    pub(super) history_noted: String,
    next_id: u64,
    pub(super) results: Option<SearchResults>,
    pub(super) row_cache: RowCache,

    // Selection (by hit, so it survives a refreshed result list)
    pub(super) selected: HashSet<Hit>,
    pub(super) cursor: Option<usize>,
    pub(super) anchor: Option<usize>,
    pub(super) scroll_to_cursor: bool,
    pub(super) list_view: (f32, f32),
    pub(super) release_pointer: bool,

    pub(super) icons: IconCache,
    pub(super) type_names: std::collections::HashMap<(String, bool), String>,
    pub(super) preview: Preview,

    // Dialogs
    pub(super) about_open: bool,
    pub(super) help_open: bool,
    pub(super) columns_open: bool,
    /// When the column chooser was last closed (egui time).
    pub(super) columns_closed_at: f64,
    pub(super) history_open: bool,
    pub(super) rename: Option<RenameDialog>,
    pub(super) confirm_delete: Option<Vec<String>>,
    pub(super) filter_dialog: Option<FilterDialog>,
    pub(super) bookmark_dialog: Option<BookmarkDialog>,
    pub(super) toasts: Vec<Toast>,
    pub(super) new_folder_text: String,
    pub(super) new_exclude_folder: String,
    pub(super) new_exclude_name: String,
    pub(super) autostart: bool,
    pub(super) hotkey_text: String,

    pub(super) tx: Sender<Msg>,
    rx: Receiver<Msg>,
    desktop: Option<Desktop>,
    desk_rx: Receiver<DesktopEvent>,
    quitting: bool,
    /// Frames left before a `--tray` start hides the window (the window
    /// system shows it right after the first frame, so hiding must wait).
    hide_countdown: u8,
    pub(super) logo: Option<TextureHandle>,
    shot: Option<Shot>,
    /// For screenshots: what to select once results are in.
    pub(super) pending_demo_select: Option<DemoSelect>,
    /// For background threads that need to wake the window.
    pub(super) pending_ctx: Option<egui::Context>,
}

fn window_handle(cc: &eframe::CreationContext<'_>) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

/// Starts the window.
pub fn run(options: Options, icon: egui::IconData) -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_title(if options.demo {
            "FindForge (demo)"
        } else {
            "FindForge"
        })
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size([900.0, 560.0])
        .with_icon(icon.clone());
    let native = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "FindForge",
        native,
        Box::new(move |cc| Ok(Box::new(App::new(cc, options, icon)))),
    )
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, options: Options, icon: egui::IconData) -> Self {
        let ctx = &cc.egui_ctx;
        theme::install(ctx);
        let demo = options.demo;
        let mut settings = if demo {
            demo_settings()
        } else {
            Settings::load(&sys::app_dir())
        };
        if options.light {
            settings.theme = ThemeChoice::Light;
        }
        theme::apply_choice(ctx, settings.theme);

        let logo = (icon.width > 0).then(|| {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            );
            ctx.load_texture("logo", image, egui::TextureOptions::LINEAR)
        });
        let wake = {
            let ctx = ctx.clone();
            Box::new(move || ctx.request_repaint())
        };
        let now = sys::now_unix() as u32;
        let engine = if demo {
            let index = demo::index(now);
            let status = demo::status(&index, now);
            let e = Engine::with_index(index, wake);
            e.set_status(status);
            e
        } else {
            Engine::start(
                engine::config_from_settings(&settings, Some(sys::app_dir())),
                wake,
            )
        };
        let (tx, rx) = channel();
        let (desk_tx, desk_rx) = channel();
        let hwnd = window_handle(cc);
        let live = !demo && options.screenshot.is_none();
        let desktop = if live && hwnd != 0 {
            let wake = ctx.clone();
            Desktop::start(hwnd, desk_tx, Box::new(move || wake.request_repaint()))
        } else {
            None
        };
        if let Some(d) = &desktop {
            if settings.hotkey_enabled {
                d.set_hotkey(desktop::parse_hotkey(&settings.hotkey));
            }
            d.set_tray(settings.tray_icon);
        }
        let mut app = App {
            demo,
            elevated: if demo { true } else { sys::is_elevated() },
            hwnd,
            status: engine.status(),
            engine,
            tab: Tab::Search,
            utc_offset: sys::local_utc_offset(),
            search: options.initial_query.clone().unwrap_or_default(),
            focus_search: true,
            select_search_text: false,
            search_has_focus: false,
            filter: None,
            submitted: None,
            submitted_epoch: 0,
            last_submit: Instant::now(),
            last_typed: Instant::now(),
            history_noted: String::new(),
            next_id: 0,
            results: None,
            row_cache: RowCache::default(),
            selected: HashSet::new(),
            cursor: None,
            anchor: None,
            scroll_to_cursor: false,
            list_view: (0.0, 0.0),
            release_pointer: false,
            icons: IconCache::new(tx.clone(), ctx.clone(), demo),
            type_names: Default::default(),
            preview: Preview::default(),
            about_open: false,
            help_open: false,
            columns_open: false,
            columns_closed_at: -1.0,
            history_open: false,
            rename: None,
            confirm_delete: None,
            filter_dialog: None,
            bookmark_dialog: None,
            toasts: Vec::new(),
            new_folder_text: String::new(),
            new_exclude_folder: String::new(),
            new_exclude_name: String::new(),
            autostart: live && desktop::autostart_enabled(),
            hotkey_text: settings.hotkey.clone(),
            settings,
            tx,
            rx,
            desktop,
            desk_rx,
            quitting: false,
            hide_countdown: if options.start_hidden && live { 4 } else { 0 },
            logo,
            shot: options.screenshot.clone().map(|path| Shot {
                path,
                frames: 0,
                requested: false,
            }),
            pending_demo_select: None,
            pending_ctx: Some(ctx.clone()),
        };
        app.apply_view(options.view.as_deref().unwrap_or("search"));
        app
    }

    /// Opens the view named on the command line (used for screenshots).
    fn apply_view(&mut self, view: &str) {
        if self.demo && self.search.is_empty() {
            self.search = "report".into();
        }
        match view {
            "bookmarks" => self.tab = Tab::Bookmarks,
            "history" => self.tab = Tab::History,
            "index" => self.tab = Tab::Index,
            "settings" => self.tab = Tab::Settings,
            "about" => self.about_open = true,
            "syntax" => self.help_open = true,
            "columns" => self.columns_open = true,
            "preview" => {
                self.settings.preview = true;
                self.search = "report ext:txt|ext:png".into();
                self.pending_demo_select = Some(DemoSelect::Rows(vec![1]));
            }
            "filter" => {
                self.search.clear();
                self.filter = Some("Video".into());
                self.settings.sort_key = "size".into();
                self.settings.sort_ascending = false;
            }
            "rename" => self.pending_demo_select = Some(DemoSelect::Rename(2)),
            "delete" => self.pending_demo_select = Some(DemoSelect::Delete(vec![1, 2, 4])),
            "empty" => self.search = "quarterly zebra invoice".into(),
            "rtl" => {
                self.settings.preview = true;
                self.search = "ڕاپۆرت | گۆرانی | تقرير | بەڵگە".into();
                self.pending_demo_select = Some(DemoSelect::Rows(vec![1]));
            }
            "select" => self.pending_demo_select = Some(DemoSelect::Rows(vec![1, 2, 3, 5])),
            _ => self.pending_demo_select = Some(DemoSelect::Rows(vec![2])),
        }
    }

    // ----------------------------------------------------------------------------------
    // Plumbing
    // ----------------------------------------------------------------------------------

    pub(super) fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast {
            text: text.into(),
            error,
            born: Instant::now(),
        });
    }

    /// Saves the settings (never in demo mode).
    pub(super) fn save_settings(&mut self) {
        if self.demo {
            return;
        }
        if let Err(e) = self.settings.save(&sys::app_dir()) {
            self.toast(e, true);
        }
    }

    /// Tells the engine that what is indexed has changed.
    pub(super) fn reindex(&mut self) {
        if self.demo {
            return;
        }
        self.engine.reconfigure(engine::config_from_settings(
            &self.settings,
            Some(sys::app_dir()),
        ));
    }

    pub(super) fn all_filters(&self) -> Vec<FilterDef> {
        let mut v = query::builtin_filters();
        v.extend(self.settings.user_filters.iter().cloned());
        v
    }

    fn filter_query(&self) -> Option<String> {
        let name = self.filter.as_ref()?;
        self.all_filters()
            .into_iter()
            .find(|f| &f.name == name)
            .map(|f| f.query)
    }

    pub(super) fn search_options(&self) -> SearchOptions {
        SearchOptions {
            match_case: self.settings.match_case,
            whole_word: self.settings.whole_word,
            match_path: self.settings.match_path,
            regex: self.settings.regex,
            match_diacritics: self.settings.match_diacritics,
            include_hidden: self.settings.include_hidden,
            include_system: self.settings.include_system,
        }
    }

    /// Sends the current search to the search thread when it differs from
    /// what was sent last, or when the index changed (at most once a second).
    fn drive_search(&mut self, ctx: &egui::Context) {
        let want = Submitted {
            text: self.search.clone(),
            filter: self.filter_query(),
            options: self.search_options(),
            sort: SortKey::from_id(&self.settings.sort_key),
            ascending: self.settings.sort_ascending,
        };
        let epoch = self.engine.epoch();
        let changed = self.submitted.as_ref() != Some(&want);
        let mut stale = epoch != self.submitted_epoch;
        if stale && !changed {
            // The index keeps changing on a busy drive: do not search on
            // every single change.
            let wait = Duration::from_millis(1000).saturating_sub(self.last_submit.elapsed());
            if !wait.is_zero() && self.results.is_some() {
                ctx.request_repaint_after(wait);
                stale = false;
            }
        }
        if changed || stale {
            if changed {
                self.last_typed = Instant::now();
            }
            self.next_id += 1;
            self.engine.submit(SearchRequest {
                id: self.next_id,
                text: want.text.clone(),
                filter: want.filter.clone(),
                options: want.options,
                sort: want.sort,
                ascending: want.ascending,
            });
            self.submitted = Some(want);
            self.submitted_epoch = epoch;
            self.last_submit = Instant::now();
        }
        if let Some(r) = self.engine.poll_results() {
            self.take_results(r);
        }
        // A search that stays put for a while goes into the history.
        let text = self.search.trim().to_string();
        if !text.is_empty()
            && text != self.history_noted
            && self.last_typed.elapsed() > Duration::from_secs(3)
            && self
                .results
                .as_ref()
                .map(|r| r.error.is_none())
                .unwrap_or(false)
        {
            self.note_history();
        } else if !text.is_empty() && text != self.history_noted {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// Remembers the current search text in the history.
    pub(super) fn note_history(&mut self) {
        let text = self.search.trim().to_string();
        if text.is_empty() || text == self.history_noted {
            return;
        }
        self.history_noted = text.clone();
        if self.settings.keep_history && self.settings.history.first() != Some(&text) {
            self.settings.push_history(&text);
            self.save_settings();
        }
    }

    fn take_results(&mut self, r: SearchResults) {
        // Keep the cursor on the same item if it is still there.
        let old_cursor_hit = self
            .cursor
            .and_then(|c| self.results.as_ref().and_then(|o| o.hits.get(c).copied()));
        self.cursor = match old_cursor_hit {
            Some(hit) => {
                // Look near the old position first, then everywhere.
                let near = self.cursor.unwrap_or(0);
                let lo = near.saturating_sub(2000).min(r.hits.len());
                let hi = (near + 2000).min(r.hits.len());
                r.hits[lo..hi]
                    .iter()
                    .position(|h| *h == hit)
                    .map(|p| p + lo)
                    .or_else(|| r.hits.iter().position(|h| *h == hit))
            }
            None => None,
        };
        if self.cursor.is_none() {
            self.anchor = None;
        }
        if !self.selected.is_empty() {
            if self.selected.len() <= 50_000 {
                let present: HashSet<Hit> = r
                    .hits
                    .iter()
                    .copied()
                    .filter(|h| self.selected.contains(h))
                    .collect();
                self.selected = present;
            } else if self.selected.len() > r.hits.len() {
                self.selected = r.hits.iter().copied().collect();
            }
        }
        self.row_cache = RowCache::default();
        self.results = Some(r);
        self.run_demo_select();
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Toast(text, error) => self.toast(text, error),
                Msg::Icon(key, image) => self.icons.loaded(ctx, key, image),
                Msg::Preview(loaded) => self.preview.loaded(ctx, loaded),
                Msg::Removed(path) => self.engine.note_removed(&path),
            }
        }
        while let Ok(ev) = self.desk_rx.try_recv() {
            match ev {
                DesktopEvent::Hotkey | DesktopEvent::Show => {
                    self.tab = Tab::Search;
                    self.focus_search = true;
                    self.select_search_text = true;
                }
                DesktopEvent::Exit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                DesktopEvent::HotkeyUnavailable => self.toast(
                    format!(
                        "The hotkey {} is already used by another program.",
                        self.settings.hotkey
                    ),
                    true,
                ),
            }
        }
    }

    /// Applies hotkey / tray settings to the background window.
    pub(super) fn apply_desktop_settings(&mut self) {
        if let Some(d) = &self.desktop {
            d.set_hotkey(if self.settings.hotkey_enabled {
                desktop::parse_hotkey(&self.settings.hotkey)
            } else {
                None
            });
            d.set_tray(self.settings.tray_icon);
        }
    }

    pub(super) fn modal_open(&self) -> bool {
        self.about_open
            || self.help_open
            || self.rename.is_some()
            || self.confirm_delete.is_some()
            || self.filter_dialog.is_some()
            || self.bookmark_dialog.is_some()
    }

    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quitting
            && self.settings.close_to_tray
            && self.settings.tray_icon
            && self.desktop.is_some()
            && self.shot.is_none()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            desktop::hide_window(self.hwnd);
        }
        if self.hide_countdown > 1 {
            self.hide_countdown -= 1;
            ctx.request_repaint();
        } else if self.hide_countdown == 1 {
            self.hide_countdown = 0;
            if self.settings.tray_icon && self.desktop.is_some() {
                desktop::hide_window(self.hwnd);
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        }
    }

    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(shot) = &mut self.shot else { return };
        ctx.request_repaint();
        shot.frames += 1;
        if shot.frames == 1 {
            ctx.set_pixels_per_point(1.0);
        }
        if shot.frames == 3 {
            // Now that one point is one pixel, this gives an exact 1280 x 800 image.
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(1280.0, 800.0)));
        }
        let image = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let [w, h] = image.size;
            if let Some(parent) = shot.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let result = image::save_buffer(
                &shot.path,
                image.as_raw(),
                w as u32,
                h as u32,
                image::ColorType::Rgba8,
            );
            if let Err(e) = result {
                eprintln!("screenshot failed: {e}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            self.shot = None;
            return;
        }
        let busy = self.results.is_none()
            || self.status.busy
            || self.icons.pending() > 0
            || self.preview.loading();
        if !shot.requested && shot.frames > 40 && (!busy || shot.frames > 1500) {
            shot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
    }

    pub(super) fn taking_screenshot(&self) -> bool {
        self.shot.is_some()
    }

    // ----------------------------------------------------------------------------------
    // Frame: header and navigation
    // ----------------------------------------------------------------------------------

    fn header(&mut self, ctx: &egui::Context) {
        let p = pal_ctx(ctx);
        egui::TopBottomPanel::top("header")
            .exact_height(64.0)
            .frame(Frame::new().fill(p.rail).inner_margin(Margin::symmetric(18, 0)))
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter().hline(
                    full.x_range().expand(18.0),
                    full.bottom(),
                    Stroke::new(1.0_f32, p.border),
                );
                ui.horizontal_centered(|ui| {
                    // Brand
                    let (rect, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
                    if let Some(logo) = &self.logo {
                        ui.painter().image(
                            logo.id(),
                            rect,
                            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    ui.add_space(2.0);
                    ui.label(RichText::new("FindForge").font(semibold(19.0)));
                    if self.demo {
                        widgets::badge(ui, "DEMO", p.info)
                            .on_hover_text("An invented index. Nothing on this PC is read or changed.");
                    }
                    ui.add_space(190.0 - ui.min_rect().width().min(190.0));
                    self.search_box(ui);
                    self.option_toggles(ui);

                    // Right side: rights and About
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button(ui, Icon::INFO, "About FindForge").clicked() {
                            self.about_open = true;
                        }
                        ui.add_space(4.0);
                        let fast = "With administrator rights, NTFS drives are indexed in seconds from the master file table and kept up to date from the change journal.";
                        if self.elevated {
                            widgets::badge(ui, &Icon::SHIELD.with("Administrator"), p.accent)
                                .on_hover_text(fast);
                        } else {
                            let b = widgets::ghost_button(ui, "Restart as administrator")
                                .on_hover_text(format!("{fast} Without them FindForge walks the folders (slower the first time) and watches them for changes."));
                            if b.clicked() {
                                self.restart_as_admin(ctx);
                            }
                        }
                    });
                });
            });
    }

    pub(super) fn restart_as_admin(&mut self, ctx: &egui::Context) {
        if self.demo {
            self.toast("Demo: nothing on this PC is changed.", false);
            return;
        }
        match sys::restart_as_admin("--restarted") {
            Ok(()) => {
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Err(e) => self.toast(e, true),
        }
    }

    fn search_box(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        let width = (ui.available_width() - 470.0).clamp(260.0, 620.0);
        let enabled = !self.modal_open();
        let frame = Frame::new()
            .fill(if p.dark { p.card } else { p.bg })
            .stroke(Stroke::new(
                1.0_f32,
                if self.search_has_focus {
                    p.accent
                } else {
                    p.border
                },
            ))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(Margin::symmetric(12, 6));
        let out =
            frame.show(ui, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(Icon::SEARCH.glyph())
                            .size(15.0)
                            .color(p.faint),
                    );
                    let hint = match self.filter.as_deref() {
                        Some(f) => format!("Search in {f}"),
                        None => "Search every file and folder".to_string(),
                    };
                    let edit = egui::TextEdit::singleline(&mut self.search)
                        .id(egui::Id::new("search-box"))
                        .hint_text(RichText::new(hint).color(p.faint))
                        .frame(false)
                        .desired_width(ui.available_width() - 88.0)
                        .font(regular(16.0));
                    let mut output = ui.add_enabled_ui(enabled, |ui| edit.show(ui)).inner;
                    let resp = output.response.clone();
                    if self.focus_search && enabled {
                        resp.request_focus();
                        self.focus_search = false;
                        if self.select_search_text {
                            self.select_search_text = false;
                            let end = egui::text::CCursor::new(self.search.chars().count());
                            output.state.cursor.set_char_range(Some(
                                egui::text::CCursorRange::two(egui::text::CCursor::new(0), end),
                            ));
                            output.state.store(ui.ctx(), resp.id);
                        }
                    }
                    self.search_has_focus = resp.has_focus();
                    if resp.changed() {
                        self.tab = Tab::Search;
                    }
                    if !self.search.is_empty() {
                        if widgets::icon_button(ui, Icon::CLOSE, "Clear (Esc)").clicked() {
                            self.search.clear();
                            self.focus_search = true;
                        }
                    } else {
                        ui.add_space(4.0);
                        ui.label(RichText::new("Ctrl+F").size(11.5).color(p.faint));
                        ui.add_space(4.0);
                    }
                    let hist = widgets::icon_button(ui, Icon::CHEVRON_DOWN, "Recent searches");
                    if hist.clicked() {
                        self.history_open = !self.history_open;
                    }
                });
            });
        // Recent searches, right under the box.
        if self.history_open {
            let rect = out.response.rect;
            let mut picked: Option<String> = None;
            let area = egui::Area::new(egui::Id::new("history-popup"))
                .order(egui::Order::Foreground)
                .fixed_pos(rect.left_bottom() + vec2(0.0, 4.0))
                .show(ui.ctx(), |ui| {
                    Frame::new()
                        .fill(p.card)
                        .stroke(Stroke::new(1.0_f32, p.border))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::same(6))
                        .show(ui, |ui| {
                            ui.set_width(rect.width() - 12.0);
                            ui.spacing_mut().item_spacing.y = 2.0;
                            if self.settings.history.is_empty() {
                                ui.label(
                                    RichText::new("  Searches you run will be listed here.")
                                        .color(p.faint),
                                );
                            }
                            for h in self.settings.history.iter().take(12) {
                                let (r, resp) = ui.allocate_exact_size(
                                    vec2(ui.available_width(), 30.0),
                                    Sense::click(),
                                );
                                if resp.hovered() {
                                    ui.painter().rect_filled(r, CornerRadius::same(7), p.hover);
                                }
                                ui.painter().text(
                                    r.left_center() + vec2(14.0, 0.0),
                                    Align2::CENTER_CENTER,
                                    Icon::HISTORY.glyph(),
                                    regular(13.0),
                                    p.faint,
                                );
                                widgets::text_in(
                                    ui,
                                    Rect::from_min_max(
                                        r.min + vec2(30.0, 0.0),
                                        r.max - vec2(8.0, 0.0),
                                    ),
                                    h,
                                    regular(14.0),
                                    p.text,
                                );
                                if resp.clicked() {
                                    picked = Some(h.clone());
                                }
                            }
                        });
                });
            if let Some(h) = picked {
                self.search = h;
                self.tab = Tab::Search;
                self.history_open = false;
                self.focus_search = true;
            } else if ui.input(|i| i.pointer.any_click())
                && !area
                    .response
                    .rect
                    .contains(ui.input(|i| i.pointer.interact_pos()).unwrap_or_default())
                && !rect.contains(ui.input(|i| i.pointer.interact_pos()).unwrap_or_default())
            {
                self.history_open = false;
            }
        }
    }

    /// Match case / whole word / path / regex switches next to the search box.
    fn option_toggles(&mut self, ui: &mut Ui) {
        let p = pal(ui);
        ui.add_space(2.0);
        let mut changed = false;
        let mut toggle = |ui: &mut Ui, label: &str, tip: &str, value: &mut bool| {
            let (fill, fg, stroke) = if *value {
                (
                    p.accent.gamma_multiply(0.16),
                    p.accent,
                    Stroke::new(1.0_f32, p.accent.gamma_multiply(0.6)),
                )
            } else {
                (
                    Color32::TRANSPARENT,
                    p.muted,
                    Stroke::new(1.0_f32, p.border),
                )
            };
            let b = egui::Button::new(RichText::new(label).font(semibold(12.5)).color(fg))
                .fill(fill)
                .stroke(stroke)
                .corner_radius(CornerRadius::same(8))
                .min_size(vec2(34.0, 32.0));
            let prev = ui.spacing().button_padding;
            ui.spacing_mut().button_padding = vec2(6.0, 4.0);
            let r = ui.add(b).on_hover_text(tip);
            ui.spacing_mut().button_padding = prev;
            if r.clicked() {
                *value = !*value;
                changed = true;
            }
        };
        ui.spacing_mut().item_spacing.x = 4.0;
        toggle(ui, "Aa", "Match case", &mut self.settings.match_case);
        toggle(ui, "W", "Whole words only", &mut self.settings.whole_word);
        toggle(
            ui,
            "\\",
            "Match the full path, not just the name",
            &mut self.settings.match_path,
        );
        toggle(ui, ".*", "Regular expression", &mut self.settings.regex);
        ui.spacing_mut().item_spacing.x = 8.0;
        if widgets::icon_button(ui, Icon::HELP, "Search syntax").clicked() {
            self.help_open = true;
        }
        if changed {
            self.save_settings();
            self.focus_search = true;
        }
    }

    fn rail(&mut self, ctx: &egui::Context) {
        let p = pal_ctx(ctx);
        egui::SidePanel::left("rail")
            .exact_width(208.0)
            .resizable(false)
            .frame(
                Frame::new()
                    .fill(p.rail)
                    .inner_margin(Margin::symmetric(10, 14)),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter().vline(
                    full.right() + 10.0,
                    full.y_range().expand(14.0),
                    Stroke::new(1.0_f32, p.border),
                );
                ui.spacing_mut().item_spacing.y = 4.0;
                let items: [(Tab, Icon, &str, Option<usize>); 5] = [
                    (Tab::Search, Icon::SEARCH, "Search", None),
                    (
                        Tab::Bookmarks,
                        Icon::STAR,
                        "Bookmarks",
                        Some(self.settings.bookmarks.len()),
                    ),
                    (
                        Tab::History,
                        Icon::HISTORY,
                        "History",
                        Some(self.settings.history.len()),
                    ),
                    (
                        Tab::Index,
                        Icon::DATABASE,
                        "Index",
                        Some(self.status.volumes.len()),
                    ),
                    (Tab::Settings, Icon::SETTINGS, "Settings", None),
                ];
                for (tab, icon, label, count) in items {
                    if tab == Tab::Settings {
                        ui.add_space(8.0);
                        widgets::divider(ui);
                        ui.add_space(8.0);
                    }
                    let active = self.tab == tab;
                    let (rect, resp) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
                    if active {
                        ui.painter()
                            .rect_filled(rect, CornerRadius::same(9), p.selected);
                        let bar = Rect::from_min_size(
                            rect.left_center() - vec2(0.0, 9.0),
                            vec2(3.0, 18.0),
                        );
                        ui.painter()
                            .rect_filled(bar, CornerRadius::same(2), p.accent);
                    } else if resp.hovered() {
                        ui.painter()
                            .rect_filled(rect, CornerRadius::same(9), p.hover);
                    }
                    let fg = if active { p.text } else { p.muted };
                    ui.painter().text(
                        rect.left_center() + vec2(24.0, 0.0),
                        Align2::CENTER_CENTER,
                        icon.glyph(),
                        regular(16.0),
                        if active { p.accent } else { fg },
                    );
                    ui.painter().text(
                        rect.left_center() + vec2(46.0, -1.0),
                        Align2::LEFT_CENTER,
                        label,
                        if active {
                            semibold(14.5)
                        } else {
                            regular(14.5)
                        },
                        fg,
                    );
                    if let Some(n) = count {
                        ui.painter().text(
                            rect.right_center() - vec2(12.0, 1.0),
                            Align2::RIGHT_CENTER,
                            n.to_string(),
                            regular(12.5),
                            p.faint,
                        );
                    }
                    if resp.clicked() {
                        self.tab = tab;
                        if tab == Tab::Search {
                            self.focus_search = true;
                        }
                    }
                }
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    ui.add_space(2.0);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                                .size(12.0)
                                .color(p.faint),
                        );
                    });
                    // Index state at a glance.
                    ui.add_space(6.0);
                    let (text, color, spin) = index_summary(&self.status, p.ok, p.warn, p.muted);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        if spin {
                            ui.add(egui::Spinner::new().size(12.0).color(p.accent));
                        } else {
                            let (dot, _) = ui.allocate_exact_size(vec2(8.0, 16.0), Sense::hover());
                            ui.painter().circle_filled(dot.center(), 4.0, color);
                        }
                        ui.label(RichText::new(text).size(12.0).color(p.muted));
                    });
                });
            });
    }

    // ----------------------------------------------------------------------------------
    // Keyboard
    // ----------------------------------------------------------------------------------

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let (find, escape, f5) = ctx.input(|i| {
            (
                (i.modifiers.command && i.key_pressed(Key::F)) || i.key_pressed(Key::F3),
                i.key_pressed(Key::Escape),
                i.key_pressed(Key::F5),
            )
        });
        if find && !self.modal_open() {
            self.tab = Tab::Search;
            self.focus_search = true;
            self.select_search_text = true;
        }
        if f5 && !self.modal_open() {
            if self.demo {
                self.toast("Demo: the invented index never changes.", false);
            } else {
                self.engine.rescan(None);
                self.toast("Rescanning…", false);
            }
        }
        if escape {
            if self.rename.is_some() {
                self.rename = None;
            } else if self.confirm_delete.is_some() {
                self.confirm_delete = None;
            } else if self.filter_dialog.is_some() {
                self.filter_dialog = None;
            } else if self.bookmark_dialog.is_some() {
                self.bookmark_dialog = None;
            } else if self.about_open {
                self.about_open = false;
            } else if self.help_open {
                self.help_open = false;
            } else if self.history_open {
                self.history_open = false;
            } else if self.columns_open {
                self.columns_open = false;
            } else if self.tab != Tab::Search {
                self.tab = Tab::Search;
                self.focus_search = true;
            } else if !self.selected.is_empty() && !self.search_has_focus {
                self.selected.clear();
            } else if !self.search.is_empty() {
                self.search.clear();
                self.focus_search = true;
            } else if self.filter.is_some() {
                self.filter = None;
            }
        }
        if self.tab == Tab::Search && !self.modal_open() {
            self.results_keys(ctx);
        }
    }

    // ----------------------------------------------------------------------------------
    // Dialogs and toasts
    // ----------------------------------------------------------------------------------

    fn about_dialog(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let p = pal_ctx(ctx);
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::new("about"))
            .frame(
                Frame::new()
                    .fill(p.card)
                    .stroke(Stroke::new(1.0_f32, p.border))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::same(28)),
            )
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.vertical_centered(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(84.0, 84.0), Sense::hover());
                    if let Some(logo) = &self.logo {
                        ui.painter().image(
                            logo.id(),
                            rect,
                            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    ui.add_space(6.0);
                    ui.label(RichText::new("FindForge").font(semibold(26.0)));
                    ui.label(
                        RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                            .color(p.muted),
                    );
                    ui.add_space(10.0);
                    ui.label("Every file name on your drives, found as you type.");
                    ui.add_space(10.0);
                    ui.label(RichText::new("Developed by Zheer Khalid").font(semibold(15.0)));
                    ui.add_space(2.0);
                    ui.hyperlink_to(
                        RichText::new("zheer.dev").color(p.accent),
                        "https://zheer.dev",
                    );
                    ui.hyperlink_to(
                        RichText::new("github.com/zheerkhalid/findforge").color(p.accent),
                        "https://github.com/zheerkhalid/findforge",
                    );
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new("Free and open source under the MIT License.")
                            .size(12.0)
                            .color(p.faint),
                    );
                    ui.add_space(14.0);
                    if widgets::primary_button(ui, "Close", true).clicked() {
                        close = true;
                    }
                });
            });
        if close || modal.should_close() {
            self.about_open = false;
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts
            .retain(|t| t.born.elapsed().as_secs_f32() < if t.error { 8.0 } else { 3.5 });
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let p = pal_ctx(ctx);
        egui::Area::new(egui::Id::new("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, vec2(-18.0, -46.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
                    for t in self.toasts.iter().rev().take(4) {
                        let color = if t.error { p.danger } else { p.accent };
                        Frame::new()
                            .fill(p.card)
                            .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.7)))
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(14, 10))
                            .show(ui, |ui| {
                                ui.set_max_width(420.0);
                                ui.horizontal(|ui| {
                                    let icon = if t.error { Icon::ERROR } else { Icon::DONE };
                                    ui.label(RichText::new(icon.glyph()).color(color));
                                    ui.add(egui::Label::new(t.text.as_str()).wrap());
                                });
                            });
                    }
                });
            });
    }

    /// The standard page header: title, subtitle and right-aligned actions.
    pub(super) fn page_header(
        ui: &mut Ui,
        title: &str,
        subtitle: &str,
        actions: impl FnOnce(&mut Ui),
    ) {
        let p = pal(ui);
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), 52.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(RichText::new(title).font(semibold(24.0)));
                    ui.label(RichText::new(subtitle).color(p.muted));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), actions);
            },
        );
        ui.add_space(6.0);
    }

    /// Adds the current search to the bookmarks (after asking for a name).
    pub(super) fn bookmark_current(&mut self) {
        let query = self.search.trim().to_string();
        if query.is_empty() && self.filter.is_none() {
            self.toast("Type a search first, then bookmark it.", false);
            return;
        }
        let name = if query.is_empty() {
            self.filter.clone().unwrap_or_default()
        } else {
            query.clone()
        };
        self.bookmark_dialog = Some(BookmarkDialog {
            index: None,
            name,
            query,
            filter: self.filter.clone(),
        });
    }

    pub(super) fn run_bookmark(&mut self, b: &Bookmark) {
        self.search = b.query.clone();
        self.filter = b
            .filter
            .clone()
            .filter(|f| self.all_filters().iter().any(|x| &x.name == f));
        self.tab = Tab::Search;
        self.focus_search = true;
    }
}

/// One line about the state of the index: `(text, colour, busy)`.
pub(super) fn index_summary(
    status: &EngineStatus,
    ok: Color32,
    warn: Color32,
    idle: Color32,
) -> (String, Color32, bool) {
    if let Some(v) = status.volumes.iter().find_map(|v| match &v.state {
        VolState::Scanning { found } => Some(format!(
            "Indexing {} {}",
            v.root,
            crate::util::fmt_count(*found)
        )),
        VolState::Loading => Some(format!("Loading {}", v.root)),
        _ => None,
    }) {
        return (v, warn, true);
    }
    if status.volumes.is_empty() {
        return ("Nothing indexed".into(), idle, false);
    }
    if status.volumes.iter().any(|v| v.refreshing) {
        return ("Refreshing the index".into(), ok, true);
    }
    if status
        .volumes
        .iter()
        .any(|v| matches!(v.state, VolState::Failed(_)))
    {
        return ("A drive could not be indexed".into(), warn, false);
    }
    if status.volumes.iter().all(|v| v.live) {
        ("Index is live".into(), ok, false)
    } else {
        ("Index ready".into(), idle, false)
    }
}

/// Settings for the demo: invented bookmarks, history and a filter.
fn demo_settings() -> Settings {
    let mut s = Settings::default();
    s.history = [
        "report",
        "ext:mp3;flac size:>20mb",
        "holiday *.jpg dm:lastyear",
        "invoice 2026 !draft",
        "parent:C:\\Users\\Alex\\Downloads size:>100mb",
        "\"annual report\"",
        "dupe: ext:pdf",
        "regex:^IMG_\\d{4}\\.cr3$",
        "budget|forecast ext:xlsx",
        "empty:",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    s.bookmarks = vec![
        Bookmark {
            name: "Large videos".into(),
            query: "size:>1gb".into(),
            filter: Some("Video".into()),
        },
        Bookmark {
            name: "Changed today".into(),
            query: "dm:today".into(),
            filter: None,
        },
        Bookmark {
            name: "Reports of 2026".into(),
            query: "report 2026 ext:pdf;docx;xlsx".into(),
            filter: None,
        },
        Bookmark {
            name: "Duplicate photos".into(),
            query: "dupe:".into(),
            filter: Some("Pictures".into()),
        },
        Bookmark {
            name: "Empty folders".into(),
            query: "empty:".into(),
            filter: None,
        },
        Bookmark {
            name: "Old installers in Downloads".into(),
            query: "downloads\\ ext:exe;msi dm:<2026".into(),
            filter: None,
        },
    ];
    s.user_filters = vec![
        FilterDef {
            name: "Source code".into(),
            query: "ext:rs;ts;tsx;py;css;js;json".into(),
        },
        FilterDef {
            name: "Huge files".into(),
            query: "size:>1gb".into(),
        },
    ];
    s.exclude_names = vec!["node_modules".into(), "*.tmp".into()];
    s.exclude_folders = vec!["C:\\Windows\\Temp".into()];
    s.tray_icon = true;
    s.hotkey_enabled = true;
    // Newest first shows a nice mix of kinds in the demo.
    s.sort_key = "modified".into();
    s.sort_ascending = false;
    s
}

/// What a demo screenshot wants selected once results are in.
pub(super) enum DemoSelect {
    Rows(Vec<usize>),
    Rename(usize),
    Delete(Vec<usize>),
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.status = self.engine.status();
        self.drain_messages(ctx);
        self.handle_close(ctx);
        self.handle_screenshot(ctx);
        self.drive_search(ctx);
        self.handle_keys(ctx);

        self.header(ctx);
        self.rail(ctx);

        let p = pal_ctx(ctx);
        match self.tab {
            Tab::Search => self.search_view(ctx),
            _ => {
                egui::CentralPanel::default()
                    .frame(
                        Frame::new()
                            .fill(p.bg)
                            .inner_margin(Margin::symmetric(24, 18)),
                    )
                    .show(ctx, |ui| match self.tab {
                        Tab::Bookmarks => self.bookmarks_ui(ui),
                        Tab::History => self.history_ui(ui),
                        Tab::Index => self.index_ui(ui),
                        _ => self.settings_ui(ui),
                    });
            }
        }

        self.dialogs(ctx);
        self.about_dialog(ctx);
        self.toasts_ui(ctx);
        if self.status.busy {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // Screenshots must not depend on where the mouse happens to be:
        // no hover highlights, no tooltips.
        if self.shot.is_some() {
            raw_input.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::PointerMoved(_)
                        | egui::Event::MouseMoved(_)
                        | egui::Event::PointerButton { .. }
                        | egui::Event::MouseWheel { .. }
                )
            });
            raw_input.events.push(egui::Event::PointerGone);
        }
        // A drag-out or a shell menu ran its own message loop and swallowed
        // the mouse-up: tell egui that the button is no longer down.
        if self.release_pointer {
            self.release_pointer = false;
            raw_input.events.push(egui::Event::PointerGone);
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.desktop = None;
        self.engine.shutdown();
    }
}
