use super::*;
use crate::index::{attr, Excludes, Meta, VolumeBuilder, VolumeKind, VolumeMeta};

// 2026-03-18 (a Wednesday) 15:30:00 local, UTC+3.
const NOW: Now = Now {
    unix: 1_773_837_000,
    utc_offset_secs: 3 * 3600,
};
const HOUR: u32 = 3600;
const DAY: u32 = 86_400;
const MB: u64 = 1 << 20;

fn t(ago: u32) -> u32 {
    NOW.unix as u32 - ago
}

fn file(b: &mut VolumeBuilder, id: u64, parent: u64, name: &str, size: u64, ago: u32, attrs: u32) {
    b.push(
        id,
        parent,
        name,
        attrs,
        Meta {
            size,
            mtime: t(ago),
            ctime: t(ago + 30 * DAY),
        },
    );
}

fn dir(b: &mut VolumeBuilder, id: u64, parent: u64, name: &str) {
    b.push(id, parent, name, attr::DIRECTORY, Meta::default());
}

/// Two invented drives.
fn sample() -> Index {
    let mut c = VolumeBuilder::new(VolumeMeta::new("C:", VolumeKind::NtfsWalk));
    c.set_root_frn(5);
    dir(&mut c, 10, 5, "Docs");
    file(&mut c, 11, 10, "Annual Report 2026.pdf", 2 * MB, HOUR, 0);
    file(
        &mut c,
        12,
        10,
        "annual report 2025.PDF",
        3 * MB,
        400 * DAY,
        0,
    );
    file(&mut c, 13, 10, "Résumé.docx", 40_000, 2 * DAY, 0);
    file(&mut c, 14, 10, "notes.txt", 0, 10 * DAY, attr::HIDDEN);
    file(
        &mut c,
        15,
        10,
        "draft report.txt",
        900,
        DAY + HOUR,
        attr::READONLY,
    );
    dir(&mut c, 20, 10, "Old");
    file(&mut c, 21, 20, "report.txt", 100, 800 * DAY, 0);
    dir(&mut c, 22, 20, "Empty");
    dir(&mut c, 30, 5, "Music");
    file(&mut c, 31, 30, "song1.mp3", 4 * MB, 50 * DAY, 0);
    file(&mut c, 32, 30, "song2.mp3", 5 * MB, 51 * DAY, 0);
    file(&mut c, 33, 30, "song10.mp3", 6 * MB, 52 * DAY, 0);
    file(&mut c, 34, 30, "Song.FLAC", 30 * MB, 53 * DAY, 0);
    file(
        &mut c,
        40,
        5,
        "pagefile.sys",
        4096 * MB,
        HOUR,
        attr::HIDDEN | attr::SYSTEM,
    );
    let mut d = VolumeBuilder::new(VolumeMeta::new("D:", VolumeKind::Walk));
    d.set_root_frn(5);
    dir(&mut d, 10, 5, "Backup");
    file(&mut d, 11, 10, "report.txt", 100, 5 * DAY, 0);
    file(&mut d, 12, 10, "photo.JPG", 5 * MB, 6 * DAY, 0);
    file(&mut d, 13, 10, "archive.tar.gz", 200 * MB, 7 * DAY, 0);
    file(&mut d, 14, 10, "README", 10, 8 * DAY, 0);
    file(&mut d, 15, 10, "名前 ファイル.txt", 10, 9 * DAY, 0);
    Index {
        volumes: vec![
            c.finish(&Excludes::default(), true),
            d.finish(&Excludes::default(), false),
        ],
        epoch: 1,
    }
}

fn run_with(
    index: &Index,
    text: &str,
    filter: Option<&str>,
    options: SearchOptions,
) -> Vec<String> {
    let q = parse(text, filter, &options, NOW).unwrap_or_else(|e| panic!("{text:?}: {e}"));
    let cancel = AtomicBool::new(false);
    let mut hits = search(index, &q, &cancel);
    sort(index, &mut hits, SortKey::Path, true, &cancel);
    let mut names: Vec<String> = hits.iter().map(|h| index.path(*h)).collect();
    names.sort();
    names
}

