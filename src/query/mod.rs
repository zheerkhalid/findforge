//! The query language and the matcher.
//!
//! `parse` turns the text of the search box into a tree of tests, `search`
//! runs that tree over every entry of the index on all processor cores, and
//! `sort` orders the hits.

mod parse;
mod text;

use crate::index::{flags, Entry, Hit, Index, VolumeStore, EXT_OTHER, NONE};
use parse::{Node, Parsed, Pred};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering as CmpOrdering;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use text::{name_key, natural_cmp};

/// Switches that change how the text of a query is matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchOptions {
    pub match_case: bool,
    pub whole_word: bool,
    /// Match terms against the full path instead of the name only.
    pub match_path: bool,
    /// Treat the whole query as one regular expression.
    pub regex: bool,
    /// When false, `resume` also matches `résumé`.
    pub match_diacritics: bool,
    /// Include entries with the hidden / system attribute.
    pub include_hidden: bool,
    pub include_system: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            match_case: false,
            whole_word: false,
            match_path: false,
            regex: false,
            match_diacritics: false,
            include_hidden: true,
            include_system: true,
        }
    }
}

/// "Now" for relative dates (`dm:today`), passed in so tests are repeatable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Now {
    /// Unix seconds (UTC).
    pub unix: i64,
    /// Seconds to add to UTC to get local time.
    pub utc_offset_secs: i32,
}

/// A named, reusable piece of query (`Audio` = `ext:mp3;flac;...`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterDef {
    pub name: String,
    pub query: String,
}

/// The ready-made filters: Audio, Video, Pictures, Documents, Compressed,
/// Executables, Folders.
pub fn builtin_filters() -> Vec<FilterDef> {
    [
        ("Audio", format!("ext:{}", parse::AUDIO)),
        ("Video", format!("ext:{}", parse::VIDEO)),
        ("Pictures", format!("ext:{}", parse::PICTURES)),
        ("Documents", format!("ext:{}", parse::DOCUMENTS)),
        ("Compressed", format!("ext:{}", parse::COMPRESSED)),
        ("Executables", format!("ext:{}", parse::EXECUTABLES)),
        ("Folders", "type:folder".to_string()),
    ]
    .into_iter()
    .map(|(name, query)| FilterDef {
        name: name.to_string(),
        query,
    })
    .collect()
}

/// What the results are sorted by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SortKey {
    Name,
    Path,
    Size,
    Modified,
    Created,
    /// By extension, then name.
    Type,
}

impl SortKey {
    /// Column id (as used in the settings) to sort key.
    pub fn from_id(id: &str) -> SortKey {
        match id {
            "path" => SortKey::Path,
            "size" => SortKey::Size,
            "modified" => SortKey::Modified,
            "created" => SortKey::Created,
            "type" => SortKey::Type,
            _ => SortKey::Name,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Path => "path",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Created => "created",
            SortKey::Type => "type",
        }
    }
}

/// A parsed, ready-to-run query.
#[derive(Clone, Debug)]
pub struct Query {
    parsed: Parsed,
    options: SearchOptions,
}

impl Query {
    /// True when the query matches everything (no terms, no filter).
    pub fn is_match_all(&self) -> bool {
        matches!(&self.parsed.root, Node::And(v) if v.is_empty())
    }

    /// Byte ranges of `name` to highlight as "this is why it matched":
    /// sorted, not overlapping, always on character boundaries.
    pub fn highlights(&self, name: &str) -> Vec<Range<usize>> {
        let mut out = Vec::new();
        for t in &self.parsed.highlight {
            t.highlight(name, &mut out);
        }
        text::merge_ranges(out)
    }
}

/// Parses `text` (and an optional filter query that is ANDed with it).
/// Errors are short, human-readable sentences.
pub fn parse(
    text: &str,
    filter: Option<&str>,
    options: &SearchOptions,
    now: Now,
) -> Result<Query, String> {
    if text.len() > 16 * 1024 {
        return Err("That search is too long.".into());
    }
    Ok(Query {
        parsed: parse::parse(text, filter, options, now)?,
        options: *options,
    })
}

