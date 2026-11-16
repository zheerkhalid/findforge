//! Indexing by walking folders (no administrator rights needed).
//!
//! Every folder is listed in bulk with `GetFileInformationByHandleEx`, which
//! returns names, sizes, times, attributes and, on NTFS, the file id of each
//! entry. Several threads walk at once.

use crate::index::{
    attr, filetime_to_unix, Change, Excludes, Meta, VolumeBuilder, VolumeKind, VolumeMeta,
    VolumeStore, NO_FRN,
};
use crate::sys::wide;
use std::ffi::c_void;
use std::os::windows::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// One entry of a folder listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirEntry {
    pub name: String,
    pub attrs: u32,
    pub file_id: u64,
    pub meta: Meta,
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Decodes a buffer of `FILE_ID_BOTH_DIR_INFO` records (`.` and `..` are
/// skipped). Stops quietly at the first record that does not fit.
pub(crate) fn parse_dir_buffer(buf: &[u8], mut each: impl FnMut(DirEntry)) {
    // Field offsets of FILE_ID_BOTH_DIR_INFO.
    const CREATION: usize = 8;
    const LAST_WRITE: usize = 24;
    const END_OF_FILE: usize = 40;
    const ATTRIBUTES: usize = 56;
    const NAME_LENGTH: usize = 60;
    const FILE_ID: usize = 96;
    const NAME: usize = 104;
    let mut off = 0usize;
    loop {
        let Some(rec) = buf.get(off..) else { return };
        let (Some(next), Some(name_len)) = (u32_at(rec, 0), u32_at(rec, NAME_LENGTH)) else {
            return;
        };
        let Some(name_bytes) = rec.get(NAME..NAME + name_len as usize) else {
            return;
        };
        let units: Vec<u16> = name_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let is_dot = units == [b'.' as u16] || units == [b'.' as u16, b'.' as u16];
        if !is_dot && !units.is_empty() {
            let (Some(ctime), Some(mtime), Some(size), Some(attrs), Some(file_id)) = (
                u64_at(rec, CREATION),
                u64_at(rec, LAST_WRITE),
                u64_at(rec, END_OF_FILE),
                u32_at(rec, ATTRIBUTES),
                u64_at(rec, FILE_ID),
            ) else {
                return;
            };
            each(DirEntry {
                name: String::from_utf16_lossy(&units),
                attrs,
                file_id,
                meta: Meta {
                    size,
                    mtime: filetime_to_unix(mtime),
                    ctime: filetime_to_unix(ctime),
                },
            });
        }
        if next == 0 {
            return;
        }
        off = match off.checked_add(next as usize) {
            Some(o) if o > off => o,
            _ => return,
        };
    }
}

/// `C:` -> `\\?\C:\`, `C:\a` -> `\\?\C:\a\`, `\\server\share` -> `\\?\UNC\server\share\`.
pub(crate) fn extended_dir_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('\\');
    let mut out = if let Some(unc) = trimmed.strip_prefix("\\\\?\\") {
        format!("\\\\?\\{unc}")
    } else if let Some(unc) = trimmed.strip_prefix("\\\\") {
        format!("\\\\?\\UNC\\{unc}")
    } else {
        format!("\\\\?\\{trimmed}")
    };
    out.push('\\');
    out
}

struct DirHandle(HANDLE);

impl Drop for DirHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by `open_dir` and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open_dir(path: &str) -> Result<DirHandle, String> {
    let w = wide(&extended_dir_path(path));
    // SAFETY: valid NUL-terminated path; the handle is owned by DirHandle.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(w.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            HANDLE::default(),
        )
    };
    handle.map(DirHandle).map_err(|e| e.message().to_string())
}

/// The NTFS file id of an open folder.
fn id_of(handle: &DirHandle) -> Option<u64> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: valid handle and out-structure.
    unsafe { GetFileInformationByHandle(handle.0, &mut info).ok()? };
    Some(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
}

/// The NTFS file id of a folder path, if it can be opened.
pub fn file_id_of_dir(path: &str) -> Option<u64> {
    id_of(&open_dir(path).ok()?)
}

