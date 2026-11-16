//! Small pure helpers: sizes, dates, wildcards, extensions.

/// The extension of a file name without the dot ("" when there is none).
/// A leading dot alone (`.gitignore`) does not count as an extension.
pub fn extension(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => &name[i + 1..],
        _ => "",
    }
}

/// Wildcard match: `*` is any run of characters, `?` exactly one.
/// Both arguments are compared as given (lower-case them first for a
/// case-insensitive match).
pub fn wild_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Human readable size, e.g. `1.4 GB`.
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `1234567` -> `1,234,567`.
pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A calendar date and time of day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime {
    pub y: i32,
    pub m: u32,
    pub d: u32,
    pub h: u32,
    pub min: u32,
    pub s: u32,
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The inverse of [`days_from_civil`]: `(year, month, day)`.
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

/// Splits Unix seconds (already shifted to local time) into date and time.
pub fn datetime_from_unix(secs: i64) -> DateTime {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    DateTime {
        y,
        m,
        d,
        h: rem / 3600,
        min: rem % 3600 / 60,
        s: rem % 60,
    }
}

/// Formats a UTC Unix time as local `2026-01-31 14:05` ("" for 0 = unknown).
pub fn fmt_time(unix: u32, utc_offset_secs: i32) -> String {
    if unix == 0 {
        return String::new();
    }
    let t = datetime_from_unix(unix as i64 + utc_offset_secs as i64);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", t.y, t.m, t.d, t.h, t.min)
}

/// Quotes a CSV field when needed.
pub fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions() {
        assert_eq!(extension("a.txt"), "txt");
        assert_eq!(extension("archive.tar.GZ"), "GZ");
        assert_eq!(extension(".gitignore"), "");
        assert_eq!(extension("noext"), "");
        assert_eq!(extension("trailing."), "");
        assert_eq!(extension(""), "");
    }

    #[test]
    fn wildcards() {
        assert!(wild_match("*.tmp", "a.tmp"));
        assert!(wild_match("*.tmp", ".tmp"));
        assert!(!wild_match("*.tmp", "a.tmpx"));
        assert!(wild_match("a?c", "abc"));
        assert!(!wild_match("a?c", "ac"));
        assert!(wild_match("*", ""));
        assert!(wild_match("a*b*c", "a--b--b--c"));
        assert!(!wild_match("a*b*c", "a--b--b--"));
        assert!(wild_match("node_modules", "node_modules"));
        assert!(!wild_match("node_modules", "node_modules2"));
        assert!(wild_match("?é*", "xéz"));
    }

    #[test]
    fn sizes_and_counts() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(1023), "1023 B");
        assert_eq!(fmt_size(1536), "1.5 KB");
        assert_eq!(fmt_size(300 * 1024 * 1024), "300 MB");
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(1234567), "1,234,567");
    }

    #[test]
    fn dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        for days in [-1i64, 59, 60, 365, 10_957, 19_723, 20_819, 47_482] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(fmt_time(0, 3600), "");
        assert_eq!(fmt_time(1_767_225_600, 0), "2026-01-01 00:00");
        assert_eq!(fmt_time(1_767_225_600, 3 * 3600), "2026-01-01 03:00");
        assert_eq!(fmt_time(1_767_225_600, -60), "2025-12-31 23:59");
    }

    #[test]
    fn csv() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }
}