// --------------------------------------------------------------------------------------
// Searching
// --------------------------------------------------------------------------------------

/// Case-insensitive hash of a name (for `dupe:`).
fn name_hash(name: &str) -> u64 {
    const SEED: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = SEED;
    if name.is_ascii() {
        for b in name.bytes() {
            h = (h ^ b.to_ascii_lowercase() as u64).wrapping_mul(PRIME);
        }
    } else {
        for c in name.chars().flat_map(|c| c.to_lowercase()) {
            h = (h ^ c as u64).wrapping_mul(PRIME);
        }
    }
    h
}

/// Per-volume lookups prepared once per search.
struct VolPlan {
    /// For every `Pred::Ext`: is this extension id wanted? (+ the list itself
    /// for entries whose extension is not in the table).
    ext_tables: Vec<Vec<bool>>,
    /// For every `Pred::Parent`: the slot of that folder on this volume.
    parent_slots: Vec<Option<u32>>,
    /// For `empty:`: does this slot have children?
    has_children: Vec<bool>,
    root_lower: String,
}

struct Scratch {
    norm: String,
    path: String,
    has_path: bool,
}

struct Env<'a> {
    parsed: &'a Parsed,
    vol: &'a VolumeStore,
    plan: &'a VolPlan,
    /// Sorted hashes of names that occur more than once.
    dupes: &'a [u64],
}

impl Env<'_> {
    fn eval(&self, node: &Node, slot: u32, e: &Entry, s: &mut Scratch) -> bool {
        match node {
            Node::And(v) => v.iter().all(|n| self.eval(n, slot, e, s)),
            Node::Or(v) => v.iter().any(|n| self.eval(n, slot, e, s)),
            Node::Not(n) => !self.eval(n, slot, e, s),
            Node::Leaf(p) => self.test(p, slot, e, s),
        }
    }

    fn test(&self, pred: &Pred, slot: u32, e: &Entry, s: &mut Scratch) -> bool {
        match pred {
            Pred::Kind(dir) => e.is_dir() == *dir,
            Pred::Ext(i) => {
                if e.ext == EXT_OTHER {
                    let ext = crate::util::extension(self.vol.name_of(e)).to_lowercase();
                    self.parsed.ext_lists[*i].contains(&ext)
                } else {
                    self.plan.ext_tables[*i]
                        .get(e.ext as usize)
                        .copied()
                        .unwrap_or(false)
                }
            }
            Pred::Size(min, max) => e.size >= *min && e.size <= *max,
            Pred::Date { created, min, max } => {
                let t = if *created { e.ctime } else { e.mtime };
                t != 0 && t >= *min && t <= *max
            }
            Pred::Attrib(mask) => e.flags & mask == *mask,
            Pred::Root(r) => self.plan.root_lower.starts_with(r.as_str()),
            Pred::Parent(i) => self.plan.parent_slots[*i] == Some(e.parent),
            Pred::Len(min, max) => {
                let name = self.vol.name_of(e);
                let n = if name.is_ascii() {
                    name.len()
                } else {
                    name.chars().count()
                } as u32;
                n >= *min && n <= *max
            }
            Pred::Empty => {
                e.is_dir()
                    && !self
                        .plan
                        .has_children
                        .get(slot as usize)
                        .copied()
                        .unwrap_or(true)
            }
            Pred::Dupe => self
                .dupes
                .binary_search(&name_hash(self.vol.name_of(e)))
                .is_ok(),
            Pred::Depth(min, max) => {
                let mut depth = 0u32;
                let mut cur = e.parent;
                while cur != NONE && depth < 4096 {
                    depth += 1;
                    cur = match self.vol.entry(cur) {
                        Some(p) => p.parent,
                        None => NONE,
                    };
                }
                depth >= *min && depth <= *max
            }
            Pred::Text(t) => {
                if t.path {
                    if !s.has_path {
                        s.path.clear();
                        self.vol.path_into(slot, &mut s.path);
                        s.has_path = true;
                    }
                    t.matches(&s.path, &mut s.norm)
                } else {
                    t.matches(self.vol.name_of(e), &mut s.norm)
                }
            }
        }
    }
}