/// Lists one folder. `buf` is a reusable, 8-byte aligned buffer.
fn list_dir(path: &str, buf: &mut [u64], mut each: impl FnMut(DirEntry)) -> Result<(), String> {
    let handle = open_dir(path)?;
    let bytes = std::mem::size_of_val(buf);
    let mut class = FileIdBothDirectoryRestartInfo;
    loop {
        // SAFETY: the buffer is valid for `bytes` bytes and outlives the call.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                class,
                buf.as_mut_ptr() as *mut c_void,
                bytes as u32,
            )
        };
        if ok.is_err() {
            // ERROR_NO_MORE_FILES ends the listing; anything else ends it too.
            return Ok(());
        }
        class = FileIdBothDirectoryInfo;
        // SAFETY: reading the bytes the call just filled in.
        let view = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, bytes) };
        parse_dir_buffer(view, &mut each);
    }
}

struct Raw {
    frn: u64,
    parent: u64,
    name: String,
    attrs: u32,
    meta: Meta,
}

struct Walk<'a> {
    with_ids: bool,
    excludes: &'a Excludes,
    progress: &'a (dyn Fn(u64) + Sync),
    cancel: &'a AtomicBool,
    builder: Mutex<VolumeBuilder>,
    count: AtomicU64,
    next_id: AtomicU64,
}

impl<'a> Walk<'a> {
    fn visit<'s>(&'s self, scope: &rayon::Scope<'s>, dir_id: u64, dir_path: String) {
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let mut batch: Vec<Raw> = Vec::new();
        let mut subdirs: Vec<(u64, String)> = Vec::new();
        thread_local! {
            static BUF: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(vec![0u64; 16 * 1024]);
        }
        let listed = BUF.with(|b| {
            list_dir(&dir_path, &mut b.borrow_mut(), |e| {
                if self.excludes.name_excluded(&e.name) {
                    return;
                }
                let is_dir = e.attrs & attr::DIRECTORY != 0;
                let frn = if self.with_ids {
                    e.file_id
                } else if is_dir {
                    self.next_id.fetch_add(1, Ordering::Relaxed)
                } else {
                    NO_FRN
                };
                if is_dir && e.attrs & attr::REPARSE_POINT == 0 {
                    let sub = format!("{dir_path}\\{}", e.name);
                    if !self.excludes.folder_excluded(&sub) {
                        subdirs.push((frn, sub));
                    } else {
                        return;
                    }
                }
                batch.push(Raw {
                    frn,
                    parent: dir_id,
                    name: e.name,
                    attrs: e.attrs,
                    meta: e.meta,
                });
            })
        });
        if listed.is_err() {
            return; // no access: skip this folder
        }
        if !batch.is_empty() {
            let n = batch.len() as u64;
            {
                let mut b = self.builder.lock().unwrap();
                for r in batch {
                    b.push(r.frn, r.parent, &r.name, r.attrs, r.meta);
                }
            }
            let total = self.count.fetch_add(n, Ordering::Relaxed) + n;
            (self.progress)(total);
        }
        for (id, path) in subdirs {
            scope.spawn(move |s| self.visit(s, id, path));
        }
    }
}

/// Walks `root` (`D:` or a folder path without trailing backslash) and builds
/// its index.
///
/// With `with_ids` (NTFS only) every entry is recorded under its real file id
/// and the store has kind `NtfsWalk`, so change notifications can be applied
/// later; otherwise the kind is `Walk` and the store has no id map.
/// Folders that cannot be opened are skipped. Reparse points (junctions,
/// symbolic links) are listed but not followed.
pub fn walk_volume(
    root: &str,
    with_ids: bool,
    excludes: &Excludes,
    progress: &(dyn Fn(u64) + Sync),
    cancel: &AtomicBool,
) -> Result<VolumeStore, String> {
    let root = root.trim_end_matches('\\');
    let handle = open_dir(root).map_err(|e| format!("Could not open {root}: {e}"))?;
    let kind = if with_ids {
        VolumeKind::NtfsWalk
    } else {
        VolumeKind::Walk
    };
    let mut builder = VolumeBuilder::new(VolumeMeta::new(root, kind));
    let root_id = if with_ids {
        id_of(&handle).ok_or_else(|| format!("Could not read the file id of {root}."))?
    } else {
        0
    };
    drop(handle);
    builder.set_root_frn(root_id);
    let walk = Walk {
        with_ids,
        excludes,
        progress,
        cancel,
        builder: Mutex::new(builder),
        count: AtomicU64::new(0),
        next_id: AtomicU64::new(1),
    };
    // A pool of its own: these threads mostly wait for the disk and must not
    // hold up searches, which use the global pool.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("findforge-walk-{i}"))
        .build()
        .map_err(|e| format!("Could not start the folder walk: {e}"))?;
    pool.scope(|s| walk.visit(s, root_id, root.to_string()));
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled.".into());
    }
    let builder = walk.builder.into_inner().unwrap();
    Ok(builder.finish(excludes, with_ids))
}

