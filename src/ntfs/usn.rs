//! Parsing of change journal (USN) records and their translation into index
//! changes. Pure functions over byte slices.

use super::UsnEvent;
use crate::index::Change;
use std::collections::HashSet;

pub(crate) const REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
pub(crate) const REASON_DATA_EXTEND: u32 = 0x0000_0002;
pub(crate) const REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
pub(crate) const REASON_FILE_CREATE: u32 = 0x0000_0100;
pub(crate) const REASON_FILE_DELETE: u32 = 0x0000_0200;
pub(crate) const REASON_RENAME_OLD_NAME: u32 = 0x0000_1000;
pub(crate) const REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
pub(crate) const REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;
pub(crate) const REASON_HARD_LINK_CHANGE: u32 = 0x0001_0000;
pub(crate) const REASON_COMPRESSION_CHANGE: u32 = 0x0002_0000;
pub(crate) const REASON_ENCRYPTION_CHANGE: u32 = 0x0004_0000;
pub(crate) const REASON_REPARSE_POINT_CHANGE: u32 = 0x0010_0000;
pub(crate) const REASON_CLOSE: u32 = 0x8000_0000;

/// Reasons after which size, times or attributes may differ.
const CONTENT_REASONS: u32 = REASON_DATA_OVERWRITE
    | REASON_DATA_EXTEND
    | REASON_DATA_TRUNCATION
    | REASON_BASIC_INFO_CHANGE
    | REASON_COMPRESSION_CHANGE
    | REASON_ENCRYPTION_CHANGE
    | REASON_REPARSE_POINT_CHANGE;

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Decodes one record (version 2 or 3). `None` for other versions or
/// records that do not hold together.
fn parse_record(rec: &[u8]) -> Option<UsnEvent> {
    let major = u16_at(rec, 4)?;
    // (file id, parent id, usn, reason, attributes, name length, name offset)
    let (frn, parent, usn, reason, attrs, name_len, name_off) = match major {
        2 => (
            u64_at(rec, 8)?,
            u64_at(rec, 16)?,
            u64_at(rec, 24)? as i64,
            u32_at(rec, 40)?,
            u32_at(rec, 52)?,
            u16_at(rec, 56)? as usize,
            u16_at(rec, 58)? as usize,
        ),
        3 => {
            // 128-bit ids; on NTFS the upper halves are zero.
            if u64_at(rec, 16)? != 0 || u64_at(rec, 32)? != 0 {
                return None;
            }
            (
                u64_at(rec, 8)?,
                u64_at(rec, 24)?,
                u64_at(rec, 40)? as i64,
                u32_at(rec, 56)?,
                u32_at(rec, 68)?,
                u16_at(rec, 72)? as usize,
                u16_at(rec, 74)? as usize,
            )
        }
        _ => return None,
    };
    let name = rec.get(name_off..name_off.checked_add(name_len)?)?;
    if name_len == 0 || name_len % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = name
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(UsnEvent {
        usn,
        frn,
        parent_frn: parent,
        name: String::from_utf16_lossy(&units),
        attrs,
        reason,
    })
}

/// Decodes the output of a journal read: the position to continue from,
/// followed by records. Returns that position and the records understood;
/// `None` when the buffer is too short to hold even the position. Parsing
/// stops quietly at the first record whose length makes no sense.
pub(crate) fn parse_usn_records(buf: &[u8]) -> Option<(i64, Vec<UsnEvent>)> {
    let next = u64_at(buf, 0)? as i64;
    let mut events = Vec::new();
    let mut off = 8usize;
    while off + 8 <= buf.len() {
        let Some(len) = u32_at(buf, off) else { break };
        let len = len as usize;
        // Records are at least a header long and 8-byte aligned.
        if len < 60 || !len.is_multiple_of(8) || off + len > buf.len() {
            break;
        }
        if let Some(ev) = parse_record(&buf[off..off + len]) {
            events.push(ev);
        }
        off += len;
    }
    Some((next, events))
}

