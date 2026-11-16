//! Exporting results to CSV or plain text.

use crate::engine::{Engine, Row};
use crate::index::Hit;
use crate::util::{csv_field, fmt_time};
use std::io::Write;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Name, Path, Size, Date Modified, Date Created, Type: one row per result.
    Csv,
    /// One full path per line.
    Txt,
}

impl Format {
    /// Chooses the format from a file name (`.csv` = CSV, anything else = text).
    pub fn from_path(path: &Path) -> Format {
        match path.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("csv") => Format::Csv,
            _ => Format::Txt,
        }
    }
}

pub const CSV_HEADER: &str = "Name,Path,Size,Date Modified,Date Created,Type";

/// One CSV line (without the line break) for a row.
pub fn csv_line(row: &Row, utc_offset_secs: i32) -> String {
    format!(
        "{},{},{},{},{},{}",
        csv_field(&row.name),
        csv_field(&row.dir),
        if row.is_dir {
            String::new()
        } else {
            row.size.to_string()
        },
        fmt_time(row.mtime, utc_offset_secs),
        fmt_time(row.ctime, utc_offset_secs),
        if row.is_dir {
            "Folder"
        } else {
            row.ext.as_str()
        },
    )
}

/// Writes rows in the chosen format. CSV starts with a UTF-8 byte order mark
/// so spreadsheet programs read accented names correctly.
pub fn write_rows<W: Write>(
    out: &mut W,
    rows: &[Row],
    format: Format,
    utc_offset_secs: i32,
    first_chunk: bool,
) -> std::io::Result<()> {
    if first_chunk && format == Format::Csv {
        out.write_all(b"\xEF\xBB\xBF")?;
        writeln!(out, "{CSV_HEADER}\r")?;
    }
    for row in rows.iter().filter(|r| r.exists) {
        match format {
            Format::Csv => write!(out, "{}\r\n", csv_line(row, utc_offset_secs))?,
            Format::Txt => write!(out, "{}\r\n", row.full_path())?,
        }
    }
    Ok(())
}

/// Exports `hits` to `path`; returns how many results were written.
pub fn export(
    engine: &Engine,
    hits: &[Hit],
    path: &Path,
    format: Format,
    utc_offset_secs: i32,
) -> Result<usize, String> {
    let file =
        std::fs::File::create(path).map_err(|e| format!("Could not create the file: {e}"))?;
    let mut out = std::io::BufWriter::new(file);
    let mut written = 0;
    let mut first = true;
    if hits.is_empty() {
        write_rows(&mut out, &[], format, utc_offset_secs, true).map_err(|e| e.to_string())?;
    }
    for chunk in hits.chunks(20_000) {
        let rows = engine.rows(chunk);
        written += rows.iter().filter(|r| r.exists).count();
        write_rows(&mut out, &rows, format, utc_offset_secs, first)
            .map_err(|e| format!("Could not write the file: {e}"))?;
        first = false;
    }
    out.flush()
        .map_err(|e| format!("Could not write the file: {e}"))?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, dir: &str, size: u64, is_dir: bool) -> Row {
        Row {
            name: name.into(),
            dir: dir.into(),
            size,
            mtime: 1_767_225_600,
            ctime: 0,
            is_dir,
            ext: crate::util::extension(name).to_lowercase(),
            flags: 0,
            exists: true,
        }
    }

    #[test]
    fn csv_and_txt() {
        let rows = vec![
            row("a, b.txt", "C:\\Docs", 12, false),
            row("Folder", "C:\\", 0, true),
            Row::default(), // deleted since the search: skipped
        ];
        let mut out = Vec::new();
        write_rows(&mut out, &rows, Format::Csv, 0, true).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.trim_start_matches('\u{feff}').lines().collect();
        assert_eq!(lines[0], CSV_HEADER);
        assert_eq!(lines[1], "\"a, b.txt\",C:\\Docs,12,2026-01-01 00:00,,txt");
        assert_eq!(lines[2], "Folder,C:\\,,2026-01-01 00:00,,Folder");
        assert_eq!(lines.len(), 3);

        let mut out = Vec::new();
        write_rows(&mut out, &rows, Format::Txt, 0, true).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "C:\\Docs\\a, b.txt\r\nC:\\Folder\r\n"
        );
    }

    #[test]
    fn format_from_extension() {
        assert_eq!(Format::from_path(Path::new("x.CSV")), Format::Csv);
        assert_eq!(Format::from_path(Path::new("x.txt")), Format::Txt);
        assert_eq!(Format::from_path(Path::new("x")), Format::Txt);
    }
}
