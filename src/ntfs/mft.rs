//! Parsing of NTFS master file table records. Everything here works on
//! byte slices and never trusts what it reads: damaged input yields `None`
//! or an error, never a panic.

use crate::index::{attr, filetime_to_unix, Excludes, Meta, VolumeBuilder, VolumeStore};
use std::collections::HashMap;

pub(crate) const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub(crate) const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub(crate) const ATTR_FILE_NAME: u32 = 0x30;
pub(crate) const ATTR_DATA: u32 = 0x80;
const ATTR_END: u32 = 0xFFFF_FFFF;

const FLAG_IN_USE: u16 = 0x1;
const FLAG_DIRECTORY: u16 = 0x2;
/// The update sequence array protects every 512 bytes, whatever the sector size.
const STRIDE: usize = 512;
/// Record number of the root folder.
pub(crate) const ROOT_RECORD: u64 = 5;
/// Records below this are the file system's own files.
const FIRST_USER_RECORD: u64 = 16;

const NAMESPACE_POSIX: u8 = 0;
const NAMESPACE_WIN32: u8 = 1;
const NAMESPACE_DOS: u8 = 2;
const NAMESPACE_WIN32_AND_DOS: u8 = 3;

#[inline]
fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

#[inline]
fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

#[inline]
fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Checks the record signature and undoes the update sequence protection in
/// place. Returns false for anything that is not a sound `FILE` record.
pub(crate) fn apply_fixup(rec: &mut [u8]) -> bool {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return false;
    }
    let (Some(usa_off), Some(usa_count)) = (u16_at(rec, 4), u16_at(rec, 6)) else {
        return false;
    };
    let (usa_off, usa_count) = (usa_off as usize, usa_count as usize);
    if usa_count == 0 {
        return false;
    }
    let sectors = usa_count - 1;
    if usa_off < 42 || usa_off + usa_count * 2 > rec.len() || sectors * STRIDE > rec.len() {
        return false;
    }
    // A record is at least one stride long; all of it must be protected.
    if sectors == 0 || sectors * STRIDE < rec.len().min(STRIDE) {
        return false;
    }
    let usn = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..=sectors {
        let end = i * STRIDE;
        if rec[end - 2..end] != usn {
            return false; // torn write
        }
    }
    for i in 1..=sectors {
        let end = i * STRIDE;
        rec[end - 2] = rec[usa_off + i * 2];
        rec[end - 1] = rec[usa_off + i * 2 + 1];
    }
    true
}

/// One attribute of a record.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Attribute<'a> {
    pub kind: u32,
    /// The whole attribute, header included.
    pub raw: &'a [u8],
    pub non_resident: bool,
    /// Length of the attribute's own name in characters (0 = unnamed).
    pub name_len: u8,
}

impl<'a> Attribute<'a> {
    /// The value of a resident attribute.
    pub fn value(&self) -> Option<&'a [u8]> {
        if self.non_resident {
            return None;
        }
        let len = u32_at(self.raw, 0x10)? as usize;
        let off = u16_at(self.raw, 0x14)? as usize;
        self.raw.get(off..off.checked_add(len)?)
    }

    pub fn start_vcn(&self) -> Option<u64> {
        self.non_resident.then(|| u64_at(self.raw, 0x10)).flatten()
    }

    /// Real (logical) size of a non-resident attribute.
    pub fn real_size(&self) -> Option<u64> {
        self.non_resident.then(|| u64_at(self.raw, 0x30)).flatten()
    }

    /// The encoded data runs of a non-resident attribute.
    pub fn runs(&self) -> Option<&'a [u8]> {
        if !self.non_resident {
            return None;
        }
        let off = u16_at(self.raw, 0x20)? as usize;
        self.raw.get(off..)
    }
}

/// Iterates over the attributes of a record whose fixups have been applied.
pub(crate) struct Attributes<'a> {
    rec: &'a [u8],
    off: usize,
    end: usize,
}

pub(crate) fn attributes(rec: &[u8]) -> Attributes<'_> {
    let first = u16_at(rec, 0x14).unwrap_or(u16::MAX) as usize;
    let used = u32_at(rec, 0x18).unwrap_or(0) as usize;
    Attributes {
        rec,
        off: first,
        end: used.min(rec.len()),
    }
}

impl<'a> Iterator for Attributes<'a> {
    type Item = Attribute<'a>;

    fn next(&mut self) -> Option<Attribute<'a>> {
        if self.off < 0x30 || self.off + 8 > self.end {
            return None;
        }
        let kind = u32_at(self.rec, self.off)?;
        if kind == ATTR_END {
            return None;
        }
        let len = u32_at(self.rec, self.off + 4)? as usize;
        // A sound attribute is at least a header long, 8-byte aligned and
        // inside the used part of the record.
        if len < 0x18 || !len.is_multiple_of(8) || self.off + len > self.end {
            self.off = usize::MAX;
            return None;
        }
        let raw = &self.rec[self.off..self.off + len];
        self.off += len;
        Some(Attribute {
            kind,
            raw,
            non_resident: raw[8] != 0,
            name_len: raw[9],
        })
    }
}

/// One run of clusters: `clusters` clusters starting at `lcn` on the volume
/// (`None` = a sparse run with no clusters behind it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub lcn: Option<u64>,
    pub clusters: u64,
}

