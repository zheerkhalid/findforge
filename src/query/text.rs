//! Text matching: case and diacritics folding, substring / wildcard / regex
//! tests, and the ranges to highlight in a name.

use std::ops::Range;

/// The unaccented base of a Latin letter with a diacritic, in lower case.
/// `None` for everything else.
pub(crate) fn base_of(c: char) -> Option<&'static str> {
    Some(match c {
        'À'..='Å' | 'à'..='å' | 'Ā'..='ą' => "a",
        'Æ' | 'æ' => "ae",
        'Ç' | 'ç' | 'Ć'..='č' => "c",
        'È'..='Ë' | 'è'..='ë' | 'Ē'..='ě' => "e",
        'Ì'..='Ï' | 'ì'..='ï' | 'Ĩ'..='ı' => "i",
        'Ð' | 'ð' | 'Ď'..='đ' => "d",
        'Ñ' | 'ñ' | 'Ń'..='ŋ' => "n",
        'Ò'..='Ö' | 'ò'..='ö' | 'Ø' | 'ø' | 'Ō'..='ő' => "o",
        'Ù'..='Ü' | 'ù'..='ü' | 'Ũ'..='ų' => "u",
        'Ý' | 'ý' | 'ÿ' | 'Ŷ'..='Ÿ' => "y",
        'Þ' | 'þ' => "th",
        'ß' => "ss",
        'Ĝ'..='ģ' => "g",
        'Ĥ'..='ħ' => "h",
        'Ĳ' | 'ĳ' => "ij",
        'Ĵ' | 'ĵ' => "j",
        'Ķ'..='ĸ' => "k",
        'Ĺ'..='ł' => "l",
        'Œ' | 'œ' => "oe",
        'Ŕ'..='ř' => "r",
        'Ś'..='š' | 'ſ' => "s",
        'Ţ'..='ŧ' => "t",
        'Ŵ' | 'ŵ' => "w",
        'Ź'..='ž' => "z",
        _ => return None,
    })
}

#[inline]
fn is_combining_mark(c: char) -> bool {
    ('\u{0300}'..='\u{036F}').contains(&c)
}

/// Appends the normalised form of one character: lower case unless
/// `case_sensitive`, without diacritics when `fold`.
#[inline]
fn push_normalized(c: char, case_sensitive: bool, fold: bool, out: &mut String) {
    if c.is_ascii() {
        out.push(if case_sensitive {
            c
        } else {
            c.to_ascii_lowercase()
        });
        return;
    }
    if fold {
        if is_combining_mark(c) {
            return;
        }
        if let Some(base) = base_of(c) {
            if case_sensitive && c.is_uppercase() {
                out.extend(base.chars().map(|b| b.to_ascii_uppercase()));
            } else {
                out.push_str(base);
            }
            return;
        }
    }
    if case_sensitive {
        out.push(c);
    } else {
        for l in c.to_lowercase() {
            if fold && is_combining_mark(l) {
                continue;
            }
            match base_of(l).filter(|_| fold) {
                Some(base) => out.push_str(base),
                None => out.push(l),
            }
        }
    }
}

/// Replaces `out` with the normalised form of `s`.
pub(crate) fn normalize_into(s: &str, case_sensitive: bool, fold: bool, out: &mut String) {
    out.clear();
    for c in s.chars() {
        push_normalized(c, case_sensitive, fold, out);
    }
}

pub(crate) fn normalize(s: &str, case_sensitive: bool, fold: bool) -> String {
    let mut out = String::with_capacity(s.len());
    normalize_into(s, case_sensitive, fold, &mut out);
    out
}

/// What a text term looks for.
#[derive(Clone, Debug)]
pub(crate) enum Pattern {
    /// Anywhere in the text.
    Substr(String),
    /// The whole text must match; `*` and `?` are wildcards.
    Wild(String),
    Exact(String),
    Starts(String),
    Ends(String),
    Regex(regex::Regex),
}

