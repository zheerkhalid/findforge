//! The query language: text to a tree of tests.

use super::text::{normalize, Pattern, TextMatch};
use super::{Now, SearchOptions};
use crate::util::days_from_civil;

/// One test on an entry.
#[derive(Clone, Debug)]
pub(crate) enum Pred {
    Text(TextMatch),
    /// Index into `Parsed::ext_lists`.
    Ext(usize),
    /// Inclusive size range in bytes; files only.
    Size(u64, u64),
    /// Inclusive Unix-time range; `created` picks the date created.
    Date {
        created: bool,
        min: u32,
        max: u32,
    },
    /// Folder (true) or file (false).
    Kind(bool),
    /// Index into `Parsed::parents`: the exact containing folder.
    Parent(usize),
    /// Number of parent folders.
    Depth(u32, u32),
    /// Name length in characters.
    Len(u32, u32),
    /// The name occurs more than once in the index.
    Dupe,
    /// A folder with nothing in it.
    Empty,
    /// Flag bits that must all be set.
    Attrib(u16),
    /// The volume root starts with this (lower case, e.g. `d:`).
    Root(String),
    /// A file small enough to be read for a `content:` term; the text itself
    /// is looked for afterwards (see `content.rs`).
    Content,
}

/// Text to look for inside files (`content:invoice`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContentTerm {
    /// Lower case unless `case_sensitive`.
    pub needle: String,
    pub case_sensitive: bool,
}

/// Larger files are never read for `content:`.
pub(crate) const CONTENT_MAX_BYTES: u64 = 32 << 20;

const CONTENT_NESTED: &str =
    "content: cannot be used with | or !. Narrow the search with other words instead.";

#[derive(Clone, Debug)]
pub(crate) enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Pred),
}

impl Node {
    /// Rough cost, so cheap tests run first.
    fn cost(&self) -> u32 {
        match self {
            Node::And(v) | Node::Or(v) => v.iter().map(|n| n.cost()).max().unwrap_or(0),
            Node::Not(n) => n.cost(),
            Node::Leaf(p) => match p {
                Pred::Kind(_)
                | Pred::Ext(_)
                | Pred::Size(..)
                | Pred::Date { .. }
                | Pred::Attrib(_)
                | Pred::Root(_)
                | Pred::Parent(_) => 1,
                Pred::Len(..) | Pred::Empty => 2,
                Pred::Dupe => 4,
                Pred::Depth(..) => 6,
                Pred::Content => 30,
                Pred::Text(t) => match (&t.pattern, t.path) {
                    (_, true) => 20,
                    (Pattern::Regex(_), false) => 8,
                    _ => 5,
                },
            },
        }
    }