fn run(text: &str) -> Vec<String> {
    run_with(&sample(), text, None, SearchOptions::default())
}

/// Just the names (sorted), for compact assertions.
fn names(text: &str) -> Vec<String> {
    let mut v: Vec<String> = run(text)
        .into_iter()
        .map(|p| p.rsplit('\\').next().unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn err(text: &str) -> String {
    parse(text, None, &SearchOptions::default(), NOW)
        .err()
        .unwrap_or_else(|| panic!("{text:?} should be an error"))
}

#[test]
fn empty_query_matches_everything_but_roots() {
    let idx = sample();
    assert_eq!(run("").len(), idx.total());
    assert_eq!(run("   ").len(), idx.total());
    let q = parse("", None, &SearchOptions::default(), NOW).unwrap();
    assert!(q.is_match_all());
    assert!(!parse("a", None, &SearchOptions::default(), NOW)
        .unwrap()
        .is_match_all());
    assert!(
        !parse("", Some("type:folder"), &SearchOptions::default(), NOW)
            .unwrap()
            .is_match_all()
    );
}

#[test]
fn and_or_not_groups() {
    assert_eq!(
        names("report"),
        vec![
            "Annual Report 2026.pdf",
            "annual report 2025.PDF",
            "draft report.txt",
            "report.txt",
            "report.txt"
        ]
    );
    assert_eq!(names("report 2026"), vec!["Annual Report 2026.pdf"]);
    assert_eq!(
        names("2026|2025"),
        vec!["Annual Report 2026.pdf", "annual report 2025.PDF"]
    );
    // OR binds tighter than AND.
    assert_eq!(names("annual 2026|2025"), names("2026|2025"));
    assert_eq!(
        names("report !annual !draft"),
        vec!["report.txt", "report.txt"]
    );
    assert_eq!(
        names("<annual 2026>|<song flac>"),
        vec!["Annual Report 2026.pdf", "Song.FLAC"]
    );
    assert_eq!(names("!<report|song|e|a|o> !txt"), vec!["Music"]);
    // Stray operators are harmless.
    assert_eq!(names("| report 2026 |"), vec!["Annual Report 2026.pdf"]);
    assert_eq!(names("report 2026 !"), vec!["Annual Report 2026.pdf"]);
    assert!(err("<report").contains("<"));
}

#[test]
fn phrases_and_wildcards() {
    assert_eq!(
        names("\"annual report\""),
        vec!["Annual Report 2026.pdf", "annual report 2025.PDF"]
    );
    assert_eq!(names("\"report annual\""), Vec::<String>::new());
    assert_eq!(names("*.mp3"), vec!["song1.mp3", "song10.mp3", "song2.mp3"]);
    assert_eq!(names("song?.mp3"), vec!["song1.mp3", "song2.mp3"]);
    assert_eq!(
        names("song"),
        vec!["Song.FLAC", "song1.mp3", "song10.mp3", "song2.mp3"]
    );
    assert_eq!(names("s*g.*"), vec!["Song.FLAC"]);
    // A quoted star is a literal star.
    assert_eq!(names("\"*.mp3\""), Vec::<String>::new());
    assert_eq!(names("名前"), vec!["名前 ファイル.txt"]);
    assert_eq!(names("\"名前 ファ\""), vec!["名前 ファイル.txt"]);
}

#[test]
fn case_word_and_diacritics() {
    assert_eq!(names("case:Report"), vec!["Annual Report 2026.pdf"]);
    assert_eq!(names("case:REPORT"), Vec::<String>::new());
    assert_eq!(names("resume"), vec!["Résumé.docx"]);
    assert_eq!(names("RÉSUMÉ"), vec!["Résumé.docx"]);
    assert_eq!(names("diacritics:resume"), Vec::<String>::new());
    assert_eq!(names("diacritics:résumé"), vec!["Résumé.docx"]);
    assert_eq!(names("ww:song"), vec!["Song.FLAC"]);
    assert_eq!(names("wholeword:report").len(), 5);
    assert_eq!(names("ww:repor"), Vec::<String>::new());
    let idx = sample();
    let strict = SearchOptions {
        match_case: true,
        ..Default::default()
    };
    assert_eq!(run_with(&idx, "song", None, strict).len(), 3);
    assert_eq!(run_with(&idx, "nocase:song", None, strict).len(), 4);
    let ww = SearchOptions {
        whole_word: true,
        ..Default::default()
    };
    assert_eq!(run_with(&idx, "song", None, ww).len(), 1);
    let exact_accents = SearchOptions {
        match_diacritics: true,
        ..Default::default()
    };
    assert!(run_with(&idx, "resume", None, exact_accents).is_empty());
}

#[test]
fn anchors() {
    assert_eq!(names("startwith:annual").len(), 2);
    assert_eq!(names("startwith:report"), vec!["report.txt", "report.txt"]);
    assert_eq!(names("endwith:.gz"), vec!["archive.tar.gz"]);
    assert_eq!(names("exact:readme"), vec!["README"]);
    assert_eq!(names("exact:read"), Vec::<String>::new());
}

#[test]
fn path_terms() {
    assert_eq!(
        names("docs\\"),
        vec![
            "Annual Report 2026.pdf",
            "Old",
            "Résumé.docx",
            "annual report 2025.PDF",
            "draft report.txt",
            "notes.txt",
            "report.txt",
            "Empty"
        ]
        .into_iter()
        .map(String::from)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
    );
    assert_eq!(names("old\\report"), vec!["report.txt"]);
    assert_eq!(names("old/report"), vec!["report.txt"]);
    assert_eq!(run("path:backup report"), vec!["D:\\Backup\\report.txt"]);
    assert_eq!(
        names("path:d:"),
        vec![
            "Backup",
            "README",
            "archive.tar.gz",
            "photo.JPG",
            "report.txt",
            "名前 ファイル.txt"
        ]
    );
    assert_eq!(run("C:\\Music\\*.flac"), vec!["C:\\Music\\Song.FLAC"]);
    // name: forces the name even for text with a backslash.
    assert_eq!(names("name:old\\report"), Vec::<String>::new());
    let idx = sample();
    let by_path = SearchOptions {
        match_path: true,
        ..Default::default()
    };
    assert_eq!(run_with(&idx, "music", None, by_path).len(), 5);
    assert_eq!(run_with(&idx, "name:music", None, by_path).len(), 1);
    assert_eq!(names("parent:c:\\docs\\old"), vec!["Empty", "report.txt"]);
    assert_eq!(names("infolder:D:\\Backup\\ report"), vec!["report.txt"]);
    assert_eq!(names("parent:C:\\Nowhere"), Vec::<String>::new());
    assert_eq!(names("drive:d report"), vec!["report.txt"]);
    assert_eq!(names("root:C: report").len(), 4);
    assert_eq!(
        names("depth:1"),
        vec!["Backup", "Docs", "Music", "pagefile.sys"]
    );
    assert_eq!(names("depth:>2"), vec!["Empty", "report.txt"]);
    assert_eq!(names("parents:3"), vec!["Empty", "report.txt"]);
}

#[test]
fn extensions_and_kinds() {
    assert_eq!(
        names("ext:mp3"),
        vec!["song1.mp3", "song10.mp3", "song2.mp3"]
    );
    assert_eq!(
        names("ext:MP3;.flac"),
        vec!["Song.FLAC", "song1.mp3", "song10.mp3", "song2.mp3"]
    );
    assert_eq!(
        names("ext:pdf"),
        vec!["Annual Report 2026.pdf", "annual report 2025.PDF"]
    );
    assert_eq!(names("ext:gz"), vec!["archive.tar.gz"]);
    assert_eq!(names("ext:nothing"), Vec::<String>::new());
    assert_eq!(names("ext:"), vec!["README"]);
    assert_eq!(
        names("type:folder"),
        vec!["Backup", "Docs", "Empty", "Music", "Old"]
    );
    assert_eq!(names("type:file").len(), sample().total() - 5);
    assert_eq!(names("type:audio"), names("ext:mp3;flac"));
    assert_eq!(names("type:picture"), vec!["photo.JPG"]);
    assert_eq!(names("folder:o"), vec!["Docs", "Old"]);
    assert_eq!(names("file:old"), Vec::<String>::new());
    assert_eq!(names("folder:"), names("type:folder"));
    assert!(err("type:banana").starts_with("type:"));
}

#[test]
fn sizes_dates_lengths_attributes() {
    assert_eq!(names("size:>100mb"), vec!["archive.tar.gz", "pagefile.sys"]);
    assert_eq!(
        names("size:1mb..5mb"),
        vec![
            "Annual Report 2026.pdf",
            "annual report 2025.PDF",
            "photo.JPG",
            "song1.mp3",
            "song2.mp3"
        ]
    );
    assert_eq!(names("size:empty"), vec!["notes.txt"]);
    assert_eq!(
        names("size:<=10"),
        vec!["README", "notes.txt", "名前 ファイル.txt"]
    );
    assert_eq!(names("size:5mb"), vec!["photo.JPG", "song2.mp3"]);
    assert_eq!(names("size:huge"), vec!["Song.FLAC"]);
    assert!(err("size:abc").starts_with("size:"));

    assert_eq!(
        names("dm:today"),
        vec!["Annual Report 2026.pdf", "pagefile.sys"]
    );
    assert_eq!(names("dm:yesterday"), vec!["draft report.txt"]);
    assert_eq!(
        names("dm:thisweek"),
        vec![
            "Annual Report 2026.pdf",
            "Résumé.docx",
            "draft report.txt",
            "pagefile.sys"
        ]
    );
    assert_eq!(
        names("dm:last7days report"),
        vec!["Annual Report 2026.pdf", "draft report.txt", "report.txt"]
    );
    assert_eq!(names("dm:2025"), vec!["annual report 2025.PDF"]);
    assert_eq!(names("dm:<2025"), vec!["report.txt"]);
    assert_eq!(
        names("datemodified:>=2026-03-17 !sys"),
        vec!["Annual Report 2026.pdf", "draft report.txt"]
    );
    // Created 30 days before modified.
    assert_eq!(
        names("dc:2026-02-16"),
        vec!["Annual Report 2026.pdf", "pagefile.sys"]
    );
    assert_eq!(names("datecreated:lastmonth song"), Vec::<String>::new());
    // Folders in this sample have no dates: they never match a date.
    assert!(names("dm:thisyear").iter().all(|n| n != "Docs"));
    assert!(err("dm:someday").starts_with("dm:"));
    assert!(err("dc:2026-99").starts_with("dc:"));

    assert_eq!(names("len:3"), vec!["Old"]);
    assert_eq!(
        names("len:>20"),
        vec!["Annual Report 2026.pdf", "annual report 2025.PDF"]
    );
    assert_eq!(
        names("len:5..6"),
        vec!["Backup", "Empty", "Music", "README"]
    );
    assert_eq!(
        names("len:11"),
        vec!["Résumé.docx", "名前 ファイル.txt"],
        "characters, not bytes"
    );
    assert!(err("len:long").starts_with("len:"));

    assert_eq!(names("attrib:h"), vec!["notes.txt", "pagefile.sys"]);
    assert_eq!(names("attrib:hs"), vec!["pagefile.sys"]);
    assert_eq!(names("attrib:r"), vec!["draft report.txt"]);
    assert_eq!(names("attrib:d"), names("type:folder"));
    assert!(err("attrib:x").starts_with("attrib:"));
}

#[test]
fn dupes_and_empty_folders() {
    assert_eq!(
        run("dupe:"),
        vec!["C:\\Docs\\Old\\report.txt", "D:\\Backup\\report.txt"]
    );
    assert_eq!(run("dupe: drive:d"), vec!["D:\\Backup\\report.txt"]);
    assert_eq!(names("empty:"), vec!["Empty"]);
    assert_eq!(
        names("!empty: type:folder"),
        vec!["Backup", "Docs", "Music", "Old"]
    );
    // With an argument these are ordinary text.
    assert_eq!(names("dupe:x"), Vec::<String>::new());
}

#[test]
fn hidden_and_system_can_be_left_out() {
    let idx = sample();
    let no_hidden = SearchOptions {
        include_hidden: false,
        ..Default::default()
    };
    let all = run_with(&idx, "", None, no_hidden);
    assert!(!all
        .iter()
        .any(|p| p.ends_with("notes.txt") || p.ends_with("pagefile.sys")));
    let no_system = SearchOptions {
        include_system: false,
        ..Default::default()
    };
    let all = run_with(&idx, "", None, no_system);
    assert!(all.iter().any(|p| p.ends_with("notes.txt")));
    assert!(!all.iter().any(|p| p.ends_with("pagefile.sys")));
}

#[test]
fn regex_mode_and_modifier() {
    assert_eq!(
        names("regex:^song\\d+\\.mp3$"),
        vec!["song1.mp3", "song10.mp3", "song2.mp3"]
    );
    assert_eq!(names("regex:^SONG\\d\\."), vec!["song1.mp3", "song2.mp3"]);
    assert_eq!(names("case:regex:^SONG"), Vec::<String>::new());
    assert_eq!(
        names("regex:report|song flac"),
        vec!["Song.FLAC"],
        "the rest of the token is the expression"
    );
    assert!(err("regex:(").contains("regular expression"));
    let idx = sample();
    let re = SearchOptions {
        regex: true,
        ..Default::default()
    };
    assert_eq!(run_with(&idx, "^(annual|draft) report", None, re).len(), 3);
    assert_eq!(run_with(&idx, "", None, re).len(), idx.total());
    let re_path = SearchOptions {
        regex: true,
        match_path: true,
        ..Default::default()
    };
    assert_eq!(
        run_with(&idx, r"^d:\\backup\\[^\\]+\.txt$", None, re_path).len(),
        2
    );
    assert!(parse("(", None, &re, NOW).is_err());
    // The filter stays in the plain language even in regex mode.
    assert_eq!(run_with(&idx, "^s", Some("ext:mp3"), re).len(), 3);
}

#[test]
fn filters_are_anded() {
    let idx = sample();
    let filters = builtin_filters();
    let names_of: Vec<&str> = filters.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names_of,
        vec![
            "Audio",
            "Video",
            "Pictures",
            "Documents",
            "Compressed",
            "Executables",
            "Folders"
        ]
    );
    let get = |n: &str| filters.iter().find(|f| f.name == n).unwrap().query.clone();
    let o = SearchOptions::default();
    assert_eq!(run_with(&idx, "", Some(&get("Audio")), o).len(), 4);
    assert_eq!(run_with(&idx, "song1", Some(&get("Audio")), o).len(), 2);
    assert_eq!(
        run_with(&idx, "", Some(&get("Pictures")), o),
        vec!["D:\\Backup\\photo.JPG"]
    );
    assert_eq!(
        run_with(&idx, "", Some(&get("Compressed")), o),
        vec!["D:\\Backup\\archive.tar.gz"]
    );
    assert_eq!(run_with(&idx, "", Some(&get("Folders")), o).len(), 5);
    assert_eq!(
        run_with(&idx, "report", Some(&get("Documents")), o).len(),
        5
    );
    for f in &filters {
        assert!(parse("", Some(&f.query), &o, NOW).is_ok(), "{}", f.name);
    }
    assert!(parse("x", Some("size:nonsense"), &o, NOW).is_err());
}

#[test]
fn unknown_prefixes_are_plain_text() {
    assert_eq!(names("nosuchfunction:report"), Vec::<String>::new());
    // A drive letter is not a function.
    assert_eq!(run("d:\\backup\\readme"), vec!["D:\\Backup\\README"]);
    assert!(parse("a:b:c d: :", None, &SearchOptions::default(), NOW).is_ok());
}

#[test]
fn highlights() {
    let o = SearchOptions::default();
    let q = parse("annual resume|2026 !draft ext:pdf", None, &o, NOW).unwrap();
    let name = "My Résumé annual 2026.pdf";
    let marks: Vec<&str> = q.highlights(name).into_iter().map(|r| &name[r]).collect();
    assert_eq!(marks, vec!["Résumé", "annual", "2026"]);
    assert!(
        q.highlights("draft").is_empty(),
        "negated terms are not highlighted"
    );
    let q = parse("rep port", None, &o, NOW).unwrap();
    assert_eq!(
        q.highlights("report"),
        vec![0..6],
        "overlapping ranges merge"
    );
    let q = parse("docs\\rep", None, &o, NOW).unwrap();
    assert!(
        q.highlights("report").is_empty(),
        "path terms are not highlighted"
    );
    let q = parse("x", Some("report"), &o, NOW).unwrap();
    assert!(
        q.highlights("report").is_empty(),
        "filter terms are not highlighted"
    );
    let re = SearchOptions { regex: true, ..o };
    let q = parse(r"\d+", None, &re, NOW).unwrap();
    assert_eq!(q.highlights("song10.mp3"), vec![4..6]);
}

fn sorted(key: SortKey, ascending: bool) -> Vec<String> {
    let idx = sample();
    let q = parse("type:file !sys", None, &SearchOptions::default(), NOW).unwrap();
    let cancel = AtomicBool::new(false);
    let mut hits = search(&idx, &q, &cancel);
    sort(&idx, &mut hits, key, ascending, &cancel);
    hits.iter().map(|h| idx.name(*h).to_string()).collect()
}

#[test]
fn sorting() {
    let by_name = sorted(SortKey::Name, true);
    assert_eq!(
        by_name,
        vec![
            "annual report 2025.PDF",
            "Annual Report 2026.pdf",
            "archive.tar.gz",
            "draft report.txt",
            "notes.txt",
            "photo.JPG",
            "README",
            "report.txt",
            "report.txt",
            "Résumé.docx",
            "Song.FLAC",
            "song1.mp3",
            "song2.mp3",
            "song10.mp3",
            "名前 ファイル.txt",
        ]
    );
    let mut reversed = sorted(SortKey::Name, false);
    reversed.reverse();
    assert_eq!(reversed, by_name);

    let by_size = sorted(SortKey::Size, false);
    assert_eq!(
        &by_size[..3],
        &["archive.tar.gz", "Song.FLAC", "song10.mp3"]
    );
    assert_eq!(by_size.last().unwrap(), "notes.txt");
    // Equal sizes fall back to the name.
    let small = sorted(SortKey::Size, true);
    assert_eq!(&small[..3], &["notes.txt", "README", "名前 ファイル.txt"]);

    let newest = sorted(SortKey::Modified, false);
    assert_eq!(
        &newest[..3],
        &["Annual Report 2026.pdf", "draft report.txt", "Résumé.docx"]
    );
    let oldest_created = sorted(SortKey::Created, true);
    assert_eq!(oldest_created[0], "report.txt");

    let by_type = sorted(SortKey::Type, true);
    assert_eq!(by_type[0], "README", "no extension sorts first among files");
    let exts: Vec<String> = by_type
        .iter()
        .map(|n| crate::util::extension(n).to_lowercase())
        .collect();
    let mut want = exts.clone();
    want.sort();
    assert_eq!(exts, want);

    let idx = sample();
    let q = parse("", None, &SearchOptions::default(), NOW).unwrap();
    let cancel = AtomicBool::new(false);
    let mut hits = search(&idx, &q, &cancel);
    sort(&idx, &mut hits, SortKey::Path, true, &cancel);
    let paths: Vec<String> = hits.iter().map(|h| idx.path(*h)).collect();
    assert_eq!(
        &paths[..4],
        &[
            "C:\\Docs",
            "C:\\Music",
            "C:\\pagefile.sys",
            "C:\\Docs\\annual report 2025.PDF"
        ]
    );
    assert_eq!(paths.last().unwrap(), "D:\\Backup\\名前 ファイル.txt");
    // Folders sort first by type.
    sort(&idx, &mut hits, SortKey::Type, true, &cancel);
    assert!(hits[..5].iter().all(|h| idx.entry(*h).unwrap().is_dir()));
}

#[test]
fn cancelled_work_stops() {
    let idx = sample();
    let q = parse("", None, &SearchOptions::default(), NOW).unwrap();
    let cancel = AtomicBool::new(true);
    assert!(search(&idx, &q, &cancel).is_empty());
}

#[test]
fn odd_input_never_panics() {
    let idx = sample();
    let cancel = AtomicBool::new(false);
    let o = SearchOptions::default();
    let pieces = [
        "\"", "<", ">", "|", "!", " ", ":", "\\", "*", "?", "size:", "dm:", "ext:", "regex:", "a",
        "é", "名", "\u{301}", "\u{0}", "len:", "<>", "!!", "||", "case:", "path:", "..", ">=",
        "\u{feff}", "parent:", "drive:", "type:", "attrib:", "depth:", "🙂",
    ];
    let mut seed = 0x1234_5678_9abc_def0u64;
    for _ in 0..3000 {
        let mut text = String::new();
        for _ in 0..(seed % 7 + 1) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            text.push_str(pieces[(seed % pieces.len() as u64) as usize]);
        }
        if let Ok(q) = parse(&text, None, &o, NOW) {
            let hits = search(&idx, &q, &cancel);
            for h in hits.iter().take(3) {
                for r in q.highlights(idx.name(*h)) {
                    assert!(idx.name(*h).get(r).is_some(), "{text:?}");
                }
            }
        }
    }
    assert!(parse(&"a ".repeat(20_000), None, &o, NOW).is_err());
    assert!(parse(&"<".repeat(200), None, &o, NOW).is_err());
    assert!(parse(&"!".repeat(5000), None, &o, NOW).is_ok());
}

