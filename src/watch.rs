//! Live change notifications for a folder tree (no administrator rights needed).
//!
//! `ReadDirectoryChangesExW` with extended information reports, for every
//! change below a folder, the NTFS file id of the file and of its parent
//! together with attributes, size and times: exactly what the index needs.

use crate::index::{filetime_to_unix, Change, Meta};
use crate::sys::wide;
use std::ffi::c_void;
use std::thread::JoinHandle;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, HANDLE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadDirectoryChangesExW, ReadDirectoryNotifyExtendedInformation,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
    FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// What a watcher reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// Changes keyed by NTFS file id, in the order they happened.
    Changes(Vec<Change>),
    /// Notifications were lost (too many at once): the tree must be rescanned.
    Overflow,
    /// The watch ended on its own (drive removed, access lost).
    Closed(String),
}

const ACTION_ADDED: u32 = 1;
const ACTION_REMOVED: u32 = 2;
const ACTION_MODIFIED: u32 = 3;
const ACTION_RENAMED_OLD: u32 = 4;
const ACTION_RENAMED_NEW: u32 = 5;

/// One decoded `FILE_NOTIFY_EXTENDED_INFORMATION` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub action: u32,
    pub file_id: u64,
    pub parent_id: u64,
    /// Path relative to the watched folder.
    pub rel_path: String,
    pub attrs: u32,
    pub meta: Meta,
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Decodes a notification buffer. Stops quietly at the first record that
/// does not fit.
pub(crate) fn parse_notices(buf: &[u8]) -> Vec<Notice> {
    // Field offsets of FILE_NOTIFY_EXTENDED_INFORMATION.
    const ACTION: usize = 4;
    const CREATION: usize = 8;
    const MODIFIED: usize = 16;
    const FILE_SIZE: usize = 48;
    const ATTRIBUTES: usize = 56;
    const FILE_ID: usize = 64;
    const PARENT_ID: usize = 72;
    const NAME_LENGTH: usize = 80;
    const NAME: usize = 84;
    let mut out = Vec::new();
    let mut off = 0usize;
    while let Some(rec) = buf.get(off..) {
        let (Some(next), Some(name_len)) = (u32_at(rec, 0), u32_at(rec, NAME_LENGTH)) else {
            break;
        };
        let Some(name) = rec.get(NAME..NAME + name_len as usize) else {
            break;
        };
        let units: Vec<u16> = name
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let (Some(action), Some(ctime), Some(mtime), Some(size), Some(attrs), Some(id), Some(pid)) = (
            u32_at(rec, ACTION),
            u64_at(rec, CREATION),
            u64_at(rec, MODIFIED),
            u64_at(rec, FILE_SIZE),
            u32_at(rec, ATTRIBUTES),
            u64_at(rec, FILE_ID),
            u64_at(rec, PARENT_ID),
        ) else {
            break;
        };
        out.push(Notice {
            action,
            file_id: id,
            parent_id: pid,
            rel_path: String::from_utf16_lossy(&units),
            attrs,
            meta: Meta {
                size,
                mtime: filetime_to_unix(mtime),
                ctime: filetime_to_unix(ctime),
            },
        });
        if next == 0 {
            break;
        }
        off = match off.checked_add(next as usize) {
            Some(o) if o > off => o,
            _ => break,
        };
    }
    out
}

/// A rename's "old name" half that is waiting for its "new name" half.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingOld {
    file_id: u64,
    rel_path: String,
}

fn delete_for(root: &str, file_id: u64, rel_path: &str) -> Change {
    if file_id != 0 {
        Change::Delete { frn: file_id }
    } else {
        Change::DeletePath {
            path: format!("{}\\{rel_path}", root.trim_end_matches('\\')),
        }
    }
}

/// Turns notices into index changes. A trailing "renamed, old name" record
/// is kept in `pending`: its other half normally opens the next buffer.
pub(crate) fn notices_to_changes(
    root: &str,
    notices: Vec<Notice>,
    pending: &mut Option<PendingOld>,
) -> Vec<Change> {
    let mut out = Vec::new();
    for n in notices {
        if let Some(old) = pending.take() {
            let same_file = n.action == ACTION_RENAMED_NEW
                && (old.file_id == 0 || n.file_id == 0 || old.file_id == n.file_id);
            if !same_file {
                // The file left the tree (or the new name got lost): it is gone.
                out.push(delete_for(root, old.file_id, &old.rel_path));
            }
        }
        match n.action {
            ACTION_ADDED | ACTION_MODIFIED | ACTION_RENAMED_NEW => {
                let name = n.rel_path.rsplit('\\').next().unwrap_or("").to_string();
                if n.file_id == 0 || name.is_empty() {
                    continue;
                }
                out.push(Change::Upsert {
                    frn: n.file_id,
                    parent_frn: n.parent_id,
                    name,
                    attrs: n.attrs,
                    meta: Some(n.meta),
                });
            }
            ACTION_REMOVED => out.push(delete_for(root, n.file_id, &n.rel_path)),
            ACTION_RENAMED_OLD => {
                *pending = Some(PendingOld {
                    file_id: n.file_id,
                    rel_path: n.rel_path,
                })
            }
            _ => {}
        }
    }
    out
}

