//! The in-memory index: one compact store per volume (or indexed folder).
//!
//! Every file and folder is a 32-byte [`Entry`] that points at its name in a
//! shared UTF-8 pool and at its parent by slot number, so a full path is
//! rebuilt by walking up the parents. On NTFS the file reference number of
//! each entry is mapped to its slot, which is what makes live updates from
//! the change journal (or from directory change notifications) cheap.

use crate::util;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

/// "No slot": the parent of the root, and empty cells of the id map.
pub const NONE: u32 = u32::MAX;
/// "No file id" for [`VolumeBuilder::push`].
pub const NO_FRN: u64 = u64::MAX;
/// Extension id used when the extension table is full.
pub const EXT_OTHER: u16 = u16::MAX;

/// Bits of [`Entry::flags`].
pub mod flags {
    pub const DIR: u16 = 1;
    pub const HIDDEN: u16 = 2;
    pub const SYSTEM: u16 = 4;
    /// A free slot; such entries never match and have an empty name.
    pub const DELETED: u16 = 8;
    /// The root of the volume (slot 0); its name is the drive or folder path.
    pub const ROOT: u16 = 16;
    pub const READONLY: u16 = 32;
    pub const REPARSE: u16 = 64;
}

/// Win32 file attribute bits used when converting to [`flags`].
pub mod attr {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const REPARSE_POINT: u32 = 0x400;
}

/// Converts Win32 file attributes to entry flags.
pub fn flags_from_attrs(attrs: u32) -> u16 {
    let mut f = 0;
    if attrs & attr::DIRECTORY != 0 {
        f |= flags::DIR;
    }
    if attrs & attr::HIDDEN != 0 {
        f |= flags::HIDDEN;
    }
    if attrs & attr::SYSTEM != 0 {
        f |= flags::SYSTEM;
    }
    if attrs & attr::READONLY != 0 {
        f |= flags::READONLY;
    }
    if attrs & attr::REPARSE_POINT != 0 {
        f |= flags::REPARSE;
    }
    f
}

/// Converts a Windows FILETIME (100 ns ticks since 1601) to Unix seconds,
/// clamped to what fits in 32 bits. 0 means "unknown".
pub fn filetime_to_unix(ft: u64) -> u32 {
    const EPOCH_DIFF: u64 = 11_644_473_600;
    let secs = ft / 10_000_000;
    if secs <= EPOCH_DIFF {
        0
    } else {
        (secs - EPOCH_DIFF).min(u32::MAX as u64) as u32
    }
}

/// One file or folder. 32 bytes, no padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// Size in bytes (0 for folders).
    pub size: u64,
    /// Offset of the name in the volume's name pool.
    pub name_off: u32,
    /// Slot of the parent folder; [`NONE`] for the root.
    pub parent: u32,
    /// Date modified, Unix seconds (UTC); 0 = unknown.
    pub mtime: u32,
    /// Date created, Unix seconds (UTC); 0 = unknown.
    pub ctime: u32,
    /// Length of the name in bytes (UTF-8).
    pub name_len: u16,
    /// See [`flags`].
    pub flags: u16,
    /// Index into the volume's extension table; 0 = no extension.
    pub ext: u16,
    pub spare: u16,
}

const ENTRY_SIZE: usize = std::mem::size_of::<Entry>();
const _: () = assert!(ENTRY_SIZE == 32);

impl Entry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }
    #[inline]
    pub fn is_live(&self) -> bool {
        self.flags & flags::DELETED == 0
    }
    #[inline]
    pub fn is_root(&self) -> bool {
        self.flags & flags::ROOT != 0
    }
}

/// How a volume's index was built and is kept up to date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeKind {
    /// Master file table scan + change journal (needs administrator rights).
    NtfsMft = 0,
    /// Folder walk that records NTFS file ids + change notifications.
    NtfsWalk = 1,
    /// Plain folder walk, rescanned periodically (non-NTFS, network, folders).
    Walk = 2,
}

impl VolumeKind {
    fn from_u8(v: u8) -> Option<VolumeKind> {
        match v {
            0 => Some(VolumeKind::NtfsMft),
            1 => Some(VolumeKind::NtfsWalk),
            2 => Some(VolumeKind::Walk),
            _ => None,
        }
    }
}

/// Facts about a volume that are saved with its index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeMeta {
    /// `C:` for a drive, or a folder path without a trailing backslash.
    pub root: String,
    pub label: String,
    /// File system name, e.g. `NTFS`.
    pub fs: String,
    pub serial: u32,
    pub kind: VolumeKind,
    /// Change journal id and the next record to read (NtfsMft only).
    pub journal_id: u64,
    pub next_usn: i64,
    /// When the last full scan finished (Unix seconds) and how long it took.
    pub scanned_unix: u64,
    pub scan_ms: u32,
}

impl VolumeMeta {
    pub fn new(root: &str, kind: VolumeKind) -> VolumeMeta {
        VolumeMeta {
            root: root.trim_end_matches('\\').to_string(),
            label: String::new(),
            fs: String::new(),
            serial: 0,
            kind,
            journal_id: 0,
            next_usn: 0,
            scanned_unix: 0,
            scan_ms: 0,
        }
    }
}

/// Folders and name patterns that are left out of the index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Excludes {
    /// Full folder paths, lower case, no trailing backslash.
    pub folders: Vec<String>,
    /// Wildcard patterns matched against names, lower case (`*.tmp`, `node_modules`).
    pub names: Vec<String>,
}

impl Excludes {
    pub fn new(folders: &[String], names: &[String]) -> Excludes {
        Excludes {
            folders: folders
                .iter()
                .map(|f| f.trim().trim_end_matches('\\').to_lowercase())
                .filter(|f| !f.is_empty())
                .collect(),
            names: names
                .iter()
                .map(|n| n.trim().to_lowercase())
                .filter(|n| !n.is_empty())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty() && self.names.is_empty()
    }

    /// Does a name match one of the name patterns?
    pub fn name_excluded(&self, name: &str) -> bool {
        if self.names.is_empty() {
            return false;
        }
        let lower = name.to_lowercase();
        self.names.iter().any(|p| util::wild_match(p, &lower))
    }

    /// Is this full path one of the excluded folders (exactly)?
    pub fn folder_excluded(&self, path: &str) -> bool {
        if self.folders.is_empty() {
            return false;
        }
        let lower = path.trim_end_matches('\\').to_lowercase();
        self.folders.contains(&lower)
    }
}

/// Size and times of a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    pub size: u64,
    pub mtime: u32,
    pub ctime: u32,
}

/// One change to apply to a volume that tracks file ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// After this change the file `frn` exists with this parent, name and
    /// attributes: a create, a rename, a move or an attribute change.
    /// `meta: None` keeps the known size and times (zero for a new entry).
    Upsert {
        frn: u64,
        parent_frn: u64,
        name: String,
        attrs: u32,
        meta: Option<Meta>,
    },
    /// The file is gone.
    Delete { frn: u64 },
    /// The file at this full path is gone (for sources that cannot name the
    /// id of a deleted file). Slower than `Delete`: the path is resolved by
    /// scanning.
    DeletePath { path: String },
    /// Only the size or times changed.
    Meta { frn: u64, meta: Meta },
}

/// What [`VolumeStore::apply`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Created(u32),
    /// The name or the parent changed.
    Renamed(u32),
    /// Attributes, size or times changed.
    Updated(u32),
    Deleted(u32),
    /// Unknown file, unknown parent, excluded, or nothing to do.
    Ignored,
}

/// The index of one volume or folder.
#[derive(Clone, Debug)]
pub struct VolumeStore {
    pub meta: VolumeMeta,
    /// Not saved; set by the engine after loading.
    pub excludes: Excludes,
    entries: Vec<Entry>,
    names: Vec<u8>,
    /// MFT record number -> slot.
    frn_map: IdMap,
    free: Vec<u32>,
    /// Slots of deleted folders: reusable only after [`VolumeStore::sweep_orphans`].
    pending_free: Vec<u32>,
    exts: Vec<Box<str>>,
    ext_lookup: HashMap<Box<str>, u16>,
    live: usize,
    /// Bytes of the name pool that no entry uses any more.
    wasted: usize,
}