/// One text test against a name or a full path.
#[derive(Clone, Debug)]
pub(crate) struct TextMatch {
    pub pattern: Pattern,
    /// Test the full path instead of the name.
    pub path: bool,
    pub case_sensitive: bool,
    /// Ignore diacritics.
    pub fold: bool,
    pub whole_word: bool,
}

#[inline]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b >= 0x80
}

/// Finds `needle` in `hay` from `start`. With `ci`, ASCII letters of `hay`
/// are compared case-insensitively (`needle` must already be lower case).
fn find(hay: &[u8], needle: &[u8], ci: bool, start: usize) -> Option<usize> {
    if needle.is_empty() {
        return (start <= hay.len()).then_some(start);
    }
    if start >= hay.len() || hay.len() - start < needle.len() {
        return None;
    }
    if !ci {
        return memchr::memmem::find(&hay[start..], needle).map(|p| p + start);
    }
    let lo = needle[0];
    let up = lo.to_ascii_uppercase();
    let last_start = hay.len() - needle.len();
    if hay.len() <= 64 {
        // File names are short: a plain loop beats repeated memchr calls.
        let rest = &needle[1..];
        for pos in start..=last_start {
            let h = hay[pos];
            if (h == lo || h == up) && hay[pos + 1..pos + needle.len()].eq_ignore_ascii_case(rest) {
                return Some(pos);
            }
        }
        return None;
    }
    let mut i = start;
    while i <= last_start {
        let window = &hay[i..=last_start];
        let p = if lo != up {
            memchr::memchr2(lo, up, window)
        } else {
            memchr::memchr(lo, window)
        };
        let pos = i + p?;
        if hay[pos..pos + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(pos);
        }
        i = pos + 1;
    }
    None
}

#[inline]
fn bytes_eq(a: &[u8], b: &[u8], ci: bool) -> bool {
    if ci {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

#[inline]
fn char_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xFF => 4,
        // A continuation byte: step one so progress is always made.
        _ => 1,
    }
}