fn plan_for(vol: &VolumeStore, parsed: &Parsed) -> VolPlan {
    let ext_tables = parsed
        .ext_lists
        .iter()
        .map(|list| {
            let mut table = vec![false; vol.exts().len()];
            for ext in list {
                if let Some(id) = vol.ext_id(ext) {
                    if let Some(cell) = table.get_mut(id as usize) {
                        *cell = true;
                    }
                }
            }
            table
        })
        .collect();
    let parent_slots = parsed.parents.iter().map(|p| vol.resolve_path(p)).collect();
    let mut has_children = Vec::new();
    if parsed.uses_empty {
        has_children = vec![false; vol.entries().len()];
        for e in vol.entries() {
            if e.is_live() && e.parent != NONE {
                if let Some(cell) = has_children.get_mut(e.parent as usize) {
                    *cell = true;
                }
            }
        }
    }
    VolPlan {
        ext_tables,
        parent_slots,
        has_children,
        root_lower: vol.meta.root.to_lowercase(),
    }
}

/// Hashes of all names that occur at least twice, sorted.
fn duplicate_names(index: &Index, cancel: &AtomicBool) -> Vec<u64> {
    let mut hashes: Vec<u64> = index
        .volumes
        .par_iter()
        .flat_map_iter(|vol| {
            vol.entries()
                .iter()
                .filter(|e| e.is_live() && !e.is_root())
                .map(move |e| name_hash(vol.name_of(e)))
        })
        .collect();
    if cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    hashes.par_sort_unstable();
    let mut dupes = Vec::new();
    let mut i = 0;
    while i < hashes.len() {
        let mut j = i + 1;
        while j < hashes.len() && hashes[j] == hashes[i] {
            j += 1;
        }
        if j - i > 1 {
            dupes.push(hashes[i]);
        }
        i = j;
    }
    dupes
}

const CHUNK: usize = 16 * 1024;

/// Runs a query over the whole index, in parallel. Results are in index
/// order (unsorted). Stops early and returns what it has when `cancel` is set.
pub fn search(index: &Index, query: &Query, cancel: &AtomicBool) -> Vec<Hit> {
    let parsed = &query.parsed;
    let skip_flags = flags::DELETED
        | flags::ROOT
        | if query.options.include_hidden {
            0
        } else {
            flags::HIDDEN
        }
        | if query.options.include_system {
            0
        } else {
            flags::SYSTEM
        };
    let match_all = query.is_match_all();
    let dupes = if parsed.uses_dupe {
        duplicate_names(index, cancel)
    } else {
        Vec::new()
    };
    let mut hits: Vec<Hit> = Vec::new();
    for (v, vol) in index.volumes.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let plan = plan_for(vol, parsed);
        let env = Env {
            parsed,
            vol,
            plan: &plan,
            dupes: &dupes,
        };
        let parts: Vec<Vec<Hit>> = vol
            .entries()
            .par_chunks(CHUNK)
            .enumerate()
            .map_init(
                || Scratch {
                    norm: String::new(),
                    path: String::new(),
                    has_path: false,
                },
                |scratch, (c, chunk)| {
                    let mut out = Vec::new();
                    if cancel.load(Ordering::Relaxed) {
                        return out;
                    }
                    let base = c * CHUNK;
                    for (i, e) in chunk.iter().enumerate() {
                        if e.flags & skip_flags != 0 {
                            continue;
                        }
                        let slot = (base + i) as u32;
                        scratch.has_path = false;
                        if match_all || env.eval(&parsed.root, slot, e, scratch) {
                            out.push(Hit {
                                vol: v as u16,
                                slot,
                            });
                        }
                    }
                    out
                },
            )
            .collect();
        hits.reserve(parts.iter().map(|p| p.len()).sum());
        for p in parts {
            hits.extend(p);
        }
    }
    hits
}

// --------------------------------------------------------------------------------------
// Sorting
// --------------------------------------------------------------------------------------