/// Decodes a data run list. Errors on anything malformed.
pub(crate) fn decode_runs(mut b: &[u8]) -> Result<Vec<Run>, String> {
    let bad = || "The master file table has an unreadable cluster list.".to_string();
    let mut runs = Vec::new();
    let mut lcn: i64 = 0;
    loop {
        let Some((&header, rest)) = b.split_first() else {
            // Running off the end without a terminator: accept what we have
            // only if at least the list started properly.
            return if runs.is_empty() {
                Err(bad())
            } else {
                Ok(runs)
            };
        };
        if header == 0 {
            return Ok(runs);
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = (header >> 4) as usize;
        if len_size == 0 || len_size > 8 || off_size > 8 || rest.len() < len_size + off_size {
            return Err(bad());
        }
        let mut clusters = 0u64;
        for (i, byte) in rest[..len_size].iter().enumerate() {
            clusters |= (*byte as u64) << (8 * i);
        }
        if clusters == 0 || clusters > (1 << 52) {
            return Err(bad());
        }
        let run = if off_size == 0 {
            Run {
                lcn: None,
                clusters,
            }
        } else {
            let field = &rest[len_size..len_size + off_size];
            let mut delta = 0i64;
            for (i, byte) in field.iter().enumerate() {
                delta |= (*byte as i64) << (8 * i);
            }
            // Sign-extend.
            let shift = 64 - 8 * off_size as u32;
            let delta = (delta << shift) >> shift;
            lcn = lcn.checked_add(delta).ok_or_else(bad)?;
            if lcn < 0 {
                return Err(bad());
            }
            Run {
                lcn: Some(lcn as u64),
                clusters,
            }
        };
        runs.push(run);
        if runs.len() > 1_000_000 {
            return Err(bad());
        }
        b = &rest[len_size + off_size..];
    }
}

/// What the index needs from one record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ParsedRecord {
    pub record_no: u64,
    pub sequence: u16,
    pub is_dir: bool,
    /// Record number of the base record when this is an extension record.
    pub base: Option<u64>,
    pub has_attribute_list: bool,
    /// From `$STANDARD_INFORMATION`: created, modified (FILETIME), attributes.
    pub times: Option<(u64, u64, u32)>,
    /// Best name found: parent reference, name, namespace rank.
    pub name: Option<(u64, String, u8)>,
    /// Size of the unnamed data stream, if its first extent is in this record.
    pub data_size: Option<u64>,
    /// Size recorded next to the name (may be stale; last resort).
    pub name_size: u64,
}

/// How much we like a name's namespace (higher is better).
fn namespace_rank(ns: u8) -> u8 {
    match ns {
        NAMESPACE_WIN32 | NAMESPACE_WIN32_AND_DOS => 3,
        NAMESPACE_POSIX => 2,
        NAMESPACE_DOS => 1,
        _ => 0,
    }
}

/// Parses one record in place (fixups are applied to `rec`). `None` for
/// records that are free, damaged, or not `FILE` records.
pub(crate) fn parse_record(rec: &mut [u8], record_no: u64) -> Option<ParsedRecord> {
    if !apply_fixup(rec) {
        return None;
    }
    let rec = &*rec;
    let flags = u16_at(rec, 0x16)?;
    if flags & FLAG_IN_USE == 0 {
        return None;
    }
    let base_ref = u64_at(rec, 0x20)?;
    let mut out = ParsedRecord {
        record_no,
        sequence: u16_at(rec, 0x10)?,
        is_dir: flags & FLAG_DIRECTORY != 0,
        base: (base_ref != 0).then_some(base_ref & 0x0000_FFFF_FFFF_FFFF),
        ..Default::default()
    };
    for a in attributes(rec) {
        match a.kind {
            ATTR_STANDARD_INFORMATION => {
                if let Some(v) = a.value() {
                    if let (Some(c), Some(m), Some(at)) =
                        (u64_at(v, 0), u64_at(v, 8), u32_at(v, 0x20))
                    {
                        out.times = Some((c, m, at));
                    }
                }
            }
            ATTR_ATTRIBUTE_LIST => out.has_attribute_list = true,
            ATTR_FILE_NAME => {
                let Some(v) = a.value() else { continue };
                let (Some(parent), Some(size), Some(&chars), Some(&ns)) =
                    (u64_at(v, 0), u64_at(v, 0x30), v.get(0x40), v.get(0x41))
                else {
                    continue;
                };
                let Some(name) = v.get(0x42..0x42 + chars as usize * 2) else {
                    continue;
                };
                let rank = namespace_rank(ns);
                let better = match &out.name {
                    Some((_, _, have)) => rank > *have,
                    None => true,
                };
                if better && chars > 0 {
                    let units: Vec<u16> = name
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    out.name = Some((parent, String::from_utf16_lossy(&units), rank));
                    out.name_size = size;
                }
            }
            ATTR_DATA if a.name_len == 0 => {
                if a.non_resident {
                    if a.start_vcn() == Some(0) {
                        out.data_size = a.real_size();
                    }
                } else if let Some(v) = a.value() {
                    out.data_size = Some(v.len() as u64);
                }
            }
            _ => {}
        }
    }
    Some(out)
}

/// One entry of an `$ATTRIBUTE_LIST`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListEntry {
    pub kind: u32,
    pub start_vcn: u64,
    /// Record number that holds the attribute.
    pub record_no: u64,
    pub named: bool,
}

pub(crate) fn parse_attribute_list(mut b: &[u8]) -> Vec<ListEntry> {
    let mut out = Vec::new();
    while b.len() >= 0x1A {
        let (Some(kind), Some(len), Some(vcn), Some(reference)) =
            (u32_at(b, 0), u16_at(b, 4), u64_at(b, 8), u64_at(b, 0x10))
        else {
            break;
        };
        let len = len as usize;
        if len < 0x1A || len > b.len() {
            break;
        }
        out.push(ListEntry {
            kind,
            start_vcn: vcn,
            record_no: reference & 0x0000_FFFF_FFFF_FFFF,
            named: b[6] != 0,
        });
        b = &b[len..];
        if out.len() > 100_000 {
            break;
        }
    }
    out
}