/// Wildcard match over UTF-8 bytes: `*` = any run, `?` = one character.
fn wild(text: &[u8], pat: &[u8], ci: bool) -> bool {
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);
    while ti < text.len() {
        if pi < pat.len() && pat[pi] == b'?' {
            ti = (ti + char_len(text[ti])).min(text.len());
            pi += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if pi < pat.len()
            && (text[ti] == pat[pi] || (ci && text[ti].eq_ignore_ascii_case(&pat[pi])))
        {
            ti += 1;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark = (mark + char_len(text[mark])).min(text.len());
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

impl TextMatch {
    /// Does `hay` match? `scratch` is a reusable buffer.
    pub fn matches(&self, hay: &str, scratch: &mut String) -> bool {
        if let Pattern::Regex(re) = &self.pattern {
            return re.is_match(hay);
        }
        // ASCII text needs no normalising: compare in place.
        let (text, ci): (&[u8], bool) = if hay.is_ascii() {
            (hay.as_bytes(), !self.case_sensitive)
        } else {
            normalize_into(hay, self.case_sensitive, self.fold, scratch);
            (scratch.as_bytes(), false)
        };
        match &self.pattern {
            Pattern::Substr(n) => {
                let n = n.as_bytes();
                if !self.whole_word {
                    return find(text, n, ci, 0).is_some();
                }
                let mut from = 0;
                while let Some(pos) = find(text, n, ci, from) {
                    let end = pos + n.len();
                    let before = pos == 0 || !is_word_byte(text[pos - 1]);
                    let after = end >= text.len() || !is_word_byte(text[end]);
                    if before && after {
                        return true;
                    }
                    from = pos + 1;
                }
                false
            }
            Pattern::Wild(p) => wild(text, p.as_bytes(), ci),
            Pattern::Exact(n) => bytes_eq(text, n.as_bytes(), ci),
            Pattern::Starts(n) => {
                text.len() >= n.len() && bytes_eq(&text[..n.len()], n.as_bytes(), ci)
            }
            Pattern::Ends(n) => {
                text.len() >= n.len() && bytes_eq(&text[text.len() - n.len()..], n.as_bytes(), ci)
            }
            Pattern::Regex(_) => unreachable!(),
        }
    }

    /// Appends the byte ranges of `name` that this term matched.
    pub fn highlight(&self, name: &str, out: &mut Vec<Range<usize>>) {
        if self.path {
            return;
        }
        if let Pattern::Regex(re) = &self.pattern {
            if let Some(m) = re.find(name) {
                if !m.is_empty() {
                    out.push(m.range());
                }
            }
            return;
        }
        // Normalised text plus, for every normalised byte, the original
        // character it came from (start, end).
        let mut norm = String::with_capacity(name.len());
        let mut map: Vec<(u32, u32)> = Vec::with_capacity(name.len());
        for (i, c) in name.char_indices() {
            let before = norm.len();
            push_normalized(c, self.case_sensitive, self.fold, &mut norm);
            for _ in before..norm.len() {
                map.push((i as u32, (i + c.len_utf8()) as u32));
            }
        }
        let text = norm.as_bytes();
        let mut add = |start: usize, end: usize| {
            if end > start && end <= map.len() {
                out.push(map[start].0 as usize..map[end - 1].1 as usize);
            }
        };
        match &self.pattern {
            Pattern::Substr(n) | Pattern::Exact(n) | Pattern::Starts(n) | Pattern::Ends(n) => {
                let from = match &self.pattern {
                    Pattern::Ends(_) => text.len().saturating_sub(n.len()),
                    _ => 0,
                };
                let mut from = from;
                while let Some(pos) = find(text, n.as_bytes(), false, from) {
                    let end = pos + n.len();
                    let ok = !self.whole_word
                        || ((pos == 0 || !is_word_byte(text[pos - 1]))
                            && (end >= text.len() || !is_word_byte(text[end])));
                    if ok {
                        add(pos, end);
                        break;
                    }
                    from = pos + 1;
                }
            }
            Pattern::Wild(p) => {
                // Highlight the literal pieces, left to right.
                let mut from = 0;
                for piece in p.split(['*', '?']).filter(|s| !s.is_empty()) {
                    match find(text, piece.as_bytes(), false, from) {
                        Some(pos) => {
                            add(pos, pos + piece.len());
                            from = pos + piece.len();
                        }
                        None => break,
                    }
                }
            }
            Pattern::Regex(_) => {}
        }
    }
}

/// Sorts ranges and merges the ones that touch or overlap.
pub(crate) fn merge_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| (r.start, r.end));
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Case-insensitive "natural" order: runs of digits compare as numbers, so
/// `file2` sorts before `file10`.
pub(crate) fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // Identical beginnings compare equal: skip them, then step back to the
    // start of the number (or character) the first difference falls into.
    let mut n = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    while n > 0 && (a[n - 1].is_ascii_digit() || (n < a.len() && a[n] & 0xC0 == 0x80)) {
        n -= 1;
    }
    let (mut i, mut j) = (n, n);
    loop {
        match (i < a.len(), j < b.len()) {
            (false, false) => return Ordering::Equal,
            (false, true) => return Ordering::Less,
            (true, false) => return Ordering::Greater,
            _ => {}
        }
        let (x, y) = (a[i], b[j]);
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < a.len() && a[i] == b'0' {
                i += 1;
            }
            while j < b.len() && b[j] == b'0' {
                j += 1;
            }
            let (di, dj) = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let ord = (i - di)
                .cmp(&(j - dj))
                .then_with(|| a[di..i].cmp(&b[dj..j]))
                // Same number: fewer leading zeros first, to keep the order total.
                .then_with(|| (di - si).cmp(&(dj - sj)));
            if ord != Ordering::Equal {
                return ord;
            }
            continue;
        }
        if x < 0x80 && y < 0x80 {
            let ord = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
            if ord != Ordering::Equal {
                return ord;
            }
            i += 1;
            j += 1;
            continue;
        }
        // Non-ASCII somewhere: compare the rest character by character.
        // SAFETY: i and j only ever advance over whole ASCII bytes, so they
        // are on character boundaries of the original strings.
        let (ra, rb) = unsafe {
            (
                std::str::from_utf8_unchecked(&a[i..]),
                std::str::from_utf8_unchecked(&b[j..]),
            )
        };
        let (ca, cb) = (ra.chars().next().unwrap(), rb.chars().next().unwrap());
        let ord = ca.to_lowercase().cmp(cb.to_lowercase());
        if ord != Ordering::Equal {
            return ord;
        }
        i += ca.len_utf8();
        j += cb.len_utf8();
    }
}