/// File id (record number) -> slot. Dense for whole volumes, where record
/// numbers are packed; sparse for an indexed folder, whose few files have
/// record numbers scattered over the whole volume.
#[derive(Clone, Debug)]
enum IdMap {
    Dense(Vec<u32>),
    Sparse(HashMap<u32, u32>),
}

impl Default for IdMap {
    fn default() -> Self {
        IdMap::Dense(Vec::new())
    }
}

impl IdMap {
    #[inline]
    fn get(&self, rec: usize) -> u32 {
        match self {
            IdMap::Dense(v) => v.get(rec).copied().unwrap_or(NONE),
            IdMap::Sparse(m) => m.get(&(rec as u32)).copied().unwrap_or(NONE),
        }
    }

    fn set(&mut self, rec: usize, slot: u32) {
        match self {
            IdMap::Dense(v) => {
                if rec >= v.len() {
                    if slot == NONE {
                        return;
                    }
                    let new_len = (rec + 1).max(v.len() + v.len() / 4);
                    v.resize(new_len, NONE);
                }
                v[rec] = slot;
            }
            IdMap::Sparse(m) => {
                if slot == NONE {
                    m.remove(&(rec as u32));
                } else {
                    m.insert(rec as u32, slot);
                }
            }
        }
    }

    /// Replaces every stored slot with `f(slot)`; `NONE` removes the cell.
    fn update(&mut self, f: impl Fn(u32) -> u32) {
        match self {
            IdMap::Dense(v) => {
                for cell in v.iter_mut() {
                    if *cell != NONE {
                        *cell = f(*cell);
                    }
                }
            }
            IdMap::Sparse(m) => {
                m.retain(|_, slot| {
                    *slot = f(*slot);
                    *slot != NONE
                });
            }
        }
    }

    fn forget_slot(&mut self, slot: u32) {
        self.update(|s| if s == slot { NONE } else { s });
    }

    fn max_slot(&self) -> Option<u32> {
        match self {
            IdMap::Dense(v) => v.iter().copied().filter(|s| *s != NONE).max(),
            IdMap::Sparse(m) => m.values().copied().max(),
        }
    }

    fn memory_bytes(&self) -> usize {
        match self {
            IdMap::Dense(v) => v.capacity() * 4,
            // Key + value + control byte per bucket, roughly.
            IdMap::Sparse(m) => m.capacity() * 9,
        }
    }

    /// Picks the smaller representation for `live` entries and trims it.
    fn settle(&mut self, live: usize) {
        if let IdMap::Dense(v) = self {
            while v.last() == Some(&NONE) {
                v.pop();
            }
            if v.len() > live.saturating_mul(4) + 4096 {
                let m: HashMap<u32, u32> = v
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| **s != NONE)
                    .map(|(r, s)| (r as u32, *s))
                    .collect();
                *self = IdMap::Sparse(m);
                return;
            }
            v.shrink_to_fit();
        }
    }

    /// For saving: `(is_sparse, cells)`; sparse cells are (record, slot) pairs.
    fn to_cells(&self) -> (bool, std::borrow::Cow<'_, [u32]>) {
        match self {
            IdMap::Dense(v) => (false, std::borrow::Cow::Borrowed(v.as_slice())),
            IdMap::Sparse(m) => {
                let mut pairs: Vec<(u32, u32)> = m.iter().map(|(k, v)| (*k, *v)).collect();
                pairs.sort_unstable();
                (
                    true,
                    std::borrow::Cow::Owned(pairs.into_iter().flat_map(|(k, v)| [k, v]).collect()),
                )
            }
        }
    }

    fn from_cells(sparse: bool, cells: Vec<u32>) -> Option<IdMap> {
        if !sparse {
            return Some(IdMap::Dense(cells));
        }
        if !cells.len().is_multiple_of(2) {
            return None;
        }
        let mut m = HashMap::with_capacity(cells.len() / 2);
        for pair in cells.as_chunks::<2>().0 {
            if pair[1] == NONE || m.insert(pair[0], pair[1]).is_some() {
                return None;
            }
        }
        Some(IdMap::Sparse(m))
    }
}

/// The record-number part of a file reference number, if it is usable as a map index.
#[inline]
fn rec(frn: u64) -> Option<usize> {
    let r = frn & 0x0000_FFFF_FFFF_FFFF;
    if frn == NO_FRN || r > 0x7FFF_FFFF {
        None
    } else {
        Some(r as usize)
    }
}

impl VolumeStore {
    fn empty(meta: VolumeMeta) -> VolumeStore {
        let mut s = VolumeStore {
            meta,
            excludes: Excludes::default(),
            entries: Vec::new(),
            names: Vec::new(),
            frn_map: IdMap::default(),
            free: Vec::new(),
            pending_free: Vec::new(),
            exts: vec!["".into()],
            ext_lookup: HashMap::new(),
            live: 0,
            wasted: 0,
        };
        let root = s.meta.root.clone();
        s.push_raw(&root, NONE, flags::DIR | flags::ROOT, Meta::default());
        s
    }

    // ---------------------------------------------------------------------------------
    // Reading
    // ---------------------------------------------------------------------------------

    /// All slots, including free ones (check [`Entry::is_live`]).
    #[inline]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The whole name pool; an entry's name is `pool[name_off..name_off + name_len]`.
    #[inline]
    pub fn name_pool(&self) -> &[u8] {
        &self.names
    }

    #[inline]
    pub fn entry(&self, slot: u32) -> Option<&Entry> {
        self.entries.get(slot as usize)
    }

    #[inline]
    pub fn name_bytes(&self, e: &Entry) -> &[u8] {
        let start = e.name_off as usize;
        &self.names[start..start + e.name_len as usize]
    }

    #[inline]
    pub fn name_of(&self, e: &Entry) -> &str {
        // SAFETY: names are only ever written from `&str` (push_raw / set_name)
        // and loaded files are validated in `load`.
        unsafe { std::str::from_utf8_unchecked(self.name_bytes(e)) }
    }

    /// The name of a slot ("" for a free or unknown slot).
    pub fn name(&self, slot: u32) -> &str {
        match self.entries.get(slot as usize) {
            Some(e) => self.name_of(e),
            None => "",
        }
    }

    /// Number of live entries, including the root.
    pub fn live_count(&self) -> usize {
        self.live
    }

    /// The extension table (lower case, without the dot); index 0 is "no extension".
    pub fn exts(&self) -> &[Box<str>] {
        &self.exts
    }

    /// The id of a lower-case extension, if any file on this volume has it.
    pub fn ext_id(&self, ext_lower: &str) -> Option<u16> {
        if ext_lower.is_empty() {
            return Some(0);
        }
        self.ext_lookup.get(ext_lower).copied()
    }

    /// The lower-case extension of an entry ("" when it has none).
    pub fn ext_of<'a>(&'a self, e: &'a Entry) -> &'a str {
        if e.ext == EXT_OTHER {
            return util::extension(self.name_of(e));
        }
        self.exts.get(e.ext as usize).map(|s| &**s).unwrap_or("")
    }

    /// Slot of a file reference number.
    pub fn slot_of(&self, frn: u64) -> Option<u32> {
        let slot = self.frn_map.get(rec(frn)?);
        (slot != NONE).then_some(slot)
    }

    /// Appends the full path of `slot` to `out` (`C:\Folder\file.txt`; `C:\` for a drive root).
    pub fn path_into(&self, slot: u32, out: &mut String) {
        let mut chain = [0u32; 256];
        let mut n = 0;
        let mut cur = slot;
        while cur != NONE && n < chain.len() {
            let Some(e) = self.entries.get(cur as usize) else {
                break;
            };
            chain[n] = cur;
            n += 1;
            cur = e.parent;
        }
        for (i, s) in chain[..n].iter().rev().enumerate() {
            if i > 0 {
                out.push('\\');
            }
            out.push_str(self.name(*s));
        }
        if n == 1 && out.ends_with(':') {
            out.push('\\');
        }
    }

    pub fn path(&self, slot: u32) -> String {
        let mut s = String::new();
        self.path_into(slot, &mut s);
        s
    }

    /// Appends the path of the folder that contains `slot`.
    pub fn parent_path_into(&self, slot: u32, out: &mut String) {
        if let Some(e) = self.entries.get(slot as usize) {
            if e.parent != NONE {
                self.path_into(e.parent, out);
            }
        }
    }

    pub fn parent_path(&self, slot: u32) -> String {
        let mut s = String::new();
        self.parent_path_into(slot, &mut s);
        s
    }

