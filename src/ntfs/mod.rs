//! NTFS: reading the master file table and the change journal.
//!
//! The parsing lives in [`mft`] and [`usn`] and is pure; this file holds the
//! few calls that touch the volume device (which need administrator rights)
//! and the scan loop, which is written against a small reader trait so that
//! it can be tested on an in-memory image.

pub(crate) mod mft;
pub(crate) mod usn;

use crate::index::{Excludes, VolumeBuilder, VolumeKind, VolumeMeta, VolumeStore};
use crate::sys::wide;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL,
    NTFS_VOLUME_DATA_BUFFER, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
};
use windows::Win32::System::IO::{DeviceIoControl, OVERLAPPED};

pub use usn::changes_from_events;

/// One record of the change journal, decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsnEvent {
    pub usn: i64,
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    /// Win32 file attributes.
    pub attrs: u32,
    /// `USN_REASON_*` bits.
    pub reason: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    /// The oldest record still in the journal.
    pub first_usn: i64,
    /// The record that will be written next.
    pub next_usn: i64,
}

// --------------------------------------------------------------------------------------
// Reading raw bytes
// --------------------------------------------------------------------------------------

/// Reads raw bytes of a volume. Offsets and lengths are always multiples of
/// the sector size.
pub(crate) trait VolumeReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String>;
}

/// A page-aligned byte buffer (raw device reads want aligned memory).
struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

impl AlignedBuf {
    const ALIGN: usize = 4096;

    fn new(len: usize) -> AlignedBuf {
        let len = len.max(Self::ALIGN);
        let layout = std::alloc::Layout::from_size_align(len, Self::ALIGN).expect("layout");
        // SAFETY: the layout has a non-zero size.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        AlignedBuf { ptr, len }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` points to `len` initialised bytes owned by self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.len, Self::ALIGN).expect("layout");
        // SAFETY: allocated in `new` with the same layout.
        unsafe { std::alloc::dealloc(self.ptr, layout) };
    }
}

/// An open volume device (`\\.\C:`).
pub(crate) struct Device {
    handle: HANDLE,
}

// SAFETY: a kernel handle may be used from any thread.
unsafe impl Send for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

const GENERIC_READ: u32 = 0x8000_0000;
const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;

fn explain(e: &windows::core::Error, what: &str) -> String {
    if e.code().0 == E_ACCESSDENIED {
        format!("Administrator rights are needed to {what}.")
    } else {
        format!("Could not {what}: {}", e.message())
    }
}

impl Device {
    /// Opens `drive` (`C:`) for reading. Nothing is ever written through it.
    fn open(drive: &str) -> Result<Device, String> {
        let letter = drive.trim_end_matches('\\');
        let ok = letter.len() == 2
            && letter.as_bytes()[0].is_ascii_alphabetic()
            && letter.as_bytes()[1] == b':';
        if !ok {
            return Err(format!("\"{drive}\" is not a drive letter."));
        }
        let path = wide(&format!("\\\\.\\{letter}"));
        // SAFETY: valid NUL-terminated path; the handle is owned by Device.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                HANDLE::default(),
            )
        }
        .map_err(|e| explain(&e, &format!("read drive {letter} directly")))?;
        Ok(Device { handle })
    }

    /// A control call with an optional input structure and an output buffer.
    fn ioctl<I>(
        &self,
        code: u32,
        input: Option<&I>,
        out: *mut c_void,
        out_len: u32,
    ) -> windows::core::Result<u32> {
        let mut returned = 0u32;
        // SAFETY: the input (if any) and output buffers are valid for the
        // sizes passed and outlive the call.
        unsafe {
            DeviceIoControl(
                self.handle,
                code,
                input.map(|i| i as *const I as *const c_void),
                input.map(|_| std::mem::size_of::<I>() as u32).unwrap_or(0),
                Some(out),
                out_len,
                Some(&mut returned),
                None,
            )?;
        }
        Ok(returned)
    }

    fn volume_data(&self) -> Result<NTFS_VOLUME_DATA_BUFFER, String> {
        // The call may append an extended block: leave room for it.
        let mut buf = [0u64; 32];
        self.ioctl::<()>(
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            buf.as_mut_ptr() as *mut c_void,
            std::mem::size_of_val(&buf) as u32,
        )
        .map_err(|e| explain(&e, "read the volume's layout"))?;
        // SAFETY: the buffer is larger than the structure, 8-byte aligned and
        // was filled in by the call.
        Ok(unsafe { std::ptr::read(buf.as_ptr() as *const NTFS_VOLUME_DATA_BUFFER) })
    }

    fn journal_data(&self) -> windows::core::Result<USN_JOURNAL_DATA_V0> {
        let mut buf = [0u64; 16];
        self.ioctl::<()>(
            FSCTL_QUERY_USN_JOURNAL,
            None,
            buf.as_mut_ptr() as *mut c_void,
            std::mem::size_of_val(&buf) as u32,
        )?;
        // SAFETY: as in `volume_data`.
        Ok(unsafe { std::ptr::read(buf.as_ptr() as *const USN_JOURNAL_DATA_V0) })
    }
}