/// Sorts hits in place (parallel). Ties are broken by name, then by index
/// position; folders are not grouped separately.
pub fn sort(index: &Index, hits: &mut [Hit], key: SortKey, ascending: bool, cancel: &AtomicBool) {
    if hits.len() < 2 {
        return;
    }
    let name_of = |h: Hit| index.name(h);
    let by_name = |a: Hit, b: Hit| natural_cmp(name_of(a), name_of(b)).then_with(|| a.cmp(&b));

    // Rank of every folder that contains a hit, in path order. Used as the
    // key for `Path` and as the last tie-break of the other orders.
    let folder_ranks = |hits: &[Hit]| -> Vec<Vec<u32>> {
        let mut tables: Vec<Vec<u32>> = index
            .volumes
            .iter()
            .map(|v| vec![u32::MAX; v.entries().len()])
            .collect();
        let mut dirs: Vec<(u16, u32)> = Vec::new();
        for h in hits {
            let Some(e) = index.entry(*h) else { continue };
            if let Some(cell) = tables[h.vol as usize].get_mut(e.parent as usize) {
                if *cell == u32::MAX {
                    *cell = 0;
                    dirs.push((h.vol, e.parent));
                }
            }
        }
        let mut paths: Vec<(String, u16, u32)> = dirs
            .par_iter()
            .map(|(v, slot)| (index.volumes[*v as usize].path(*slot), *v, *slot))
            .collect();
        paths.par_sort_unstable_by(|a, b| {
            natural_cmp(&a.0, &b.0).then_with(|| (a.1, a.2).cmp(&(b.1, b.2)))
        });
        for (rank, (_, v, slot)) in paths.iter().enumerate() {
            tables[*v as usize][*slot as usize] = rank as u32;
        }
        tables
    };

    // Every element carries three words: the main key (or the start of the
    // name) and the beginning of the name in sortable form, so that almost
    // every comparison is settled without touching the name pool.
    type Key = [u64; 3];
    let word = |b: &[u8]| u64::from_be_bytes(b.try_into().unwrap());
    let with_name = |main: u64, h: Hit| -> Key {
        let k = name_key::<16>(name_of(h));
        [main, word(&k[..8]), word(&k[8..])]
    };
    let mut keyed: Vec<(Key, Hit)> = match key {
        SortKey::Name => hits
            .par_iter()
            .map(|h| {
                let k = name_key::<24>(name_of(*h));
                ([word(&k[..8]), word(&k[8..16]), word(&k[16..])], *h)
            })
            .collect(),
        SortKey::Size => hits
            .par_iter()
            .map(|h| {
                (
                    with_name(index.entry(*h).map(|e| e.size).unwrap_or(0), *h),
                    *h,
                )
            })
            .collect(),
        SortKey::Modified => hits
            .par_iter()
            .map(|h| {
                let t = index.entry(*h).map(|e| e.mtime as u64).unwrap_or(0);
                (with_name(t, *h), *h)
            })
            .collect(),
        SortKey::Created => hits
            .par_iter()
            .map(|h| {
                let t = index.entry(*h).map(|e| e.ctime as u64).unwrap_or(0);
                (with_name(t, *h), *h)
            })
            .collect(),
        SortKey::Type => hits
            .par_iter()
            .map(|h| {
                let k = match index.entry(*h) {
                    // Folders first, then files by extension.
                    Some(e) if e.is_dir() => 0,
                    Some(e) => {
                        let ext = index.volumes[h.vol as usize].ext_of(e);
                        let mut b = [0u8; 8];
                        for (i, c) in ext.bytes().take(7).enumerate() {
                            b[i + 1] = c;
                        }
                        b[0] = 1;
                        u64::from_be_bytes(b)
                    }
                    None => u64::MAX,
                };
                (with_name(k, *h), *h)
            })
            .collect(),
        SortKey::Path => {
            let tables = folder_ranks(hits);
            hits.par_iter()
                .map(|h| {
                    let rank = index
                        .entry(*h)
                        .and_then(|e| tables[h.vol as usize].get(e.parent as usize).copied())
                        .unwrap_or(u32::MAX);
                    (with_name(rank as u64, *h), *h)
                })
                .collect()
        }
    };
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    match key {
        SortKey::Type => keyed.par_sort_unstable_by(|a, b| {
            a.0[0]
                .cmp(&b.0[0])
                .then_with(|| {
                    // Extensions longer than the key share it: compare them fully.
                    let ext = |h: Hit| {
                        index
                            .entry(h)
                            .map(|e| index.volumes[h.vol as usize].ext_of(e))
                            .unwrap_or("")
                    };
                    if a.0[0] & 0xFF != 0 {
                        ext(a.1).cmp(ext(b.1))
                    } else {
                        CmpOrdering::Equal
                    }
                })
                .then_with(|| a.0[1..].cmp(&b.0[1..]))
                .then_with(|| by_name(a.1, b.1))
        }),
        _ => keyed.par_sort_unstable_by(|a, b| a.0.cmp(&b.0).then_with(|| by_name(a.1, b.1))),
    }
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    if ascending {
        for (dst, (_, h)) in hits.iter_mut().zip(keyed.iter()) {
            *dst = *h;
        }
    } else {
        for (dst, (_, h)) in hits.iter_mut().zip(keyed.iter().rev()) {
            *dst = *h;
        }
    }
}