    /// Finds a child by name (case-insensitive). Linear in the size of the
    /// volume: fine for the odd lookup, not for bulk work.
    pub fn find_child(&self, parent: u32, name: &str) -> Option<u32> {
        let lower = name.to_lowercase();
        self.entries.iter().enumerate().find_map(|(i, e)| {
            (e.parent == parent
                && e.is_live()
                && e.name_len as usize == name.len()
                && (self.name_of(e) == name || self.name_of(e).to_lowercase() == lower))
                .then_some(i as u32)
        })
    }

    /// Resolves a full path (`C:\a\b`) to a slot. Linear per component.
    pub fn resolve_path(&self, path: &str) -> Option<u32> {
        let root = &self.meta.root;
        let path = path.trim_end_matches('\\');
        if path.len() < root.len() || !path.is_char_boundary(root.len()) {
            return None;
        }
        let (head, rest) = path.split_at(root.len());
        if !head.eq_ignore_ascii_case(root) && head.to_lowercase() != root.to_lowercase() {
            return None;
        }
        if rest.is_empty() {
            return Some(0);
        }
        let rest = rest.strip_prefix('\\')?;
        let mut cur = 0u32;
        for part in rest.split('\\') {
            cur = self.find_child(cur, part)?;
        }
        Some(cur)
    }