    fn order(&mut self) {
        match self {
            Node::And(v) => {
                for n in v.iter_mut() {
                    n.order();
                }
                v.sort_by_key(|n| n.cost());
            }
            Node::Or(v) => {
                for n in v.iter_mut() {
                    n.order();
                }
            }
            Node::Not(n) => n.order(),
            Node::Leaf(_) => {}
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Parsed {
    pub root: Node,
    /// Lower-case extension lists referenced by `Pred::Ext`.
    pub ext_lists: Vec<Vec<String>>,
    /// Folder paths referenced by `Pred::Parent`.
    pub parents: Vec<String>,
    pub uses_dupe: bool,
    pub uses_empty: bool,
    /// Positive name terms, for highlighting.
    pub highlight: Vec<TextMatch>,
    /// Texts that must all be inside a file (`content:`).
    pub contents: Vec<ContentTerm>,
}

pub(crate) const AUDIO: &str = "aac;ac3;aif;aifc;aiff;amr;ape;au;cda;dts;fla;flac;m1a;m2a;m3u;m3u8;m4a;m4b;m4p;mid;midi;mka;mp2;mp3;mpa;mpc;oga;ogg;opus;ra;snd;wav;weba;wma;wv";
pub(crate) const VIDEO: &str = "3g2;3gp;3gp2;3gpp;amv;asf;avi;divx;drc;f4v;flv;m1v;m2t;m2ts;m2v;m4v;mkv;mod;mov;mp2v;mp4;mp4v;mpe;mpeg;mpg;mpv2;mts;mxf;ogm;ogv;qt;rm;rmvb;swf;ts;vob;webm;wmv;xvid";
pub(crate) const PICTURES: &str = "ani;apng;arw;avif;bmp;cr2;cr3;cur;dds;dng;emf;gif;heic;heif;ico;jfif;jpe;jpeg;jpg;jxl;nef;orf;pcx;png;psd;raf;raw;rw2;svg;tga;tif;tiff;webp;wmf;xcf";
pub(crate) const DOCUMENTS: &str = "c;chm;cpp;css;csv;cxx;doc;docm;docx;dot;dotm;dotx;epub;h;hpp;htm;html;hxx;ini;java;js;json;log;lua;md;mht;mhtml;mobi;odp;ods;odt;pdf;php;pot;potm;potx;pps;ppsm;ppsx;ppt;pptm;pptx;ps;py;rs;rtf;tex;toml;ts;tsx;txt;vsd;vsdx;wpd;wps;wri;xlam;xls;xlsb;xlsm;xlsx;xlt;xltm;xltx;xml;xps;yaml;yml";
pub(crate) const COMPRESSED: &str = "7z;ace;arj;bz2;cab;cpio;deb;dmg;gz;gzip;img;iso;jar;lha;lz;lzh;lzma;pak;r00;r01;rar;rpm;tar;tbz;tbz2;tgz;txz;tz;vhd;vhdx;wim;xz;z;zip;zst";
pub(crate) const EXECUTABLES: &str =
    "appx;appxbundle;bat;cmd;com;cpl;exe;gadget;msc;msi;msix;msixbundle;msp;ps1;scr;vbs;wsf";

struct Ctx<'a> {
    options: &'a SearchOptions,
    now: Now,
    out: Parsed,
}

#[derive(Debug, PartialEq, Eq)]
enum Tok {
    /// A term; `quoted` when it started with a double quote.
    Term {
        text: String,
        quoted: bool,
    },
    Or,
    Not,
    Open,
    Close,
}

/// Splits query text into tokens. Quotes group words (and are removed);
/// `|`, a leading `!`, and `<` `>` are operators.
fn tokenize(text: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut has_cur = false; // distinguishes "" from nothing
    let mut cur_quoted = false;
    let mut in_quote = false;
    let mut depth = 0usize;
    let flush = |toks: &mut Vec<Tok>, cur: &mut String, has: &mut bool, quoted: &mut bool| {
        if *has {
            toks.push(Tok::Term {
                text: std::mem::take(cur),
                quoted: *quoted,
            });
        }
        *has = false;
        *quoted = false;
    };
    for c in text.chars() {
        if in_quote {
            if c == '"' {
                in_quote = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                if !has_cur {
                    cur_quoted = true;
                }
                has_cur = true;
                in_quote = true;
            }
            c if c.is_whitespace() => flush(&mut toks, &mut cur, &mut has_cur, &mut cur_quoted),
            '|' => {
                flush(&mut toks, &mut cur, &mut has_cur, &mut cur_quoted);
                toks.push(Tok::Or);
            }
            '!' if !has_cur => toks.push(Tok::Not),
            '<' if !has_cur => {
                depth += 1;
                toks.push(Tok::Open);
            }
            // `size:>1mb` and `dm:>=2026` keep their comparison signs.
            '>' if depth > 0 && !cur.ends_with([':', '<', '>', '=']) => {
                flush(&mut toks, &mut cur, &mut has_cur, &mut cur_quoted);
                depth -= 1;
                toks.push(Tok::Close);
            }
            _ => {
                cur.push(c);
                has_cur = true;
            }
        }
    }
    flush(&mut toks, &mut cur, &mut has_cur, &mut cur_quoted);
    toks
}

/// Parses query text (and an optional filter query ANDed with it).
pub(crate) fn parse(
    text: &str,
    filter: Option<&str>,
    options: &SearchOptions,
    now: Now,
) -> Result<Parsed, String> {
    // Filters are always written in the plain language.
    let plain = SearchOptions {
        regex: false,
        match_path: false,
        whole_word: false,
        match_case: false,
        ..*options
    };
    let mut ctx = Ctx {
        options,
        now,
        out: Parsed {
            root: Node::And(Vec::new()),
            ext_lists: Vec::new(),
            parents: Vec::new(),
            uses_dupe: false,
            uses_empty: false,
            highlight: Vec::new(),
            contents: Vec::new(),
        },
    };
    let mut parts = Vec::new();
    if options.regex {
        // The whole text is one regular expression.
        let t = text.trim();
        if !t.is_empty() {
            let tm = TextMatch {
                pattern: Pattern::Regex(build_regex(t, options.match_case)?),
                path: options.match_path,
                case_sensitive: options.match_case,
                fold: false,
                whole_word: false,
            };
            ctx.out.highlight.push(tm.clone());
            parts.push(Node::Leaf(Pred::Text(tm)));
        }
    } else {
        parts.push(parse_expr(&mut ctx, text, true)?);
    }
    if let Some(f) = filter {
        ctx.options = &plain;
        let node = parse_expr(&mut ctx, f, false)?;
        ctx.options = options;
        parts.push(node);
    }
    let mut root = flatten(Node::And(parts));
    if !ctx.out.contents.is_empty() && !content_at_top(&root, true) {
        return Err(CONTENT_NESTED.into());
    }
    root.order();
    ctx.out.root = root;
    Ok(ctx.out)
}

/// True when every `content:` leaf is reached through AND nodes only.
fn content_at_top(node: &Node, top: bool) -> bool {
    match node {
        Node::And(v) => v.iter().all(|n| content_at_top(n, top)),
        Node::Or(v) => v.iter().all(|n| content_at_top(n, false)),
        Node::Not(n) => content_at_top(n, false),
        Node::Leaf(Pred::Content) => top,
        Node::Leaf(_) => true,
    }
}

fn flatten(node: Node) -> Node {
    match node {
        Node::And(v) => {
            let mut out = Vec::new();
            for n in v {
                match flatten(n) {
                    Node::And(inner) => out.extend(inner),
                    other => out.push(other),
                }
            }
            if out.len() == 1 {
                out.pop().unwrap()
            } else {
                Node::And(out)
            }
        }
        Node::Or(v) => {
            let mut out: Vec<Node> = v.into_iter().map(flatten).collect();
            if out.len() == 1 {
                out.pop().unwrap()
            } else {
                Node::Or(out)
            }
        }
        Node::Not(n) => Node::Not(Box::new(flatten(*n))),
        leaf => leaf,
    }
}

fn parse_expr(ctx: &mut Ctx, text: &str, highlight: bool) -> Result<Node, String> {
    let toks = tokenize(text);
    let mut pos = 0;
    let node = parse_and(ctx, &toks, &mut pos, highlight, 0)?;
    if pos < toks.len() {
        return Err("There is a > without a matching <.".into());
    }
    Ok(node)
}

fn parse_and(
    ctx: &mut Ctx,
    toks: &[Tok],
    pos: &mut usize,
    highlight: bool,
    depth: usize,
) -> Result<Node, String> {
    if depth > 64 {
        return Err("The search has too many nested groups.".into());
    }
    let mut parts = Vec::new();
    while *pos < toks.len() && toks[*pos] != Tok::Close {
        parts.push(parse_or(ctx, toks, pos, highlight, depth)?);
    }
    Ok(Node::And(parts))
}

fn parse_or(
    ctx: &mut Ctx,
    toks: &[Tok],
    pos: &mut usize,
    highlight: bool,
    depth: usize,
) -> Result<Node, String> {
    let mut alts = Vec::new();
    // A stray leading `|` is ignored.
    while *pos < toks.len() && toks[*pos] == Tok::Or {
        *pos += 1;
    }
    if *pos >= toks.len() || toks[*pos] == Tok::Close {
        return Ok(Node::And(Vec::new()));
    }
    alts.push(parse_unary(ctx, toks, pos, highlight, depth)?);
    while *pos < toks.len() && toks[*pos] == Tok::Or {
        while *pos < toks.len() && toks[*pos] == Tok::Or {
            *pos += 1;
        }
        if *pos >= toks.len() || toks[*pos] == Tok::Close {
            break; // trailing `|`
        }
        alts.push(parse_unary(ctx, toks, pos, highlight, depth)?);
    }
    Ok(if alts.len() == 1 {
        alts.pop().unwrap()
    } else {
        Node::Or(alts)
    })
}

fn parse_unary(
    ctx: &mut Ctx,
    toks: &[Tok],
    pos: &mut usize,
    highlight: bool,
    depth: usize,
) -> Result<Node, String> {
    match &toks[*pos] {
        Tok::Not => {
            *pos += 1;
            if *pos >= toks.len() || matches!(toks[*pos], Tok::Close | Tok::Or) {
                // A lone `!` excludes nothing.
                return Ok(Node::And(Vec::new()));
            }
            let inner = parse_unary(ctx, toks, pos, false, depth + 1)?;
            Ok(Node::Not(Box::new(inner)))
        }
        Tok::Open => {
            *pos += 1;
            let inner = parse_and(ctx, toks, pos, highlight, depth + 1)?;
            if *pos >= toks.len() {
                return Err("There is a < without a matching >.".into());
            }
            *pos += 1; // the Close
            Ok(inner)
        }
        Tok::Term { text, quoted } => {
            *pos += 1;
            parse_term(ctx, text, *quoted, highlight)
        }
        Tok::Or | Tok::Close => unreachable!("handled by the callers"),
    }
}

fn build_regex(pattern: &str, case_sensitive: bool) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .size_limit(4 << 20)
        .build()
        .map_err(|e| {
            let detail = e.to_string();
            let last = detail
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("");
            format!(
                "That regular expression is not valid ({}).",
                last.trim().trim_start_matches("error: ")
            )
        })
}

#[derive(Default)]
struct Mods {
    case: Option<bool>,
    regex: bool,
    path: Option<bool>,
    whole_word: Option<bool>,
    diacritics: Option<bool>,
    only: Option<bool>, // Some(true) = folders only
    anchor: Option<u8>, // 1 starts, 2 ends, 3 exact
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn parse_term(ctx: &mut Ctx, token: &str, quoted: bool, highlight: bool) -> Result<Node, String> {
    let mut mods = Mods::default();
    let mut rest = token;
    if !quoted {
        // Modifiers may be stacked: `case:ww:Report`.
        'mods: loop {
            type Modifier = (&'static str, fn(&mut Mods));
            let table: [Modifier; 13] = [
                ("case:", |m| m.case = Some(true)),
                ("nocase:", |m| m.case = Some(false)),
                ("regex:", |m| m.regex = true),
                ("path:", |m| m.path = Some(true)),
                ("name:", |m| m.path = Some(false)),
                ("ww:", |m| m.whole_word = Some(true)),
                ("wholeword:", |m| m.whole_word = Some(true)),
                ("diacritics:", |m| m.diacritics = Some(true)),
                ("file:", |m| m.only = Some(false)),
                ("folder:", |m| m.only = Some(true)),
                ("startwith:", |m| m.anchor = Some(1)),
                ("endwith:", |m| m.anchor = Some(2)),
                ("exact:", |m| m.anchor = Some(3)),
            ];
            for (prefix, set) in table {
                if let Some(r) = strip_prefix_ci(rest, prefix) {
                    set(&mut mods);
                    rest = r;
                    if mods.regex {
                        break 'mods; // the rest is the expression, verbatim
                    }
                    continue 'mods;
                }
            }
            break;
        }
        if !mods.regex {
            if let Some(arg) = strip_prefix_ci(rest, "content:") {
                if arg.is_empty() {
                    return Err(
                        "content: expects text to look for inside files, for example content:invoice."
                            .into(),
                    );
                }
                let case_sensitive = mods.case.unwrap_or(ctx.options.match_case);
                ctx.out.contents.push(ContentTerm {
                    needle: if case_sensitive {
                        arg.to_string()
                    } else {
                        arg.to_lowercase()
                    },
                    case_sensitive,
                });
                return Ok(Node::Leaf(Pred::Content));
            }
            if let Some(node) = parse_function(ctx, rest)? {
                return Ok(with_kind(mods.only, node));
            }
        }
    }
    if rest.is_empty() {
        // `file:` / `folder:` alone, or an empty phrase.
        return Ok(match mods.only {
            Some(dir) => Node::Leaf(Pred::Kind(dir)),
            None => Node::And(Vec::new()),
        });
    }
    let o = ctx.options;
    let case_sensitive = mods.case.unwrap_or(o.match_case);
    let fold = !(mods.diacritics.unwrap_or(o.match_diacritics));
    let tm = if mods.regex {
        TextMatch {
            pattern: Pattern::Regex(build_regex(rest, case_sensitive)?),
            path: mods.path.unwrap_or(o.match_path),
            case_sensitive,
            fold: false,
            whole_word: false,
        }
    } else {
        let has_sep = rest.contains(['\\', '/']);
        let path = mods.path.unwrap_or(o.match_path || has_sep);
        let text = if path {
            rest.replace('/', "\\")
        } else {
            rest.to_string()
        };
        let norm = normalize(&text, case_sensitive, fold);
        let pattern = match mods.anchor {
            Some(1) => Pattern::Starts(norm),
            Some(2) => Pattern::Ends(norm),
            Some(3) => Pattern::Exact(norm),
            _ if !quoted && norm.contains(['*', '?']) => Pattern::Wild(norm),
            _ => Pattern::Substr(norm),
        };
        TextMatch {
            pattern,
            path,
            case_sensitive,
            fold,
            whole_word: mods.whole_word.unwrap_or(o.whole_word),
        }
    };
    if highlight && !tm.path {
        ctx.out.highlight.push(tm.clone());
    }
    Ok(with_kind(mods.only, Node::Leaf(Pred::Text(tm))))
}

fn with_kind(only: Option<bool>, node: Node) -> Node {
    match only {
        Some(dir) => Node::And(vec![Node::Leaf(Pred::Kind(dir)), node]),
        None => node,
    }
}

fn ext_list(ctx: &mut Ctx, list: &str) -> Node {
    let exts: Vec<String> = list
        .split([';', ','])
        .map(|e| {
            e.trim()
                .trim_start_matches("*.")
                .trim_start_matches('.')
                .to_lowercase()
        })
        .filter(|e| !e.is_empty())
        .collect();
    ctx.out.ext_lists.push(exts);
    Node::And(vec![
        Node::Leaf(Pred::Kind(false)),
        Node::Leaf(Pred::Ext(ctx.out.ext_lists.len() - 1)),
    ])
}

/// Parses `name:argument` functions. `Ok(None)` when `token` is not one.
fn parse_function(ctx: &mut Ctx, token: &str) -> Result<Option<Node>, String> {
    let Some(colon) = token.find(':') else {
        return Ok(None);
    };
    let (name, arg) = (token[..colon].to_ascii_lowercase(), &token[colon + 1..]);
    let leaf = |p: Pred| Ok(Some(Node::Leaf(p)));
    match name.as_str() {
        "ext" => {
            if arg.is_empty() {
                // `ext:` alone = files without an extension.
                ctx.out.ext_lists.push(vec![String::new()]);
                return Ok(Some(Node::And(vec![
                    Node::Leaf(Pred::Kind(false)),
                    Node::Leaf(Pred::Ext(ctx.out.ext_lists.len() - 1)),
                ])));
            }
            Ok(Some(ext_list(ctx, arg)))
        }
        "size" => {
            let (min, max) = parse_size(arg)?;
            Ok(Some(Node::And(vec![
                Node::Leaf(Pred::Kind(false)),
                Node::Leaf(Pred::Size(min, max)),
            ])))
        }
        "dm" | "datemodified" | "dc" | "datecreated" => {
            let (min, max) = parse_date(arg, ctx.now, &name)?;
            leaf(Pred::Date {
                created: name == "dc" || name == "datecreated",
                min,
                max,
            })
        }
        "type" => match arg.to_ascii_lowercase().as_str() {
            "folder" | "folders" | "dir" | "directory" => leaf(Pred::Kind(true)),
            "file" | "files" => leaf(Pred::Kind(false)),
            "audio" | "music" => Ok(Some(ext_list(ctx, AUDIO))),
            "video" | "videos" => Ok(Some(ext_list(ctx, VIDEO))),
            "picture" | "pictures" | "image" | "images" | "photo" | "photos" => {
                Ok(Some(ext_list(ctx, PICTURES)))
            }
            "document" | "documents" | "doc" | "docs" => Ok(Some(ext_list(ctx, DOCUMENTS))),
            "compressed" | "archive" | "archives" => Ok(Some(ext_list(ctx, COMPRESSED))),
            "executable" | "executables" | "program" | "programs" => {
                Ok(Some(ext_list(ctx, EXECUTABLES)))
            }
            _ => Err(
                "type: expects folder, file, audio, video, picture, document, compressed or executable."
                    .into(),
            ),
        },
        "parent" | "infolder" => {
            let p = arg.replace('/', "\\");
            let p = p.trim_end_matches('\\');
            if p.is_empty() {
                return Err("parent: expects a folder path, for example parent:C:\\Projects.".into());
            }
            ctx.out.parents.push(p.to_string());
            leaf(Pred::Parent(ctx.out.parents.len() - 1))
        }
        "depth" | "parents" => {
            let (min, max) = parse_number_range(arg).ok_or_else(|| {
                format!("{name}: expects a number, for example {name}:3 or {name}:>5.")
            })?;
            leaf(Pred::Depth(min, max))
        }
        "len" => {
            let (min, max) = parse_number_range(arg)
                .ok_or("len: expects a number of characters, for example len:>100 or len:5..10.")?;
            leaf(Pred::Len(min, max))
        }
        "dupe" if arg.is_empty() => {
            ctx.out.uses_dupe = true;
            leaf(Pred::Dupe)
        }
        "empty" if arg.is_empty() => {
            ctx.out.uses_empty = true;
            leaf(Pred::Empty)
        }
        "attrib" | "attributes" => {
            use crate::index::flags;
            let mut mask = 0u16;
            for c in arg.chars() {
                mask |= match c.to_ascii_lowercase() {
                    'h' => flags::HIDDEN,
                    's' => flags::SYSTEM,
                    'r' => flags::READONLY,
                    'd' => flags::DIR,
                    'l' => flags::REPARSE,
                    _ => {
                        return Err(
                            "attrib: expects letters from h (hidden), s (system), r (read-only), d (folder), l (link)."
                                .into(),
                        )
                    }
                };
            }
            if mask == 0 {
                return Err("attrib: expects at least one letter, for example attrib:h.".into());
            }
            leaf(Pred::Attrib(mask))
        }
        "root" | "drive" | "volume" => {
            let mut r = arg.trim_end_matches('\\').to_lowercase();
            if r.is_empty() {
                return Err("drive: expects a drive letter, for example drive:d.".into());
            }
            if r.len() == 1 && r.as_bytes()[0].is_ascii_alphabetic() {
                r.push(':');
            }
            leaf(Pred::Root(r))
        }
        _ => Ok(None),
    }
}

/// Splits a comparison argument: `>=10` -> (">=", "10").
fn split_cmp(arg: &str) -> (&str, &str) {
    for op in [">=", "<=", ">", "<", "="] {
        if let Some(rest) = arg.strip_prefix(op) {
            return (op, rest);
        }
    }
    ("", arg)
}

fn parse_number_range(arg: &str) -> Option<(u32, u32)> {
    if let Some((a, b)) = arg.split_once("..") {
        let (a, b) = (a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?);
        return Some((a.min(b), a.max(b)));
    }
    let (op, v) = split_cmp(arg);
    let v: u32 = v.trim().parse().ok()?;
    Some(match op {
        ">" => (v.checked_add(1)?, u32::MAX),
        ">=" => (v, u32::MAX),
        "<" => (0, v.checked_sub(1)?),
        "<=" => (0, v),
        _ => (v, v),
    })
}

const SIZE_HELP: &str =
    "size: expects something like >10mb, <500kb, 1mb..5mb, or a word: empty, tiny, small, medium, large, huge, gigantic.";

/// One size value: `(bytes, bytes covered by the last written digit)`.
fn parse_size_value(s: &str) -> Option<(u64, u64)> {
    let s = s.trim().to_ascii_lowercase();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    if num.is_empty() || num.matches('.').count() > 1 {
        return None;
    }
    let mult: u64 = match unit.trim() {
        "" | "b" | "byte" | "bytes" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        "t" | "tb" => 1 << 40,
        _ => return None,
    };
    let value: f64 = num.parse().ok()?;
    let decimals = num.split_once('.').map(|(_, d)| d.len()).unwrap_or(0) as i32;
    let bytes = value * mult as f64;
    if !bytes.is_finite() || bytes > 9.0e18 {
        return None;
    }
    let step = (mult as f64 / 10f64.powi(decimals)).max(1.0);
    Some((bytes.round() as u64, step as u64))
}

fn parse_size(arg: &str) -> Result<(u64, u64), String> {
    const KB: u64 = 1 << 10;
    const MB: u64 = 1 << 20;
    let bad = || SIZE_HELP.to_string();
    match arg.to_ascii_lowercase().as_str() {
        "empty" => return Ok((0, 0)),
        "tiny" => return Ok((1, 10 * KB)),
        "small" => return Ok((10 * KB + 1, 100 * KB)),
        "medium" => return Ok((100 * KB + 1, MB)),
        "large" => return Ok((MB + 1, 16 * MB)),
        "huge" => return Ok((16 * MB + 1, 128 * MB)),
        "gigantic" => return Ok((128 * MB + 1, u64::MAX)),
        _ => {}
    }
    if let Some((a, b)) = arg.split_once("..") {
        let (a, _) = parse_size_value(a).ok_or_else(bad)?;
        let (b, _) = parse_size_value(b).ok_or_else(bad)?;
        return Ok((a.min(b), a.max(b)));
    }
    let (op, v) = split_cmp(arg);
    let (v, step) = parse_size_value(v).ok_or_else(bad)?;
    Ok(match op {
        ">" => (v.saturating_add(1), u64::MAX),
        ">=" => (v, u64::MAX),
        "<" => (0, v.saturating_sub(1)),
        "<=" => (0, v),
        "=" => (v, v),
        // `size:10mb` = everything that shows as 10 MB.
        _ => (v, v.saturating_add(step - 1)),
    })
}

/// Clamps local-time seconds to a UTC `u32` time.
fn to_utc(local_secs: i64, now: Now) -> u32 {
    (local_secs - now.utc_offset_secs as i64).clamp(1, u32::MAX as i64) as u32
}

fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (days_from_civil(ny, nm, 1) - days_from_civil(y, m, 1)) as u32
}

/// An absolute date: `2026`, `2026-03`, `2026-03-15` (also with `/` or `.`).
/// Returns local seconds `[start, end)`.
fn parse_abs_date(s: &str) -> Option<(i64, i64)> {
    let parts: Vec<&str> = s.split(['-', '/', '.']).collect();
    if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    if parts[0].len() != 4 || !parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    if !(1970..=2105).contains(&y) {
        return None;
    }
    let day = 86_400i64;
    match parts.len() {
        1 => Some((
            days_from_civil(y, 1, 1) * day,
            days_from_civil(y + 1, 1, 1) * day,
        )),
        2 => {
            let m: u32 = parts[1].parse().ok()?;
            if !(1..=12).contains(&m) {
                return None;
            }
            let start = days_from_civil(y, m, 1);
            Some((start * day, (start + days_in_month(y, m) as i64) * day))
        }
        _ => {
            let m: u32 = parts[1].parse().ok()?;
            let d: u32 = parts[2].parse().ok()?;
            if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
                return None;
            }
            let start = days_from_civil(y, m, d);
            Some((start * day, (start + 1) * day))
        }
    }
}

