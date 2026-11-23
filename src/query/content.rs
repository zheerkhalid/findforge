//! `content:` terms: looking inside files.
//!
//! The index only knows names, so this is a second pass over the files that
//! already passed every other test of the query: each one is read (up to
//! 32 MB) and kept when it contains all the texts asked for. It is opt-in and
//! slower than a name search by nature, so the number of files read is capped.

use super::parse::{ContentTerm, CONTENT_MAX_BYTES};
use super::Query;
use crate::index::{Hit, Index};
use rayon::prelude::*;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};

/// At most this many files are read for one search.
pub const CONTENT_FILE_LIMIT: usize = 100_000;

/// How a content search went.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentStats {
    /// Files that were read and looked through.
    pub searched: usize,
    /// Files left out: unreadable, or not text.
    pub skipped: usize,
    /// True when there were more candidates than the limit, so only the
    /// first ones were read.
    pub limited: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Match,
    NoMatch,
    Skipped,
    Cancelled,
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(hay, needle).is_some()
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| {
            if big_endian {
                u16::from_be_bytes(*p)
            } else {
                u16::from_le_bytes(*p)
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Whether `data` (the bytes of a file) contains every term. `None` when the
/// file is not text.
pub(crate) fn text_matches(mut data: Vec<u8>, terms: &[ContentTerm]) -> Option<bool> {
    let utf16 = match data.as_slice() {
        [0xFF, 0xFE, ..] => Some(false),
        [0xFE, 0xFF, ..] => Some(true),
        _ => None,
    };
    if let Some(big_endian) = utf16 {
        let text = decode_utf16(&data[2..], big_endian);
        let lower = terms
            .iter()
            .any(|t| !t.case_sensitive)
            .then(|| text.to_lowercase());
        return Some(terms.iter().all(|t| {
            let hay = match (&lower, t.case_sensitive) {
                (Some(l), false) => l.as_str(),
                _ => text.as_str(),
            };
            hay.contains(t.needle.as_str())
        }));
    }
    if data[..data.len().min(8192)].contains(&0) {
        return None;
    }
    // Case-sensitive terms are looked for in the bytes as they are.
    if !terms
        .iter()
        .filter(|t| t.case_sensitive)
        .all(|t| contains(&data, t.needle.as_bytes()))
    {
        return Some(false);
    }
    let folded: Vec<&ContentTerm> = terms.iter().filter(|t| !t.case_sensitive).collect();
    if folded.is_empty() {
        return Some(true);
    }
    if folded.iter().all(|t| t.needle.is_ascii()) {
        // ASCII needles: lower-casing the ASCII letters in place is enough.
        data.make_ascii_lowercase();
        return Some(folded.iter().all(|t| contains(&data, t.needle.as_bytes())));
    }
    let lower = String::from_utf8_lossy(&data).to_lowercase();
    Some(folded.iter().all(|t| lower.contains(t.needle.as_str())))
}

fn check_file(path: &str, terms: &[ContentTerm]) -> Verdict {
    let Ok(file) = std::fs::File::open(path) else {
        return Verdict::Skipped;
    };
    let mut data = Vec::new();
    // The file may have grown since it was indexed: never read past the limit.
    if file
        .take(CONTENT_MAX_BYTES + 1)
        .read_to_end(&mut data)
        .is_err()
        || data.len() as u64 > CONTENT_MAX_BYTES
    {
        return Verdict::Skipped;
    }
    match text_matches(data, terms) {
        Some(true) => Verdict::Match,
        Some(false) => Verdict::NoMatch,
        None => Verdict::Skipped,
    }
}

/// The files one content search has to read. It is made while the index is
/// at hand and holds everything the reading needs, so the (slow) reading
/// itself can run without holding on to the index.
pub struct ContentJob {
    hits: Vec<Hit>,
    paths: Vec<String>,
    terms: Vec<ContentTerm>,
    limited: bool,
}

/// Prepares the reading of `hits` (the files that passed every other test of
/// `query`). At most [`CONTENT_FILE_LIMIT`] files are taken.
pub fn content_job(index: &Index, query: &Query, mut hits: Vec<Hit>) -> ContentJob {
    let limited = hits.len() > CONTENT_FILE_LIMIT;
    hits.truncate(CONTENT_FILE_LIMIT);
    let paths = hits.par_iter().map(|h| index.path(*h)).collect();
    ContentJob {
        hits,
        paths,
        terms: query.parsed.contents.clone(),
        limited,
    }
}

impl ContentJob {
    /// Reads the files and keeps the hits whose file contains every text.
    /// Hits keep their order. Returns early (with partial results) when
    /// `cancel` is set.
    pub fn run(self, cancel: &AtomicBool) -> (Vec<Hit>, ContentStats) {
        let terms = &self.terms;
        let verdicts: Vec<Verdict> = self
            .paths
            .par_iter()
            .map(|path| {
                if cancel.load(Ordering::Relaxed) {
                    return Verdict::Cancelled;
                }
                check_file(path, terms)
            })
            .collect();
        let mut stats = ContentStats {
            limited: self.limited,
            ..Default::default()
        };
        let mut kept = Vec::new();
        for (hit, verdict) in self.hits.into_iter().zip(verdicts) {
            match verdict {
                Verdict::Match => {
                    stats.searched += 1;
                    kept.push(hit);
                }
                Verdict::NoMatch => stats.searched += 1,
                Verdict::Skipped => stats.skipped += 1,
                Verdict::Cancelled => {}
            }
        }
        (kept, stats)
    }

    /// Reads nothing and finds nothing (the demo, whose files are invented).
    pub fn skip(self) -> (Vec<Hit>, ContentStats) {
        (Vec::new(), ContentStats::default())
    }
}

/// Keeps the hits whose file contains every `content:` text of `query`.
/// Hits keep their order. Returns early (with partial results) when `cancel`
/// is set; a query without `content:` returns the hits untouched.
pub fn filter_by_content(
    index: &Index,
    query: &Query,
    hits: Vec<Hit>,
    cancel: &AtomicBool,
) -> (Vec<Hit>, ContentStats) {
    if query.parsed.contents.is_empty() {
        return (hits, ContentStats::default());
    }
    content_job(index, query, hits).run(cancel)
}

#[cfg(test)]
mod tests {
    use super::super::{parse, search, Now, SearchOptions};
    use super::*;
    use crate::index::Excludes;

    const NOW: Now = Now {
        unix: 1_773_837_000,
        utc_offset_secs: 0,
    };

    fn term(needle: &str, case_sensitive: bool) -> ContentTerm {
        ContentTerm {
            needle: needle.to_string(),
            case_sensitive,
        }
    }

    fn utf16le(text: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        for u in text.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    #[test]
    fn text_is_matched_in_the_usual_encodings() {
        let one =
            |data: &[u8], needle: &str, cs: bool| text_matches(data.to_vec(), &[term(needle, cs)]);
        assert_eq!(one(b"Total Due: 40", "total due", false), Some(true));
        assert_eq!(one(b"Total Due: 40", "total due", true), Some(false));
        assert_eq!(one(b"Total Due: 40", "Total Due", true), Some(true));
        assert_eq!(one(b"nothing here", "invoice", false), Some(false));
        // Non-ASCII, case-insensitive.
        assert_eq!(
            one("RÉSUMÉ of Alex".as_bytes(), "résumé", false),
            Some(true)
        );
        assert_eq!(one("ڕاپۆرتی ساڵانە".as_bytes(), "ساڵانە", false), Some(true));
        // UTF-16 with a byte order mark, both ways round.
        assert_eq!(one(&utf16le("Invoice Nº 7"), "invoice", false), Some(true));
        let mut be = vec![0xFE, 0xFF];
        for u in "Invoice".encode_utf16() {
            be.extend_from_slice(&u.to_be_bytes());
        }
        assert_eq!(one(&be, "voice", true), Some(true));
        assert_eq!(one(&be, "Voice", true), Some(false));
        // Binary data is not text.
        assert_eq!(one(b"MZ\0\0invoice\0", "invoice", false), None);
        // Every term must be there.
        let both = [term("alpha", false), term("Beta", true)];
        assert_eq!(text_matches(b"ALPHA Beta".to_vec(), &both), Some(true));
        assert_eq!(text_matches(b"ALPHA beta".to_vec(), &both), Some(false));
        // Invalid UTF-8 does not stop a match elsewhere in the file.
        assert_eq!(one(b"\xff\xfdbad invoice", "invoice", false), Some(true));
    }

    #[test]
    fn files_in_a_temp_folder_are_searched() {
        let dir =
            std::env::temp_dir().join(format!("findforge-content-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let write = |name: &str, data: &[u8]| std::fs::write(dir.join(name), data).unwrap();
        write(
            "match.txt",
            b"Dear customer,\r\nInvoice 2026-114 is due.\r\n",
        );
        write("other.txt", b"Nothing of interest in this one.");
        write("wide.txt", &utf16le("The INVOICE is attached."));
        write("binary.bin", b"\x7fELF\0\0\0invoice\0\0");
        write(
            "sub\\kurdish.txt",
            "ئەمە پسوولەیە: invoice ژمارە ٧".as_bytes(),
        );
        write("sub\\invoice.md", b"The name matches, the text does not.");
        write("empty.txt", b"");

        let cancel = AtomicBool::new(false);
        let store = crate::walk::walk_volume(
            &dir.to_string_lossy(),
            false,
            &Excludes::default(),
            &|_| {},
            &cancel,
        )
        .unwrap();
        let index = Index {
            volumes: vec![store],
            epoch: 1,
        };
        let run = |text: &str| {
            let q = parse(text, None, &SearchOptions::default(), NOW).unwrap();
            assert!(q.has_content());
            let hits = search(&index, &q, &cancel);
            let (hits, stats) = filter_by_content(&index, &q, hits, &cancel);
            let mut names: Vec<String> = hits.iter().map(|h| index.name(*h).to_string()).collect();
            names.sort();
            (names, stats)
        };

        let (names, stats) = run("content:invoice");
        assert_eq!(names, ["kurdish.txt", "match.txt", "wide.txt"]);
        // Six files have something in them; the binary one is skipped.
        assert_eq!(
            stats,
            ContentStats {
                searched: 5,
                skipped: 1,
                limited: false
            }
        );
        // Narrowed by the other terms first: only two files are read.
        let (names, stats) = run("content:invoice ext:txt !wide !other");
        assert_eq!(names, ["kurdish.txt", "match.txt"]);
        assert_eq!(stats.searched, 2);
        // Phrases, several terms, case and right-to-left text.
        assert_eq!(run("content:\"is due\"").0, ["match.txt"]);
        assert_eq!(run("content:invoice content:customer").0, ["match.txt"]);
        assert_eq!(run("case:content:INVOICE").0, ["wide.txt"]);
        assert_eq!(run("content:پسوولەیە").0, ["kurdish.txt"]);
        assert!(run("content:zebra").0.is_empty());
        // A cancelled search reads nothing.
        let q = parse("content:invoice", None, &SearchOptions::default(), NOW).unwrap();
        let hits = search(&index, &q, &cancel);
        let stop = AtomicBool::new(true);
        let (hits, stats) = filter_by_content(&index, &q, hits, &stop);
        assert!(hits.is_empty());
        assert_eq!(stats.searched + stats.skipped, 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