    /// Approximate heap use of this store in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.entries.capacity() * ENTRY_SIZE
            + self.names.capacity()
            + self.frn_map.memory_bytes()
            + (self.free.capacity() + self.pending_free.capacity()) * 4
            + self.exts.iter().map(|e| e.len() * 2 + 48).sum::<usize>()
    }

    /// Share of slots and name bytes that are garbage (0.0 - 1.0).
    pub fn garbage_ratio(&self) -> f32 {
        let slots = self.entries.len().max(1) as f32;
        let dead = (self.entries.len() - self.live) as f32 / slots;
        let pool = self.wasted as f32 / self.names.len().max(1) as f32;
        dead.max(pool)
    }

    // ---------------------------------------------------------------------------------
    // Writing
    // ---------------------------------------------------------------------------------

    fn intern_ext(&mut self, name: &str, is_dir: bool) -> u16 {
        if is_dir {
            return 0;
        }
        let ext = util::extension(name);
        if ext.is_empty() || ext.len() > 24 {
            return 0;
        }
        let lower;
        let key: &str = if ext.bytes().any(|b| b.is_ascii_uppercase() || b >= 0x80) {
            lower = ext.to_lowercase();
            &lower
        } else {
            ext
        };
        if let Some(id) = self.ext_lookup.get(key) {
            return *id;
        }
        if self.exts.len() >= EXT_OTHER as usize {
            return EXT_OTHER;
        }
        let id = self.exts.len() as u16;
        self.exts.push(key.into());
        self.ext_lookup.insert(key.into(), id);
        id
    }

    fn append_name(&mut self, name: &str) -> (u32, u16) {
        // Names longer than u16 cannot occur on Windows (255 UTF-16 units),
        // except for the root of an indexed folder, which may be a long path.
        let mut end = name.len().min(u16::MAX as usize);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let off = self.names.len() as u32;
        grow_gently(&mut self.names, end);
        self.names.extend_from_slice(&name.as_bytes()[..end]);
        (off, end as u16)
    }

    fn push_raw(&mut self, name: &str, parent: u32, fl: u16, meta: Meta) -> u32 {
        let (name_off, name_len) = self.append_name(name);
        let ext = self.intern_ext(name, fl & flags::DIR != 0);
        let e = Entry {
            size: meta.size,
            name_off,
            parent,
            mtime: meta.mtime,
            ctime: meta.ctime,
            name_len,
            flags: fl,
            ext,
            spare: 0,
        };
        self.live += 1;
        if let Some(slot) = self.free.pop() {
            self.entries[slot as usize] = e;
            slot
        } else {
            grow_gently(&mut self.entries, 1);
            self.entries.push(e);
            (self.entries.len() - 1) as u32
        }
    }

    fn map_frn(&mut self, frn: u64, slot: u32) {
        if let Some(r) = rec(frn) {
            self.frn_map.set(r, slot);
        }
    }

    fn set_name(&mut self, slot: u32, name: &str) {
        let old_len = self.entries[slot as usize].name_len as usize;
        let is_dir = self.entries[slot as usize].is_dir();
        if name.len() == old_len {
            let off = self.entries[slot as usize].name_off as usize;
            self.names[off..off + old_len].copy_from_slice(name.as_bytes());
        } else {
            self.wasted += old_len;
            let (off, len) = self.append_name(name);
            let e = &mut self.entries[slot as usize];
            e.name_off = off;
            e.name_len = len;
        }
        let ext = self.intern_ext(name, is_dir);
        self.entries[slot as usize].ext = ext;
    }

    fn tombstone(&mut self, slot: u32) {
        let e = &mut self.entries[slot as usize];
        if !e.is_live() {
            return;
        }
        let was_dir = e.is_dir();
        self.wasted += e.name_len as usize;
        e.flags = flags::DELETED;
        e.name_len = 0;
        e.size = 0;
        e.parent = NONE;
        self.live -= 1;
        if was_dir {
            self.pending_free.push(slot);
        } else {
            self.free.push(slot);
        }
    }

    /// Sets size and times of a slot.
    pub fn set_meta(&mut self, slot: u32, meta: Meta) {
        if let Some(e) = self.entries.get_mut(slot as usize) {
            if e.is_live() {
                e.size = if e.is_dir() { 0 } else { meta.size };
                e.mtime = meta.mtime;
                e.ctime = meta.ctime;
            }
        }
    }

    /// Removes one entry (and, for a folder, everything below it) by slot.
    pub fn remove_slot(&mut self, slot: u32) {
        let Some(e) = self.entries.get(slot as usize) else {
            return;
        };
        if slot == 0 || !e.is_live() {
            return;
        }
        let was_dir = e.is_dir();
        self.tombstone(slot);
        self.frn_map.forget_slot(slot);
        if was_dir {
            self.sweep_orphans();
        }
    }

    /// Applies one change. Folder deletions may leave orphans behind until
    /// [`VolumeStore::sweep_orphans`] runs (see [`VolumeStore::needs_sweep`]).
    pub fn apply(&mut self, change: &Change) -> Applied {
        match change {
            Change::Delete { frn } => {
                let Some(slot) = self.slot_of(*frn) else {
                    return Applied::Ignored;
                };
                if slot == 0 {
                    return Applied::Ignored;
                }
                if let Some(r) = rec(*frn) {
                    self.frn_map.set(r, NONE);
                }
                self.tombstone(slot);
                Applied::Deleted(slot)
            }
            Change::DeletePath { path } => match self.resolve_path(path) {
                Some(slot) if slot != 0 => {
                    self.frn_map.forget_slot(slot);
                    self.tombstone(slot);
                    Applied::Deleted(slot)
                }
                _ => Applied::Ignored,
            },
            Change::Meta { frn, meta } => match self.slot_of(*frn) {
                Some(slot) if slot != 0 => {
                    self.set_meta(slot, *meta);
                    Applied::Updated(slot)
                }
                _ => Applied::Ignored,
            },
            Change::Upsert {
                frn,
                parent_frn,
                name,
                attrs,
                meta,
            } => {
                if rec(*frn).is_none() || name.is_empty() {
                    return Applied::Ignored;
                }
                let existing = self.slot_of(*frn);
                if existing == Some(0) {
                    return Applied::Ignored;
                }
                let fl = flags_from_attrs(*attrs);
                let parent = self.slot_of(*parent_frn).filter(|p| {
                    let e = &self.entries[*p as usize];
                    e.is_live() && e.is_dir()
                });
                let excluded = |s: &VolumeStore, parent: u32| {
                    if s.excludes.name_excluded(name) {
                        return true;
                    }
                    if fl & flags::DIR != 0 && !s.excludes.folders.is_empty() {
                        let mut p = s.path(parent);
                        if !p.ends_with('\\') {
                            p.push('\\');
                        }
                        p.push_str(name);
                        return s.excludes.folder_excluded(&p);
                    }
                    false
                };
                let Some(parent) = parent.filter(|p| !excluded(self, *p)) else {
                    // Moved out of the indexed tree (or into an excluded place).
                    return match existing {
                        Some(slot) => {
                            if let Some(r) = rec(*frn) {
                                self.frn_map.set(r, NONE);
                            }
                            self.tombstone(slot);
                            Applied::Deleted(slot)
                        }
                        None => Applied::Ignored,
                    };
                };
                match existing {
                    Some(slot) => {
                        if slot == parent || self.is_ancestor(slot, parent) {
                            // A folder cannot move into itself: ignore nonsense.
                            return Applied::Ignored;
                        }
                        let e = self.entries[slot as usize];
                        let renamed = e.parent != parent || self.name_of(&e) != name;
                        if renamed {
                            self.entries[slot as usize].parent = parent;
                            if self.name_of(&e) != name {
                                self.set_name(slot, name);
                            }
                        }
                        // The kind of an existing id never changes; keep DIR as it was.
                        let keep = e.flags & (flags::DIR | flags::ROOT);
                        let new_flags = (fl & !(flags::DIR | flags::ROOT)) | keep;
                        let changed = new_flags != e.flags || meta.is_some();
                        self.entries[slot as usize].flags = new_flags;
                        if let Some(m) = meta {
                            self.set_meta(slot, *m);
                        }
                        if renamed {
                            Applied::Renamed(slot)
                        } else if changed {
                            Applied::Updated(slot)
                        } else {
                            Applied::Ignored
                        }
                    }
                    None => {
                        let mut m = meta.unwrap_or_default();
                        if fl & flags::DIR != 0 {
                            m.size = 0;
                        }
                        let slot = self.push_raw(name, parent, fl, m);
                        self.map_frn(*frn, slot);
                        Applied::Created(slot)
                    }
                }
            }
        }
    }

    fn is_ancestor(&self, ancestor: u32, mut slot: u32) -> bool {
        for _ in 0..1024 {
            let Some(e) = self.entries.get(slot as usize) else {
                return false;
            };
            if e.parent == NONE {
                return false;
            }
            if e.parent == ancestor {
                return true;
            }
            slot = e.parent;
        }
        false
    }

    /// True when a folder was deleted since the last sweep.
    pub fn needs_sweep(&self) -> bool {
        !self.pending_free.is_empty()
    }

    /// Removes every entry whose chain of parents no longer reaches the root
    /// (children of deleted folders), then releases the folder slots for reuse.
    /// Returns how many entries were removed.
    pub fn sweep_orphans(&mut self) -> usize {
        let removed = self.drop_unreachable(&[]);
        self.free.append(&mut self.pending_free);
        removed
    }

    /// Marks reachability from the root and tombstones everything else.
    /// `banned` slots (excluded folders) count as unreachable.
    fn drop_unreachable(&mut self, banned: &[u32]) -> usize {
        const UNKNOWN: u8 = 0;
        const GOOD: u8 = 1;
        const BAD: u8 = 2;
        let n = self.entries.len();
        let mut state = vec![UNKNOWN; n];
        if n > 0 {
            state[0] = GOOD;
        }
        for b in banned {
            if let Some(s) = state.get_mut(*b as usize) {
                *s = BAD;
            }
        }
        let mut chain: Vec<u32> = Vec::new();
        for i in 0..n {
            if state[i] != UNKNOWN {
                continue;
            }
            chain.clear();
            let mut cur = i as u32;
            let verdict = loop {
                let e = &self.entries[cur as usize];
                if !e.is_live() {
                    break BAD;
                }
                match state[cur as usize] {
                    GOOD => break GOOD,
                    BAD => break BAD,
                    _ => {}
                }
                chain.push(cur);
                if e.parent == NONE || e.parent as usize >= n || chain.len() > 4096 {
                    break BAD;
                }
                let p = &self.entries[e.parent as usize];
                if !p.is_dir() {
                    break BAD;
                }
                cur = e.parent;
            };
            for c in &chain {
                state[*c as usize] = verdict;
            }
        }
        let mut removed = 0;
        for i in 1..n {
            if state[i] == BAD && self.entries[i].is_live() {
                self.tombstone(i as u32);
                removed += 1;
            }
        }
        if removed > 0 {
            let entries = &self.entries;
            self.frn_map.update(|slot| {
                if entries[slot as usize].is_live() {
                    slot
                } else {
                    NONE
                }
            });
        }
        removed
    }

    /// Applies the exclude lists to an already built store (drops matching
    /// folders with everything below them, and files with excluded names).
    pub fn apply_excludes(&mut self, excludes: &Excludes) -> usize {
        self.excludes = excludes.clone();
        if excludes.is_empty() {
            return 0;
        }
        let mut banned: Vec<u32> = Vec::new();
        for folder in &excludes.folders {
            if let Some(slot) = self.resolve_path(folder) {
                if slot != 0 {
                    banned.push(slot);
                }
            }
        }
        if !excludes.names.is_empty() {
            for (i, e) in self.entries.iter().enumerate().skip(1) {
                if e.is_live() && excludes.name_excluded(self.name_of(e)) {
                    banned.push(i as u32);
                }
            }
        }
        if banned.is_empty() {
            return 0;
        }
        let removed = self.drop_unreachable(&banned);
        self.free.append(&mut self.pending_free);
        removed
    }

    /// Rebuilds the store without free slots and unused name bytes. Slot
    /// numbers change, so earlier search results become meaningless.
    pub fn compact(&mut self) {
        self.sweep_orphans();
        let n = self.entries.len();
        let mut remap = vec![NONE; n];
        let mut entries = Vec::with_capacity(self.live);
        let mut names = Vec::with_capacity(self.names.len() - self.wasted.min(self.names.len()));
        for (i, e) in self.entries.iter().enumerate() {
            if !e.is_live() {
                continue;
            }
            remap[i] = entries.len() as u32;
            let mut ne = *e;
            ne.name_off = names.len() as u32;
            names.extend_from_slice(self.name_bytes(e));
            entries.push(ne);
        }
        for e in entries.iter_mut() {
            if e.parent != NONE {
                e.parent = remap[e.parent as usize];
            }
        }
        self.frn_map.update(|slot| remap[slot as usize]);
        self.entries = entries;
        self.names = names;
        self.free = Vec::new();
        self.pending_free = Vec::new();
        self.wasted = 0;
    }

    // ---------------------------------------------------------------------------------
    // Saving and loading
    // ---------------------------------------------------------------------------------

    /// Writes the store to `path` (through a temporary file, so a crash never
    /// leaves half an index behind).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut head: Vec<u8> = Vec::new();
        put_str(&mut head, &self.meta.root);
        put_str(&mut head, &self.meta.label);
        put_str(&mut head, &self.meta.fs);
        head.extend_from_slice(&self.meta.serial.to_le_bytes());
        head.push(self.meta.kind as u8);
        head.extend_from_slice(&self.meta.journal_id.to_le_bytes());
        head.extend_from_slice(&self.meta.next_usn.to_le_bytes());
        head.extend_from_slice(&self.meta.scanned_unix.to_le_bytes());
        head.extend_from_slice(&self.meta.scan_ms.to_le_bytes());
        head.extend_from_slice(&(self.exts.len() as u32).to_le_bytes());
        for e in &self.exts {
            put_str(&mut head, e);
        }
        // Folder slots waiting for a sweep are simply not offered for reuse in
        // the saved image; `load` recounts everything it needs.
        let (map_sparse, map_cells) = self.frn_map.to_cells();
        head.push(map_sparse as u8);
        for len in [
            self.entries.len(),
            self.names.len(),
            map_cells.len(),
            self.free.len(),
        ] {
            head.extend_from_slice(&(len as u64).to_le_bytes());
        }
        // SAFETY: Entry is repr(C), 32 bytes, integers only, no padding.
        let entries = unsafe {
            std::slice::from_raw_parts(
                self.entries.as_ptr() as *const u8,
                self.entries.len() * ENTRY_SIZE,
            )
        };
        let parts: [&[u8]; 5] = [
            &head,
            entries,
            &self.names,
            u32s_as_bytes(&map_cells),
            u32s_as_bytes(&self.free),
        ];
        let mut hash = Hasher::new();
        let mut total = 0u64;
        for p in parts {
            hash.update(p);
            total += p.len() as u64;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("tmp");
        let write = || -> std::io::Result<()> {
            let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
            f.write_all(MAGIC)?;
            f.write_all(&FORMAT_VERSION.to_le_bytes())?;
            f.write_all(&total.to_le_bytes())?;
            f.write_all(&hash.finish().to_le_bytes())?;
            for p in parts {
                f.write_all(p)?;
            }
            f.flush()?;
            Ok(())
        };
        if let Err(e) = write() {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("Could not save the index: {e}"));
        }
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("Could not save the index: {e}")
        })
    }

    /// Reads a store written by [`VolumeStore::save`], checking it thoroughly:
    /// a damaged file is an error, never a panic or a broken index.
    pub fn load(path: &Path) -> Result<VolumeStore, String> {
        let data = std::fs::read(path).map_err(|e| format!("Could not read the index: {e}"))?;
        VolumeStore::from_bytes(&data)
    }

    pub fn from_bytes(data: &[u8]) -> Result<VolumeStore, String> {
        let bad = || "The saved index is damaged.".to_string();
        if data.len() < 28 || &data[..8] != MAGIC {
            return Err("Not a FindForge index file.".into());
        }
        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err("The saved index is from another version.".into());
        }
        let total = u64::from_le_bytes(data[12..20].try_into().unwrap());
        let want = u64::from_le_bytes(data[20..28].try_into().unwrap());
        let body = &data[28..];
        if body.len() as u64 != total {
            return Err(bad());
        }
        let mut hash = Hasher::new();
        hash.update(body);
        if hash.finish() != want {
            return Err(bad());
        }
        let mut r = Reader { data: body, pos: 0 };
        let root = r.str().ok_or_else(bad)?;
        let label = r.str().ok_or_else(bad)?;
        let fs = r.str().ok_or_else(bad)?;
        let serial = r.u32().ok_or_else(bad)?;
        let kind = VolumeKind::from_u8(r.u8().ok_or_else(bad)?).ok_or_else(bad)?;
        let journal_id = r.u64().ok_or_else(bad)?;
        let next_usn = r.u64().ok_or_else(bad)? as i64;
        let scanned_unix = r.u64().ok_or_else(bad)?;
        let scan_ms = r.u32().ok_or_else(bad)?;
        let ext_count = r.u32().ok_or_else(bad)? as usize;
        if ext_count == 0 || ext_count > EXT_OTHER as usize {
            return Err(bad());
        }
        let mut exts: Vec<Box<str>> = Vec::with_capacity(ext_count);
        let mut ext_lookup = HashMap::with_capacity(ext_count);
        for i in 0..ext_count {
            let e: Box<str> = r.str().ok_or_else(bad)?.into();
            if i > 0 {
                ext_lookup.insert(e.clone(), i as u16);
            }
            exts.push(e);
        }
        let map_sparse = match r.u8().ok_or_else(bad)? {
            0 => false,
            1 => true,
            _ => return Err(bad()),
        };
        let n_entries = r.u64().ok_or_else(bad)? as usize;
        let n_names = r.u64().ok_or_else(bad)? as usize;
        let n_map = r.u64().ok_or_else(bad)? as usize;
        let n_free = r.u64().ok_or_else(bad)? as usize;
        if n_entries == 0 || n_entries > u32::MAX as usize - 1 || n_names > u32::MAX as usize {
            return Err(bad());
        }
        let entry_bytes = r
            .take(n_entries.checked_mul(ENTRY_SIZE).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        let names = r.take(n_names).ok_or_else(bad)?.to_vec();
        let map_bytes = r
            .take(n_map.checked_mul(4).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        let free_bytes = r
            .take(n_free.checked_mul(4).ok_or_else(bad)?)
            .ok_or_else(bad)?;
        if r.pos != body.len() {
            return Err(bad());
        }
        let mut entries: Vec<Entry> = Vec::with_capacity(n_entries);
        // SAFETY: the buffer has room for n_entries entries, the source has
        // exactly that many bytes and every bit pattern is a valid Entry.
        unsafe {
            std::ptr::copy_nonoverlapping(
                entry_bytes.as_ptr(),
                entries.as_mut_ptr() as *mut u8,
                n_entries * ENTRY_SIZE,
            );
            entries.set_len(n_entries);
        }
        let frn_map = IdMap::from_cells(map_sparse, bytes_to_u32s(map_bytes)).ok_or_else(bad)?;
        let _saved_free = bytes_to_u32s(free_bytes);

        // Validate everything the rest of the program relies on.
        let mut live = 0usize;
        let mut used = 0usize;
        let mut free = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            if !e.is_live() {
                if e.name_len != 0 {
                    return Err(bad());
                }
                free.push(i as u32);
                continue;
            }
            let start = e.name_off as usize;
            let end = start + e.name_len as usize;
            if end > names.len() || std::str::from_utf8(&names[start..end]).is_err() {
                return Err(bad());
            }
            let root_ok = (i == 0) == (e.parent == NONE) && (i == 0) == e.is_root();
            if !root_ok || (e.parent != NONE && e.parent as usize >= n_entries) {
                return Err(bad());
            }
            if e.ext != EXT_OTHER && e.ext as usize >= exts.len() {
                return Err(bad());
            }
            live += 1;
            used += e.name_len as usize;
        }
        if !entries[0].is_live() || !entries[0].is_dir() {
            return Err(bad());
        }
        if frn_map
            .max_slot()
            .map(|m| m as usize >= n_entries)
            .unwrap_or(false)
        {
            return Err(bad());
        }
        let mut store = VolumeStore {
            meta: VolumeMeta {
                root,
                label,
                fs,
                serial,
                kind,
                journal_id,
                next_usn,
                scanned_unix,
                scan_ms,
            },
            excludes: Excludes::default(),
            entries,
            wasted: names.len() - used,
            names,
            frn_map,
            free,
            pending_free: Vec::new(),
            exts,
            ext_lookup,
            live,
        };
        // Parents must be live folders and must not form cycles: one sweep
        // settles both (it removes whatever does not reach the root).
        store.sweep_orphans();
        let entries = &store.entries;
        store.frn_map.update(|slot| {
            if entries[slot as usize].is_live() {
                slot
            } else {
                NONE
            }
        });
        Ok(store)
    }
}