/// Turns journal records into index changes, in order. The second list holds
/// the ids of files whose size or times may have changed (creates and data
/// changes): the caller looks those up on disk afterwards.
pub fn changes_from_events(events: &[UsnEvent]) -> (Vec<Change>, Vec<u64>) {
    let mut changes = Vec::new();
    let mut stat: Vec<u64> = Vec::new();
    let mut stat_set: HashSet<u64> = HashSet::new();
    fn want_stat(frn: u64, stat: &mut Vec<u64>, set: &mut HashSet<u64>) {
        if set.insert(frn) {
            stat.push(frn);
        }
    }
    for e in events {
        let r = e.reason;
        if r & REASON_FILE_DELETE != 0 {
            // The file is gone once its last handle closes.
            if r & REASON_CLOSE != 0 {
                changes.push(Change::Delete { frn: e.frn });
                if stat_set.remove(&e.frn) {
                    stat.retain(|f| *f != e.frn);
                }
            }
            continue;
        }
        let names_it = r & (REASON_FILE_CREATE | REASON_RENAME_NEW_NAME) != 0;
        if r & REASON_RENAME_OLD_NAME != 0 && r & REASON_RENAME_NEW_NAME == 0 {
            continue; // the record with the new name follows
        }
        if r & REASON_HARD_LINK_CHANGE != 0 && !names_it {
            continue; // names another link of the same file: do not rename
        }
        let upsert = || Change::Upsert {
            frn: e.frn,
            parent_frn: e.parent_frn,
            name: e.name.clone(),
            attrs: e.attrs,
            meta: None,
        };
        if names_it {
            changes.push(upsert());
            if r & (REASON_FILE_CREATE | REASON_CLOSE) != 0 {
                want_stat(e.frn, &mut stat, &mut stat_set);
            }
        } else if r & REASON_CLOSE != 0 && r & CONTENT_REASONS != 0 {
            changes.push(upsert());
            want_stat(e.frn, &mut stat, &mut stat_set);
        }
    }
    (changes, stat)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::index::{attr, Applied, Excludes, Meta, VolumeBuilder, VolumeKind, VolumeMeta};

    pub(crate) fn v2(
        usn: i64,
        frn: u64,
        parent: u64,
        name: &str,
        attrs: u32,
        reason: u32,
    ) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let len = (60 + units.len() * 2).div_ceil(8) * 8;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[60 + i * 2..62 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    fn v3(usn: i64, frn: u64, parent: u64, name: &str, reason: u32) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let len = (76 + units.len() * 2).div_ceil(8) * 8;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&3u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[24..32].copy_from_slice(&parent.to_le_bytes());
        r[40..48].copy_from_slice(&usn.to_le_bytes());
        r[56..60].copy_from_slice(&reason.to_le_bytes());
        r[72..74].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
        r[74..76].copy_from_slice(&76u16.to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            r[76 + i * 2..78 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        r
    }

    pub(crate) fn buffer(next: i64, records: &[Vec<u8>]) -> Vec<u8> {
        let mut b = next.to_le_bytes().to_vec();
        for r in records {
            b.extend_from_slice(r);
        }
        b
    }

    #[test]
    fn records_are_decoded() {
        let buf = buffer(
            4096,
            &[
                v2(
                    100,
                    0x0003_0000_0000_0040,
                    0x0005_0000_0000_0005,
                    "new — ünï.txt",
                    0x20,
                    REASON_FILE_CREATE,
                ),
                v3(200, 77, 5, "third.bin", REASON_FILE_DELETE | REASON_CLOSE),
            ],
        );
        let (next, events) = parse_usn_records(&buf).unwrap();
        assert_eq!(next, 4096);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            UsnEvent {
                usn: 100,
                frn: 0x0003_0000_0000_0040,
                parent_frn: 0x0005_0000_0000_0005,
                name: "new — ünï.txt".into(),
                attrs: 0x20,
                reason: REASON_FILE_CREATE,
            }
        );
        assert_eq!(events[1].frn, 77);
        assert_eq!(events[1].parent_frn, 5);
        assert_eq!(events[1].name, "third.bin");
        assert_eq!(events[1].usn, 200);
        // Only the position: no records.
        assert_eq!(parse_usn_records(&buffer(7, &[])), Some((7, vec![])));
    }

    #[test]
    fn damaged_buffers_are_handled() {
        let good = buffer(
            999,
            &[
                v2(1, 10, 5, "a.txt", 0, REASON_FILE_CREATE),
                v2(2, 11, 5, "b.txt", 0, REASON_FILE_CREATE),
            ],
        );
        assert_eq!(parse_usn_records(&good).unwrap().1.len(), 2);
        assert!(parse_usn_records(&[]).is_none());
        assert!(parse_usn_records(&[1, 2, 3]).is_none());
        for cut in 8..good.len() {
            let (next, events) = parse_usn_records(&good[..cut]).unwrap();
            assert_eq!(next, 999);
            assert!(events.len() <= 2);
        }
        // Record length 0, tiny, unaligned, or past the end.
        for len in [0u32, 8, 59, 61, 0xFFFF_FFF8] {
            let mut bad = good.clone();
            bad[8..12].copy_from_slice(&len.to_le_bytes());
            assert!(
                parse_usn_records(&bad).unwrap().1.is_empty(),
                "length {len}"
            );
        }
        // Name offset / length out of range: that record is skipped, the next one read.
        let mut bad = good.clone();
        bad[8 + 58..8 + 60].copy_from_slice(&5000u16.to_le_bytes());
        let (_, events) = parse_usn_records(&bad).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].name, "b.txt");
        let mut bad = good.clone();
        bad[8 + 56..8 + 58].copy_from_slice(&5000u16.to_le_bytes());
        assert_eq!(parse_usn_records(&bad).unwrap().1.len(), 1);
        // Unknown major version: skipped.
        let mut bad = good.clone();
        bad[8 + 4..8 + 6].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(parse_usn_records(&bad).unwrap().1.len(), 1);
        // Version 3 with ids that do not fit 64 bits: skipped.
        let mut big = v3(1, 10, 5, "x", REASON_FILE_CREATE);
        big[16] = 1;
        assert!(parse_usn_records(&buffer(1, &[big])).unwrap().1.is_empty());
        for b in [0u8, 0x3C, 0x40, 0xFF] {
            let _ = parse_usn_records(&vec![b; 300]);
        }
    }

    fn ev(frn: u64, parent: u64, name: &str, attrs: u32, reason: u32) -> UsnEvent {
        UsnEvent {
            usn: 0,
            frn,
            parent_frn: parent,
            name: name.into(),
            attrs,
            reason,
        }
    }

    #[test]
    fn events_become_changes() {
        // Create, write, close.
        let (changes, stat) = changes_from_events(&[
            ev(40, 5, "new.txt", 0x20, REASON_FILE_CREATE),
            ev(
                40,
                5,
                "new.txt",
                0x20,
                REASON_FILE_CREATE | REASON_DATA_EXTEND,
            ),
            ev(
                40,
                5,
                "new.txt",
                0x20,
                REASON_FILE_CREATE | REASON_DATA_EXTEND | REASON_CLOSE,
            ),
        ]);
        assert_eq!(changes.len(), 3);
        assert!(changes.iter().all(|c| matches!(
            c,
            Change::Upsert {
                frn: 40,
                meta: None,
                ..
            }
        )));
        assert_eq!(stat, vec![40]);

        // Rename: the old-name record is ignored.
        let (changes, stat) = changes_from_events(&[
            ev(40, 5, "new.txt", 0x20, REASON_RENAME_OLD_NAME),
            ev(40, 9, "moved.txt", 0x20, REASON_RENAME_NEW_NAME),
            ev(
                40,
                9,
                "moved.txt",
                0x20,
                REASON_RENAME_NEW_NAME | REASON_CLOSE,
            ),
        ]);
        assert_eq!(changes.len(), 2);
        assert!(
            matches!(&changes[0], Change::Upsert { parent_frn: 9, name, .. } if name == "moved.txt")
        );
        assert_eq!(stat, vec![40]);

        // Overwrite without close: nothing yet; with close: refresh.
        let (changes, stat) =
            changes_from_events(&[ev(40, 9, "moved.txt", 0x20, REASON_DATA_OVERWRITE)]);
        assert!(changes.is_empty() && stat.is_empty());
        let (changes, stat) = changes_from_events(&[ev(
            40,
            9,
            "moved.txt",
            0x22,
            REASON_DATA_OVERWRITE | REASON_BASIC_INFO_CHANGE | REASON_CLOSE,
        )]);
        assert!(matches!(&changes[0], Change::Upsert { attrs: 0x22, .. }));
        assert_eq!(stat, vec![40]);

        // Delete: only on close.
        let (changes, _) = changes_from_events(&[ev(40, 9, "moved.txt", 0, REASON_FILE_DELETE)]);
        assert!(changes.is_empty());
        let (changes, stat) =
            changes_from_events(&[ev(40, 9, "moved.txt", 0, REASON_FILE_DELETE | REASON_CLOSE)]);
        assert_eq!(changes, vec![Change::Delete { frn: 40 }]);
        assert!(stat.is_empty());

        // A temp file created and deleted in one batch leaves nothing to look up.
        let (changes, stat) = changes_from_events(&[
            ev(50, 5, "~tmp", 0, REASON_FILE_CREATE),
            ev(50, 5, "~tmp", 0, REASON_FILE_CREATE | REASON_CLOSE),
            ev(50, 5, "~tmp", 0, REASON_FILE_DELETE | REASON_CLOSE),
            ev(51, 5, "kept", 0, REASON_FILE_CREATE | REASON_CLOSE),
        ]);
        assert_eq!(
            changes.last().unwrap(),
            &Change::Upsert {
                frn: 51,
                parent_frn: 5,
                name: "kept".into(),
                attrs: 0,
                meta: None
            }
        );
        assert!(changes.contains(&Change::Delete { frn: 50 }));
        assert_eq!(stat, vec![51]);

        // Created, deleted and created again: looked up once.
        let (_, stat) = changes_from_events(&[
            ev(61, 5, "a", 0, REASON_FILE_CREATE | REASON_CLOSE),
            ev(61, 5, "a", 0, REASON_FILE_DELETE | REASON_CLOSE),
            ev(61, 5, "a", 0, REASON_FILE_CREATE | REASON_CLOSE),
        ]);
        assert_eq!(stat, vec![61]);

        // A record number reused right after a delete is looked up.
        let (_, stat) = changes_from_events(&[
            ev(60, 5, "a", 0, REASON_FILE_DELETE | REASON_CLOSE),
            ev(60, 5, "b", 0, REASON_FILE_CREATE | REASON_CLOSE),
        ]);
        assert_eq!(stat, vec![60]);

        // A new hard link must not rename the entry; nor do pure security changes matter.
        let (changes, stat) = changes_from_events(&[
            ev(
                40,
                77,
                "second-link.txt",
                0,
                REASON_HARD_LINK_CHANGE | REASON_CLOSE,
            ),
            ev(40, 9, "moved.txt", 0, 0x800 | REASON_CLOSE),
            ev(40, 9, "moved.txt", 0, REASON_CLOSE),
        ]);
        assert!(changes.is_empty() && stat.is_empty());
        assert_eq!(changes_from_events(&[]), (vec![], vec![]));
    }

    #[test]
    fn journal_changes_applied_to_an_index() {
        const DIR: u32 = attr::DIRECTORY;
        let mut b = VolumeBuilder::new(VolumeMeta::new("C:", VolumeKind::NtfsMft));
        b.set_root_frn(5);
        b.push(30, 5, "Docs", DIR, Meta::default());
        b.push(31, 5, "Archive", DIR, Meta::default());
        b.push(
            40,
            30,
            "old.txt",
            0,
            Meta {
                size: 9,
                mtime: 1,
                ctime: 1,
            },
        );
        let mut store = b.finish(&Excludes::default(), true);
        let mut apply = |events: &[UsnEvent]| -> (Vec<Applied>, Vec<u64>) {
            let (changes, stat) = changes_from_events(events);
            let applied = changes.iter().map(|c| store.apply(c)).collect();
            if store.needs_sweep() {
                store.sweep_orphans();
            }
            (applied, stat)
        };

        // Create a file and a folder with a file in it.
        let (applied, stat) = apply(&[
            ev(50, 30, "report.docx", 0x20, REASON_FILE_CREATE),
            ev(
                50,
                30,
                "report.docx",
                0x20,
                REASON_FILE_CREATE | REASON_DATA_EXTEND | REASON_CLOSE,
            ),
            ev(51, 30, "Sub", DIR, REASON_FILE_CREATE | REASON_CLOSE),
            ev(52, 51, "inner.txt", 0x20, REASON_FILE_CREATE | REASON_CLOSE),
        ]);
        assert!(matches!(applied[0], Applied::Created(_)));
        assert_eq!(
            applied[1],
            Applied::Ignored,
            "the same facts again change nothing"
        );
        assert_eq!(stat, vec![50, 51, 52]);
        // Rename in place.
        let (applied, _) = apply(&[
            ev(50, 30, "report.docx", 0x20, REASON_RENAME_OLD_NAME),
            ev(
                50,
                30,
                "final report.docx",
                0x20,
                REASON_RENAME_NEW_NAME | REASON_CLOSE,
            ),
        ]);
        assert!(matches!(applied[0], Applied::Renamed(_)));
        // Move the folder: its contents follow.
        let (applied, _) = apply(&[
            ev(51, 30, "Sub", DIR, REASON_RENAME_OLD_NAME),
            ev(51, 31, "Sub", DIR, REASON_RENAME_NEW_NAME | REASON_CLOSE),
        ]);
        assert!(matches!(applied[0], Applied::Renamed(_)));
        // Delete a file.
        let (applied, _) = apply(&[ev(40, 30, "old.txt", 0, REASON_FILE_DELETE | REASON_CLOSE)]);
        assert!(matches!(applied[0], Applied::Deleted(_)));
        drop(apply);
        let list = |s: &crate::index::VolumeStore| {
            let mut v: Vec<String> = (1..s.entries().len() as u32)
                .filter(|i| s.entry(*i).unwrap().is_live())
                .map(|i| s.path(i))
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            list(&store),
            vec![
                "C:\\Archive",
                "C:\\Archive\\Sub",
                "C:\\Archive\\Sub\\inner.txt",
                "C:\\Docs",
                "C:\\Docs\\final report.docx",
            ]
        );
        // Delete the folder (the journal reports children first, but even if
        // a child record were lost the sweep removes what is left behind).
        let (changes, _) =
            changes_from_events(&[ev(51, 31, "Sub", DIR, REASON_FILE_DELETE | REASON_CLOSE)]);
        for c in &changes {
            store.apply(c);
        }
        assert!(store.needs_sweep());
        assert_eq!(store.sweep_orphans(), 1);
        assert_eq!(
            list(&store),
            vec!["C:\\Archive", "C:\\Docs", "C:\\Docs\\final report.docx"]
        );
        // Events for files in folders the index does not know are ignored.
        let (changes, _) = changes_from_events(&[ev(
            90,
            12345,
            "elsewhere",
            0,
            REASON_FILE_CREATE | REASON_CLOSE,
        )]);
        assert_eq!(store.apply(&changes[0]), Applied::Ignored);
    }
}
