//! The engine: owns the index, builds it, keeps it live and answers searches.
//!
//! One manager thread drives everything about the index (loading saved
//! indexes, scans, journal polling, change notifications, saving); one search
//! thread runs queries so the window never waits. The user interface talks to
//! both through [`Engine`].

use crate::index::{Applied, Change, Excludes, Hit, Index, VolumeKind, VolumeStore};
use crate::ntfs::{self, Journal};
use crate::query::{self, Now, SearchOptions, SortKey};
use crate::settings::Settings;
use crate::watch::{WatchEvent, Watcher};
use crate::{sys, volumes, walk};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// --------------------------------------------------------------------------------------
// Configuration
// --------------------------------------------------------------------------------------

/// Something to index: a drive or a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSpec {
    /// `C:` or a folder path without a trailing backslash.
    pub root: String,
    pub is_drive: bool,
    pub fs: String,
    pub label: String,
    pub serial: u32,
}

impl RootSpec {
    pub fn is_ntfs(&self) -> bool {
        self.fs.eq_ignore_ascii_case("NTFS")
    }
}

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub roots: Vec<RootSpec>,
    pub excludes: Excludes,
    pub rescan_minutes: u32,
    /// Where saved indexes live; `None` = never read or write them.
    pub data_dir: Option<PathBuf>,
    /// Use the master file table and the change journal where possible.
    pub elevated: bool,
    /// Rescan volumes whose saved index cannot be brought up to date from the
    /// change journal. False = use saved indexes as they are (quick command
    /// line searches).
    pub refresh_loaded: bool,
}

/// Works out what to index from the settings and the drives present.
pub fn roots_from_settings(s: &Settings) -> Vec<RootSpec> {
    let mut roots: Vec<RootSpec> = Vec::new();
    for v in volumes::list() {
        let wanted = match v.kind {
            volumes::DriveKind::Fixed => s.index_fixed_drives,
            volumes::DriveKind::Removable => s.index_removable,
            _ => false,
        };
        let skipped = s
            .skip_volumes
            .iter()
            .any(|x| x.trim_end_matches('\\').eq_ignore_ascii_case(&v.root));
        if wanted && !skipped && !v.fs.is_empty() {
            roots.push(RootSpec {
                root: v.root.clone(),
                is_drive: true,
                fs: v.fs.clone(),
                label: v.label.clone(),
                serial: v.serial,
            });
        }
    }
    for f in &s.folders {
        let folder = f.trim().trim_end_matches('\\').to_string();
        if folder.is_empty() {
            continue;
        }
        let lower = folder.to_lowercase();
        let covered = roots.iter().any(|r| {
            let root = r.root.to_lowercase();
            lower == root || lower.starts_with(&format!("{root}\\"))
        });
        if covered {
            continue;
        }
        let (label, fs, serial) = volumes::drive_root_of(&folder)
            .and_then(|d| volumes::volume_information(&d))
            .unwrap_or_default();
        roots.push(RootSpec {
            root: folder,
            is_drive: false,
            fs,
            label,
            serial,
        });
    }
    roots
}

pub fn config_from_settings(s: &Settings, data_dir: Option<PathBuf>) -> EngineConfig {
    EngineConfig {
        roots: roots_from_settings(s),
        excludes: Excludes::new(&s.exclude_folders, &s.exclude_names),
        rescan_minutes: s.rescan_minutes,
        data_dir,
        elevated: sys::is_elevated(),
        refresh_loaded: true,
    }
}

// --------------------------------------------------------------------------------------
// Status
// --------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolState {
    /// Reading the saved index.
    Loading,
    /// First scan; `found` entries so far.
    Scanning {
        found: u64,
    },
    Ready,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct VolStatus {
    pub root: String,
    pub label: String,
    pub fs: String,
    pub kind: VolumeKind,
    pub state: VolState,
    /// A rescan is running behind an index that is already usable.
    pub refreshing: bool,
    pub entries: usize,
    pub memory_bytes: usize,
    /// Changes arrive as they happen (journal or notifications).
    pub live: bool,
    /// When the index of this volume last changed (Unix seconds; 0 = never).
    pub last_update_unix: u64,
    /// When the last full scan finished and how long it took.
    pub scanned_unix: u64,
    pub scan_ms: u32,
    /// The index shown came from disk rather than from a scan in this run.
    pub from_disk: bool,
    /// Anything the user should know ("standard rights: folder walk").
    pub note: String,
}

#[derive(Clone, Debug, Default)]
pub struct EngineStatus {
    pub volumes: Vec<VolStatus>,
    pub total_entries: usize,
    pub memory_bytes: usize,
    pub elevated: bool,
    /// Some volume is still loading or doing its first scan.
    pub busy: bool,
    pub last_save_unix: u64,
}

// --------------------------------------------------------------------------------------
// Searching
// --------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SearchRequest {
    /// Chosen by the caller; echoed in the results.
    pub id: u64,
    pub text: String,
    /// Query of the active filter, if any.
    pub filter: Option<String>,
    pub options: SearchOptions,
    pub sort: SortKey,
    pub ascending: bool,
}