#[test]
fn help_and_demo_index() {
    let help = syntax_help();
    assert!(help.len() >= 20);
    assert!(help.iter().all(|(a, b)| !a.is_empty() && b.ends_with('.')));
    // Every single-example line parses.
    for (example, _) in &help {
        for part in example.split("   ") {
            assert!(
                parse(part, None, &SearchOptions::default(), NOW).is_ok(),
                "{part}"
            );
        }
    }
    let idx = crate::demo::index(NOW.unix as u32);
    let found = run_with(
        &idx,
        "quarterly report ext:xlsx",
        None,
        SearchOptions::default(),
    );
    assert_eq!(found.len(), 2);
    assert_eq!(compare_names("a2", "a10"), std::cmp::Ordering::Less);
}

/// Prints timings over a synthetic two-million-entry index.
/// `cargo test --release --lib query::tests::bench -- --ignored --nocapture`
#[test]
#[ignore = "benchmark; run in release mode"]
fn bench_two_million_entries() {
    use std::time::Instant;
    let idx = crate::demo::synthetic(2_000_000);
    println!(
        "entries {}  memory {:.1} MB  {:.1} bytes/entry  threads {}",
        idx.total(),
        idx.memory_bytes() as f64 / 1_048_576.0,
        idx.memory_bytes() as f64 / idx.total() as f64,
        rayon::current_num_threads()
    );
    let cancel = AtomicBool::new(false);
    let o = SearchOptions::default();
    let mut all = Vec::new();
    for text in [
        "",
        "e",
        "repo",
        "report budget",
        "*-final-*.pdf",
        "notes\\report",
        "ext:jpg;png",
        "size:>40mb",
        "dm:2024",
        "regex:^song-.*7\\.mp3$",
        "dupe:",
        "empty:",
        "!report",
        "len:>24",
    ] {
        let q = parse(text, None, &o, NOW).unwrap();
        let mut best = f64::MAX;
        let mut count = 0;
        for _ in 0..5 {
            let started = Instant::now();
            let hits = search(&idx, &q, &cancel);
            best = best.min(started.elapsed().as_secs_f64() * 1000.0);
            count = hits.len();
            if text.is_empty() {
                all = hits;
            }
        }
        println!("search {text:<24} {count:>9} hits  {best:>8.2} ms");
    }
    for key in [
        SortKey::Name,
        SortKey::Size,
        SortKey::Modified,
        SortKey::Type,
        SortKey::Path,
    ] {
        let mut best = f64::MAX;
        for _ in 0..3 {
            let mut hits = all.clone();
            let started = Instant::now();
            sort(&idx, &mut hits, key, true, &cancel);
            best = best.min(started.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "sort   {:<24} {:>9} hits  {best:>8.2} ms",
            key.id(),
            all.len()
        );
    }
}