impl VolumeReader for Device {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        // Raw volume reads want aligned memory: bounce through an aligned
        // buffer when the caller's is not.
        if !(buf.as_ptr() as usize).is_multiple_of(AlignedBuf::ALIGN) {
            let mut tmp = AlignedBuf::new(buf.len());
            let n = buf.len();
            self.read_at(offset, &mut tmp.as_mut_slice()[..n])?;
            buf.copy_from_slice(&tmp.as_mut_slice()[..n]);
            return Ok(());
        }
        let mut done = 0usize;
        while done < buf.len() {
            let pos = offset + done as u64;
            let take = (buf.len() - done).min(32 << 20);
            let mut ov = OVERLAPPED::default();
            ov.Anonymous.Anonymous.Offset = pos as u32;
            ov.Anonymous.Anonymous.OffsetHigh = (pos >> 32) as u32;
            let mut got = 0u32;
            // SAFETY: the slice is valid for `take` bytes; the OVERLAPPED only
            // carries the offset (the handle is synchronous).
            unsafe {
                ReadFile(
                    self.handle,
                    Some(&mut buf[done..done + take]),
                    Some(&mut got),
                    Some(&mut ov),
                )
            }
            .map_err(|e| explain(&e, "read the master file table"))?;
            if got == 0 {
                return Err("The drive returned less data than expected.".into());
            }
            done += got as usize;
        }
        Ok(())
    }
}

// --------------------------------------------------------------------------------------
// The scan
// --------------------------------------------------------------------------------------

/// Where the master file table is and how it is laid out.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry {
    pub sector: u64,
    pub cluster: u64,
    pub record_size: usize,
    /// Volume offset of the first MFT record, in bytes.
    pub mft_offset: u64,
    /// How much of the `$MFT` file is in use, in bytes.
    pub mft_valid_len: u64,
}

const CHUNK: usize = 8 << 20;