/// Compares two hits by name the way the results list does (for callers
/// that merge or check order).
pub fn compare_names(a: &str, b: &str) -> CmpOrdering {
    natural_cmp(a, b)
}

/// One line of the syntax help: `(example, explanation)`.
pub fn syntax_help() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "report 2026",
            "Both words must be in the name (space means AND).",
        ),
        ("report|invoice", "Either word (| means OR)."),
        (
            "report !draft",
            "Has \"report\" but not \"draft\" (! means NOT).",
        ),
        ("\"annual report\"", "The exact phrase, spaces included."),
        ("<jpg|png> holiday", "Angle brackets group terms."),
        (
            "*.tmp   img_????.jpg",
            "Wildcards for the whole name: * is any text, ? is one character.",
        ),
        (
            "docs\\report",
            "A backslash makes the term match the full path.",
        ),
        ("path:backup", "Match this term against the full path."),
        ("ext:jpg;png;gif", "Files with one of these extensions."),
        (
            "size:>10mb   size:1mb..5mb",
            "By size: > >= < <= = and ranges; units b, kb, mb, gb, tb.",
        ),
        (
            "size:empty   size:huge",
            "Named sizes: empty, tiny, small, medium, large, huge, gigantic.",
        ),
        (
            "dm:today   dm:lastweek",
            "Date modified: also yesterday, thisweek, thismonth, lastyear.",
        ),
        (
            "dm:last7days   dm:last2hours",
            "Modified within a period that ends now.",
        ),
        (
            "dm:2026-03   dm:>2026-01-15",
            "A year, month or day; comparisons and a..b ranges work too.",
        ),
        ("dc:thisyear", "Date created, with the same choices as dm:."),
        (
            "type:folder   type:file",
            "Only folders or files; also type:audio, video, picture, document.",
        ),
        (
            "folder:backup   file:backup",
            "The term must be a folder name, or a file name.",
        ),
        ("parent:C:\\Projects", "Items directly inside this folder."),
        ("drive:d", "Only this drive."),
        ("depth:<3", "By how many folders deep the item is."),
        ("len:>100", "By the length of the name in characters."),
        ("dupe:", "Names that occur more than once."),
        ("empty:", "Folders with nothing in them."),
        (
            "attrib:h",
            "By attribute: h hidden, s system, r read-only, d folder, l link.",
        ),
        (
            "case:Report   ww:port",
            "Match case, or whole words only, for one term.",
        ),
        (
            "startwith:img   endwith:.bak",
            "Anchor a term to the start or the end of the name.",
        ),
        ("exact:readme.md", "The whole name, nothing more."),
        (
            "regex:^img_\\d{4}",
            "A regular expression for one term (or switch on regex mode).",
        ),
        (
            "diacritics:résumé",
            "Accents must match exactly (normally resume also finds résumé).",
        ),
    ]
}

#[cfg(test)]
mod tests;