/// A date word (`today`, `lastweek`, `last30days`...). Local seconds `[start, end)`.
fn parse_date_word(word: &str, now: Now) -> Option<(i64, i64)> {
    let day = 86_400i64;
    let local = now.unix + now.utc_offset_secs as i64;
    let today = local.div_euclid(day);
    let (y, m, _) = crate::util::civil_from_days(today);
    // 1970-01-01 was a Thursday; weeks start on Monday.
    let week_start = today - (today + 3).rem_euclid(7);
    let month_start = |y: i32, m: u32| days_from_civil(y, m, 1);
    let prev_month = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
    let next_month = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    Some(match word {
        "today" => (today * day, (today + 1) * day),
        "yesterday" => ((today - 1) * day, today * day),
        "thisweek" => (week_start * day, (week_start + 7) * day),
        "lastweek" | "pastweek" => ((week_start - 7) * day, week_start * day),
        "thismonth" => (
            month_start(y, m) * day,
            month_start(next_month.0, next_month.1) * day,
        ),
        "lastmonth" | "pastmonth" => (
            month_start(prev_month.0, prev_month.1) * day,
            month_start(y, m) * day,
        ),
        "thisyear" => (month_start(y, 1) * day, month_start(y + 1, 1) * day),
        "lastyear" | "pastyear" => (month_start(y - 1, 1) * day, month_start(y, 1) * day),
        _ => {
            // last<N><unit>: a sliding window that ends now.
            let rest = word
                .strip_prefix("last")
                .or_else(|| word.strip_prefix("past"))?;
            let split = rest.find(|c: char| !c.is_ascii_digit())?;
            let n: i64 = rest[..split].parse().ok()?;
            let unit = match &rest[split..] {
                "hour" | "hours" => 3600,
                "day" | "days" => day,
                "week" | "weeks" => 7 * day,
                "month" | "months" => 30 * day,
                "year" | "years" => 365 * day,
                _ => return None,
            };
            if n <= 0 || n > 100_000 {
                return None;
            }
            (local - n * unit, local + 1)
        }
    })
}