#[derive(Clone, Debug)]
pub struct SearchResults {
    pub id: u64,
    pub hits: Arc<Vec<Hit>>,
    /// Index epoch the hits belong to.
    pub epoch: u64,
    pub took: Duration,
    /// Parse error, if the query could not be understood.
    pub error: Option<String>,
    /// Sum of the sizes of all hits.
    pub total_bytes: u64,
    /// The parsed query (for highlighting); `None` on a parse error.
    pub query: Option<Arc<query::Query>>,
}

/// Display data of one result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    /// Folder that contains it.
    pub dir: String,
    pub size: u64,
    pub mtime: u32,
    pub ctime: u32,
    pub is_dir: bool,
    /// Lower-case extension without the dot.
    pub ext: String,
    pub flags: u16,
    /// False when the entry has been deleted since the search ran.
    pub exists: bool,
}

impl Row {
    pub fn full_path(&self) -> String {
        if self.dir.is_empty() {
            self.name.clone()
        } else if self.dir.ends_with('\\') {
            format!("{}{}", self.dir, self.name)
        } else {
            format!("{}\\{}", self.dir, self.name)
        }
    }
}

/// Reads the display data of a hit.
pub fn row_of(index: &Index, hit: Hit) -> Row {
    let Some(vol) = index.volumes.get(hit.vol as usize) else {
        return Row::default();
    };
    match vol.entry(hit.slot).filter(|e| e.is_live()) {
        Some(e) => Row {
            name: vol.name_of(e).to_string(),
            dir: vol.parent_path(hit.slot),
            size: e.size,
            mtime: e.mtime,
            ctime: e.ctime,
            is_dir: e.is_dir(),
            ext: vol.ext_of(e).to_string(),
            flags: e.flags,
            exists: true,
        },
        None => Row::default(),
    }
}

struct SearchSlot {
    state: Mutex<SearchState>,
    wake: Condvar,
    cancel: AtomicBool,
}

#[derive(Default)]
struct SearchState {
    request: Option<SearchRequest>,
    results: Option<SearchResults>,
    stop: bool,
}

struct Shared {
    status: Mutex<EngineStatus>,
    epoch: AtomicU64,
    wake: Box<dyn Fn() + Send + Sync>,
}

/// Handle to the running engine.
pub struct Engine {
    index: Arc<RwLock<Index>>,
    shared: Arc<Shared>,
    tx: Mutex<Option<Sender<Msg>>>,
    search: Arc<SearchSlot>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

fn now() -> Now {
    Now {
        unix: sys::now_unix() as i64,
        utc_offset_secs: sys::local_utc_offset(),
    }
}

fn run_search(index: &Index, req: &SearchRequest, cancel: &AtomicBool) -> Option<SearchResults> {
    let started = Instant::now();
    let query = match query::parse(&req.text, req.filter.as_deref(), &req.options, now()) {
        Ok(q) => q,
        Err(e) => {
            return Some(SearchResults {
                id: req.id,
                hits: Arc::new(Vec::new()),
                epoch: index.epoch,
                took: started.elapsed(),
                error: Some(e),
                total_bytes: 0,
                query: None,
            })
        }
    };
    let mut hits = query::search(index, &query, cancel);
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    let total_bytes = {
        use rayon::prelude::*;
        hits.par_iter()
            .map(|h| index.entry(*h).map(|e| e.size).unwrap_or(0))
            .sum()
    };
    query::sort(index, &mut hits, req.sort, req.ascending, cancel);
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    Some(SearchResults {
        id: req.id,
        hits: Arc::new(hits),
        epoch: index.epoch,
        took: started.elapsed(),
        error: None,
        total_bytes,
        query: Some(Arc::new(query)),
    })
}

/// Runs one search right now on the calling thread (command line, tests).
pub fn search_now(index: &Index, req: &SearchRequest) -> SearchResults {
    let cancel = AtomicBool::new(false);
    run_search(index, req, &cancel).expect("not cancelled")
}

fn search_thread(index: Arc<RwLock<Index>>, slot: Arc<SearchSlot>, shared: Arc<Shared>) {
    loop {
        let req = {
            let mut st = slot.state.lock().unwrap();
            loop {
                if st.stop {
                    return;
                }
                if let Some(r) = st.request.take() {
                    slot.cancel.store(false, Ordering::SeqCst);
                    break r;
                }
                st = slot.wake.wait(st).unwrap();
            }
        };
        let results = {
            let idx = index.read().unwrap();
            run_search(&idx, &req, &slot.cancel)
        };
        if let Some(results) = results {
            let mut st = slot.state.lock().unwrap();
            // A newer request makes these results stale: drop them.
            if st.request.is_none() {
                st.results = Some(results);
                drop(st);
                (shared.wake)();
            }
        }
    }
}

impl Engine {
    /// Starts indexing according to `config`. `wake` is called (from
    /// background threads) whenever there is something new to show.
    pub fn start(config: EngineConfig, wake: Box<dyn Fn() + Send + Sync>) -> Engine {
        let engine = Engine::with_index(Index::default(), wake);
        let (tx, rx) = channel();
        let manager = Manager {
            index: engine.index.clone(),
            shared: engine.shared.clone(),
            tx: tx.clone(),
            config,
            vols: Vec::new(),
            last_save: Instant::now(),
            last_save_unix: 0,
        };
        let handle = std::thread::Builder::new()
            .name("findforge-index".into())
            .spawn(move || manager.run(rx))
            .expect("spawn index thread");
        *engine.tx.lock().unwrap() = Some(tx);
        engine.threads.lock().unwrap().push(handle);
        engine
    }