/// Makes room for `extra` more items without the usual doubling: a large
/// index that gains a few entries must not suddenly take twice the memory.
fn grow_gently<T>(v: &mut Vec<T>, extra: usize) {
    if v.capacity() - v.len() < extra {
        v.reserve_exact(extra + v.len() / 16 + 1024);
    }
}

const MAGIC: &[u8; 8] = b"FFINDEX\x01";
const FORMAT_VERSION: u32 = 1;

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn u32s_as_bytes(v: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding; the length is exact.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}

fn bytes_to_u32s(b: &[u8]) -> Vec<u32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let s = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        if n > 1 << 20 {
            return None;
        }
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

/// A fast, non-cryptographic 64-bit checksum (detects damaged index files).
struct Hasher {
    h: u64,
    len: u64,
}

impl Hasher {
    fn new() -> Hasher {
        Hasher {
            h: 0x243F_6A88_85A3_08D3,
            len: 0,
        }
    }

    /// Chunks must be fed in the same sizes when verifying, except that the
    /// result only depends on the bytes when every chunk but the last is a
    /// multiple of 8 long; to be independent of chunking, tails are padded
    /// and the total length is mixed in at the end.
    fn update(&mut self, data: &[u8]) {
        // Mix in the chunk as if it were preceded by its position, so that
        // the same bytes hash the same whether fed in one piece or several.
        let mut i = 0;
        // Align to the 8-byte grid of the whole stream.
        let mis = (self.len % 8) as usize;
        if mis != 0 {
            let take = (8 - mis).min(data.len());
            for (k, b) in data[..take].iter().enumerate() {
                self.h ^= (*b as u64) << (8 * (mis + k));
            }
            i = take;
            if mis + take == 8 {
                self.h = self.h.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
            }
        }
        let rest = &data[i..];
        let (words, tail) = rest.as_chunks::<8>();
        for c in words {
            let w = u64::from_le_bytes(*c);
            self.h = (self.h ^ w)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .rotate_left(29);
        }
        for (k, b) in tail.iter().enumerate() {
            self.h ^= (*b as u64) << (8 * k);
        }
        self.len += data.len() as u64;
    }

    fn finish(mut self) -> u64 {
        self.h = (self.h ^ self.len).wrapping_mul(0xD6E8_FEB8_6659_FD93);
        self.h ^ (self.h >> 32)
    }
}