/// Reads and parses the whole master file table through `reader`.
pub(crate) fn scan_with(
    reader: &dyn VolumeReader,
    geo: &Geometry,
    meta: VolumeMeta,
    excludes: &Excludes,
    progress: &(dyn Fn(u64) + Sync),
    cancel: &AtomicBool,
) -> Result<VolumeStore, String> {
    let odd = || "This drive has a layout FindForge does not understand.".to_string();
    let pow2 = |v: u64| v != 0 && v & (v - 1) == 0;
    if !pow2(geo.sector)
        || !pow2(geo.cluster)
        || !pow2(geo.record_size as u64)
        || !(512..=65_536).contains(&geo.record_size)
        || geo.sector > 65_536
        || geo.cluster > (CHUNK as u64)
        || geo.cluster < geo.sector
        || !geo.mft_offset.is_multiple_of(geo.sector)
    {
        return Err(odd());
    }
    let record_size = geo.record_size;
    // The first record describes where the rest of the table is.
    let first_len = (record_size as u64).div_ceil(geo.sector) * geo.sector;
    let mut first = AlignedBuf::new(first_len as usize);
    reader.read_at(
        geo.mft_offset,
        &mut first.as_mut_slice()[..first_len as usize],
    )?;
    let extents = mft::mft_extents(
        &mut first.as_mut_slice()[..record_size],
        geo.cluster,
        geo.sector,
        &mut |off, buf| reader.read_at(off, buf),
    )?;
    if extents.first().map(|e| e.volume_offset) != Some(geo.mft_offset) {
        return Err(odd());
    }
    let table_len: u64 = extents.iter().map(|e| e.len).sum();
    let total = geo.mft_valid_len.min(table_len) / record_size as u64 * record_size as u64;
    let mut assembler = mft::Assembler::new(VolumeBuilder::with_capacity(
        meta,
        (total / record_size as u64) as usize,
    ));
    let mut buf = AlignedBuf::new(CHUNK);
    let mut pos = 0u64;
    while pos < total {
        if cancel.load(Ordering::Relaxed) {
            return Err("Cancelled.".into());
        }
        let want = ((total - pos).min(CHUNK as u64)).div_ceil(geo.sector) * geo.sector;
        let want = want.min(table_len - pos) as usize;
        // Fill the chunk piece by piece from the extents it spans.
        let mut done = 0usize;
        while done < want {
            let at = pos + done as u64;
            let ext = extents
                .iter()
                .find(|e| at >= e.file_offset && at < e.file_offset + e.len)
                .ok_or_else(odd)?;
            let within = at - ext.file_offset;
            let take = ((ext.len - within) as usize).min(want - done);
            reader.read_at(
                ext.volume_offset + within,
                &mut buf.as_mut_slice()[done..done + take],
            )?;
            done += take;
        }
        let usable = (want as u64).min(total - pos) as usize / record_size * record_size;
        let first_record = pos / record_size as u64;
        for r in mft::parse_chunk(&mut buf.as_mut_slice()[..usable], record_size, first_record) {
            assembler.add(r);
        }
        progress(assembler.count());
        pos += CHUNK as u64;
    }
    Ok(assembler.finish(excludes))
}

/// Indexes a whole NTFS volume by reading its master file table.
/// `drive` is `C:`. Needs administrator rights.
///
/// The returned store has kind `NtfsMft` and `meta.journal_id` /
/// `meta.next_usn` set to the journal position taken *before* the scan, so
/// replaying the journal from there catches everything that changed meanwhile.
/// `progress` is called now and then with the number of entries found so far.
pub fn scan_volume(
    drive: &str,
    excludes: &Excludes,
    progress: &(dyn Fn(u64) + Sync),
    cancel: &AtomicBool,
) -> Result<VolumeStore, String> {
    let device = Device::open(drive)?;
    let data = device.volume_data()?;
    let mut meta = VolumeMeta::new(drive, VolumeKind::NtfsMft);
    // No journal on this volume is not an error: the index is then refreshed
    // by rescanning. FindForge never creates a journal (that would be a write).
    if let Ok(j) = device.journal_data() {
        meta.journal_id = j.UsnJournalID;
        meta.next_usn = j.NextUsn;
    }
    if data.MftStartLcn < 0 || data.MftValidDataLength < 0 {
        return Err("This drive has a layout FindForge does not understand.".into());
    }
    let geo = Geometry {
        sector: data.BytesPerSector as u64,
        cluster: data.BytesPerCluster as u64,
        record_size: data.BytesPerFileRecordSegment as usize,
        mft_offset: (data.MftStartLcn as u64)
            .checked_mul(data.BytesPerCluster as u64)
            .ok_or("This drive has a layout FindForge does not understand.")?,
        mft_valid_len: data.MftValidDataLength as u64,
    };
    scan_with(&device, &geo, meta, excludes, progress, cancel)
}

// --------------------------------------------------------------------------------------
// The change journal
// --------------------------------------------------------------------------------------

/// An open change journal of one volume.
pub struct Journal {
    device: Device,
    buf: Vec<u64>,
}

/// At most this many records are returned by one [`Journal::read`], so a
/// long backlog is applied in pieces.
const MAX_EVENTS_PER_READ: usize = 100_000;

impl Journal {
    /// Opens the journal of `drive` (`C:`). Needs administrator rights.
    pub fn open(drive: &str) -> Result<Journal, String> {
        let device = Device::open(drive)?;
        device
            .journal_data()
            .map_err(|e| explain(&e, "read the change journal"))?;
        Ok(Journal {
            device,
            buf: vec![0u64; 16 * 1024],
        })
    }

    pub fn info(&self) -> Result<JournalInfo, String> {
        let j = self
            .device
            .journal_data()
            .map_err(|e| explain(&e, "read the change journal"))?;
        Ok(JournalInfo {
            journal_id: j.UsnJournalID,
            first_usn: j.FirstUsn,
            next_usn: j.NextUsn,
        })
    }