/// A piece of the `$MFT` file on the volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Extent {
    /// Offset of the piece inside the `$MFT` file, in bytes.
    pub file_offset: u64,
    /// Where it is on the volume, in bytes.
    pub volume_offset: u64,
    pub len: u64,
}

fn extents_from_runs(runs: &[Run], start_vcn: u64, cluster: u64) -> Result<Vec<Extent>, String> {
    let mut out = Vec::new();
    let mut vcn = start_vcn;
    for r in runs {
        let lcn = r
            .lcn
            .ok_or("The master file table has a gap, which should never happen.")?;
        let overflow = || "The master file table has an unreadable cluster list.".to_string();
        out.push(Extent {
            file_offset: vcn.checked_mul(cluster).ok_or_else(overflow)?,
            volume_offset: lcn.checked_mul(cluster).ok_or_else(overflow)?,
            len: r.clusters.checked_mul(cluster).ok_or_else(overflow)?,
        });
        vcn = vcn.checked_add(r.clusters).ok_or_else(overflow)?;
    }
    Ok(out)
}

/// Reads raw bytes of the volume: `(volume offset, buffer)`.
pub(crate) type ReadFn<'a> = dyn FnMut(u64, &mut [u8]) -> Result<(), String> + 'a;

/// A name found in a record: parent reference, name, namespace rank.
type FoundName = (u64, String, u8);

/// Reads `len` bytes of the `$MFT` file at `file_offset` through the extents
/// known so far.
fn read_through(
    extents: &[Extent],
    file_offset: u64,
    len: usize,
    read: &mut ReadFn<'_>,
) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; len];
    let mut done = 0usize;
    while done < len {
        let pos = file_offset + done as u64;
        let ext = extents
            .iter()
            .find(|e| pos >= e.file_offset && pos < e.file_offset + e.len)
            .ok_or("A part of the master file table could not be located.")?;
        let within = pos - ext.file_offset;
        let take = ((ext.len - within) as usize).min(len - done);
        read(ext.volume_offset + within, &mut out[done..done + take])?;
        done += take;
    }
    Ok(out)
}

/// Works out where the whole `$MFT` file lies, starting from its first
/// record. `read(volume_offset, buffer)` reads raw bytes of the volume;
/// offsets and lengths passed to it are multiples of `sector`.
pub(crate) fn mft_extents(
    record0: &mut [u8],
    cluster: u64,
    sector: u64,
    read: &mut ReadFn<'_>,
) -> Result<Vec<Extent>, String> {
    let bad = || "The first record of the master file table is damaged.".to_string();
    if cluster == 0 || sector == 0 || !apply_fixup(record0) {
        return Err(bad());
    }
    let record_size = record0.len() as u64;
    let rec: &[u8] = record0;
    let mut extents: Vec<Extent> = Vec::new();
    let mut list: Option<Vec<ListEntry>> = None;
    let mut list_runs: Option<(Vec<Run>, u64)> = None;
    for a in attributes(rec) {
        match a.kind {
            ATTR_DATA if a.name_len == 0 && a.non_resident => {
                let runs = decode_runs(a.runs().ok_or_else(bad)?)?;
                extents.extend(extents_from_runs(
                    &runs,
                    a.start_vcn().unwrap_or(0),
                    cluster,
                )?);
            }
            ATTR_ATTRIBUTE_LIST => {
                if a.non_resident {
                    let runs = decode_runs(a.runs().ok_or_else(bad)?)?;
                    list_runs = Some((runs, a.real_size().unwrap_or(0)));
                } else {
                    list = Some(parse_attribute_list(a.value().ok_or_else(bad)?));
                }
            }
            _ => {}
        }
    }
    if extents.is_empty() || extents[0].file_offset != 0 {
        return Err(bad());
    }
    // A sector-aligned read helper for small pieces.
    let mut read_aligned = |volume_offset: u64, len: usize| -> Result<Vec<u8>, String> {
        let start = volume_offset / sector * sector;
        let end = (volume_offset + len as u64).div_ceil(sector) * sector;
        let mut buf = vec![0u8; (end - start) as usize];
        read(start, &mut buf)?;
        let skip = (volume_offset - start) as usize;
        Ok(buf[skip..skip + len].to_vec())
    };
    if let Some((runs, size)) = list_runs {
        // The attribute list itself lives in clusters of its own.
        if size > 16 << 20 {
            return Err(bad());
        }
        let mut bytes = Vec::with_capacity(size as usize);
        for r in &runs {
            let lcn = r.lcn.ok_or_else(bad)?;
            let len = r.clusters.checked_mul(cluster).ok_or_else(bad)?;
            if len > 16 << 20 {
                return Err(bad());
            }
            bytes.extend(read_aligned(lcn * cluster, len as usize)?);
        }
        bytes.truncate(size as usize);
        list = Some(parse_attribute_list(&bytes));
    }
    if let Some(list) = list {
        // Further pieces of the cluster list live in extension records, which
        // are themselves inside the pieces already known.
        let mut more: Vec<ListEntry> = list
            .into_iter()
            .filter(|e| e.kind == ATTR_DATA && !e.named && e.record_no != 0)
            .collect();
        more.sort_by_key(|e| e.start_vcn);
        more.dedup_by_key(|e| e.record_no);
        for entry in more {
            let offset = entry.record_no.checked_mul(record_size).ok_or_else(bad)?;
            let mut ext_rec = {
                let mut r = |off: u64, buf: &mut [u8]| -> Result<(), String> {
                    let v = read_aligned(off, buf.len())?;
                    buf.copy_from_slice(&v);
                    Ok(())
                };
                read_through(&extents, offset, record_size as usize, &mut r)?
            };
            if !apply_fixup(&mut ext_rec) {
                return Err(bad());
            }
            for a in attributes(&ext_rec) {
                if a.kind == ATTR_DATA && a.name_len == 0 && a.non_resident {
                    let runs = decode_runs(a.runs().ok_or_else(bad)?)?;
                    let start = a.start_vcn().ok_or_else(bad)?;
                    extents.extend(extents_from_runs(&runs, start, cluster)?);
                }
            }
        }
    }
    extents.sort_by_key(|e| e.file_offset);
    // The pieces must follow each other without holes or overlaps.
    let mut expect = 0u64;
    for e in &extents {
        if e.file_offset != expect || e.len == 0 {
            return Err("The master file table's layout does not add up.".into());
        }
        expect += e.len;
    }
    Ok(extents)
}

