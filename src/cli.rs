//! The command line client: `FindForge.exe -s "query"`.

use crate::engine::{self, Engine, SearchRequest, VolState};
use crate::query::{SearchOptions, SortKey};
use crate::settings::Settings;
use crate::{export, sys};
use std::io::Write;
use std::time::{Duration, Instant};

/// Options of one command line search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliSearch {
    pub query: String,
    pub limit: Option<usize>,
    pub sort: SortKey,
    pub descending: bool,
    pub csv: bool,
    pub count_only: bool,
    /// Wait for a full rescan before searching.
    pub refresh: bool,
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub regex: bool,
}

/// Parses the arguments after the program name. `Ok(None)` when this is not
/// a command line search at all.
pub fn parse_args(args: &[String]) -> Result<Option<CliSearch>, String> {
    let Some(pos) = args.iter().position(|a| a == "-s" || a == "--search") else {
        return Ok(None);
    };
    let query = args
        .get(pos + 1)
        .cloned()
        .ok_or("Missing the text to search for after -s.")?;
    let mut s = CliSearch {
        query,
        limit: None,
        sort: SortKey::Name,
        descending: false,
        csv: false,
        count_only: false,
        refresh: false,
        match_case: false,
        whole_word: false,
        match_path: false,
        regex: false,
    };
    let mut i = 0;
    while i < args.len() {
        if i == pos {
            i += 2;
            continue;
        }
        match args[i].as_str() {
            "-n" | "--limit" => {
                let v = args.get(i + 1).ok_or("Missing the number after --limit.")?;
                s.limit = Some(v.parse().map_err(|_| format!("\"{v}\" is not a number."))?);
                i += 1;
            }
            "--sort" => {
                let v = args.get(i + 1).ok_or("Missing the column after --sort.")?;
                s.sort = match v.as_str() {
                    "name" | "path" | "size" | "modified" | "created" | "type" => {
                        SortKey::from_id(v)
                    }
                    _ => {
                        return Err(format!(
                            "Cannot sort by \"{v}\" (name, path, size, modified, created, type)."
                        ))
                    }
                };
                i += 1;
            }
            "--desc" => s.descending = true,
            "--csv" => s.csv = true,
            "--count" => s.count_only = true,
            "--refresh" => s.refresh = true,
            "--case" => s.match_case = true,
            "--whole-word" => s.whole_word = true,
            "--path" => s.match_path = true,
            "--regex" => s.regex = true,
            other => return Err(format!("Unknown option \"{other}\".")),
        }
        i += 1;
    }
    Ok(Some(s))
}

/// Runs a command line search and prints the results; returns the exit code
/// (0 = found something, 1 = nothing found, 2 = error).
pub fn run(search: &CliSearch) -> i32 {
    let dir = sys::app_dir();
    let settings = Settings::load(&dir);
    let mut config = engine::config_from_settings(&settings, Some(dir));
    config.refresh_loaded = search.refresh;
    if config.roots.is_empty() {
        eprintln!("Nothing is set to be indexed. Open FindForge and choose drives in Settings.");
        return 2;
    }
    let engine = Engine::start(config, Box::new(|| {}));
    // Wait until every volume has an index (from disk or from a first scan).
    let started = Instant::now();
    let mut told = false;
    loop {
        let st = engine.status();
        let pending = st.volumes.is_empty()
            || st.volumes.iter().any(|v| {
                matches!(v.state, VolState::Loading | VolState::Scanning { .. })
                    || (search.refresh && v.refreshing)
            });
        if !pending {
            break;
        }
        if !told && started.elapsed() > Duration::from_millis(1500) {
            eprintln!("Indexing, please wait...");
            told = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let request = SearchRequest {
        id: 1,
        text: search.query.clone(),
        filter: None,
        options: SearchOptions {
            match_case: search.match_case,
            whole_word: search.whole_word,
            match_path: search.match_path,
            regex: search.regex,
            match_diacritics: settings.match_diacritics,
            include_hidden: settings.include_hidden,
            include_system: settings.include_system,
        },
        sort: search.sort,
        ascending: !search.descending,
    };
    let results = {
        let idx = engine.index().read().unwrap();
        engine::search_now(&idx, &request)
    };
    let code = if let Some(e) = &results.error {
        eprintln!("{e}");
        2
    } else {
        let hits = &results.hits[..search.limit.unwrap_or(usize::MAX).min(results.hits.len())];
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        let offset = sys::local_utc_offset();
        let mut ok = true;
        if search.count_only {
            ok = writeln!(out, "{}", results.hits.len()).is_ok();
        } else {
            let format = if search.csv {
                export::Format::Csv
            } else {
                export::Format::Txt
            };
            let mut first = true;
            for chunk in hits.chunks(20_000) {
                let rows = engine.rows(chunk);
                // A closed pipe (`| head`) is not an error worth reporting.
                if export::write_rows(&mut out, &rows, format, offset, first).is_err() {
                    ok = false;
                    break;
                }
                first = false;
            }
        }
        let _ = out.flush();
        if !ok {
            0
        } else if results.hits.is_empty() {
            1
        } else {
            0
        }
    };
    engine.shutdown();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn argument_parsing() {
        assert_eq!(parse_args(&args(&["--demo"])), Ok(None));
        let s = parse_args(&args(&["-s", "report ext:pdf"]))
            .unwrap()
            .unwrap();
        assert_eq!(s.query, "report ext:pdf");
        assert_eq!(s.limit, None);
        assert_eq!(s.sort, SortKey::Name);
        let s = parse_args(&args(&[
            "--limit",
            "20",
            "--search",
            "*.mp3",
            "--sort",
            "size",
            "--desc",
            "--csv",
            "--case",
            "--regex",
            "--path",
            "--whole-word",
            "--count",
            "--refresh",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(s.query, "*.mp3");
        assert_eq!(s.limit, Some(20));
        assert_eq!(s.sort, SortKey::Size);
        assert!(s.descending && s.csv && s.match_case && s.regex && s.match_path);
        assert!(s.whole_word && s.count_only && s.refresh);
        // The query may look like an option.
        let s = parse_args(&args(&["-s", "--csv"])).unwrap().unwrap();
        assert_eq!(s.query, "--csv");
        assert!(!s.csv);
        assert!(parse_args(&args(&["-s"])).is_err());
        assert!(parse_args(&args(&["-s", "x", "--limit", "many"])).is_err());
        assert!(parse_args(&args(&["-s", "x", "--sort", "colour"])).is_err());
        assert!(parse_args(&args(&["-s", "x", "--bogus"])).is_err());
    }
}