    /// An engine over a fixed index: nothing is read from or written to disk
    /// (demo mode, tests).
    pub fn with_index(index: Index, wake: Box<dyn Fn() + Send + Sync>) -> Engine {
        let shared = Arc::new(Shared {
            status: Mutex::new(EngineStatus::default()),
            epoch: AtomicU64::new(index.epoch),
            wake,
        });
        let index = Arc::new(RwLock::new(index));
        let search = Arc::new(SearchSlot {
            state: Mutex::new(SearchState::default()),
            wake: Condvar::new(),
            cancel: AtomicBool::new(false),
        });
        let handle = {
            let (index, search, shared) = (index.clone(), search.clone(), shared.clone());
            std::thread::Builder::new()
                .name("findforge-search".into())
                .spawn(move || search_thread(index, search, shared))
                .expect("spawn search thread")
        };
        Engine {
            index,
            shared,
            tx: Mutex::new(None),
            search,
            threads: Mutex::new(vec![handle]),
        }
    }

    /// Replaces the status shown for a fixed index (demo mode).
    pub fn set_status(&self, status: EngineStatus) {
        *self.shared.status.lock().unwrap() = status;
    }

    pub fn index(&self) -> &Arc<RwLock<Index>> {
        &self.index
    }

    /// Counter that changes whenever the index changes.
    pub fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> EngineStatus {
        self.shared.status.lock().unwrap().clone()
    }

    /// Queues a search; an unfinished earlier one is abandoned.
    pub fn submit(&self, request: SearchRequest) {
        let mut st = self.search.state.lock().unwrap();
        st.request = Some(request);
        self.search.cancel.store(true, Ordering::SeqCst);
        drop(st);
        self.search.wake.notify_one();
    }

    /// Takes the newest finished results, if any arrived since the last call.
    pub fn poll_results(&self) -> Option<SearchResults> {
        self.search.state.lock().unwrap().results.take()
    }

    /// Display data for some hits (one short read lock).
    pub fn rows(&self, hits: &[Hit]) -> Vec<Row> {
        let idx = self.index.read().unwrap();
        hits.iter().map(|h| row_of(&idx, *h)).collect()
    }

    /// Sum of the sizes of some hits.
    pub fn total_size(&self, hits: impl IntoIterator<Item = Hit>) -> u64 {
        let idx = self.index.read().unwrap();
        hits.into_iter()
            .map(|h| idx.entry(h).map(|e| e.size).unwrap_or(0))
            .sum()
    }

    /// Full paths of some hits; deleted entries are left out.
    pub fn paths(&self, hits: &[Hit]) -> Vec<String> {
        let idx = self.index.read().unwrap();
        hits.iter()
            .map(|h| idx.path(*h))
            .filter(|p| !p.is_empty())
            .collect()
    }