/// Gathers parsed records and feeds the index builder, crediting what
/// extension records hold to their base records.
pub(crate) struct Assembler {
    builder: VolumeBuilder,
    /// Base records that may be completed by extension records.
    partial: HashMap<u64, ParsedRecord>,
    /// (base record number, name, data size) found in extension records.
    extras: Vec<(u64, Option<FoundName>, Option<u64>)>,
    pushed: u64,
}

impl Assembler {
    pub fn new(builder: VolumeBuilder) -> Assembler {
        Assembler {
            builder,
            partial: HashMap::new(),
            extras: Vec::new(),
            pushed: 0,
        }
    }

    pub fn count(&self) -> u64 {
        self.pushed
    }

    fn push(&mut self, r: &ParsedRecord) {
        let Some((parent, name, _)) = &r.name else {
            return;
        };
        let (ctime, mtime, mut attrs) = r.times.unwrap_or((0, 0, 0));
        attrs &= !attr::DIRECTORY;
        if r.is_dir {
            attrs |= attr::DIRECTORY;
        }
        let size = if r.is_dir {
            0
        } else {
            r.data_size.unwrap_or(r.name_size)
        };
        let frn = r.record_no | (r.sequence as u64) << 48;
        self.builder.push(
            frn,
            *parent,
            name,
            attrs,
            Meta {
                size,
                mtime: filetime_to_unix(mtime),
                ctime: filetime_to_unix(ctime),
            },
        );
        self.pushed += 1;
    }

    pub fn add(&mut self, r: ParsedRecord) {
        if let Some(base) = r.base {
            if r.name.is_some() || r.data_size.is_some() {
                self.extras.push((base, r.name, r.data_size));
            }
            return;
        }
        if r.record_no == ROOT_RECORD {
            self.builder
                .set_root_frn(ROOT_RECORD | (r.sequence as u64) << 48);
            return;
        }
        if r.record_no < FIRST_USER_RECORD {
            return;
        }
        if r.has_attribute_list {
            self.partial.insert(r.record_no, r);
        } else {
            self.push(&r);
        }
    }

    pub fn finish(mut self, excludes: &Excludes) -> VolumeStore {
        for (base, name, data_size) in std::mem::take(&mut self.extras) {
            if let Some(p) = self.partial.get_mut(&base) {
                if let Some((parent, n, rank)) = name {
                    let better = match &p.name {
                        Some((_, _, have)) => rank > *have,
                        None => true,
                    };
                    if better {
                        p.name = Some((parent, n, rank));
                    }
                }
                if p.data_size.is_none() {
                    p.data_size = data_size;
                }
            }
        }
        let mut partial: Vec<ParsedRecord> =
            std::mem::take(&mut self.partial).into_values().collect();
        partial.sort_by_key(|r| r.record_no);
        for r in &partial {
            self.push(r);
        }
        self.builder.finish(excludes, true)
    }
}

