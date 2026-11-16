//! `--selftest`: exercises the whole pipeline against a throwaway folder in
//! the temp directory (never against the user's files), and `--bench`, which
//! indexes real drives read-only and prints numbers.

use crate::engine::{self, Engine, EngineConfig, RootSpec, SearchRequest, VolState};
use crate::index::{Excludes, VolumeKind, VolumeStore};
use crate::query::{SearchOptions, SortKey};
use crate::{export, shell, sys, util, volumes};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn request(text: &str) -> SearchRequest {
    SearchRequest {
        id: 0,
        text: text.to_string(),
        filter: None,
        options: SearchOptions::default(),
        sort: SortKey::Name,
        ascending: true,
    }
}

fn count(engine: &Engine, text: &str) -> Result<usize, String> {
    let idx = engine.index().read().unwrap();
    let r = engine::search_now(&idx, &request(text));
    match r.error {
        Some(e) => Err(format!("query {text:?} failed: {e}")),
        None => Ok(r.hits.len()),
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) -> Result<(), String> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        if ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Err(format!("timed out waiting for: {what}"))
}

fn wait_ready(engine: &Engine) -> Result<(), String> {
    wait_until("the index to be ready", || {
        let st = engine.status();
        !st.volumes.is_empty()
            && st
                .volumes
                .iter()
                .all(|v| matches!(v.state, VolState::Ready | VolState::Failed(_)) && !v.refreshing)
    })?;
    let st = engine.status();
    match st.volumes.iter().find_map(|v| match &v.state {
        VolState::Failed(e) => Some(e.clone()),
        _ => None,
    }) {
        Some(e) => Err(format!("indexing failed: {e}")),
        None => Ok(()),
    }
}

fn spec_for(folder: &Path) -> RootSpec {
    let root = folder.to_string_lossy().trim_end_matches('\\').to_string();
    let (label, fs, serial) = volumes::drive_root_of(&root)
        .and_then(|d| volumes::volume_information(&d))
        .unwrap_or_default();
    RootSpec {
        root,
        is_drive: false,
        fs,
        label,
        serial,
    }
}

fn kind_name(kind: VolumeKind) -> &'static str {
    match kind {
        VolumeKind::NtfsMft => "master file table + change journal",
        VolumeKind::NtfsWalk => "folder walk + change notifications",
        VolumeKind::Walk => "folder walk, rescanned periodically",
    }
}