    /// Reads every record from `from_usn` up to the current end of the
    /// journal without waiting for new ones, appends them to `out` and
    /// returns the position to continue from. An error means the position is
    /// no longer valid (journal recreated or wrapped): rescan the volume.
    pub fn read(
        &mut self,
        journal_id: u64,
        from_usn: i64,
        out: &mut Vec<UsnEvent>,
    ) -> Result<i64, String> {
        let mut pos = from_usn;
        let started = out.len();
        loop {
            let request = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: pos,
                ReasonMask: u32::MAX,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: 0,
                UsnJournalID: journal_id,
            };
            let bytes = self
                .device
                .ioctl(
                    FSCTL_READ_USN_JOURNAL,
                    Some(&request),
                    self.buf.as_mut_ptr() as *mut c_void,
                    (self.buf.len() * 8) as u32,
                )
                .map_err(|e| explain(&e, "read the change journal"))?;
            // SAFETY: the call wrote `bytes` bytes into the buffer.
            let view = unsafe {
                std::slice::from_raw_parts(self.buf.as_ptr() as *const u8, bytes as usize)
            };
            let Some((next, events)) = usn::parse_usn_records(view) else {
                return Ok(pos);
            };
            let got = events.len();
            out.extend(events);
            if next <= pos || bytes <= 8 {
                return Ok(next.max(pos));
            }
            pos = next;
            if got == 0 || out.len() - started >= MAX_EVENTS_PER_READ {
                return Ok(pos);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mft::testkit::*;
    use super::*;
    use crate::index::attr;
    use std::cell::Cell;

    const SECTOR: u64 = 512;
    const CLUSTER: u64 = 4096;
    const REC: usize = 1024;

    /// A volume held in memory. Checks that every read is sector-aligned.
    struct Image {
        bytes: Vec<u8>,
        reads: Cell<usize>,
        fail_at: Option<u64>,
    }

    impl VolumeReader for Image {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            assert_eq!(offset % SECTOR, 0, "offset must be sector-aligned");
            assert_eq!(
                buf.len() as u64 % SECTOR,
                0,
                "length must be sector-aligned"
            );
            self.reads.set(self.reads.get() + 1);
            if let Some(bad) = self.fail_at {
                if offset <= bad && bad < offset + buf.len() as u64 {
                    return Err("The drive reported a read error.".into());
                }
            }
            let end = offset as usize + buf.len();
            let src = self
                .bytes
                .get(offset as usize..end)
                .ok_or("read past the end of the volume")?;
            buf.copy_from_slice(src);
            Ok(())
        }
    }

    const WIN32: u8 = 1;
    const DOS: u8 = 2;

    fn frn(record: u64, seq: u64) -> u64 {
        record | seq << 48
    }

    /// An image whose master file table is in two pieces: clusters 4..12
    /// (records 0..32) and clusters 40..48 (records 32..64). The second piece
    /// is described by an extension record, as on a badly fragmented volume.
    fn fragmented_image() -> (Image, Geometry) {
        let mut bytes = vec![0u8; 64 * CLUSTER as usize];
        let mut put = |record: u64, spec: &RecordSpec| {
            let cluster = if record < 32 { 4 } else { 40 - 8 };
            let off = cluster * CLUSTER as usize + record as usize * REC;
            bytes[off..off + REC].copy_from_slice(&build(REC, spec));
        };
        // Record 0: $MFT. First extent only, plus an attribute list that
        // points at record 20 for the rest.
        let mut first_run = run(8, 4);
        first_run.push(0);
        let mut list = Vec::new();
        for (vcn, rec) in [(0u64, 0u64), (8, 20)] {
            let mut e = vec![0u8; 0x20];
            e[0..4].copy_from_slice(&mft::ATTR_DATA.to_le_bytes());
            e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
            e[8..16].copy_from_slice(&vcn.to_le_bytes());
            e[16..24].copy_from_slice(&frn(rec, 1).to_le_bytes());
            list.extend(e);
        }
        put(
            0,
            &RecordSpec::file(
                1,
                vec![
                    std_info(1, 1, attr::HIDDEN | attr::SYSTEM),
                    resident(mft::ATTR_ATTRIBUTE_LIST, 0, &list),
                    file_name(frn(5, 5), "$MFT", 3, 0),
                    non_resident(mft::ATTR_DATA, 0, 0, 64 * REC as u64, &first_run),
                ],
            ),
        );
        let mut second_run = run(8, 40);
        second_run.push(0);
        put(
            20,
            &RecordSpec {
                base_ref: frn(0, 1),
                ..RecordSpec::file(1, vec![non_resident(mft::ATTR_DATA, 0, 8, 0, &second_run)])
            },
        );
        put(
            5,
            &RecordSpec::dir(5, vec![std_info(1, 1, 0), file_name(frn(5, 5), ".", 3, 0)]),
        );
        put(
            11,
            &RecordSpec::dir(11, vec![file_name(frn(5, 5), "$Extend", 3, 0)]),
        );
        put(
            25,
            &RecordSpec::file(1, vec![file_name(frn(11, 11), "$UsnJrnl", 3, 0)]),
        );
        put(
            30,
            &RecordSpec::dir(
                2,
                vec![
                    std_info(1_600_000_000, 1_650_000_000, 0),
                    file_name(frn(5, 5), "DOCUME~1", DOS, 0),
                    file_name(frn(5, 5), "Documents and notes", WIN32, 0),
                ],
            ),
        );
        put(
            31,
            &RecordSpec::file(
                4,
                vec![
                    std_info(1_700_000_000, 1_700_000_500, attr::READONLY),
                    file_name(frn(30, 2), "report — final.txt", WIN32, 0),
                    resident(mft::ATTR_DATA, 0, b"twenty-two bytes here!"),
                ],
            ),
        );
        // In the second piece of the table.
        put(
            40,
            &RecordSpec::dir(
                1,
                vec![
                    std_info(5, 6, attr::HIDDEN),
                    file_name(frn(5, 5), "Music", WIN32, 0),
                ],
            ),
        );
        let mut data_run = run(2000, 50);
        data_run.push(0);
        put(
            41,
            &RecordSpec::file(
                9,
                vec![
                    std_info(7, 8, 0),
                    file_name(frn(40, 1), "song.flac", WIN32, 0),
                    non_resident(mft::ATTR_DATA, 0, 0, 8_000_000, &data_run),
                ],
            ),
        );
        let mut deleted = RecordSpec::file(3, vec![file_name(frn(40, 1), "deleted.tmp", WIN32, 0)]);
        deleted.in_use = false;
        put(42, &deleted);
        put(
            63,
            &RecordSpec::file(1, vec![file_name(frn(30, 2), "last record.bin", WIN32, 77)]),
        );
        // An orphan: its folder does not exist.
        put(
            50,
            &RecordSpec::file(1, vec![file_name(frn(999, 1), "orphan", WIN32, 0)]),
        );
        let image = Image {
            bytes,
            reads: Cell::new(0),
            fail_at: None,
        };
        let geo = Geometry {
            sector: SECTOR,
            cluster: CLUSTER,
            record_size: REC,
            mft_offset: 4 * CLUSTER,
            mft_valid_len: 64 * REC as u64,
        };
        (image, geo)
    }

    fn meta() -> VolumeMeta {
        VolumeMeta::new("C:", VolumeKind::NtfsMft)
    }

    fn all_paths(store: &VolumeStore) -> Vec<String> {
        let mut v: Vec<String> = (1..store.entries().len() as u32)
            .filter(|i| store.entry(*i).unwrap().is_live())
            .map(|i| store.path(i))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn scans_a_fragmented_image() {
        let (image, geo) = fragmented_image();
        let seen = std::sync::atomic::AtomicU64::new(0);
        let cancel = AtomicBool::new(false);
        let store = scan_with(
            &image,
            &geo,
            meta(),
            &Excludes::default(),
            &|n| seen.store(n, Ordering::Relaxed),
            &cancel,
        )
        .unwrap();
        assert_eq!(
            all_paths(&store),
            vec![
                "C:\\Documents and notes",
                "C:\\Documents and notes\\last record.bin",
                "C:\\Documents and notes\\report — final.txt",
                "C:\\Music",
                "C:\\Music\\song.flac",
            ]
        );
        assert!(seen.load(Ordering::Relaxed) >= 5);
        assert_eq!(store.meta.kind, VolumeKind::NtfsMft);
        let report = store
            .resolve_path("C:\\Documents and notes\\report — final.txt")
            .unwrap();
        let e = store.entry(report).unwrap();
        assert_eq!(e.size, 22);
        assert_eq!((e.ctime, e.mtime), (1_700_000_000, 1_700_000_500));
        assert!(e.flags & crate::index::flags::READONLY != 0);
        assert_eq!(store.slot_of(frn(31, 4)), Some(report));
        let song = store.resolve_path("C:\\Music\\song.flac").unwrap();
        assert_eq!(store.entry(song).unwrap().size, 8_000_000);
        let music = store.resolve_path("C:\\Music").unwrap();
        let m = store.entry(music).unwrap();
        assert!(m.is_dir() && m.flags & crate::index::flags::HIDDEN != 0 && m.size == 0);
        assert_eq!(
            store
                .entry(
                    store
                        .resolve_path("C:\\Documents and notes\\last record.bin")
                        .unwrap()
                )
                .unwrap()
                .size,
            77
        );
        assert_eq!(store.slot_of(frn(5, 5)), Some(0));
        assert_eq!(store.garbage_ratio(), 0.0);

        // Excludes are applied.
        let ex = Excludes::new(&["C:\\Music".to_string()], &["*.bin".to_string()]);
        let filtered = scan_with(&image, &geo, meta(), &ex, &|_| {}, &cancel).unwrap();
        assert_eq!(
            all_paths(&filtered),
            vec![
                "C:\\Documents and notes",
                "C:\\Documents and notes\\report — final.txt"
            ]
        );
    }

    #[test]
    fn the_valid_length_limits_what_is_read() {
        let (image, mut geo) = fragmented_image();
        geo.mft_valid_len = 41 * REC as u64; // records 0..=40 only
        let cancel = AtomicBool::new(false);
        let store =
            scan_with(&image, &geo, meta(), &Excludes::default(), &|_| {}, &cancel).unwrap();
        assert_eq!(
            all_paths(&store),
            vec![
                "C:\\Documents and notes",
                "C:\\Documents and notes\\report — final.txt",
                "C:\\Music",
            ]
        );
        // A length that is not a whole number of records is rounded down.
        geo.mft_valid_len = 31 * REC as u64 + 100;
        let store =
            scan_with(&image, &geo, meta(), &Excludes::default(), &|_| {}, &cancel).unwrap();
        assert_eq!(all_paths(&store), vec!["C:\\Documents and notes"]);
    }

    #[test]
    fn bad_volumes_are_errors_not_panics() {
        let cancel = AtomicBool::new(false);
        let none = Excludes::default();
        let (image, geo) = fragmented_image();
        // Cancelled.
        let cancelled = AtomicBool::new(true);
        assert!(scan_with(&image, &geo, meta(), &none, &|_| {}, &cancelled).is_err());
        // Nonsense geometry.
        for g in [
            Geometry { sector: 0, ..geo },
            Geometry { sector: 500, ..geo },
            Geometry { cluster: 0, ..geo },
            Geometry {
                cluster: 256,
                ..geo
            },
            Geometry {
                record_size: 0,
                ..geo
            },
            Geometry {
                record_size: 1000,
                ..geo
            },
            Geometry {
                record_size: 1 << 20,
                ..geo
            },
            Geometry {
                mft_offset: 4 * CLUSTER + 7,
                ..geo
            },
        ] {
            assert!(scan_with(&image, &g, meta(), &none, &|_| {}, &cancel).is_err());
        }
        // The table is not where the volume says.
        let wrong = Geometry {
            mft_offset: 20 * CLUSTER,
            ..geo
        };
        assert!(scan_with(&image, &wrong, meta(), &none, &|_| {}, &cancel).is_err());
        let past = Geometry {
            mft_offset: 4000 * CLUSTER,
            ..geo
        };
        assert!(scan_with(&image, &past, meta(), &none, &|_| {}, &cancel).is_err());
        // A read error in the first record, in the extension record, in the data.
        for bad in [
            4 * CLUSTER,
            4 * CLUSTER + 20 * REC as u64,
            40 * CLUSTER + 10,
        ] {
            let (mut image, geo) = fragmented_image();
            image.fail_at = Some(bad);
            assert!(
                scan_with(&image, &geo, meta(), &none, &|_| {}, &cancel).is_err(),
                "{bad}"
            );
        }
        // The first record is rubbish / its cluster list is rubbish.
        let (mut image, geo) = fragmented_image();
        image.bytes[4 * CLUSTER as usize] = b'X';
        assert!(scan_with(&image, &geo, meta(), &none, &|_| {}, &cancel).is_err());
        let (mut image, geo) = fragmented_image();
        let start = 4 * CLUSTER as usize;
        for b in image.bytes[start + 0x38..start + 500].iter_mut() {
            *b = 0x99;
        }
        assert!(scan_with(&image, &geo, meta(), &none, &|_| {}, &cancel).is_err());
        // The extension record that describes the second piece is damaged.
        let (mut image, geo) = fragmented_image();
        image.bytes[4 * CLUSTER as usize + 20 * REC] = b'X';
        assert!(scan_with(&image, &geo, meta(), &none, &|_| {}, &cancel).is_err());
        // Random damage anywhere in the table never panics.
        let (clean, geo) = fragmented_image();
        let mut seed = 0x1357_9bdf_2468_ace0u64;
        for _ in 0..300 {
            let mut image = Image {
                bytes: clean.bytes.clone(),
                reads: Cell::new(0),
                fail_at: None,
            };
            for _ in 0..40 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let table = if seed & 1 == 0 { 4 } else { 40 } * CLUSTER as usize;
                let pos = table + (seed >> 8) as usize % (8 * CLUSTER as usize);
                image.bytes[pos] = (seed >> 40) as u8;
            }
            let _ = scan_with(&image, &geo, meta(), &none, &|_| {}, &cancel);
        }
    }

    #[test]
    fn a_contiguous_table_with_small_clusters() {
        // 512-byte clusters and 1024-byte records: every record spans two clusters.
        let cluster = 512u64;
        let records = 24usize;
        let mut bytes = vec![0u8; 200 * cluster as usize];
        let base = 16 * cluster as usize;
        let mut put = |record: usize, spec: &RecordSpec| {
            let off = base + record * REC;
            bytes[off..off + REC].copy_from_slice(&build(REC, spec));
        };
        let mut runs = run((records * REC) as u64 / cluster, 16);
        runs.push(0);
        put(
            0,
            &RecordSpec::file(
                1,
                vec![non_resident(
                    mft::ATTR_DATA,
                    0,
                    0,
                    (records * REC) as u64,
                    &runs,
                )],
            ),
        );
        put(
            5,
            &RecordSpec::dir(5, vec![file_name(frn(5, 5), ".", 3, 0)]),
        );
        put(
            16,
            &RecordSpec::dir(1, vec![file_name(frn(5, 5), "Folder", WIN32, 0)]),
        );
        put(
            23,
            &RecordSpec::file(1, vec![file_name(frn(16, 1), "file.txt", WIN32, 5)]),
        );
        let image = Image {
            bytes,
            reads: Cell::new(0),
            fail_at: None,
        };
        let geo = Geometry {
            sector: 512,
            cluster,
            record_size: REC,
            mft_offset: 16 * cluster,
            mft_valid_len: (records * REC) as u64,
        };
        let cancel = AtomicBool::new(false);
        let store = scan_with(
            &image,
            &geo,
            VolumeMeta::new("E:", VolumeKind::NtfsMft),
            &Excludes::default(),
            &|_| {},
            &cancel,
        )
        .unwrap();
        assert_eq!(
            all_paths(&store),
            vec!["E:\\Folder", "E:\\Folder\\file.txt"]
        );
        assert!(image.reads.get() >= 2);
    }

    #[test]
    fn device_names_are_checked() {
        for bad in ["", "C", "CD:", "\\\\server\\share", "1:", "C:\\Folder"] {
            assert!(Device::open(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn opening_a_drive_without_rights_explains_itself() {
        // Without administrator rights this fails with a clear sentence; with
        // them it succeeds. Either way nothing is written and nothing panics.
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        match Device::open(&drive) {
            Ok(device) => {
                let data = device.volume_data().expect("volume layout");
                assert!(data.BytesPerFileRecordSegment >= 512);
            }
            Err(e) => assert!(e.contains("Administrator rights"), "{e}"),
        }
    }
}