    fn send(&self, msg: Msg) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(msg);
        }
    }

    /// Rescans one root (`C:`) or, with `None`, everything.
    pub fn rescan(&self, root: Option<String>) {
        self.send(Msg::Rescan(root));
    }

    /// Applies new settings: adds / removes volumes, rescans if the exclude
    /// lists changed.
    pub fn reconfigure(&self, config: EngineConfig) {
        self.send(Msg::Configure(config));
    }

    /// Writes changed indexes to disk now.
    pub fn save_now(&self) {
        self.send(Msg::Save);
    }

    /// Tells the engine that the program itself changed a file (rename,
    /// delete), so results update at once even on volumes that are only
    /// rescanned periodically. `path` is the full path before the change.
    pub fn note_removed(&self, path: &str) {
        self.send(Msg::Removed(path.to_string()));
    }

    /// Stops all threads, saving changed indexes first. Blocks briefly.
    pub fn shutdown(&self) {
        let tx = self.tx.lock().unwrap().take();
        if let Some(tx) = tx {
            let (done_tx, done_rx) = channel();
            if tx.send(Msg::Shutdown(done_tx)).is_ok() {
                let _ = done_rx.recv_timeout(Duration::from_secs(20));
            }
        }
        {
            let mut st = self.search.state.lock().unwrap();
            st.stop = true;
            self.search.cancel.store(true, Ordering::SeqCst);
        }
        self.search.wake.notify_all();
        let handles: Vec<JoinHandle<()>> = self.threads.lock().unwrap().drain(..).collect();
        for h in handles {
            let _ = h.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// --------------------------------------------------------------------------------------
// The manager thread
// --------------------------------------------------------------------------------------

enum Msg {
    Configure(EngineConfig),
    Rescan(Option<String>),
    Save,
    Removed(String),
    Shutdown(Sender<()>),
    Loaded {
        root: String,
        gen: u64,
        store: Option<Box<VolumeStore>>,
    },
    Progress {
        root: String,
        gen: u64,
        found: u64,
    },
    ScanDone {
        root: String,
        gen: u64,
        kind: VolumeKind,
        result: Result<Box<VolumeStore>, String>,
        ms: u32,
    },
    Watch {
        root: String,
        event: WatchEvent,
    },
}

struct Vol {
    spec: RootSpec,
    /// Bumped whenever the volume is (re)started; older messages are ignored.
    gen: u64,
    state: VolState,
    desired: VolumeKind,
    scan_cancel: Option<Arc<AtomicBool>>,
    /// Changes that arrived while a scan was running (replayed onto its result).
    pending: Vec<Change>,
    journal: Option<Journal>,
    watcher: Option<Watcher>,
    dirty: bool,
    last_update: u64,
    last_scan: Instant,
    from_disk: bool,
    note: String,
    rescan_wanted: bool,
}

struct Manager {
    index: Arc<RwLock<Index>>,
    shared: Arc<Shared>,
    tx: Sender<Msg>,
    config: EngineConfig,
    vols: Vec<Vol>,
    last_save: Instant,
    last_save_unix: u64,
}

/// Changed indexes are written on exit and, as a safety net, once an hour:
/// a large index is tens of megabytes, too much to rewrite every few minutes.
const AUTOSAVE: Duration = Duration::from_secs(3600);
const TICK: Duration = Duration::from_millis(400);

/// File name of the saved index of a root.
pub fn index_file_name(root: &str) -> String {
    let safe: String = root
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    let hash = root
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("index-{safe}-{:08x}.ffi", hash as u32)
}

fn same_root(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
}

impl Manager {
    fn desired_kind(&self, spec: &RootSpec) -> VolumeKind {
        if !spec.is_ntfs() {
            VolumeKind::Walk
        } else if self.config.elevated && spec.is_drive {
            VolumeKind::NtfsMft
        } else {
            VolumeKind::NtfsWalk
        }
    }

    fn file_for(&self, root: &str) -> Option<PathBuf> {
        self.config
            .data_dir
            .as_ref()
            .map(|d| d.join(index_file_name(root)))
    }

    fn run(mut self, rx: Receiver<Msg>) {
        let roots = self.config.roots.clone();
        for spec in roots {
            self.add_volume(spec);
        }
        self.publish();
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Msg::Shutdown(done)) => {
                    for v in &mut self.vols {
                        if let Some(c) = v.scan_cancel.take() {
                            c.store(true, Ordering::SeqCst);
                        }
                        v.watcher = None;
                        v.journal = None;
                    }
                    self.save_dirty();
                    let _ = done.send(());
                    return;
                }
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Drain whatever else is waiting before doing periodic work.
            while let Ok(msg) = rx.try_recv() {
                if let Msg::Shutdown(done) = msg {
                    self.save_dirty();
                    let _ = done.send(());
                    return;
                }
                self.handle(msg);
            }
            self.tick();
        }
    }

    fn vol_pos(&self, root: &str) -> Option<usize> {
        self.vols.iter().position(|v| same_root(&v.spec.root, root))
    }

    fn store_pos(index: &Index, root: &str) -> Option<usize> {
        index
            .volumes
            .iter()
            .position(|v| same_root(&v.meta.root, root))
    }

    fn add_volume(&mut self, spec: RootSpec) {
        let desired = self.desired_kind(&spec);
        let root = spec.root.clone();
        self.vols.push(Vol {
            spec,
            gen: 1,
            state: VolState::Loading,
            desired,
            scan_cancel: None,
            pending: Vec::new(),
            journal: None,
            watcher: None,
            dirty: false,
            last_update: 0,
            last_scan: Instant::now(),
            from_disk: false,
            note: String::new(),
            rescan_wanted: false,
        });
        let (tx, file) = (self.tx.clone(), self.file_for(&root));
        std::thread::spawn(move || {
            let store = file.and_then(|f| VolumeStore::load(&f).ok()).map(Box::new);
            let _ = tx.send(Msg::Loaded {
                root,
                gen: 1,
                store,
            });
        });
    }

    fn remove_volume(&mut self, root: &str) {
        if let Some(i) = self.vol_pos(root) {
            let v = self.vols.remove(i);
            if let Some(c) = v.scan_cancel {
                c.store(true, Ordering::SeqCst);
            }
        }
        {
            let mut idx = self.index.write().unwrap();
            if let Some(i) = Self::store_pos(&idx, root) {
                idx.volumes.remove(i);
                idx.epoch += 1;
                self.shared.epoch.store(idx.epoch, Ordering::Relaxed);
            }
        }
        if let Some(f) = self.file_for(root) {
            let _ = std::fs::remove_file(f);
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Shutdown(_) => {}
            Msg::Configure(config) => self.configure(config),
            Msg::Rescan(root) => {
                let targets: Vec<usize> = match root {
                    Some(r) => self.vol_pos(&r).into_iter().collect(),
                    None => (0..self.vols.len()).collect(),
                };
                for i in targets {
                    // A manual rescan retries the fast method too.
                    self.vols[i].desired = self.desired_kind(&self.vols[i].spec.clone());
                    self.vols[i].note.clear();
                    self.start_scan(i);
                }
            }
            Msg::Save => self.save_dirty(),
            Msg::Removed(path) => self.removed(&path),
            Msg::Loaded { root, gen, store } => self.loaded(&root, gen, store),
            Msg::Progress { root, gen, found } => {
                if let Some(i) = self.vol_pos(&root) {
                    let v = &mut self.vols[i];
                    if v.gen == gen {
                        if let VolState::Scanning { found: f } = &mut v.state {
                            *f = found;
                        }
                    }
                }
            }
            Msg::ScanDone {
                root,
                gen,
                kind,
                result,
                ms,
            } => self.scan_done(&root, gen, kind, result, ms),
            Msg::Watch { root, event } => self.watch_event(&root, event),
        }
        self.publish();
    }

    fn configure(&mut self, config: EngineConfig) {
        let excludes_changed = config.excludes != self.config.excludes;
        let elevated_changed = config.elevated != self.config.elevated;
        self.config = config;
        let gone: Vec<String> = self
            .vols
            .iter()
            .filter(|v| {
                !self
                    .config
                    .roots
                    .iter()
                    .any(|r| same_root(&r.root, &v.spec.root))
            })
            .map(|v| v.spec.root.clone())
            .collect();
        for root in gone {
            self.remove_volume(&root);
        }
        for spec in self.config.roots.clone() {
            match self.vol_pos(&spec.root) {
                None => self.add_volume(spec),
                Some(i) => {
                    self.vols[i].spec = spec;
                    if excludes_changed || elevated_changed {
                        self.vols[i].desired = self.desired_kind(&self.vols[i].spec.clone());
                        self.start_scan(i);
                    }
                }
            }
        }
    }

    fn install(&mut self, i: usize, mut store: VolumeStore) {
        store.excludes = self.config.excludes.clone();
        let mut idx = self.index.write().unwrap();
        match Self::store_pos(&idx, &self.vols[i].spec.root) {
            Some(p) => idx.volumes[p] = store,
            None => idx.volumes.push(store),
        }
        idx.epoch += 1;
        self.shared.epoch.store(idx.epoch, Ordering::Relaxed);
        drop(idx);
        self.vols[i].state = VolState::Ready;
    }

    fn loaded(&mut self, root: &str, gen: u64, store: Option<Box<VolumeStore>>) {
        let Some(i) = self.vol_pos(root) else { return };
        if self.vols[i].gen != gen {
            return;
        }
        let spec = self.vols[i].spec.clone();
        let usable =
            store.filter(|s| same_root(&s.meta.root, &spec.root) && s.meta.serial == spec.serial);
        let Some(store) = usable else {
            self.start_scan(i);
            return;
        };
        let (kind, journal_id, next_usn) =
            (store.meta.kind, store.meta.journal_id, store.meta.next_usn);
        self.vols[i].from_disk = true;
        self.vols[i].last_update = store.meta.scanned_unix;
        self.install(i, *store);
        if self.vols[i].desired == VolumeKind::NtfsMft && kind == VolumeKind::NtfsMft {
            // Continue from where the saved index stopped, if the journal still
            // has those records.
            let resumed = Journal::open(&spec.root).and_then(|j| {
                let info = j.info()?;
                if info.journal_id == journal_id
                    && next_usn >= info.first_usn
                    && next_usn <= info.next_usn
                {
                    Ok(j)
                } else {
                    Err("The change journal no longer covers the saved index.".into())
                }
            });
            match resumed {
                Ok(j) => {
                    self.vols[i].journal = Some(j);
                }
                Err(_) if self.config.refresh_loaded => self.start_scan(i),
                Err(_) => {}
            }
        } else if self.config.refresh_loaded {
            // Show the saved index at once and refresh it behind the scenes.
            self.start_scan(i);
        } else {
            self.vols[i].last_scan = Instant::now();
        }
    }

    fn start_scan(&mut self, i: usize) {
        if let Some(c) = self.vols[i].scan_cancel.take() {
            c.store(true, Ordering::SeqCst);
        }
        let has_store = {
            let idx = self.index.read().unwrap();
            Self::store_pos(&idx, &self.vols[i].spec.root).is_some()
        };
        let kind = self.vols[i].desired;
        let root = self.vols[i].spec.root.clone();
        // Start listening before the walk so nothing that happens during it is lost.
        if kind == VolumeKind::NtfsWalk && self.vols[i].watcher.is_none() {
            let (tx, r) = (self.tx.clone(), root.clone());
            let started = Watcher::start(
                &root,
                Box::new(move |event| {
                    let _ = tx.send(Msg::Watch {
                        root: r.clone(),
                        event,
                    });
                }),
            );
            self.vols[i].watcher = started.ok();
        }
        if kind != VolumeKind::NtfsWalk {
            self.vols[i].watcher = None;
        }
        let v = &mut self.vols[i];
        v.gen += 1;
        v.pending.clear();
        v.rescan_wanted = false;
        v.journal = None;
        v.last_scan = Instant::now();
        if !has_store {
            v.state = VolState::Scanning { found: 0 };
        }
        let cancel = Arc::new(AtomicBool::new(false));
        v.scan_cancel = Some(cancel.clone());
        let (tx, gen, excludes) = (self.tx.clone(), v.gen, self.config.excludes.clone());
        std::thread::Builder::new()
            .name("findforge-scan".into())
            .spawn(move || {
                let started = Instant::now();
                let last = Mutex::new(Instant::now());
                let progress = |found: u64| {
                    let mut l = last.lock().unwrap();
                    if l.elapsed() > Duration::from_millis(150) {
                        *l = Instant::now();
                        let _ = tx.send(Msg::Progress {
                            root: root.clone(),
                            gen,
                            found,
                        });
                    }
                };
                let result = match kind {
                    VolumeKind::NtfsMft => ntfs::scan_volume(&root, &excludes, &progress, &cancel),
                    VolumeKind::NtfsWalk => {
                        walk::walk_volume(&root, true, &excludes, &progress, &cancel)
                    }
                    VolumeKind::Walk => {
                        walk::walk_volume(&root, false, &excludes, &progress, &cancel)
                    }
                };
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                let _ = tx.send(Msg::ScanDone {
                    root: root.clone(),
                    gen,
                    kind,
                    result: result.map(Box::new),
                    ms: started.elapsed().as_millis().min(u32::MAX as u128) as u32,
                });
            })
            .expect("spawn scan thread");
    }

    fn scan_done(
        &mut self,
        root: &str,
        gen: u64,
        kind: VolumeKind,
        result: Result<Box<VolumeStore>, String>,
        ms: u32,
    ) {
        let Some(i) = self.vol_pos(root) else { return };
        if self.vols[i].gen != gen {
            return;
        }
        self.vols[i].scan_cancel = None;
        self.vols[i].last_scan = Instant::now();
        match result {
            Err(e) => {
                if kind == VolumeKind::NtfsMft {
                    // No rights (or an unusual volume): fall back to the walk.
                    self.vols[i].desired = VolumeKind::NtfsWalk;
                    self.vols[i].note = format!(
                        "Fast indexing was not possible ({e}) Using the folder walk instead."
                    );
                    self.start_scan(i);
                    return;
                }
                let has_store = {
                    let idx = self.index.read().unwrap();
                    Self::store_pos(&idx, root).is_some()
                };
                if has_store {
                    self.vols[i].note = format!("The last rescan failed: {e}");
                } else {
                    self.vols[i].state = VolState::Failed(e);
                }
            }
            Ok(store) => {
                let mut store = *store;
                let spec = self.vols[i].spec.clone();
                store.meta.label = spec.label.clone();
                store.meta.fs = spec.fs.clone();
                store.meta.serial = spec.serial;
                store.meta.scanned_unix = sys::now_unix();
                store.meta.scan_ms = ms;
                store.excludes = self.config.excludes.clone();
                let pending = std::mem::take(&mut self.vols[i].pending);
                for c in &pending {
                    store.apply(c);
                }
                if store.needs_sweep() {
                    store.sweep_orphans();
                }
                self.install(i, store);
                let v = &mut self.vols[i];
                v.from_disk = false;
                v.dirty = true;
                v.last_update = sys::now_unix();
                if kind == VolumeKind::NtfsMft {
                    match Journal::open(root) {
                        Ok(j) => v.journal = Some(j),
                        Err(e) => v.note = format!("Live updates are off: {e}"),
                    }
                }
                self.save_volume(i);
            }
        }
    }

    /// Applies changes to the store of `root`. Returns whether anything changed.
    fn apply_changes(&mut self, i: usize, changes: &[Change]) -> bool {
        self.apply_changes_noting_new_folders(i, changes, None)
    }

    /// Like `apply_changes`; the paths of folders that were newly created are
    /// appended to `new_folders`.
    fn apply_changes_noting_new_folders(
        &mut self,
        i: usize,
        changes: &[Change],
        mut new_folders: Option<&mut Vec<String>>,
    ) -> bool {
        if changes.is_empty() {
            return false;
        }
        let root = self.vols[i].spec.root.clone();
        let mut changed = false;
        {
            let mut idx = self.index.write().unwrap();
            if let Some(p) = Self::store_pos(&idx, &root) {
                let store = &mut idx.volumes[p];
                for c in changes {
                    match store.apply(c) {
                        Applied::Ignored => {}
                        Applied::Created(slot) => {
                            changed = true;
                            if let Some(list) = new_folders.as_deref_mut() {
                                if store.entry(slot).map(|e| e.is_dir()).unwrap_or(false) {
                                    list.push(store.path(slot));
                                }
                            }
                        }
                        _ => changed = true,
                    }
                }
                if store.needs_sweep() {
                    store.sweep_orphans();
                }
                if changed {
                    idx.epoch += 1;
                    self.shared.epoch.store(idx.epoch, Ordering::Relaxed);
                }
            }
        }
        if self.vols[i].scan_cancel.is_some() {
            self.vols[i].pending.extend_from_slice(changes);
        }
        if changed {
            self.vols[i].dirty = true;
            self.vols[i].last_update = sys::now_unix();
            (self.shared.wake)();
        }
        changed
    }

    fn watch_event(&mut self, root: &str, event: WatchEvent) {
        let Some(i) = self.vol_pos(root) else { return };
        match event {
            WatchEvent::Changes(changes) => {
                let mut new_folders = Vec::new();
                self.apply_changes_noting_new_folders(i, &changes, Some(&mut new_folders));
                // A folder moved in from outside the watched tree arrives as a
                // single "added" notice: list what it brought along.
                for folder in new_folders.iter().take(64) {
                    let inside = walk::tree_changes(folder, 200_000);
                    self.apply_changes(i, &inside);
                }
            }
            WatchEvent::Overflow => self.vols[i].rescan_wanted = true,
            WatchEvent::Closed(why) => {
                self.vols[i].watcher = None;
                self.vols[i].note = format!("Live updates stopped: {why}");
            }
        }
    }

    /// The program itself removed or renamed `path`: drop it from the index
    /// of volumes that would otherwise only notice at the next rescan.
    fn removed(&mut self, path: &str) {
        let mut idx = self.index.write().unwrap();
        let mut hit = None;
        for (p, store) in idx.volumes.iter().enumerate() {
            if store.meta.kind == VolumeKind::Walk {
                if let Some(slot) = store.resolve_path(path) {
                    hit = Some((p, slot));
                    break;
                }
            }
        }
        if let Some((p, slot)) = hit {
            if slot != 0 {
                idx.volumes[p].remove_slot(slot);
                idx.epoch += 1;
                self.shared.epoch.store(idx.epoch, Ordering::Relaxed);
                drop(idx);
                (self.shared.wake)();
            }
        }
    }

    fn tick(&mut self) {
        let mut publish = false;
        for i in 0..self.vols.len() {
            // Change journal.
            if self.vols[i].journal.is_some() && self.vols[i].scan_cancel.is_none() {
                publish |= self.poll_journal(i);
            }
            let v = &self.vols[i];
            if v.scan_cancel.is_some() || matches!(v.state, VolState::Loading) {
                continue;
            }
            let live = v.journal.is_some() || v.watcher.is_some();
            let due = !live
                && v.last_scan.elapsed()
                    > Duration::from_secs(self.config.rescan_minutes.max(1) as u64 * 60);
            if v.rescan_wanted || (due && !matches!(v.state, VolState::Failed(_))) {
                self.start_scan(i);
                publish = true;
            }
        }
        if self.last_save.elapsed() > AUTOSAVE {
            self.save_dirty();
            publish = true;
        }
        if publish {
            self.publish();
        }
    }

    fn poll_journal(&mut self, i: usize) -> bool {
        let root = self.vols[i].spec.root.clone();
        let (journal_id, from) = {
            let idx = self.index.read().unwrap();
            match Self::store_pos(&idx, &root) {
                Some(p) => (idx.volumes[p].meta.journal_id, idx.volumes[p].meta.next_usn),
                None => return false,
            }
        };
        let mut events = Vec::new();
        let read = self.vols[i]
            .journal
            .as_mut()
            .map(|j| j.read(journal_id, from, &mut events));
        let next = match read {
            Some(Ok(next)) => next,
            Some(Err(_)) => {
                // The journal was recreated or wrapped: start over.
                self.vols[i].journal = None;
                self.start_scan(i);
                return true;
            }
            None => return false,
        };
        if next == from && events.is_empty() {
            return false;
        }
        let (changes, stat_frns) = ntfs::changes_from_events(&events);
        let changed = self.apply_changes(i, &changes);
        // Sizes and times are not in the journal: look them up.
        let mut targets: Vec<(u64, String)> = Vec::new();
        {
            let idx = self.index.read().unwrap();
            if let Some(p) = Self::store_pos(&idx, &root) {
                let store = &idx.volumes[p];
                for frn in stat_frns {
                    if let Some(slot) = store.slot_of(frn) {
                        targets.push((frn, store.path(slot)));
                    }
                }
            }
        }
        let metas: Vec<Change> = targets
            .into_iter()
            .filter_map(|(frn, path)| walk::stat(&path).map(|(_, meta)| Change::Meta { frn, meta }))
            .collect();
        let changed = self.apply_changes(i, &metas) || changed;
        {
            let mut idx = self.index.write().unwrap();
            if let Some(p) = Self::store_pos(&idx, &root) {
                idx.volumes[p].meta.next_usn = next;
            }
        }
        if !changed && !events.is_empty() {
            // The position moved: worth saving eventually, nothing to redraw.
            self.vols[i].dirty = true;
        }
        changed
    }

    fn save_volume(&mut self, i: usize) {
        let root = self.vols[i].spec.root.clone();
        let Some(file) = self.file_for(&root) else {
            self.vols[i].dirty = false;
            return;
        };
        // Tidy up first if a lot of garbage has piled up.
        let needs_compact = {
            let idx = self.index.read().unwrap();
            Self::store_pos(&idx, &root)
                .map(|p| idx.volumes[p].garbage_ratio() > 0.2)
                .unwrap_or(false)
        };
        if needs_compact {
            let mut idx = self.index.write().unwrap();
            if let Some(p) = Self::store_pos(&idx, &root) {
                idx.volumes[p].compact();
                idx.epoch += 1;
                self.shared.epoch.store(idx.epoch, Ordering::Relaxed);
            }
            drop(idx);
            (self.shared.wake)();
        }
        let idx = self.index.read().unwrap();
        if let Some(p) = Self::store_pos(&idx, &root) {
            match idx.volumes[p].save(&file) {
                Ok(()) => {
                    drop(idx);
                    self.vols[i].dirty = false;
                    self.last_save_unix = sys::now_unix();
                }
                Err(e) => {
                    drop(idx);
                    self.vols[i].note = e;
                }
            }
        }
    }

    fn save_dirty(&mut self) {
        self.last_save = Instant::now();
        for i in 0..self.vols.len() {
            if self.vols[i].dirty && self.vols[i].scan_cancel.is_none() {
                self.save_volume(i);
            }
        }
    }

    fn publish(&self) {
        let idx = self.index.read().unwrap();
        let mut status = EngineStatus {
            elevated: self.config.elevated,
            last_save_unix: self.last_save_unix,
            ..Default::default()
        };
        for v in &self.vols {
            let store = Self::store_pos(&idx, &v.spec.root).map(|p| &idx.volumes[p]);
            let entries = store.map(|s| s.live_count().saturating_sub(1)).unwrap_or(0);
            let memory = store.map(|s| s.memory_bytes()).unwrap_or(0);
            status.total_entries += entries;
            status.memory_bytes += memory;
            status.busy |= matches!(v.state, VolState::Loading | VolState::Scanning { .. });
            status.volumes.push(VolStatus {
                root: v.spec.root.clone(),
                label: v.spec.label.clone(),
                fs: v.spec.fs.clone(),
                kind: store.map(|s| s.meta.kind).unwrap_or(v.desired),
                state: v.state.clone(),
                refreshing: v.scan_cancel.is_some() && store.is_some(),
                entries,
                memory_bytes: memory,
                live: v.journal.is_some() || v.watcher.is_some(),
                last_update_unix: v.last_update,
                scanned_unix: store.map(|s| s.meta.scanned_unix).unwrap_or(0),
                scan_ms: store.map(|s| s.meta.scan_ms).unwrap_or(0),
                from_disk: v.from_disk,
                note: v.note.clone(),
            });
        }
        drop(idx);
        *self.shared.status.lock().unwrap() = status;
        (self.shared.wake)();
    }
}