/// Lists everything below `dir` (a full path) as index changes, parents
/// before children, for a folder that appeared in an already indexed NTFS
/// tree (moved in from elsewhere). At most `limit` entries.
pub fn tree_changes(dir: &str, limit: usize) -> Vec<Change> {
    let mut out = Vec::new();
    let Some(root_id) = file_id_of_dir(dir) else {
        return out;
    };
    let mut buf = vec![0u64; 16 * 1024];
    let mut stack = vec![(root_id, dir.trim_end_matches('\\').to_string())];
    while let Some((id, path)) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let _ = list_dir(&path, &mut buf, |e| {
            if e.attrs & attr::DIRECTORY != 0 && e.attrs & attr::REPARSE_POINT == 0 {
                stack.push((e.file_id, format!("{path}\\{}", e.name)));
            }
            out.push(Change::Upsert {
                frn: e.file_id,
                parent_frn: id,
                name: e.name,
                attrs: e.attrs,
                meta: Some(e.meta),
            });
        });
    }
    out
}

/// Attributes, size and times of one path, or `None` if it does not exist.
pub fn stat(path: &str) -> Option<(u32, Meta)> {
    let md = std::fs::symlink_metadata(path).ok()?;
    Some((
        md.file_attributes(),
        Meta {
            size: md.file_size(),
            mtime: filetime_to_unix(md.last_write_time()),
            ctime: filetime_to_unix(md.creation_time()),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn record(name: &str, attrs: u32, size: u64, file_id: u64, last: bool) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let len = (104 + units.len() * 2).div_ceil(8) * 8;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(if last { 0 } else { len as u32 }).to_le_bytes());
        // 2026-01-01T00:00:00Z as FILETIME, created and written.
        r[8..16].copy_from_slice(&134_116_992_000_000_000u64.to_le_bytes());
        r[24..32].copy_from_slice(&134_116_992_010_000_000u64.to_le_bytes());
        r[40..48].copy_from_slice(&size.to_le_bytes());
        r[56..60].copy_from_slice(&attrs.to_le_bytes());
        r[60..64].copy_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        r[96..104].copy_from_slice(&file_id.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[104 + i * 2..106 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    fn parse_all(buf: &[u8]) -> Vec<DirEntry> {
        let mut v = Vec::new();
        parse_dir_buffer(buf, |e| v.push(e));
        v
    }

    #[test]
    fn dir_buffers_are_decoded() {
        let mut buf = Vec::new();
        buf.extend(record(".", attr::DIRECTORY, 0, 1, false));
        buf.extend(record("..", attr::DIRECTORY, 0, 2, false));
        buf.extend(record(
            "Docs",
            attr::DIRECTORY,
            0,
            0x0003_0000_0000_0040,
            false,
        ));
        buf.extend(record("résumé — 名前.txt", attr::HIDDEN, 1234, 77, true));
        buf.extend(vec![0xAAu8; 64]); // stale bytes after the last record
        let v = parse_all(&buf);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "Docs");
        assert_eq!(v[0].file_id, 0x0003_0000_0000_0040);
        assert!(v[0].attrs & attr::DIRECTORY != 0);
        assert_eq!(v[1].name, "résumé — 名前.txt");
        assert_eq!(v[1].meta.size, 1234);
        assert_eq!(v[1].meta.ctime, 1_767_225_600);
        assert_eq!(v[1].meta.mtime, 1_767_225_601);
    }

    #[test]
    fn damaged_dir_buffers_do_not_panic() {
        let good = {
            let mut b = record("alpha.txt", 0, 5, 9, false);
            b.extend(record("beta.txt", 0, 6, 10, true));
            b
        };
        assert_eq!(parse_all(&good).len(), 2);
        assert!(parse_all(&[]).is_empty());
        for cut in 0..good.len() {
            let _ = parse_all(&good[..cut]);
        }
        // A name length that runs past the buffer.
        let mut bad = good.clone();
        bad[60..64].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(parse_all(&bad).is_empty());
        // A next offset that points outside.
        let mut bad = good.clone();
        bad[0..4].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        assert_eq!(parse_all(&bad).len(), 1);
        // Garbage of every byte value.
        for b in [0u8, 1, 0x7F, 0xFF] {
            let _ = parse_all(&vec![b; 500]);
        }
    }

    #[test]
    fn extended_paths() {
        assert_eq!(extended_dir_path("C:"), "\\\\?\\C:\\");
        assert_eq!(extended_dir_path("C:\\Docs\\"), "\\\\?\\C:\\Docs\\");
        assert_eq!(
            extended_dir_path("\\\\server\\share\\x"),
            "\\\\?\\UNC\\server\\share\\x\\"
        );
        assert_eq!(extended_dir_path("\\\\?\\D:\\a"), "\\\\?\\D:\\a\\");
    }

    pub(crate) fn make_tree(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("findforge-walk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Docs").join("Old")).unwrap();
        std::fs::create_dir_all(root.join("Music")).unwrap();
        std::fs::create_dir_all(root.join("node_modules").join("pkg")).unwrap();
        std::fs::create_dir_all(root.join("Skip me").join("deep")).unwrap();
        std::fs::write(root.join("Docs").join("report.txt"), b"hello world").unwrap();
        std::fs::write(root.join("Docs").join("Old").join("a.tmp"), b"x").unwrap();
        std::fs::write(root.join("Docs").join("Old").join("keep.md"), b"12345").unwrap();
        std::fs::write(root.join("Music").join("song — ünï.mp3"), vec![7u8; 4000]).unwrap();
        std::fs::write(root.join("node_modules").join("pkg").join("index.js"), b"1").unwrap();
        std::fs::write(root.join("Skip me").join("deep").join("x.txt"), b"1").unwrap();
        // A path far longer than 260 characters.
        let mut long = root.join("Long");
        for i in 0..12 {
            long = long.join(format!("a-rather-long-folder-name-number-{i:02}"));
        }
        std::fs::create_dir_all(format!("\\\\?\\{}", long.display())).unwrap();
        std::fs::write(format!("\\\\?\\{}\\deep.txt", long.display()), b"deep").unwrap();
        root
    }

    fn rel_paths(store: &VolumeStore, root: &str) -> Vec<String> {
        let mut v: Vec<String> = (1..store.entries().len() as u32)
            .filter(|i| store.entry(*i).unwrap().is_live())
            .map(|i| store.path(i)[root.len()..].to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn walks_a_temp_tree_both_ways() {
        let root_buf = make_tree("both");
        let root = root_buf.to_string_lossy().into_owned();
        let none = Excludes::default();
        let cancel = AtomicBool::new(false);
        let seen = AtomicU64::new(0);
        let progress = |n: u64| {
            seen.fetch_max(n, Ordering::Relaxed);
        };
        let with_ids = walk_volume(&root, true, &none, &progress, &cancel).unwrap();
        let plain = walk_volume(&format!("{root}\\"), false, &none, &|_| {}, &cancel).unwrap();
        assert_eq!(with_ids.meta.kind, VolumeKind::NtfsWalk);
        assert_eq!(plain.meta.kind, VolumeKind::Walk);
        let a = rel_paths(&with_ids, &root);
        assert_eq!(a, rel_paths(&plain, &root));
        // 7 folders + 6 files, plus the long chain (13 folders + 1 file).
        assert_eq!(a.len(), 27, "{a:?}");
        assert_eq!(seen.load(Ordering::Relaxed) as usize, a.len());
        for want in [
            "\\Docs",
            "\\Docs\\Old\\keep.md",
            "\\Docs\\report.txt",
            "\\Music\\song — ünï.mp3",
            "\\node_modules\\pkg\\index.js",
        ] {
            assert!(a.iter().any(|p| p == want), "{want} missing from {a:?}");
        }
        assert!(a.iter().any(|p| p.ends_with("\\deep.txt") && p.len() > 300));

        // Sizes, kinds and ids.
        let report = with_ids
            .resolve_path(&format!("{root}\\Docs\\report.txt"))
            .unwrap();
        let e = with_ids.entry(report).unwrap();
        assert_eq!(e.size, 11);
        assert!(!e.is_dir() && e.mtime > 1_700_000_000 && e.ctime > 1_700_000_000);
        let docs = with_ids.resolve_path(&format!("{root}\\Docs")).unwrap();
        assert!(with_ids.entry(docs).unwrap().is_dir());
        let docs_id = file_id_of_dir(&format!("{root}\\Docs")).unwrap();
        assert_eq!(with_ids.slot_of(docs_id), Some(docs));
        assert_eq!(with_ids.slot_of(file_id_of_dir(&root).unwrap()), Some(0));
        assert_eq!(plain.slot_of(docs_id), None, "plain walks keep no ids");
        assert_eq!(
            stat(&format!("{root}\\Docs\\report.txt")).unwrap().1.size,
            11
        );
        assert!(stat(&format!("{root}\\missing")).is_none());

        // Excludes: a folder path and name patterns.
        let ex = Excludes::new(
            &[format!("{root}\\Skip me")],
            &["node_modules".to_string(), "*.tmp".to_string()],
        );
        let filtered = walk_volume(&root, true, &ex, &|_| {}, &cancel).unwrap();
        let f = rel_paths(&filtered, &root);
        assert!(!f
            .iter()
            .any(|p| p.contains("Skip me") || p.contains("node_modules") || p.ends_with(".tmp")));
        assert!(f.iter().any(|p| p == "\\Docs\\Old\\keep.md"));

        // A subtree as changes: applying them to a store without it gives the same paths.
        let changes = tree_changes(&format!("{root}\\Docs"), 1000);
        assert_eq!(changes.len(), 4);
        assert!(tree_changes(&format!("{root}\\Docs"), 1).len() <= 3);
        assert!(tree_changes(&format!("{root}\\nope"), 10).is_empty());

        // Cancelling and missing roots are errors, not panics.
        let cancelled = AtomicBool::new(true);
        assert!(walk_volume(&root, true, &none, &|_| {}, &cancelled).is_err());
        assert!(walk_volume(&format!("{root}\\missing"), true, &none, &|_| {}, &cancel).is_err());
        std::fs::remove_dir_all(format!("\\\\?\\{root}")).unwrap();
    }

    #[test]
    fn junctions_are_listed_but_not_followed() {
        let root_buf = make_tree("junction");
        let root = root_buf.to_string_lossy().into_owned();
        // `mklink /J` needs no special rights; skip quietly if it fails.
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(format!("{root}\\LinkToDocs"))
            .arg(format!("{root}\\Docs"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if made {
            let cancel = AtomicBool::new(false);
            let store = walk_volume(&root, true, &Excludes::default(), &|_| {}, &cancel).unwrap();
            let paths = rel_paths(&store, &root);
            assert!(paths.iter().any(|p| p == "\\LinkToDocs"));
            assert!(!paths.iter().any(|p| p.starts_with("\\LinkToDocs\\")));
            let _ = std::fs::remove_dir(format!("{root}\\LinkToDocs"));
        }
        std::fs::remove_dir_all(format!("\\\\?\\{root}")).unwrap();
    }

    /// Walks the system drive (read-only) and prints numbers only.
    /// `cargo test --release --lib walk::tests::bench -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark over a real drive; prints numbers only"]
    fn bench_system_drive() {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let store = walk_volume(&drive, true, &Excludes::default(), &|_| {}, &cancel).unwrap();
        let secs = started.elapsed().as_secs_f64();
        let n = store.live_count();
        println!(
            "walk: {n} entries in {secs:.2} s ({:.0} per second), index {:.1} MB, {:.1} bytes per entry",
            n as f64 / secs,
            store.memory_bytes() as f64 / 1_048_576.0,
            store.memory_bytes() as f64 / n as f64
        );
    }
}