/// Runs the self-test; returns the log lines, or the log so far plus what failed.
pub fn run() -> Result<Vec<String>, String> {
    let base = std::env::temp_dir().join(format!("findforge-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let mut log = Vec::new();
    let result = run_in(&base, &mut log);
    let _ = std::fs::remove_dir_all(&base);
    match result {
        Ok(()) => {
            log.push("temp folder removed".into());
            Ok(log)
        }
        Err(e) => Err(format!("{}\nFAILED: {e}", log.join("\n"))),
    }
}

fn check(log: &mut Vec<String>, what: &str, got: usize, want: usize) -> Result<(), String> {
    if got == want {
        log.push(format!("ok  {what}: {got}"));
        Ok(())
    } else {
        Err(format!("{what}: expected {want}, got {got}"))
    }
}

fn run_in(base: &Path, log: &mut Vec<String>) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    let tree = base.join("tree");
    let data = base.join("index");
    for d in ["Docs\\Reports", "Pictures", "Music\\Album", "Empty folder"] {
        std::fs::create_dir_all(tree.join(d)).map_err(io)?;
    }
    let files: [(&str, usize); 10] = [
        ("Docs\\Reports\\annual report 2026.pdf", 2_500_000),
        ("Docs\\Reports\\annual report 2025.pdf", 1_200),
        ("Docs\\Reports\\draft report.txt", 300),
        ("Docs\\résumé.docx", 40_000),
        ("Docs\\notes.txt", 0),
        ("Docs\\old notes.bak", 10),
        ("Pictures\\holiday-0001.jpg", 800_000),
        ("Pictures\\holiday-0002.jpg", 900_000),
        ("Music\\Album\\01 Morning Light.mp3", 3_000_000),
        ("Music\\Album\\cover.jpg", 50_000),
    ];
    for (rel, size) in files {
        std::fs::write(tree.join(rel), vec![b'x'; size]).map_err(io)?;
    }
    log.push("created a temp tree: 6 folders, 10 files".into());

    let config = EngineConfig {
        roots: vec![spec_for(&tree)],
        excludes: Excludes::new(&[], &["*.bak".to_string()]),
        rescan_minutes: 60,
        data_dir: Some(data.clone()),
        elevated: false,
        refresh_loaded: true,
    };
    let engine = Engine::start(config.clone(), Box::new(|| {}));
    wait_ready(&engine)?;
    let st = engine.status();
    let vol = st.volumes[0].clone();
    log.push(format!(
        "indexed by: {} ({}), live updates: {}",
        kind_name(vol.kind),
        if vol.fs.is_empty() { "?" } else { &vol.fs },
        vol.live
    ));
    // 6 folders + 9 files (the .bak file is excluded).
    check(log, "entries indexed", st.total_entries, 15)?;

    // Searches.
    check(log, "\"report\"", count(&engine, "report")?, 4)?;
    check(
        log,
        "\"annual report\" phrase",
        count(&engine, "\"annual report\"")?,
        2,
    )?;
    check(
        log,
        "report !draft ext:pdf",
        count(&engine, "report !draft ext:pdf")?,
        2,
    )?;
    check(
        log,
        "resume (diacritics folded)",
        count(&engine, "resume")?,
        1,
    )?;
    check(log, "*.jpg", count(&engine, "*.jpg")?, 3)?;
    check(
        log,
        "pictures\\ (path term)",
        count(&engine, "pictures\\")?,
        2,
    )?;
    check(log, "size:>1mb", count(&engine, "size:>1mb")?, 2)?;
    check(log, "size:empty", count(&engine, "size:empty")?, 1)?;
    check(
        log,
        "dm:today type:file",
        count(&engine, "dm:today type:file")?,
        9,
    )?;
    check(log, "type:folder", count(&engine, "type:folder")?, 6)?;
    check(log, "empty:", count(&engine, "empty:")?, 1)?;
    check(
        log,
        "regex:^holiday-\\d{4}",
        count(&engine, "regex:^holiday-\\d{4}")?,
        2,
    )?;
    check(log, "excluded *.bak", count(&engine, "bak")?, 0)?;
    if count(&engine, "size:nonsense").is_ok() {
        return Err("a malformed query was accepted".into());
    }
    log.push("ok  a malformed query is reported, not run".into());

    // Sorting.
    {
        let idx = engine.index().read().unwrap();
        let mut req = request("type:file");
        req.sort = SortKey::Size;
        req.ascending = false;
        let r = engine::search_now(&idx, &req);
        let rows: Vec<engine::Row> = r.hits.iter().map(|h| engine::row_of(&idx, *h)).collect();
        if rows.first().map(|r| r.size) != Some(3_000_000)
            || !rows.windows(2).all(|w| w[0].size >= w[1].size)
        {
            return Err("sorting by size is wrong".into());
        }
        if r.total_bytes != rows.iter().map(|r| r.size).sum::<u64>() {
            return Err("the total size is wrong".into());
        }
        log.push("ok  sorted by size, largest first".into());
    }

    // Live changes (or a rescan where the volume cannot be watched).
    let live = vol.live;
    let settle = |what: &str, text: &str, want: usize| -> Result<(), String> {
        if !live {
            engine.rescan(None);
        }
        wait_until(what, || count(&engine, text).ok() == Some(want))
    };
    std::fs::write(tree.join("Docs\\fresh idea.txt"), b"hello").map_err(io)?;
    settle("a new file to appear", "fresh idea", 1)?;
    log.push("ok  a new file appears".into());
    wait_until("its size to be known", || {
        count(&engine, "\"fresh idea\" size:5").ok() == Some(1)
    })
    .or_else(|e| if live { Err(e) } else { Ok(()) })?;
    std::fs::rename(
        tree.join("Docs\\fresh idea.txt"),
        tree.join("Docs\\renamed idea.txt"),
    )
    .map_err(io)?;
    settle("the rename to show", "renamed idea", 1)?;
    check(log, "old name after rename", count(&engine, "fresh")?, 0)?;
    std::fs::rename(
        tree.join("Docs\\renamed idea.txt"),
        tree.join("Pictures\\moved idea.txt"),
    )
    .map_err(io)?;
    settle("the move to show", "pictures\\moved", 1)?;
    log.push("ok  a move to another folder is followed".into());
    std::fs::rename(tree.join("Music\\Album"), tree.join("Music\\Renamed album")).map_err(io)?;
    settle(
        "the folder rename to show",
        "\"renamed album\\\" ext:mp3",
        1,
    )?;
    log.push("ok  renaming a folder carries its files along".into());
    std::fs::remove_file(tree.join("Pictures\\moved idea.txt")).map_err(io)?;
    settle("the deletion to show", "moved idea", 0)?;
    log.push("ok  a deleted file disappears".into());
    std::fs::remove_dir_all(tree.join("Music")).map_err(io)?;
    settle("the folder deletion to show", "music|mp3|cover", 0)?;
    log.push("ok  a deleted folder disappears with its contents".into());

    // Renaming through the program's own action.
    let old = tree.join("Docs\\notes.txt").to_string_lossy().into_owned();
    if shell::rename(&old, "bad/name").is_ok() {
        return Err("an invalid file name was accepted".into());
    }
    shell::rename(&old, "notes renamed.txt")?;
    engine.note_removed(&old);
    settle("the in-app rename to show", "\"notes renamed\"", 1)?;
    log.push("ok  rename action (and an invalid name is refused)".into());

    // Export.
    let csv = base.join("export.csv");
    let hits = {
        let idx = engine.index().read().unwrap();
        engine::search_now(&idx, &request("report")).hits
    };
    let written = export::export(&engine, &hits, &csv, export::Format::Csv, 0)?;
    let text = std::fs::read_to_string(&csv).map_err(io)?;
    check(log, "CSV rows exported", written, 4)?;
    check(log, "CSV lines (with header)", text.lines().count(), 5)?;

    // Save, reload from disk, and survive a damaged index file.
    let before = engine.status().total_entries;
    engine.shutdown();
    let saved = data.join(engine::index_file_name(&config.roots[0].root));
    let size = std::fs::metadata(&saved)
        .map_err(|_| "the index was not saved".to_string())?
        .len();
    log.push(format!(
        "ok  index saved ({size} bytes for {before} entries)"
    ));
    let loaded = VolumeStore::load(&saved)?;
    check(
        log,
        "entries in the saved file",
        loaded.live_count() - 1,
        before,
    )?;

    let mut quick = config.clone();
    quick.refresh_loaded = false;
    let engine = Engine::start(quick, Box::new(|| {}));
    wait_ready(&engine)?;
    let st = engine.status();
    if !st.volumes[0].from_disk {
        return Err("the saved index was not used".into());
    }
    check(
        log,
        "entries after loading from disk",
        st.total_entries,
        before,
    )?;
    check(
        log,
        "\"report\" after loading",
        count(&engine, "report")?,
        4,
    )?;
    engine.shutdown();

    let mut bytes = std::fs::read(&saved).map_err(io)?;
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x5A;
    std::fs::write(&saved, &bytes).map_err(io)?;
    let engine = Engine::start(config, Box::new(|| {}));
    wait_ready(&engine)?;
    let st = engine.status();
    if st.volumes[0].from_disk {
        return Err("a damaged index file was trusted".into());
    }
    check(
        log,
        "entries after a damaged index (rescanned)",
        st.total_entries,
        before,
    )?;
    engine.shutdown();
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `--bench`: indexes real drives (read-only) and prints timings. Only
/// numbers are printed, never names. Returns the exit code.
pub fn bench(drive: Option<&str>) -> i32 {
    if let Some(n) = drive.and_then(|d| d.strip_prefix("synthetic")) {
        let entries = n.trim_start_matches(':').parse().unwrap_or(2_000_000usize);
        let started = Instant::now();
        let index = crate::demo::synthetic(entries.clamp(1000, 20_000_000));
        println!(
            "FindForge {} benchmark on a made-up index: {} entries built in {:.2} s; index memory {:.1} MB ({:.1} bytes/entry); threads: {}",
            env!("CARGO_PKG_VERSION"),
            util::fmt_count(index.total() as u64),
            started.elapsed().as_secs_f64(),
            index.memory_bytes() as f64 / 1_048_576.0,
            index.memory_bytes() as f64 / index.total().max(1) as f64,
            rayon::current_num_threads()
        );
        let engine = Engine::with_index(index, Box::new(|| {}));
        bench_queries(&engine);
        bench_save_load(&engine);
        engine.shutdown();
        return 0;
    }
    let elevated = sys::is_elevated();
    let mut roots: Vec<RootSpec> = volumes::list()
        .into_iter()
        .filter(|v| matches!(v.kind, volumes::DriveKind::Fixed))
        .map(|v| RootSpec {
            root: v.root,
            is_drive: true,
            fs: v.fs,
            label: String::new(),
            serial: v.serial,
        })
        .collect();
    if let Some(d) = drive {
        let d = d.trim_end_matches('\\');
        roots.retain(|r| r.root.eq_ignore_ascii_case(d));
    }
    if roots.is_empty() {
        println!("No matching fixed drive.");
        return 2;
    }
    println!(
        "FindForge {} benchmark; administrator rights: {}; threads: {}",
        env!("CARGO_PKG_VERSION"),
        if elevated { "yes" } else { "no" },
        rayon::current_num_threads()
    );
    let before_ws = sys::process_memory();
    let started = Instant::now();
    let engine = Engine::start(
        EngineConfig {
            roots,
            excludes: Excludes::default(),
            rescan_minutes: 24 * 60,
            data_dir: None,
            elevated,
            refresh_loaded: true,
        },
        Box::new(|| {}),
    );
    if let Err(e) = wait_bench_ready(&engine) {
        println!("{e}");
        return 2;
    }
    let wall = started.elapsed();
    let st = engine.status();
    for (i, v) in st.volumes.iter().enumerate() {
        println!(
            "drive {}: {} ({}) entries {}  scan {:.2} s  index memory {:.1} MB  {:.1} bytes/entry  live {}{}",
            i + 1,
            kind_name(v.kind),
            v.fs,
            util::fmt_count(v.entries as u64),
            v.scan_ms as f64 / 1000.0,
            v.memory_bytes as f64 / 1_048_576.0,
            v.memory_bytes as f64 / v.entries.max(1) as f64,
            v.live,
            if v.note.is_empty() { String::new() } else { format!("  note: {}", v.note) },
        );
    }
    println!(
        "total: {} entries in {:.2} s wall; index memory {:.1} MB ({:.1} bytes/entry); process working set {:.1} MB (was {:.1} MB before indexing)",
        util::fmt_count(st.total_entries as u64),
        wall.as_secs_f64(),
        st.memory_bytes as f64 / 1_048_576.0,
        st.memory_bytes as f64 / st.total_entries.max(1) as f64,
        sys::process_memory() as f64 / 1_048_576.0,
        before_ws as f64 / 1_048_576.0,
    );

    bench_queries(&engine);
    bench_save_load(&engine);
    engine.shutdown();
    0
}

fn bench_queries(engine: &Engine) {
    println!("query latency, best of 5 (search + sort by name):");
    let queries = [
        "",
        "e",
        "a",
        "the",
        "report",
        "config",
        "*.dll",
        "*.txt",
        "ext:jpg;png",
        "size:>100mb",
        "dm:today",
        "dm:thisyear ext:pdf",
        "windows\\system",
        "regex:^a.*\\.exe$",
        "!e",
        "dupe:",
        "empty:",
        "type:folder",
        "len:>60",
    ];
    for q in queries {
        let idx = engine.index().read().unwrap();
        let mut best = Duration::MAX;
        let mut hits = 0;
        for _ in 0..5 {
            let r = engine::search_now(&idx, &request(q));
            best = best.min(r.took);
            hits = r.hits.len();
        }
        println!(
            "  {:<22} {:>10} hits  {:>8.2} ms",
            format!("\"{q}\""),
            util::fmt_count(hits as u64),
            ms(best)
        );
    }
    for (label, key) in [
        ("size", SortKey::Size),
        ("date modified", SortKey::Modified),
        ("path", SortKey::Path),
        ("type", SortKey::Type),
    ] {
        let idx = engine.index().read().unwrap();
        let mut req = request("");
        req.sort = key;
        let mut best = Duration::MAX;
        for _ in 0..3 {
            best = best.min(engine::search_now(&idx, &req).took);
        }
        println!("  everything by {label:<14} {:>18.2} ms", ms(best));
    }
}

fn bench_save_load(engine: &Engine) {
    // Saving and loading, in a temp folder that is removed again.
    let dir: PathBuf = std::env::temp_dir().join(format!("findforge-bench-{}", std::process::id()));
    {
        let idx = engine.index().read().unwrap();
        for (i, vol) in idx.volumes.iter().enumerate() {
            let file = dir.join(format!("bench-{i}.ffi"));
            let t = Instant::now();
            if let Err(e) = vol.save(&file) {
                println!("  save failed: {e}");
                continue;
            }
            let save = t.elapsed();
            let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            let t = Instant::now();
            let ok = VolumeStore::load(&file).is_ok();
            println!(
                "  drive {}: save {:.0} ms, file {:.1} MB, load {:.0} ms ({})",
                i + 1,
                ms(save),
                size as f64 / 1_048_576.0,
                ms(t.elapsed()),
                if ok { "verified" } else { "FAILED" }
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn wait_bench_ready(engine: &Engine) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let st = engine.status();
        let pending = st.volumes.is_empty()
            || st.volumes.iter().any(|v| {
                matches!(v.state, VolState::Loading | VolState::Scanning { .. }) || v.refreshing
            });
        if !pending {
            return match st.volumes.iter().find_map(|v| match &v.state {
                VolState::Failed(e) => Some(e.clone()),
                _ => None,
            }) {
                Some(e) => Err(format!("Indexing failed: {e}")),
                None => Ok(()),
            };
        }
        if started.elapsed() > Duration::from_secs(1800) {
            return Err("Indexing took longer than 30 minutes; giving up.".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn selftest_passes() {
        match super::run() {
            Ok(log) => assert!(log.len() > 20, "{log:?}"),
            Err(e) => panic!("{e}"),
        }
    }
}