/// Builds a [`VolumeStore`] from a scan. Entries may arrive in any order;
/// parents are resolved in [`VolumeBuilder::finish`].
pub struct VolumeBuilder {
    store: VolumeStore,
    /// Parent file id of each slot (index = slot).
    parents: Vec<u64>,
}

impl VolumeBuilder {
    /// Starts a store whose root (slot 0) is named `meta.root`.
    pub fn new(meta: VolumeMeta) -> VolumeBuilder {
        VolumeBuilder {
            store: VolumeStore::empty(meta),
            parents: vec![NO_FRN],
        }
    }

    pub fn with_capacity(meta: VolumeMeta, entries: usize) -> VolumeBuilder {
        let mut b = VolumeBuilder::new(meta);
        b.store.entries.reserve(entries);
        b.parents.reserve(entries);
        b.store.names.reserve(entries.saturating_mul(18));
        b
    }

    /// Tells the builder which file id is the root folder (NTFS: record 5).
    pub fn set_root_frn(&mut self, frn: u64) {
        self.store.map_frn(frn, 0);
    }

    pub fn meta_mut(&mut self) -> &mut VolumeMeta {
        &mut self.store.meta
    }

    /// Number of entries pushed so far (including the root).
    pub fn len(&self) -> usize {
        self.store.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.entries.len() <= 1
    }

    /// Adds an entry. `frn` may be [`NO_FRN`] for files that never need to be
    /// found again by id; `parent_frn` must be the id of a pushed folder (or
    /// the root id) by the time `finish` runs, otherwise the entry is dropped.
    /// A second push for the same `frn` replaces the first.
    pub fn push(&mut self, frn: u64, parent_frn: u64, name: &str, attrs: u32, meta: Meta) {
        if name.is_empty() {
            return;
        }
        let fl = flags_from_attrs(attrs);
        if let Some(old) = self.store.slot_of(frn) {
            if old == 0 {
                return;
            }
            self.store.tombstone(old);
            // Reuse of slots is not wanted while building: keep order simple.
            self.store.free.clear();
            self.store.pending_free.clear();
        }
        let mut m = meta;
        if fl & flags::DIR != 0 {
            m.size = 0;
        }
        let slot = self.store.push_raw(name, NONE, fl, m);
        self.parents.push(parent_frn);
        debug_assert_eq!(self.parents.len(), self.store.entries.len());
        self.store.map_frn(frn, slot);
    }

    /// Updates size / times of an already pushed id (used when the data of a
    /// file record turns up after its name). Unknown ids are ignored.
    pub fn set_meta(&mut self, frn: u64, meta: Meta) {
        if let Some(slot) = self.store.slot_of(frn) {
            self.store.set_meta(slot, meta);
        }
    }

    /// Resolves parents, drops entries that do not hang off the root, applies
    /// the exclude lists and returns the finished store. With `keep_ids`
    /// false the id map is discarded (plain walks with made-up ids).
    pub fn finish(mut self, excludes: &Excludes, keep_ids: bool) -> VolumeStore {
        let n = self.store.entries.len();
        for i in 1..n {
            if !self.store.entries[i].is_live() {
                continue;
            }
            let parent = self.store.slot_of(self.parents[i]).unwrap_or(NONE);
            // An entry with no parent would look like a second root: point it
            // at itself-is-invalid marker instead and let the sweep remove it.
            self.store.entries[i].parent = if parent == i as u32 { NONE } else { parent };
        }
        drop(self.parents);
        let mut store = self.store;
        store.drop_unreachable(&[]);
        store.apply_excludes(excludes);
        store.free.append(&mut store.pending_free);
        if !keep_ids {
            store.frn_map = IdMap::default();
        }
        if store.garbage_ratio() > 0.02 {
            store.compact();
        }
        store.entries.shrink_to_fit();
        store.names.shrink_to_fit();
        store.frn_map.settle(store.live);
        store
    }
}

/// A search result: a slot of a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hit {
    pub vol: u16,
    pub slot: u32,
}

/// Everything that is indexed.
#[derive(Clone, Debug, Default)]
pub struct Index {
    pub volumes: Vec<VolumeStore>,
    /// Bumped by the engine on every change; results from an older epoch may be stale.
    pub epoch: u64,
}

impl Index {
    /// Live entries over all volumes (roots not counted).
    pub fn total(&self) -> usize {
        self.volumes
            .iter()
            .map(|v| v.live_count().saturating_sub(1))
            .sum()
    }

    pub fn memory_bytes(&self) -> usize {
        self.volumes.iter().map(|v| v.memory_bytes()).sum()
    }

    pub fn entry(&self, hit: Hit) -> Option<&Entry> {
        self.volumes
            .get(hit.vol as usize)?
            .entry(hit.slot)
            .filter(|e| e.is_live())
    }

    pub fn name(&self, hit: Hit) -> &str {
        self.volumes
            .get(hit.vol as usize)
            .map(|v| v.name(hit.slot))
            .unwrap_or("")
    }

    /// Full path of a hit ("" if it no longer exists).
    pub fn path(&self, hit: Hit) -> String {
        match self.volumes.get(hit.vol as usize) {
            Some(v) if self.entry(hit).is_some() => v.path(hit.slot),
            _ => String::new(),
        }
    }

    /// Path of the folder containing a hit.
    pub fn parent_path(&self, hit: Hit) -> String {
        match self.volumes.get(hit.vol as usize) {
            Some(v) if self.entry(hit).is_some() => v.parent_path(hit.slot),
            _ => String::new(),
        }
    }