/// The first `N` bytes of an encoding of `name` whose byte order is exactly
/// the order of [`natural_cmp`]: letters in lower case, a run of digits as
/// (marker, number of significant digits, the digits, number of leading
/// zeros). Keys that differ decide the order; equal keys need the full
/// comparison. The key stops at the first non-ASCII character.
pub(crate) fn name_key<const N: usize>(name: &str) -> [u8; N] {
    let mut key = [0u8; N];
    let b = name.as_bytes();
    let (mut i, mut k) = (0usize, 0usize);
    let mut put = |k: &mut usize, v: u8| {
        if *k < N {
            key[*k] = v;
        }
        *k += 1;
    };
    while i < b.len() && k < N {
        let c = b[i];
        if c >= 0x80 {
            put(&mut k, 0xFF);
            break;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i] == b'0' {
                i += 1;
            }
            let digits = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            put(&mut k, b'0');
            put(&mut k, (i - digits).min(255) as u8);
            for d in &b[digits..i] {
                put(&mut k, *d);
            }
            put(&mut k, (digits - start).min(255) as u8);
            continue;
        }
        put(&mut k, c.to_ascii_lowercase());
        i += 1;
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn tm(pattern: Pattern) -> TextMatch {
        TextMatch {
            pattern,
            path: false,
            case_sensitive: false,
            fold: true,
            whole_word: false,
        }
    }

    fn hit(t: &TextMatch, hay: &str) -> bool {
        t.matches(hay, &mut String::new())
    }

    #[test]
    fn normalizing() {
        assert_eq!(normalize("Résumé ÀÉÎÕÜ", false, true), "resume aeiou");
        assert_eq!(normalize("Résumé", true, true), "Resume");
        assert_eq!(normalize("Résumé", false, false), "résumé");
        assert_eq!(
            normalize("Straße Œuvre Æon", false, true),
            "strasse oeuvre aeon"
        );
        assert_eq!(normalize("Łódź Ñandú", false, true), "lodz nandu");
        // Decomposed accents (letter + combining mark).
        assert_eq!(normalize("Cafe\u{301}", false, true), "cafe");
        assert_eq!(normalize("ÇA", true, true), "CA");
        assert_eq!(normalize("名前.TXT", false, true), "名前.txt");
    }

    #[test]
    fn substrings() {
        let t = tm(Pattern::Substr("report".into()));
        assert!(hit(&t, "Annual REPORT 2026.pdf"));
        assert!(hit(&t, "réport.txt"), "diacritics are folded");
        assert!(!hit(&t, "repor.txt"));
        assert!(!hit(&t, ""));
        let cs = TextMatch {
            case_sensitive: true,
            ..tm(Pattern::Substr("Report".into()))
        };
        assert!(hit(&cs, "My Report.txt"));
        assert!(!hit(&cs, "my report.txt"));
        let strict = TextMatch {
            fold: false,
            ..tm(Pattern::Substr("resume".into()))
        };
        assert!(!hit(&strict, "résumé.pdf"));
        assert!(hit(&strict, "RESUME.pdf"));
        // A needle that survives normalising as non-ASCII.
        let cjk = tm(Pattern::Substr("名前".into()));
        assert!(hit(&cjk, "私の名前.txt"));
        assert!(!hit(&cjk, "name.txt"));
        // Needle longer than the text, repeated first letters.
        assert!(!hit(&tm(Pattern::Substr("aaaa".into())), "aaa"));
        assert!(hit(&tm(Pattern::Substr("aab".into())), "AAAAB"));
    }

    #[test]
    fn whole_words() {
        let t = TextMatch {
            whole_word: true,
            ..tm(Pattern::Substr("port".into()))
        };
        assert!(hit(&t, "port.txt"));
        assert!(hit(&t, "the_port-map"));
        assert!(hit(&t, "report port"), "a later occurrence may qualify");
        assert!(!hit(&t, "report.txt"));
        assert!(!hit(&t, "ports"));
        assert!(
            !hit(&t, "portée"),
            "letters with accents are word characters"
        );
    }

    #[test]
    fn wildcards() {
        let t = tm(Pattern::Wild("*.jp?g".into()));
        assert!(hit(&t, "photo.JPEG"));
        assert!(!hit(&t, "photo.jpg"));
        assert!(!hit(&t, "photo.jpeg.bak"));
        let t = tm(Pattern::Wild("a?c*".into()));
        assert!(hit(&t, "ABC"));
        assert!(hit(&t, "aéc-rest"), "? is one character, not one byte");
        assert!(!hit(&t, "ac"));
        let t = tm(Pattern::Wild("*".into()));
        assert!(hit(&t, "") && hit(&t, "anything"));
        let t = tm(Pattern::Wild("?".into()));
        assert!(hit(&t, "名") && !hit(&t, "名前") && !hit(&t, ""));
        assert!(hit(&tm(Pattern::Wild("a*b*c".into())), "a--b--b--c"));
        assert!(!hit(&tm(Pattern::Wild("a*b*c".into())), "a--b--b--"));
    }

    #[test]
    fn exact_starts_ends_regex() {
        assert!(hit(&tm(Pattern::Exact("readme.md".into())), "README.md"));
        assert!(!hit(
            &tm(Pattern::Exact("readme.md".into())),
            "readme.md.bak"
        ));
        assert!(hit(&tm(Pattern::Starts("img_".into())), "IMG_0042.cr3"));
        assert!(!hit(&tm(Pattern::Starts("img_".into())), "my img_1"));
        assert!(hit(&tm(Pattern::Ends(".tar.gz".into())), "backup.TAR.GZ"));
        assert!(!hit(&tm(Pattern::Ends(".tar.gz".into())), "gz"));
        let re = regex::RegexBuilder::new(r"^img_\d{4}\.")
            .case_insensitive(true)
            .build()
            .unwrap();
        assert!(hit(&tm(Pattern::Regex(re.clone())), "IMG_0042.cr3"));
        assert!(!hit(&tm(Pattern::Regex(re)), "IMG_42.cr3"));
    }

    fn marks(t: &TextMatch, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        t.highlight(name, &mut out);
        merge_ranges(out)
            .into_iter()
            .map(|r| name[r].to_string())
            .collect()
    }

    #[test]
    fn highlights_refer_to_the_original_text() {
        let t = tm(Pattern::Substr("resume".into()));
        assert_eq!(marks(&t, "My Résumé 2026.pdf"), vec!["Résumé"]);
        assert_eq!(marks(&t, "nothing here"), Vec::<String>::new());
        let t = tm(Pattern::Substr("ss".into()));
        assert_eq!(marks(&t, "Straße.txt"), vec!["ß"]);
        let t = tm(Pattern::Wild("*rep*20??.pdf".into()));
        assert_eq!(
            marks(&t, "Annual report 2026.pdf"),
            vec!["rep", "20", ".pdf"]
        );
        let t = tm(Pattern::Ends(".txt".into()));
        assert_eq!(marks(&t, "a.txt.TXT"), vec![".TXT"]);
        let t = TextMatch {
            whole_word: true,
            ..tm(Pattern::Substr("port".into()))
        };
        assert_eq!(marks(&t, "report port.txt"), vec!["port"]);
        let mut out = Vec::new();
        t.highlight("report port.txt", &mut out);
        assert_eq!(out, vec![7..11]);
        let path = TextMatch {
            path: true,
            ..tm(Pattern::Substr("a".into()))
        };
        assert!(marks(&path, "a").is_empty());
        assert_eq!(merge_ranges(vec![5..7, 0..2, 1..3, 3..4]), vec![0..4, 5..7]);
    }

    #[test]
    fn natural_order() {
        let mut names = vec![
            "file10.txt",
            "File2.txt",
            "file1.txt",
            "apple",
            "Banana",
            "file02.txt",
            "éclair",
            "zebra",
            "file",
            "_under",
            "10",
            "9",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec![
                "9",
                "10",
                "_under",
                "apple",
                "Banana",
                "file",
                "file1.txt",
                "File2.txt",
                "file02.txt",
                "file10.txt",
                "zebra",
                "éclair",
            ]
        );
        assert_eq!(natural_cmp("abc", "ABC"), Ordering::Equal);
        assert_eq!(natural_cmp("a", "a1"), Ordering::Less);
        assert_eq!(natural_cmp("x007", "x7"), Ordering::Greater);
        assert_eq!(natural_cmp("É", "é"), Ordering::Equal);
    }

    #[test]
    fn sort_keys_agree_with_the_full_order() {
        let names = [
            "",
            "a",
            "A",
            "ab",
            "ab-x",
            "ab1",
            "ab10",
            "ab2",
            "ab:",
            "abc",
            "Abd",
            "ab_",
            "b",
            "file 1",
            "file-1",
            "file1",
            "file10",
            "file2",
            "filea",
            "zz",
            "é",
            "ée",
            "名",
            "0",
            "00",
            "1",
            "~",
            "a b",
            "a.b",
            "a_b",
            "aB",
            "[x]",
            "`q",
            "{y}",
            "x007",
            "x7",
            "x07a",
            "x7a",
            "x70",
            "a000",
            "a0",
            "a",
            "report-invoice-01234.txt",
            "report-invoice-1234.txt",
            "report-invoice-01235.txt",
            "report-invoice-999.txt",
            "IMG_0042.jpg",
            "img_42.JPG",
            "img_043.jpg",
            "a1b2c3",
            "a1b2c10",
            "a1b10c3",
            "9",
            "10",
            "010",
            "a9é",
            "a10é",
            "a9z",
        ];
        for a in names {
            for b in names {
                for n in [4usize, 8, 16, 24] {
                    let (ka, kb): (Vec<u8>, Vec<u8>) = match n {
                        4 => (name_key::<4>(a).to_vec(), name_key::<4>(b).to_vec()),
                        8 => (name_key::<8>(a).to_vec(), name_key::<8>(b).to_vec()),
                        16 => (name_key::<16>(a).to_vec(), name_key::<16>(b).to_vec()),
                        _ => (name_key::<24>(a).to_vec(), name_key::<24>(b).to_vec()),
                    };
                    if ka != kb {
                        assert_eq!(ka.cmp(&kb), natural_cmp(a, b), "{a:?} vs {b:?} at {n}");
                    }
                }
            }
        }
        // Long keys settle typical names without the full comparison.
        assert_ne!(
            name_key::<24>("report-invoice-01234.txt"),
            name_key::<24>("report-invoice-01235.txt")
        );
        // Long needles and haystacks take the other search path.
        let long = format!("{}Needle{}", "x".repeat(80), "y".repeat(80));
        assert_eq!(find(long.as_bytes(), b"needle", true, 0), Some(80));
        assert_eq!(find(long.as_bytes(), b"needles", true, 0), None);
        assert_eq!(find(b"aAbB", b"ab", true, 0), Some(1));
        assert_eq!(find(b"aAbB", b"ab", true, 2), None);
        assert_eq!(find(b"abc", b"abc", true, 0), Some(0));
        assert_eq!(find(b"ab", b"abc", true, 0), None);
    }
}