/// Loads every saved index found in `dir` (command line searches).
pub fn load_saved(dir: &Path) -> Index {
    let mut index = Index::default();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return index;
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "ffi").unwrap_or(false))
        .collect();
    files.sort();
    for f in files {
        if let Ok(store) = VolumeStore::load(&f) {
            index.volumes.push(store);
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{attr, Meta, VolumeBuilder, VolumeMeta};

    fn tiny_index() -> Index {
        let mut b = VolumeBuilder::new(VolumeMeta::new("C:", VolumeKind::NtfsWalk));
        b.set_root_frn(5);
        b.push(10, 5, "Photos", attr::DIRECTORY, Meta::default());
        b.push(
            11,
            10,
            "beach.jpg",
            0,
            Meta {
                size: 100,
                mtime: 5,
                ctime: 4,
            },
        );
        b.push(
            12,
            10,
            "city.jpg",
            0,
            Meta {
                size: 200,
                mtime: 6,
                ctime: 4,
            },
        );
        Index {
            volumes: vec![b.finish(&Excludes::default(), true)],
            epoch: 7,
        }
    }

    fn request(id: u64, text: &str) -> SearchRequest {
        SearchRequest {
            id,
            text: text.into(),
            filter: None,
            options: SearchOptions::default(),
            sort: SortKey::Name,
            ascending: true,
        }
    }

    #[test]
    fn search_thread_answers_and_rows_resolve() {
        let woken = Arc::new(AtomicU64::new(0));
        let w = woken.clone();
        let engine = Engine::with_index(
            tiny_index(),
            Box::new(move || {
                w.fetch_add(1, Ordering::SeqCst);
            }),
        );
        engine.submit(request(1, "jpg"));
        let started = Instant::now();
        let results = loop {
            if let Some(r) = engine.poll_results() {
                break r;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "no results");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(results.id, 1);
        assert_eq!(results.epoch, 7);
        assert_eq!(results.hits.len(), 2);
        assert_eq!(results.total_bytes, 300);
        assert!(woken.load(Ordering::SeqCst) >= 1);
        let rows = engine.rows(&results.hits);
        assert_eq!(rows[0].name, "beach.jpg");
        assert_eq!(rows[0].dir, "C:\\Photos");
        assert_eq!(rows[0].full_path(), "C:\\Photos\\beach.jpg");
        assert_eq!(rows[0].ext, "jpg");
        assert!(rows[0].exists && !rows[0].is_dir);
        assert_eq!(
            engine.paths(&results.hits),
            vec!["C:\\Photos\\beach.jpg", "C:\\Photos\\city.jpg"]
        );
        // A stale hit resolves to a row that does not exist.
        let gone = engine.rows(&[Hit { vol: 0, slot: 9999 }]);
        assert!(!gone[0].exists);
        engine.shutdown();
    }

    #[test]
    fn file_names_are_safe_and_distinct() {
        let a = index_file_name("C:");
        let b = index_file_name("D:");
        let c = index_file_name("\\\\server\\share\\Folder With Spaces");
        assert_ne!(a, b);
        for n in [&a, &b, &c] {
            assert!(n.ends_with(".ffi"));
            assert!(n
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch)));
        }
        assert_eq!(index_file_name("c:"), index_file_name("c:"));
    }

    #[test]
    fn row_paths() {
        let r = Row {
            name: "a.txt".into(),
            dir: "C:\\".into(),
            ..Default::default()
        };
        assert_eq!(r.full_path(), "C:\\a.txt");
    }
}