    /// Finds the hit for a full path, if indexed.
    pub fn find_path(&self, path: &str) -> Option<Hit> {
        self.volumes.iter().enumerate().find_map(|(i, v)| {
            v.resolve_path(path).map(|slot| Hit {
                vol: i as u16,
                slot,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: u32 = attr::DIRECTORY;

    fn frn(record: u64, seq: u64) -> u64 {
        (seq << 48) | record
    }

    fn m(size: u64) -> Meta {
        Meta {
            size,
            mtime: 1_700_000_000,
            ctime: 1_600_000_000,
        }
    }

    /// C:\ { Docs { report.docx, Old { a.txt } }, Music { song.MP3 }, boot.ini }
    fn sample() -> VolumeStore {
        let mut b = VolumeBuilder::new(VolumeMeta::new("C:\\", VolumeKind::NtfsMft));
        b.set_root_frn(frn(5, 5));
        // Deliberately out of order: children before parents.
        b.push(frn(40, 1), frn(30, 1), "report.docx", 0, m(1000));
        b.push(frn(30, 1), frn(5, 5), "Docs", DIR, m(0));
        b.push(frn(31, 1), frn(5, 5), "Music", DIR, m(0));
        b.push(frn(41, 1), frn(31, 1), "song.MP3", 0, m(5_000_000));
        b.push(frn(32, 2), frn(30, 1), "Old", DIR, m(0));
        b.push(frn(42, 1), frn(32, 2), "a.txt", attr::HIDDEN, m(12));
        b.push(
            frn(43, 1),
            frn(5, 5),
            "boot.ini",
            attr::SYSTEM | attr::HIDDEN,
            m(1),
        );
        // An orphan: its parent is never pushed.
        b.push(frn(50, 1), frn(999, 1), "lost.bin", 0, m(1));
        b.finish(&Excludes::default(), true)
    }

    fn paths(s: &VolumeStore) -> Vec<String> {
        let mut v: Vec<String> = (0..s.entries().len() as u32)
            .filter(|i| s.entry(*i).unwrap().is_live())
            .map(|i| s.path(i))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn entry_is_32_bytes() {
        assert_eq!(std::mem::size_of::<Entry>(), 32);
    }

    #[test]
    fn builder_resolves_parents_and_drops_orphans() {
        let s = sample();
        assert_eq!(
            paths(&s),
            vec![
                "C:\\",
                "C:\\Docs",
                "C:\\Docs\\Old",
                "C:\\Docs\\Old\\a.txt",
                "C:\\Docs\\report.docx",
                "C:\\Music",
                "C:\\Music\\song.MP3",
                "C:\\boot.ini",
            ]
        );
        assert_eq!(s.live_count(), 8);
        assert!(s.slot_of(frn(50, 1)).is_none());
        let song = s.slot_of(frn(41, 1)).unwrap();
        let e = s.entry(song).unwrap();
        assert_eq!(s.ext_of(e), "mp3");
        assert_eq!(e.size, 5_000_000);
        assert_eq!(s.parent_path(song), "C:\\Music");
        let docs = s.entry(s.slot_of(frn(30, 1)).unwrap()).unwrap();
        assert!(docs.is_dir());
        assert_eq!(docs.size, 0);
        assert_eq!(s.ext_of(docs), "");
        let a = s.entry(s.slot_of(frn(42, 1)).unwrap()).unwrap();
        assert!(a.flags & flags::HIDDEN != 0);
        assert_eq!(s.parent_path(s.slot_of(frn(43, 1)).unwrap()), "C:\\");
    }

    #[test]
    fn resolve_path_is_case_insensitive() {
        let s = sample();
        assert_eq!(
            s.resolve_path("c:\\docs\\OLD\\A.TXT"),
            s.slot_of(frn(42, 1))
        );
        assert_eq!(s.resolve_path("C:\\"), Some(0));
        assert_eq!(s.resolve_path("C:"), Some(0));
        assert_eq!(s.resolve_path("C:\\nope"), None);
        assert_eq!(s.resolve_path("D:\\Docs"), None);
    }

    #[test]
    fn apply_create() {
        let mut s = sample();
        let r = s.apply(&Change::Upsert {
            frn: frn(60, 1),
            parent_frn: frn(31, 1),
            name: "new.flac".into(),
            attrs: 0,
            meta: Some(m(77)),
        });
        let Applied::Created(slot) = r else {
            panic!("{r:?}")
        };
        assert_eq!(s.path(slot), "C:\\Music\\new.flac");
        assert_eq!(s.entry(slot).unwrap().size, 77);
        assert_eq!(s.ext_of(s.entry(slot).unwrap()), "flac");
        assert_eq!(s.live_count(), 9);
        // Unknown parent: ignored.
        let r = s.apply(&Change::Upsert {
            frn: frn(61, 1),
            parent_frn: frn(777, 1),
            name: "x".into(),
            attrs: 0,
            meta: None,
        });
        assert_eq!(r, Applied::Ignored);
        // A file cannot be a parent.
        let r = s.apply(&Change::Upsert {
            frn: frn(62, 1),
            parent_frn: frn(60, 1),
            name: "x".into(),
            attrs: 0,
            meta: None,
        });
        assert_eq!(r, Applied::Ignored);
        assert_eq!(s.live_count(), 9);
    }

    #[test]
    fn apply_rename_and_move() {
        let mut s = sample();
        // Rename a file (different length, then same length).
        let r = s.apply(&Change::Upsert {
            frn: frn(40, 1),
            parent_frn: frn(30, 1),
            name: "final report.PDF".into(),
            attrs: 0,
            meta: None,
        });
        let Applied::Renamed(slot) = r else {
            panic!("{r:?}")
        };
        assert_eq!(s.path(slot), "C:\\Docs\\final report.PDF");
        assert_eq!(s.ext_of(s.entry(slot).unwrap()), "pdf");
        assert_eq!(
            s.entry(slot).unwrap().size,
            1000,
            "meta: None keeps the size"
        );
        let r = s.apply(&Change::Upsert {
            frn: frn(40, 1),
            parent_frn: frn(30, 1),
            name: "FINAL REPORT.pdf".into(),
            attrs: 0,
            meta: None,
        });
        assert_eq!(r, Applied::Renamed(slot));
        assert_eq!(s.name(slot), "FINAL REPORT.pdf");

        // Move a folder: everything below follows.
        let r = s.apply(&Change::Upsert {
            frn: frn(32, 2),
            parent_frn: frn(31, 1),
            name: "Archive".into(),
            attrs: DIR,
            meta: None,
        });
        assert!(matches!(r, Applied::Renamed(_)));
        assert_eq!(
            s.path(s.slot_of(frn(42, 1)).unwrap()),
            "C:\\Music\\Archive\\a.txt"
        );
        // A folder cannot be moved into its own subtree.
        let r = s.apply(&Change::Upsert {
            frn: frn(31, 1),
            parent_frn: frn(32, 2),
            name: "Music".into(),
            attrs: DIR,
            meta: None,
        });
        assert_eq!(r, Applied::Ignored);
        // Same again: nothing to do.
        let r = s.apply(&Change::Upsert {
            frn: frn(32, 2),
            parent_frn: frn(31, 1),
            name: "Archive".into(),
            attrs: DIR,
            meta: None,
        });
        assert_eq!(r, Applied::Ignored);
        // Attribute change only.
        let r = s.apply(&Change::Upsert {
            frn: frn(41, 1),
            parent_frn: frn(31, 1),
            name: "song.MP3".into(),
            attrs: attr::HIDDEN,
            meta: None,
        });
        assert!(matches!(r, Applied::Updated(_)));
        // Moved to an unknown folder: gone from the index.
        let r = s.apply(&Change::Upsert {
            frn: frn(41, 1),
            parent_frn: frn(4242, 1),
            name: "song.MP3".into(),
            attrs: 0,
            meta: None,
        });
        assert!(matches!(r, Applied::Deleted(_)));
        assert!(s.slot_of(frn(41, 1)).is_none());
    }

    #[test]
    fn apply_delete_and_meta() {
        let mut s = sample();
        let r = s.apply(&Change::Meta {
            frn: frn(40, 1),
            meta: Meta {
                size: 5,
                mtime: 9,
                ctime: 8,
            },
        });
        let Applied::Updated(slot) = r else {
            panic!("{r:?}")
        };
        let e = s.entry(slot).unwrap();
        assert_eq!((e.size, e.mtime, e.ctime), (5, 9, 8));

        assert!(matches!(
            s.apply(&Change::Delete { frn: frn(40, 1) }),
            Applied::Deleted(_)
        ));
        assert_eq!(
            s.apply(&Change::Delete { frn: frn(40, 1) }),
            Applied::Ignored
        );
        let by_path = s.slot_of(frn(43, 1)).unwrap();
        assert_eq!(
            s.apply(&Change::DeletePath {
                path: "c:\\BOOT.ini".into()
            }),
            Applied::Deleted(by_path)
        );
        assert!(s.slot_of(frn(43, 1)).is_none());
        assert_eq!(
            s.apply(&Change::DeletePath {
                path: "C:\\boot.ini".into()
            }),
            Applied::Ignored
        );
        s.apply(&Change::Upsert {
            frn: frn(43, 2),
            parent_frn: frn(5, 5),
            name: "boot.ini".into(),
            attrs: 0,
            meta: None,
        });
        assert_eq!(
            s.apply(&Change::Delete { frn: frn(5, 5) }),
            Applied::Ignored
        );
        assert_eq!(s.live_count(), 7);
        assert!(!s.needs_sweep());

        // Deleting a folder that still has children: the sweep removes them,
        // and the folder slot is not reused before that.
        let old = s.slot_of(frn(32, 2)).unwrap();
        assert!(matches!(
            s.apply(&Change::Delete { frn: frn(32, 2) }),
            Applied::Deleted(_)
        ));
        assert!(s.needs_sweep());
        let r = s.apply(&Change::Upsert {
            frn: frn(70, 1),
            parent_frn: frn(30, 1),
            name: "fresh".into(),
            attrs: DIR,
            meta: None,
        });
        let Applied::Created(fresh) = r else {
            panic!("{r:?}")
        };
        assert_ne!(
            fresh, old,
            "a deleted folder's slot must wait for the sweep"
        );
        assert_eq!(s.sweep_orphans(), 1);
        assert!(s.slot_of(frn(42, 1)).is_none());
        assert!(!s.needs_sweep());
        assert_eq!(
            paths(&s),
            vec![
                "C:\\",
                "C:\\Docs",
                "C:\\Docs\\fresh",
                "C:\\Music",
                "C:\\Music\\song.MP3",
                "C:\\boot.ini",
            ]
        );
    }

    #[test]
    fn slots_and_names_are_reused_and_compacted() {
        let mut s = sample();
        let before = s.entries().len();
        for round in 0..50u64 {
            let id = frn(100 + round, 1);
            s.apply(&Change::Upsert {
                frn: id,
                parent_frn: frn(30, 1),
                name: format!("temp-{round}.tmp"),
                attrs: 0,
                meta: None,
            });
            s.apply(&Change::Delete { frn: id });
        }
        assert_eq!(s.entries().len(), before + 1, "the free slot is reused");
        assert!(s.garbage_ratio() > 0.5);
        let want = paths(&s);
        s.compact();
        assert_eq!(paths(&s), want);
        assert_eq!(s.garbage_ratio(), 0.0);
        assert_eq!(s.entries().len(), s.live_count());
        assert_eq!(
            s.path(s.slot_of(frn(42, 1)).unwrap()),
            "C:\\Docs\\Old\\a.txt"
        );
    }

    #[test]
    fn excludes_drop_subtrees_and_names() {
        let mut b = VolumeBuilder::new(VolumeMeta::new("C:", VolumeKind::NtfsWalk));
        b.set_root_frn(5);
        b.push(30, 5, "Docs", DIR, m(0));
        b.push(31, 30, "node_modules", DIR, m(0));
        b.push(32, 31, "pkg.json", 0, m(1));
        b.push(33, 30, "keep.txt", 0, m(1));
        b.push(34, 30, "junk.TMP", 0, m(1));
        b.push(35, 5, "Temp", DIR, m(0));
        b.push(36, 35, "deep", DIR, m(0));
        b.push(37, 36, "x.txt", 0, m(1));
        let ex = Excludes::new(
            &["C:\\Temp\\".to_string()],
            &["node_modules".to_string(), "*.tmp".to_string()],
        );
        let mut s = b.finish(&ex, true);
        assert_eq!(paths(&s), vec!["C:\\", "C:\\Docs", "C:\\Docs\\keep.txt"]);
        // Live changes respect the lists too.
        let r = s.apply(&Change::Upsert {
            frn: 80,
            parent_frn: 30,
            name: "again.tmp".into(),
            attrs: 0,
            meta: None,
        });
        assert_eq!(r, Applied::Ignored);
        let r = s.apply(&Change::Upsert {
            frn: 81,
            parent_frn: 5,
            name: "temp".into(),
            attrs: DIR,
            meta: None,
        });
        assert_eq!(r, Applied::Ignored);
        // Renaming a kept file to an excluded name removes it.
        let r = s.apply(&Change::Upsert {
            frn: 33,
            parent_frn: 30,
            name: "keep.tmp".into(),
            attrs: 0,
            meta: None,
        });
        assert!(matches!(r, Applied::Deleted(_)));
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("findforge-index-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("c.ffi");
        let mut s = sample();
        s.meta.label = "System".into();
        s.meta.fs = "NTFS".into();
        s.meta.serial = 0xABCD_1234;
        s.meta.journal_id = 77;
        s.meta.next_usn = 123_456;
        s.apply(&Change::Delete { frn: frn(43, 1) });
        s.apply(&Change::Upsert {
            frn: frn(90, 1),
            parent_frn: frn(31, 1),
            name: "ünïcödé — 名前.txt".into(),
            attrs: 0,
            meta: Some(m(3)),
        });
        s.save(&file).unwrap();
        let l = VolumeStore::load(&file).unwrap();
        assert_eq!(l.meta, s.meta);
        assert_eq!(paths(&l), paths(&s));
        assert_eq!(l.live_count(), s.live_count());
        for r in [30u64, 31, 32, 40, 41, 42, 90] {
            let id = frn(r, if r == 32 { 2 } else { 1 });
            let (a, b) = (s.slot_of(id).unwrap(), l.slot_of(id).unwrap());
            assert_eq!(s.path(a), l.path(b));
            assert_eq!(s.entry(a), l.entry(b));
            assert_eq!(s.ext_of(s.entry(a).unwrap()), l.ext_of(l.entry(b).unwrap()));
        }
        // The loaded store keeps working.
        let mut l = l;
        let r = l.apply(&Change::Upsert {
            frn: frn(91, 1),
            parent_frn: frn(30, 1),
            name: "after.load".into(),
            attrs: 0,
            meta: None,
        });
        assert!(matches!(r, Applied::Created(_)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scattered_ids_use_a_sparse_map() {
        // A folder index: few entries whose record numbers are far apart.
        let mut b = VolumeBuilder::new(VolumeMeta::new("C:/Deep/Folder", VolumeKind::NtfsWalk));
        b.set_root_frn(frn(9_000_000, 3));
        b.push(frn(9_000_001, 1), frn(9_000_000, 3), "Docs", DIR, m(0));
        b.push(frn(40, 1), frn(9_000_001, 1), "a.txt", 0, m(1));
        b.push(frn(123_456_789, 7), frn(9_000_001, 1), "b.txt", 0, m(2));
        let mut s = b.finish(&Excludes::default(), true);
        assert!(s.memory_bytes() < 4096, "{}", s.memory_bytes());
        assert_eq!(s.slot_of(frn(9_000_000, 3)), Some(0));
        let a = s.slot_of(frn(40, 1)).unwrap();
        assert_eq!(s.name(a), "a.txt");
        // Changes keep working on the sparse map.
        assert!(matches!(
            s.apply(&Change::Upsert {
                frn: frn(77_000_000, 1),
                parent_frn: frn(9_000_001, 1),
                name: "c.txt".into(),
                attrs: 0,
                meta: None,
            }),
            Applied::Created(_)
        ));
        assert!(matches!(
            s.apply(&Change::Delete { frn: frn(40, 1) }),
            Applied::Deleted(_)
        ));
        assert!(s.slot_of(frn(40, 1)).is_none());
        s.remove_slot(s.slot_of(frn(123_456_789, 7)).unwrap());
        assert!(s.slot_of(frn(123_456_789, 7)).is_none());
        s.compact();
        let dir =
            std::env::temp_dir().join(format!("findforge-index-sparse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.ffi");
        s.save(&file).unwrap();
        assert!(std::fs::metadata(&file).unwrap().len() < 2000);
        let l = VolumeStore::load(&file).unwrap();
        assert_eq!(paths(&l), paths(&s));
        let c = l.slot_of(frn(77_000_000, 1)).unwrap();
        assert_eq!(l.name(c), "c.txt");
        assert_eq!(l.slot_of(frn(9_000_001, 1)), s.slot_of(frn(9_000_001, 1)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn damaged_files_are_rejected() {
        let dir =
            std::env::temp_dir().join(format!("findforge-index-damage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("c.ffi");
        sample().save(&file).unwrap();
        let good = std::fs::read(&file).unwrap();
        assert!(VolumeStore::from_bytes(&good).is_ok());
        assert!(VolumeStore::from_bytes(&[]).is_err());
        assert!(VolumeStore::from_bytes(b"hello world, this is not an index").is_err());
        // Truncated at every length: never a panic.
        for cut in (0..good.len()).step_by(7) {
            assert!(VolumeStore::from_bytes(&good[..cut]).is_err());
        }
        // Any flipped byte is caught by the checksum.
        for pos in (0..good.len()).step_by(11) {
            let mut bad = good.clone();
            bad[pos] ^= 0x40;
            assert!(VolumeStore::from_bytes(&bad).is_err(), "flip at {pos}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hasher_is_independent_of_chunking() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let mut a = Hasher::new();
        a.update(&data);
        let mut b = Hasher::new();
        b.update(&data[..3]);
        b.update(&data[3..3]);
        b.update(&data[3..20]);
        b.update(&data[20..501]);
        b.update(&data[501..]);
        assert_eq!(a.finish(), b.finish());
        let mut c = Hasher::new();
        c.update(&data[..999]);
        assert_ne!(Hasher::new().finish(), c.finish());
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_unix(0), 0);
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
        assert_eq!(filetime_to_unix(116_444_736_010_000_000), 1);
        // 2026-01-01T00:00:00Z
        assert_eq!(filetime_to_unix(134_116_992_000_000_000), 1_767_225_600);
        assert_eq!(filetime_to_unix(u64::MAX), u32::MAX);
    }

    #[test]
    fn index_helpers() {
        let idx = Index {
            volumes: vec![sample()],
            epoch: 0,
        };
        assert_eq!(idx.total(), 7);
        let hit = idx.find_path("C:\\Music\\song.mp3").unwrap();
        assert_eq!(idx.name(hit), "song.MP3");
        assert_eq!(idx.path(hit), "C:\\Music\\song.MP3");
        assert_eq!(idx.parent_path(hit), "C:\\Music");
        assert!(idx.memory_bytes() > 0);
        assert!(idx.entry(Hit { vol: 3, slot: 0 }).is_none());
    }
}