/// Flushes a rename that never got its second half.
pub(crate) fn flush_pending(root: &str, pending: &mut Option<PendingOld>) -> Option<Change> {
    pending
        .take()
        .map(|old| delete_for(root, old.file_id, &old.rel_path))
}

struct SendHandle(HANDLE);
// SAFETY: a kernel handle may be used from any thread.
unsafe impl Send for SendHandle {}

/// Watches a whole tree. Dropping the watcher stops it.
pub struct Watcher {
    stop: SendHandle,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Starts watching `root` (`C:` or a folder path) and everything below it.
    /// `on_event` is called from a background thread.
    pub fn start(
        root: &str,
        on_event: Box<dyn Fn(WatchEvent) + Send + 'static>,
    ) -> Result<Watcher, String> {
        let root = root.trim_end_matches('\\').to_string();
        let w = wide(&crate::walk::extended_dir_path(&root));
        // SAFETY: valid path; handles are closed by the thread or below.
        let dir = unsafe {
            CreateFileW(
                PCWSTR(w.as_ptr()),
                FILE_LIST_DIRECTORY.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                HANDLE::default(),
            )
        }
        .map_err(|e| format!("Could not watch {root}: {}", e.message()))?;
        // SAFETY: plain event objects.
        let events = unsafe {
            (
                CreateEventW(None, true, false, PCWSTR::null()),
                CreateEventW(None, true, false, PCWSTR::null()),
            )
        };
        let (stop, io) = match events {
            (Ok(stop), Ok(io)) => (stop, io),
            (a, b) => {
                // SAFETY: closing whatever was opened.
                unsafe {
                    let _ = CloseHandle(dir);
                    if let Ok(h) = a {
                        let _ = CloseHandle(h);
                    }
                    if let Ok(h) = b {
                        let _ = CloseHandle(h);
                    }
                }
                return Err(format!("Could not watch {root}."));
            }
        };
        let (dir_s, stop_s, io_s) = (SendHandle(dir), SendHandle(stop), SendHandle(io));
        // The first read is issued before `start` returns, so nothing that
        // happens after it can be missed.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("findforge-watch".into())
            .spawn(move || {
                let (dir, stop, io) = (dir_s, stop_s, io_s);
                run(&root, dir.0, stop.0, io.0, &on_event, ready_tx);
                // SAFETY: the thread owns these two handles.
                unsafe {
                    let _ = CloseHandle(dir.0);
                    let _ = CloseHandle(io.0);
                }
            })
            .map_err(|e| format!("Could not start the watcher: {e}"))?;
        let watcher = Watcher {
            stop: SendHandle(stop),
            thread: Some(thread),
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(watcher),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("The watcher stopped unexpectedly.".into()),
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // SAFETY: the stop event is owned by this value.
        unsafe {
            let _ = SetEvent(self.stop.0);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // SAFETY: the thread has ended; nobody else uses the event.
        unsafe {
            let _ = CloseHandle(self.stop.0);
        }
    }
}

fn run(
    root: &str,
    dir: HANDLE,
    stop: HANDLE,
    io: HANDLE,
    on_event: &(dyn Fn(WatchEvent) + Send),
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    // 8-byte aligned, 256 KB.
    let mut buf = vec![0u64; 32 * 1024];
    let bytes = (buf.len() * 8) as u32;
    let filter = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION;
    let mut pending: Option<PendingOld> = None;
    let mut ready = Some(ready);
    loop {
        let mut overlapped = OVERLAPPED {
            hEvent: io,
            ..Default::default()
        };
        // SAFETY: the buffer and the OVERLAPPED outlive the request: this
        // function does not return before the request completed or was
        // cancelled and reaped below.
        let started = unsafe {
            ReadDirectoryChangesExW(
                dir,
                buf.as_mut_ptr() as *mut c_void,
                bytes,
                true,
                filter,
                None,
                Some(&mut overlapped),
                None,
                ReadDirectoryNotifyExtendedInformation,
            )
        };
        if let Err(e) = started {
            // SAFETY: reading the thread's last error code.
            let code = unsafe { GetLastError() };
            if code != ERROR_IO_PENDING {
                let msg = format!("Could not watch {root}: {}", e.message());
                match ready.take() {
                    Some(tx) => {
                        let _ = tx.send(Err(msg));
                    }
                    None => {
                        if code == ERROR_NOTIFY_ENUM_DIR {
                            on_event(WatchEvent::Overflow);
                            continue;
                        }
                        on_event(WatchEvent::Closed(msg));
                    }
                }
                return;
            }
        }
        if let Some(tx) = ready.take() {
            let _ = tx.send(Ok(()));
        }
        // Wait for notifications or for the stop signal. While half a rename
        // is pending, only wait briefly for the other half.
        let stopped = loop {
            let timeout = if pending.is_some() { 250 } else { u32::MAX };
            // SAFETY: both handles are valid events.
            let w = unsafe { WaitForMultipleObjects(&[stop, io], false, timeout) };
            if w == WAIT_TIMEOUT {
                if let Some(c) = flush_pending(root, &mut pending) {
                    on_event(WatchEvent::Changes(vec![c]));
                }
                continue;
            }
            break w == WAIT_OBJECT_0;
        };
        if stopped {
            // SAFETY: cancel the outstanding read and wait until the kernel
            // has let go of the buffer.
            unsafe {
                let _ = CancelIoEx(dir, Some(&overlapped));
                let mut n = 0u32;
                let _ = GetOverlappedResult(dir, &overlapped, &mut n, true);
            }
            return;
        }
        let mut n = 0u32;
        // SAFETY: the request has completed (its event is set).
        let done = unsafe { GetOverlappedResult(dir, &overlapped, &mut n, false) };
        if let Err(e) = done {
            // SAFETY: reading the thread's last error code.
            if unsafe { GetLastError() } == ERROR_NOTIFY_ENUM_DIR {
                on_event(WatchEvent::Overflow);
                continue;
            }
            on_event(WatchEvent::Closed(e.message().to_string()));
            return;
        }
        if n == 0 {
            // The system could not fit the changes in the buffer.
            pending = None;
            on_event(WatchEvent::Overflow);
            continue;
        }
        // SAFETY: the kernel wrote `n` bytes into the buffer.
        let view = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, n as usize) };
        let changes = notices_to_changes(root, parse_notices(view), &mut pending);
        if !changes.is_empty() {
            on_event(WatchEvent::Changes(changes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{Excludes, VolumeStore};
    use crate::walk;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::{channel, Receiver};
    use std::time::{Duration, Instant};

    fn record(action: u32, id: u64, parent: u64, path: &str, size: u64, last: bool) -> Vec<u8> {
        let units: Vec<u16> = path.encode_utf16().collect();
        let len = (84 + units.len() * 2).div_ceil(8) * 8;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(if last { 0 } else { len as u32 }).to_le_bytes());
        r[4..8].copy_from_slice(&action.to_le_bytes());
        r[8..16].copy_from_slice(&134_116_992_000_000_000u64.to_le_bytes());
        r[16..24].copy_from_slice(&134_116_992_020_000_000u64.to_le_bytes());
        r[48..56].copy_from_slice(&size.to_le_bytes());
        r[56..60].copy_from_slice(&0x20u32.to_le_bytes());
        r[64..72].copy_from_slice(&id.to_le_bytes());
        r[72..80].copy_from_slice(&parent.to_le_bytes());
        r[80..84].copy_from_slice(&((units.len() * 2) as u32).to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[84 + i * 2..86 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    #[test]
    fn notices_are_decoded_and_mapped() {
        let mut buf = Vec::new();
        buf.extend(record(
            ACTION_ADDED,
            100,
            50,
            "Docs\\new file.txt",
            12,
            false,
        ));
        buf.extend(record(
            ACTION_MODIFIED,
            100,
            50,
            "Docs\\new file.txt",
            99,
            false,
        ));
        buf.extend(record(
            ACTION_RENAMED_OLD,
            100,
            50,
            "Docs\\new file.txt",
            99,
            false,
        ));
        buf.extend(record(
            ACTION_RENAMED_NEW,
            100,
            60,
            "Other\\renamed.txt",
            99,
            false,
        ));
        buf.extend(record(
            ACTION_REMOVED,
            100,
            60,
            "Other\\renamed.txt",
            0,
            false,
        ));
        buf.extend(record(ACTION_REMOVED, 0, 0, "Other\\no id.txt", 0, true));
        let notices = parse_notices(&buf);
        assert_eq!(notices.len(), 6);
        assert_eq!(notices[3].rel_path, "Other\\renamed.txt");
        assert_eq!(notices[1].meta.size, 99);
        assert_eq!(notices[0].meta.ctime, 1_767_225_600);
        assert_eq!(notices[0].meta.mtime, 1_767_225_602);
        let mut pending = None;
        let changes = notices_to_changes("C:\\Watched\\", notices, &mut pending);
        assert!(pending.is_none());
        assert_eq!(changes.len(), 5);
        assert!(matches!(
            &changes[0],
            Change::Upsert { frn: 100, parent_frn: 50, name, meta: Some(m), .. }
                if name == "new file.txt" && m.size == 12
        ));
        assert!(matches!(
            &changes[2],
            Change::Upsert { frn: 100, parent_frn: 60, name, .. } if name == "renamed.txt"
        ));
        assert_eq!(changes[3], Change::Delete { frn: 100 });
        assert_eq!(
            changes[4],
            Change::DeletePath {
                path: "C:\\Watched\\Other\\no id.txt".into()
            }
        );
    }

    #[test]
    fn a_rename_split_over_two_buffers() {
        let mut pending = None;
        let first = parse_notices(&record(ACTION_RENAMED_OLD, 7, 5, "a.txt", 1, true));
        assert!(notices_to_changes("C:", first, &mut pending).is_empty());
        assert!(pending.is_some());
        let second = parse_notices(&record(ACTION_RENAMED_NEW, 7, 5, "b.txt", 1, true));
        let changes = notices_to_changes("C:", second, &mut pending);
        assert_eq!(changes.len(), 1);
        assert!(matches!(&changes[0], Change::Upsert { frn: 7, name, .. } if name == "b.txt"));

        // The other half never comes: the file left the tree.
        let first = parse_notices(&record(ACTION_RENAMED_OLD, 8, 5, "gone.txt", 1, true));
        assert!(notices_to_changes("C:", first, &mut pending).is_empty());
        let other = parse_notices(&record(ACTION_ADDED, 9, 5, "else.txt", 1, true));
        let changes = notices_to_changes("C:", other, &mut pending);
        assert_eq!(changes[0], Change::Delete { frn: 8 });
        assert_eq!(changes.len(), 2);
        let first = parse_notices(&record(ACTION_RENAMED_OLD, 10, 5, "late.txt", 1, true));
        notices_to_changes("C:", first, &mut pending);
        assert_eq!(
            flush_pending("C:", &mut pending),
            Some(Change::Delete { frn: 10 })
        );
        assert_eq!(flush_pending("C:", &mut pending), None);
    }

    #[test]
    fn damaged_buffers_do_not_panic() {
        let good = {
            let mut b = record(ACTION_ADDED, 1, 2, "one.txt", 5, false);
            b.extend(record(ACTION_REMOVED, 3, 2, "two.txt", 0, true));
            b
        };
        assert_eq!(parse_notices(&good).len(), 2);
        assert!(parse_notices(&[]).is_empty());
        for cut in 0..good.len() {
            let _ = parse_notices(&good[..cut]);
        }
        let mut bad = good.clone();
        bad[80..84].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(parse_notices(&bad).is_empty());
        let mut bad = good.clone();
        bad[0..4].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        assert_eq!(parse_notices(&bad).len(), 1);
        for b in [0u8, 1, 0x7F, 0xFF] {
            let _ = parse_notices(&vec![b; 700]);
        }
        // Unknown actions are ignored.
        let odd = parse_notices(&record(99, 1, 2, "x", 0, true));
        assert!(notices_to_changes("C:", odd, &mut None).is_empty());
    }

    /// Applies watcher events to `store` until `done(store)` holds.
    fn pump(
        rx: &Receiver<WatchEvent>,
        store: &mut VolumeStore,
        seen: &mut Vec<WatchEvent>,
        done: impl Fn(&VolumeStore) -> bool,
    ) -> bool {
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(15) {
            if done(store) {
                return true;
            }
            if let Ok(ev) = rx.recv_timeout(Duration::from_millis(100)) {
                if let WatchEvent::Changes(changes) = &ev {
                    for c in changes {
                        store.apply(c);
                    }
                    if store.needs_sweep() {
                        store.sweep_orphans();
                    }
                }
                seen.push(ev);
            }
        }
        done(store)
    }

    #[test]
    fn live_changes_in_a_temp_folder_reach_the_index() {
        let root_buf =
            std::env::temp_dir().join(format!("findforge-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root_buf);
        std::fs::create_dir_all(root_buf.join("Docs")).unwrap();
        std::fs::create_dir_all(root_buf.join("Archive")).unwrap();
        std::fs::write(root_buf.join("Docs").join("old.txt"), b"old").unwrap();
        let root = root_buf.to_string_lossy().into_owned();
        let p = |rel: &str| format!("{root}\\{rel}");

        let (tx, rx) = channel();
        let watcher = Watcher::start(
            &root,
            Box::new(move |e| {
                let _ = tx.send(e);
            }),
        )
        .expect("watcher starts");
        let cancel = AtomicBool::new(false);
        let mut store =
            walk::walk_volume(&root, true, &Excludes::default(), &|_| {}, &cancel).unwrap();
        let mut seen = Vec::new();
        let has = |s: &VolumeStore, rel: &str| s.resolve_path(&format!("{root}\\{rel}")).is_some();

        // Create and write.
        std::fs::write(p("Docs\\new.txt"), b"hello").unwrap();
        assert!(pump(&rx, &mut store, &mut seen, |s| {
            s.resolve_path(&p("Docs\\new.txt"))
                .and_then(|slot| s.entry(slot))
                .map(|e| e.size == 5)
                .unwrap_or(false)
        }));
        std::fs::write(p("Docs\\new.txt"), b"hello, bigger now").unwrap();
        assert!(pump(&rx, &mut store, &mut seen, |s| {
            s.resolve_path(&p("Docs\\new.txt"))
                .and_then(|slot| s.entry(slot))
                .map(|e| e.size == 17)
                .unwrap_or(false)
        }));
        // Rename in place, then move to another folder.
        std::fs::rename(p("Docs\\new.txt"), p("Docs\\renamed.txt")).unwrap();
        assert!(pump(&rx, &mut store, &mut seen, |s| {
            has(s, "Docs\\renamed.txt") && !has(s, "Docs\\new.txt")
        }));
        std::fs::rename(p("Docs\\renamed.txt"), p("Archive\\moved.txt")).unwrap();
        assert!(pump(&rx, &mut store, &mut seen, |s| {
            has(s, "Archive\\moved.txt") && !has(s, "Docs\\renamed.txt")
        }));
        // A new folder with a file, then rename the folder: the file follows.
        std::fs::create_dir(p("Fresh")).unwrap();
        std::fs::write(p("Fresh\\inside.txt"), b"1").unwrap();
        assert!(pump(&rx, &mut store, &mut seen, |s| has(
            s,
            "Fresh\\inside.txt"
        )));
        std::fs::rename(p("Fresh"), p("Fresher")).unwrap();
        assert!(pump(&rx, &mut store, &mut seen, |s| {
            has(s, "Fresher\\inside.txt") && !has(s, "Fresh")
        }));
        // Delete a file, then a folder with its contents.
        std::fs::remove_file(p("Docs\\old.txt")).unwrap();
        assert!(pump(&rx, &mut store, &mut seen, |s| !has(
            s,
            "Docs\\old.txt"
        )));
        std::fs::remove_dir_all(p("Fresher")).unwrap();
        assert!(pump(&rx, &mut store, &mut seen, |s| {
            !has(s, "Fresher") && !has(s, "Fresher\\inside.txt")
        }));

        // The index now equals a fresh walk.
        let fresh = walk::walk_volume(&root, true, &Excludes::default(), &|_| {}, &cancel).unwrap();
        let list = |s: &VolumeStore| {
            let mut v: Vec<String> = (0..s.entries().len() as u32)
                .filter(|i| s.entry(*i).unwrap().is_live())
                .map(|i| s.path(i))
                .collect();
            v.sort();
            v
        };
        assert_eq!(list(&store), list(&fresh));
        assert!(!seen.iter().any(|e| matches!(e, WatchEvent::Closed(_))));
        // What does Windows report for removals? Deletes by id are expected.
        let by_id = seen.iter().any(|e| {
            matches!(e, WatchEvent::Changes(c) if c.iter().any(|x| matches!(x, Change::Delete { .. })))
        });
        let by_path = seen.iter().any(|e| {
            matches!(e, WatchEvent::Changes(c) if c.iter().any(|x| matches!(x, Change::DeletePath { .. })))
        });
        println!("removals reported by id: {by_id}, by path: {by_path}");
        drop(watcher);
        while rx.try_recv().is_ok() {}
        // After the watcher is gone, changes are no longer reported.
        std::fs::write(p("after.txt"), b"x").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        std::fs::remove_dir_all(&root_buf).unwrap();
    }

    #[test]
    fn watching_a_missing_folder_fails_cleanly() {
        let missing = std::env::temp_dir().join("findforge-watch-missing-folder-xyz");
        let r = Watcher::start(&missing.to_string_lossy(), Box::new(|_| {}));
        assert!(r.is_err());
    }
}