fn parse_date(arg: &str, now: Now, name: &str) -> Result<(u32, u32), String> {
    let bad = || {
        format!(
            "{name}: expects a date such as today, lastweek, last30days, 2026, 2026-03, 2026-03-15, >2026-01-01 or 2025-01-01..2025-06-30."
        )
    };
    let lower = arg.to_ascii_lowercase();
    let one = |s: &str| parse_date_word(s, now).or_else(|| parse_abs_date(s));
    if let Some((a, b)) = lower.split_once("..") {
        let (a, b) = (
            one(a.trim()).ok_or_else(bad)?,
            one(b.trim()).ok_or_else(bad)?,
        );
        let (start, end) = (a.0.min(b.0), a.1.max(b.1));
        return Ok((to_utc(start, now), to_utc(end - 1, now)));
    }
    let (op, v) = split_cmp(&lower);
    let (start, end) = one(v.trim()).ok_or_else(bad)?;
    Ok(match op {
        ">" => (to_utc(end, now), u32::MAX),
        ">=" => (to_utc(start, now), u32::MAX),
        "<" => (1, to_utc(start - 1, now)),
        "<=" => (1, to_utc(end - 1, now)),
        _ => (to_utc(start, now), to_utc(end - 1, now)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(s: &str) -> Tok {
        Tok::Term {
            text: s.into(),
            quoted: false,
        }
    }

    #[test]
    fn tokens() {
        assert_eq!(tokenize("  a  b "), vec![term("a"), term("b")]);
        assert_eq!(
            tokenize("a|b !c"),
            vec![term("a"), Tok::Or, term("b"), Tok::Not, term("c")]
        );
        assert_eq!(
            tokenize("\"annual report\" x"),
            vec![
                Tok::Term {
                    text: "annual report".into(),
                    quoted: true
                },
                term("x")
            ]
        );
        assert_eq!(
            tokenize("path:\"C:\\My Docs\""),
            vec![term("path:C:\\My Docs")]
        );
        assert_eq!(
            tokenize("<a b>|c"),
            vec![
                Tok::Open,
                term("a"),
                term("b"),
                Tok::Close,
                Tok::Or,
                term("c")
            ]
        );
        // Comparison signs are not groups; `!` inside a word is literal.
        assert_eq!(tokenize("size:>1mb"), vec![term("size:>1mb")]);
        assert_eq!(tokenize("size:<1mb"), vec![term("size:<1mb")]);
        assert_eq!(
            tokenize("<size:>=1mb dm:<2026>"),
            vec![Tok::Open, term("size:>=1mb"), term("dm:<2026"), Tok::Close]
        );
        assert_eq!(tokenize("wow!.txt"), vec![term("wow!.txt")]);
        assert_eq!(tokenize("a>b"), vec![term("a>b")]);
        assert_eq!(
            tokenize("\"\""),
            vec![Tok::Term {
                text: String::new(),
                quoted: true
            }]
        );
        assert_eq!(
            tokenize("\"open quote"),
            vec![Tok::Term {
                text: "open quote".into(),
                quoted: true
            }]
        );
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size(">10mb").unwrap(), (10 * (1 << 20) + 1, u64::MAX));
        assert_eq!(parse_size(">=1kb").unwrap(), (1024, u64::MAX));
        assert_eq!(parse_size("<1kb").unwrap(), (0, 1023));
        assert_eq!(parse_size("<=1KB").unwrap(), (0, 1024));
        assert_eq!(parse_size("=100").unwrap(), (100, 100));
        assert_eq!(parse_size("1mb..5mb").unwrap(), (1 << 20, 5 << 20));
        assert_eq!(parse_size("5mb..1mb").unwrap(), (1 << 20, 5 << 20));
        // A bare value covers everything that displays as that value.
        assert_eq!(parse_size("10mb").unwrap(), (10 << 20, (11 << 20) - 1));
        assert_eq!(parse_size("1.5gb").unwrap().0, 1_610_612_736);
        assert_eq!(parse_size("100").unwrap(), (100, 100));
        assert_eq!(parse_size("empty").unwrap(), (0, 0));
        assert_eq!(parse_size("gigantic").unwrap().1, u64::MAX);
        for bad in [
            "",
            "abc",
            ">",
            "10xb",
            "1..2..3",
            "1.2.3mb",
            "-5",
            "99999999999tb",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?}");
        }
    }

    // 2026-03-18 (a Wednesday) 15:30:00 local, UTC+3.
    const NOW: Now = Now {
        unix: 1_773_837_000,
        utc_offset_secs: 3 * 3600,
    };

    fn local_day(y: i32, m: u32, d: u32) -> u32 {
        (days_from_civil(y, m, d) * 86_400 - 3 * 3600) as u32
    }

    #[test]
    fn dates() {
        let d = |s: &str| parse_date(s, NOW, "dm").unwrap();
        assert_eq!(
            d("today"),
            (local_day(2026, 3, 18), local_day(2026, 3, 19) - 1)
        );
        assert_eq!(
            d("yesterday"),
            (local_day(2026, 3, 17), local_day(2026, 3, 18) - 1)
        );
        assert_eq!(
            d("thisweek"),
            (local_day(2026, 3, 16), local_day(2026, 3, 23) - 1)
        );
        assert_eq!(
            d("lastweek"),
            (local_day(2026, 3, 9), local_day(2026, 3, 16) - 1)
        );
        assert_eq!(
            d("thismonth"),
            (local_day(2026, 3, 1), local_day(2026, 4, 1) - 1)
        );
        assert_eq!(
            d("lastmonth"),
            (local_day(2026, 2, 1), local_day(2026, 3, 1) - 1)
        );
        assert_eq!(
            d("thisyear"),
            (local_day(2026, 1, 1), local_day(2027, 1, 1) - 1)
        );
        assert_eq!(
            d("lastyear"),
            (local_day(2025, 1, 1), local_day(2026, 1, 1) - 1)
        );
        assert_eq!(
            d("2024"),
            (local_day(2024, 1, 1), local_day(2025, 1, 1) - 1)
        );
        assert_eq!(
            d("2024-02"),
            (local_day(2024, 2, 1), local_day(2024, 3, 1) - 1)
        );
        assert_eq!(
            d("2024-02-29"),
            (local_day(2024, 2, 29), local_day(2024, 3, 1) - 1)
        );
        assert_eq!(d("2024/02/29"), d("2024-02-29"));
        assert_eq!(d(">2026-01-01"), (local_day(2026, 1, 2), u32::MAX));
        assert_eq!(d(">=2026-01-01"), (local_day(2026, 1, 1), u32::MAX));
        assert_eq!(d("<2026"), (1, local_day(2026, 1, 1) - 1));
        assert_eq!(d("<=2026-03"), (1, local_day(2026, 4, 1) - 1));
        assert_eq!(
            d("2025-01-01..2025-06-30"),
            (local_day(2025, 1, 1), local_day(2025, 7, 1) - 1)
        );
        assert_eq!(
            d("lastweek..today"),
            (local_day(2026, 3, 9), local_day(2026, 3, 19) - 1)
        );
        let (min, max) = d("last7days");
        assert_eq!(min, (NOW.unix - 7 * 86_400) as u32);
        assert_eq!(max, NOW.unix as u32);
        assert_eq!(d("last2hours").0, (NOW.unix - 7200) as u32);
        for bad in [
            "",
            "soon",
            "2026-13",
            "2026-02-30",
            "26-01-01",
            "last0days",
            "lastdays",
            "2026-1-1-1",
            ">",
        ] {
            assert!(parse_date(bad, NOW, "dm").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn number_ranges() {
        assert_eq!(parse_number_range("5"), Some((5, 5)));
        assert_eq!(parse_number_range(">5"), Some((6, u32::MAX)));
        assert_eq!(parse_number_range("<=5"), Some((0, 5)));
        assert_eq!(parse_number_range("3..9"), Some((3, 9)));
        assert_eq!(parse_number_range("<0"), None);
        assert_eq!(parse_number_range("x"), None);
    }
}