/// Parses every record in `chunk` (a whole number of records, the first one
/// being record `first_record`), in parallel.
pub(crate) fn parse_chunk(
    chunk: &mut [u8],
    record_size: usize,
    first_record: u64,
) -> Vec<ParsedRecord> {
    use rayon::prelude::*;
    chunk
        .par_chunks_exact_mut(record_size)
        .enumerate()
        .filter_map(|(i, rec)| parse_record(rec, first_record + i as u64))
        .collect()
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Builders for synthetic records, shared by the tests of this module
    //! and of the volume scan.
    use super::*;

    pub fn filetime(unix: u64) -> u64 {
        (unix + 11_644_473_600) * 10_000_000
    }

    pub fn resident(kind: u32, name_len: u8, value: &[u8]) -> Vec<u8> {
        let len = (0x18 + value.len()).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[9] = name_len;
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a[0x18..0x18 + value.len()].copy_from_slice(value);
        a
    }

    pub fn non_resident(
        kind: u32,
        name_len: u8,
        start_vcn: u64,
        real_size: u64,
        runs: &[u8],
    ) -> Vec<u8> {
        let len = (0x40 + runs.len() + 1).div_ceil(8) * 8;
        let mut a = vec![0u8; len];
        a[0..4].copy_from_slice(&kind.to_le_bytes());
        a[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        a[8] = 1;
        a[9] = name_len;
        a[0x10..0x18].copy_from_slice(&start_vcn.to_le_bytes());
        a[0x20..0x22].copy_from_slice(&0x40u16.to_le_bytes());
        a[0x28..0x30].copy_from_slice(&real_size.div_ceil(4096).saturating_mul(4096).to_le_bytes());
        a[0x30..0x38].copy_from_slice(&real_size.to_le_bytes());
        a[0x38..0x40].copy_from_slice(&real_size.to_le_bytes());
        a[0x40..0x40 + runs.len()].copy_from_slice(runs);
        a
    }

    pub fn std_info(created: u64, modified: u64, attrs: u32) -> Vec<u8> {
        let mut v = vec![0u8; 0x48];
        v[0..8].copy_from_slice(&filetime(created).to_le_bytes());
        v[8..16].copy_from_slice(&filetime(modified).to_le_bytes());
        v[0x20..0x24].copy_from_slice(&attrs.to_le_bytes());
        resident(ATTR_STANDARD_INFORMATION, 0, &v)
    }

    pub fn file_name(parent_ref: u64, name: &str, namespace: u8, size: u64) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42 + units.len() * 2];
        v[0..8].copy_from_slice(&parent_ref.to_le_bytes());
        v[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        v[0x40] = units.len() as u8;
        v[0x41] = namespace;
        for (i, u) in units.iter().enumerate() {
            v[0x42 + i * 2..0x44 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        resident(ATTR_FILE_NAME, 0, &v)
    }

    /// Encodes one data run (`delta` relative to the previous run's start).
    pub fn run(clusters: u64, delta: i64) -> Vec<u8> {
        let len_bytes = clusters.to_le_bytes();
        let len_size = (8 - clusters.leading_zeros() as usize / 8).max(1);
        let delta_bytes = delta.to_le_bytes();
        let mut off_size = 8;
        while off_size > 1 {
            let top = delta_bytes[off_size - 1];
            let next = delta_bytes[off_size - 2];
            let droppable = (top == 0 && next & 0x80 == 0) || (top == 0xFF && next & 0x80 != 0);
            if !droppable {
                break;
            }
            off_size -= 1;
        }
        let mut out = vec![(off_size as u8) << 4 | len_size as u8];
        out.extend_from_slice(&len_bytes[..len_size]);
        out.extend_from_slice(&delta_bytes[..off_size]);
        out
    }

    pub struct RecordSpec {
        pub sequence: u16,
        pub in_use: bool,
        pub is_dir: bool,
        pub base_ref: u64,
        pub attrs: Vec<Vec<u8>>,
    }

    impl RecordSpec {
        pub fn file(sequence: u16, attrs: Vec<Vec<u8>>) -> RecordSpec {
            RecordSpec {
                sequence,
                in_use: true,
                is_dir: false,
                base_ref: 0,
                attrs,
            }
        }
        pub fn dir(sequence: u16, attrs: Vec<Vec<u8>>) -> RecordSpec {
            RecordSpec {
                is_dir: true,
                ..RecordSpec::file(sequence, attrs)
            }
        }
    }

    /// Builds a record of `size` bytes with fixups in place, as on disk.
    pub fn build(size: usize, spec: &RecordSpec) -> Vec<u8> {
        let mut r = vec![0u8; size];
        let sectors = size / STRIDE;
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&((sectors + 1) as u16).to_le_bytes());
        r[0x10..0x12].copy_from_slice(&spec.sequence.to_le_bytes());
        let first = (0x30 + (sectors + 1) * 2).div_ceil(8) * 8;
        r[0x14..0x16].copy_from_slice(&(first as u16).to_le_bytes());
        let flags = if spec.in_use { FLAG_IN_USE } else { 0 }
            | if spec.is_dir { FLAG_DIRECTORY } else { 0 };
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&(size as u32).to_le_bytes());
        r[0x20..0x28].copy_from_slice(&spec.base_ref.to_le_bytes());
        let mut off = first;
        for a in &spec.attrs {
            assert!(off + a.len() + 8 <= size, "test record too small");
            r[off..off + a.len()].copy_from_slice(a);
            off += a.len();
        }
        r[off..off + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        off += 8;
        r[0x18..0x1C].copy_from_slice(&(off as u32).to_le_bytes());
        // Protect: move the last two bytes of every stride into the array.
        let usn = [0x5Au8, 0xA5];
        r[0x30..0x32].copy_from_slice(&usn);
        for i in 1..=sectors {
            let end = i * STRIDE;
            let (a, b) = (r[end - 2], r[end - 1]);
            r[0x30 + i * 2] = a;
            r[0x30 + i * 2 + 1] = b;
            r[end - 2..end].copy_from_slice(&usn);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::index::{VolumeKind, VolumeMeta};

    const REC: usize = 1024;

    fn parse(spec: &RecordSpec, no: u64) -> Option<ParsedRecord> {
        parse_record(&mut build(REC, spec), no)
    }

    #[test]
    fn a_plain_file_record() {
        let spec = RecordSpec::file(
            7,
            vec![
                std_info(1_600_000_000, 1_700_000_000, attr::HIDDEN | 0x20),
                file_name(5 | 5 << 48, "Report 2026.pdf", NAMESPACE_WIN32, 111),
                resident(ATTR_DATA, 0, b"small file contents"),
            ],
        );
        let r = parse(&spec, 40).unwrap();
        assert_eq!(r.record_no, 40);
        assert_eq!(r.sequence, 7);
        assert!(!r.is_dir && r.base.is_none() && !r.has_attribute_list);
        let (c, m, a) = r.times.unwrap();
        assert_eq!(filetime_to_unix(c), 1_600_000_000);
        assert_eq!(filetime_to_unix(m), 1_700_000_000);
        assert_eq!(a, attr::HIDDEN | 0x20);
        let (parent, name, _) = r.name.clone().unwrap();
        assert_eq!(parent, 5 | 5 << 48);
        assert_eq!(name, "Report 2026.pdf");
        assert_eq!(r.data_size, Some(19));
    }

    #[test]
    fn non_resident_data_and_named_streams() {
        let mut runs = run(100, 5000);
        runs.push(0);
        let spec = RecordSpec::file(
            1,
            vec![
                std_info(1, 2, 0),
                file_name(5, "video.mkv", NAMESPACE_WIN32_AND_DOS, 0),
                // A named stream must not be taken for the file's size.
                resident(ATTR_DATA, 4, b"zone identifier"),
                non_resident(ATTR_DATA, 0, 0, 5_000_000_000, &runs),
            ],
        );
        assert_eq!(parse(&spec, 50).unwrap().data_size, Some(5_000_000_000));
        // A later extent (starting VCN != 0) does not carry the size.
        let spec = RecordSpec {
            base_ref: 50 | 1 << 48,
            ..RecordSpec::file(1, vec![non_resident(ATTR_DATA, 0, 100, 0, &runs)])
        };
        let r = parse(&spec, 51).unwrap();
        assert_eq!(r.base, Some(50));
        assert_eq!(r.data_size, None);
    }

    #[test]
    fn the_long_name_wins_over_the_dos_name() {
        let dos_first = RecordSpec::dir(
            3,
            vec![
                std_info(1, 2, 0),
                file_name(5, "PROGRA~1", NAMESPACE_DOS, 0),
                file_name(5, "Program Files", NAMESPACE_WIN32, 0),
            ],
        );
        let r = parse(&dos_first, 60).unwrap();
        assert!(r.is_dir);
        assert_eq!(r.name.unwrap().1, "Program Files");
        let win_first = RecordSpec::file(
            3,
            vec![
                file_name(5, "Long Name.txt", NAMESPACE_WIN32, 0),
                file_name(5, "LONGNA~1.TXT", NAMESPACE_DOS, 0),
                // A second hard link: the first Win32 name is kept.
                file_name(9, "Other Link.txt", NAMESPACE_WIN32, 0),
            ],
        );
        assert_eq!(
            parse(&win_first, 61).unwrap().name.unwrap().1,
            "Long Name.txt"
        );
        let posix = RecordSpec::file(3, vec![file_name(5, "CaseSensitive", NAMESPACE_POSIX, 0)]);
        assert_eq!(parse(&posix, 62).unwrap().name.unwrap().1, "CaseSensitive");
        let only_dos = RecordSpec::file(3, vec![file_name(5, "ONLYDOS", NAMESPACE_DOS, 0)]);
        assert_eq!(parse(&only_dos, 63).unwrap().name.unwrap().1, "ONLYDOS");
    }

    #[test]
    fn unicode_names_and_long_names() {
        let long: String = "ж".repeat(255);
        let spec = RecordSpec::file(1, vec![file_name(5, &long, NAMESPACE_WIN32, 0)]);
        let r = parse_record(&mut build(4096, &spec), 70).unwrap();
        assert_eq!(r.name.unwrap().1, long);
        let spec = RecordSpec::file(1, vec![file_name(5, "名前 — 🙂.txt", NAMESPACE_WIN32, 0)]);
        assert_eq!(parse(&spec, 71).unwrap().name.unwrap().1, "名前 — 🙂.txt");
    }

    #[test]
    fn free_and_foreign_records_are_skipped() {
        let mut spec = RecordSpec::file(1, vec![file_name(5, "gone.txt", NAMESPACE_WIN32, 0)]);
        spec.in_use = false;
        assert!(parse(&spec, 80).is_none());
        assert!(parse_record(&mut vec![0u8; REC], 81).is_none());
        let mut baad = build(REC, &RecordSpec::file(1, vec![]));
        baad[0..4].copy_from_slice(b"BAAD");
        assert!(parse_record(&mut baad, 82).is_none());
        assert!(parse_record(&mut [], 83).is_none());
        assert!(parse_record(&mut b"FILE".to_vec(), 84).is_none());
    }

    #[test]
    fn fixups_are_applied_and_checked() {
        let spec = RecordSpec::file(
            1,
            vec![
                file_name(5, "fixup.bin", NAMESPACE_WIN32, 0),
                // Large enough that the value crosses the first 512-byte boundary.
                resident(
                    ATTR_DATA,
                    0,
                    &(0..600u32).map(|i| (i % 251) as u8).collect::<Vec<u8>>(),
                ),
            ],
        );
        let disk = build(REC, &spec);
        assert_eq!(&disk[510..512], &[0x5A, 0xA5], "protected on disk");
        let mut rec = disk.clone();
        assert!(apply_fixup(&mut rec));
        let a = attributes(&rec).find(|a| a.kind == ATTR_DATA).unwrap();
        let want: Vec<u8> = (0..600u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(a.value().unwrap(), &want[..], "original bytes are back");

        // A torn write: the end of a sector does not carry the sequence number.
        let mut torn = disk.clone();
        torn[1022] ^= 0xFF;
        assert!(parse_record(&mut torn, 1).is_none());
        // Update sequence array outside the record, or with a silly count.
        let mut bad = disk.clone();
        bad[4..6].copy_from_slice(&2000u16.to_le_bytes());
        assert!(!apply_fixup(&mut bad));
        let mut bad = disk.clone();
        bad[6..8].copy_from_slice(&0u16.to_le_bytes());
        assert!(!apply_fixup(&mut bad));
        let mut bad = disk.clone();
        bad[6..8].copy_from_slice(&500u16.to_le_bytes());
        assert!(!apply_fixup(&mut bad));
        let mut bad = disk.clone();
        bad[6..8].copy_from_slice(&1u16.to_le_bytes());
        assert!(!apply_fixup(&mut bad), "nothing protected");
    }

    #[test]
    fn damaged_attributes_never_panic() {
        let spec = RecordSpec::file(
            1,
            vec![
                std_info(1, 2, 0),
                file_name(5, "target.txt", NAMESPACE_WIN32, 9),
                resident(ATTR_DATA, 0, b"0123456789"),
            ],
        );
        let disk = build(REC, &spec);
        let mut plain = disk.clone();
        assert!(apply_fixup(&mut plain));
        let first = u16::from_le_bytes([plain[0x14], plain[0x15]]) as usize;

        // Attribute length 0, tiny, unaligned, or past the end of the record.
        for len in [0u32, 4, 0x17, 0x19, 0xFFFF_FFF8, 2000] {
            let mut r = plain.clone();
            r[first + 4..first + 8].copy_from_slice(&len.to_le_bytes());
            assert_eq!(attributes(&r).count(), 0, "length {len}");
        }
        // First attribute offset outside the record or inside the header.
        for off in [0u16, 8, 0x2F, 1020, 60000] {
            let mut r = plain.clone();
            r[0x14..0x16].copy_from_slice(&off.to_le_bytes());
            let _ = attributes(&r).count();
        }
        // Used size larger than the record.
        let mut r = plain.clone();
        r[0x18..0x1C].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(attributes(&r).count() >= 3);
        // Name length running past the attribute.
        let name_attr = first + plain[first + 4] as usize;
        let mut r = plain.clone();
        r[name_attr + 0x18 + 0x40] = 255;
        let got: Vec<Attribute> = attributes(&r).collect();
        assert_eq!(got.len(), 3);
        // Value offset / length past the attribute.
        let mut r = plain.clone();
        r[first + 0x10..first + 0x14].copy_from_slice(&0xFFFFu32.to_le_bytes());
        assert!(attributes(&r).next().unwrap().value().is_none());
        let mut r = plain.clone();
        r[first + 0x14..first + 0x16].copy_from_slice(&0xFFF0u16.to_le_bytes());
        assert!(attributes(&r).next().unwrap().value().is_none());

        // Flip every byte of the on-disk record in turn, and truncate it at
        // every length: parsing must simply cope.
        for i in 0..disk.len() {
            let mut r = disk.clone();
            r[i] ^= 0xFF;
            let _ = parse_record(&mut r, 5);
        }
        for cut in (0..disk.len()).step_by(3) {
            let mut r = disk[..cut].to_vec();
            let _ = parse_record(&mut r, 5);
        }
        // An attribute chain with no end marker stops at the used size.
        let mut r = plain.clone();
        let used = u32::from_le_bytes(r[0x18..0x1C].try_into().unwrap()) as usize;
        r[used - 8..used - 4].copy_from_slice(&0x90u32.to_le_bytes());
        assert!(attributes(&r).count() <= 4);
    }

    #[test]
    fn data_runs() {
        let mut list = run(16, 1000);
        list.extend(run(8, -200));
        list.extend(run(300_000, 70_000));
        list.push(0);
        list.extend([0xEE; 5]); // junk after the terminator is ignored
        assert_eq!(
            decode_runs(&list).unwrap(),
            vec![
                Run {
                    lcn: Some(1000),
                    clusters: 16
                },
                Run {
                    lcn: Some(800),
                    clusters: 8
                },
                Run {
                    lcn: Some(70_800),
                    clusters: 300_000
                },
            ]
        );
        // A sparse run has no offset field.
        assert_eq!(
            decode_runs(&[0x01, 0x10, 0x00]).unwrap(),
            vec![Run {
                lcn: None,
                clusters: 16
            }]
        );
        assert_eq!(decode_runs(&[0x00]).unwrap(), vec![]);
        // Malformed lists.
        assert!(decode_runs(&[]).is_err());
        assert!(decode_runs(&[0x10, 0x05]).is_err(), "no length field");
        assert!(
            decode_runs(&[0x19, 1, 2]).is_err(),
            "length field of 9 bytes"
        );
        assert!(
            decode_runs(&[0x91, 1, 2]).is_err(),
            "offset field of 9 bytes"
        );
        assert!(
            decode_runs(&[0x11, 0x00, 0x05, 0x00]).is_err(),
            "zero clusters"
        );
        assert!(decode_runs(&[0x21, 0x10, 0x05]).is_err(), "truncated");
        assert!(
            decode_runs(&[0x11, 0x10, 0x80, 0x00]).is_err(),
            "negative position"
        );
        // No terminator but sound so far.
        assert_eq!(decode_runs(&[0x11, 0x10, 0x05]).unwrap().len(), 1);
        for b in 0..=255u8 {
            let _ = decode_runs(&[b; 40]);
        }
    }

    #[test]
    fn attribute_lists() {
        let mut list = Vec::new();
        for (kind, vcn, rec) in [
            (0x10u32, 0u64, 0u64),
            (0x80, 0, 0),
            (0x80, 64, 30),
            (0x80, 128, 31),
        ] {
            let mut e = vec![0u8; 0x20];
            e[0..4].copy_from_slice(&kind.to_le_bytes());
            e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
            e[8..16].copy_from_slice(&vcn.to_le_bytes());
            e[16..24].copy_from_slice(&(rec | 9 << 48).to_le_bytes());
            list.extend(e);
        }
        let parsed = parse_attribute_list(&list);
        assert_eq!(parsed.len(), 4);
        assert_eq!(
            parsed[2],
            ListEntry {
                kind: 0x80,
                start_vcn: 64,
                record_no: 30,
                named: false
            }
        );
        // Damaged: zero length entry, truncated tail.
        let mut bad = list.clone();
        bad[4..6].copy_from_slice(&0u16.to_le_bytes());
        assert!(parse_attribute_list(&bad).is_empty());
        assert_eq!(parse_attribute_list(&list[..0x30]).len(), 1);
        assert!(parse_attribute_list(&[]).is_empty());
        assert!(parse_attribute_list(&[0xFF; 100]).is_empty());
    }

    #[test]
    fn extension_records_are_credited_to_their_base() {
        let mut asm = Assembler::new(VolumeBuilder::new(VolumeMeta::new(
            "C:",
            VolumeKind::NtfsMft,
        )));
        let root = |n: u64| n | 1 << 48;
        asm.add(
            parse(
                &RecordSpec::dir(
                    5,
                    vec![file_name(5 | 5 << 48, ".", NAMESPACE_WIN32_AND_DOS, 0)],
                ),
                5,
            )
            .unwrap(),
        );
        // A metafile: left out.
        asm.add(
            parse(
                &RecordSpec::file(
                    1,
                    vec![file_name(
                        5 | 5 << 48,
                        "$Bitmap",
                        NAMESPACE_WIN32_AND_DOS,
                        0,
                    )],
                ),
                6,
            )
            .unwrap(),
        );
        asm.add(
            parse(
                &RecordSpec::dir(
                    1,
                    vec![
                        std_info(10, 20, 0),
                        file_name(5 | 5 << 48, "Videos", NAMESPACE_WIN32, 0),
                    ],
                ),
                30,
            )
            .unwrap(),
        );
        // A huge fragmented file: the base holds the name and an attribute
        // list, an extension record holds the first data extent.
        let mut list_entry = vec![0u8; 0x20];
        list_entry[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        list_entry[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        list_entry[16..24].copy_from_slice(&root(41).to_le_bytes());
        asm.add(
            parse(
                &RecordSpec::file(
                    2,
                    vec![
                        std_info(100, 200, attr::READONLY),
                        resident(ATTR_ATTRIBUTE_LIST, 0, &list_entry),
                        file_name(root(30), "film.mkv", NAMESPACE_WIN32, 1234),
                    ],
                ),
                40,
            )
            .unwrap(),
        );
        let mut runs = run(1000, 9000);
        runs.push(0);
        asm.add(
            parse(
                &RecordSpec {
                    base_ref: 40 | 2 << 48,
                    ..RecordSpec::file(1, vec![non_resident(ATTR_DATA, 0, 0, 7_000_000_000, &runs)])
                },
                41,
            )
            .unwrap(),
        );
        // A file whose only name sits in an extension record.
        asm.add(
            parse(
                &RecordSpec::file(
                    1,
                    vec![
                        std_info(1, 2, 0),
                        resident(ATTR_ATTRIBUTE_LIST, 0, &list_entry),
                        resident(ATTR_DATA, 0, b"abc"),
                    ],
                ),
                42,
            )
            .unwrap(),
        );
        asm.add(
            parse(
                &RecordSpec {
                    base_ref: 42 | 1 << 48,
                    ..RecordSpec::file(
                        1,
                        vec![file_name(root(30), "many-links.txt", NAMESPACE_WIN32, 0)],
                    )
                },
                43,
            )
            .unwrap(),
        );
        // An extension record whose base does not exist: ignored.
        asm.add(
            parse(
                &RecordSpec {
                    base_ref: 999 | 1 << 48,
                    ..RecordSpec::file(1, vec![file_name(root(30), "ghost", NAMESPACE_WIN32, 0)])
                },
                44,
            )
            .unwrap(),
        );
        assert_eq!(asm.count(), 1);
        let store = asm.finish(&Excludes::default());
        let mut paths: Vec<String> = (1..store.entries().len() as u32)
            .map(|i| store.path(i))
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                "C:\\Videos",
                "C:\\Videos\\film.mkv",
                "C:\\Videos\\many-links.txt"
            ]
        );
        let film = store.resolve_path("C:\\Videos\\film.mkv").unwrap();
        let e = store.entry(film).unwrap();
        assert_eq!(e.size, 7_000_000_000);
        assert_eq!((e.ctime, e.mtime), (100, 200));
        assert!(e.flags & crate::index::flags::READONLY != 0);
        assert_eq!(store.slot_of(40 | 2 << 48), Some(film));
        assert_eq!(
            store
                .entry(store.resolve_path("C:\\Videos\\many-links.txt").unwrap())
                .unwrap()
                .size,
            3
        );
        assert_eq!(store.slot_of(5 | 5 << 48), Some(0));
    }

    #[test]
    fn chunks_are_parsed_in_parallel() {
        let mut chunk = Vec::new();
        for i in 0..64u64 {
            let spec = RecordSpec::file(
                1,
                vec![file_name(5, &format!("file-{i}.txt"), NAMESPACE_WIN32, i)],
            );
            let mut rec = build(REC, &spec);
            if i % 4 == 0 {
                rec[0] = b'X'; // every fourth record is rubbish
            }
            chunk.extend(rec);
        }
        let parsed = parse_chunk(&mut chunk, REC, 1000);
        assert_eq!(parsed.len(), 48);
        assert!(parsed
            .iter()
            .all(|r| r.record_no >= 1000 && r.record_no < 1064));
        let r = parsed.iter().find(|r| r.record_no == 1003).unwrap();
        assert_eq!(r.name.as_ref().unwrap().1, "file-3.txt");
    }
}
