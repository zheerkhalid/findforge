//! Invented data for `--demo` and for screenshots: two made-up drives filled
//! with made-up folders and files. Nothing here comes from a real computer,
//! and nothing is read from or written to disk.

use crate::engine::{EngineStatus, VolState, VolStatus};
use crate::index::{attr, Excludes, Index, Meta, VolumeBuilder, VolumeKind, VolumeMeta};

/// A tiny deterministic generator (the demo must look the same every time).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a>(&mut self, list: &[&'a str]) -> &'a str {
        list[self.below(list.len() as u64) as usize]
    }
}

const TOPICS: [&str; 24] = [
    "budget",
    "report",
    "invoice",
    "meeting notes",
    "roadmap",
    "proposal",
    "contract",
    "summary",
    "timeline",
    "checklist",
    "handbook",
    "newsletter",
    "forecast",
    "survey",
    "agenda",
    "brief",
    "estimate",
    "inventory",
    "schedule",
    "review",
    "plan",
    "draft",
    "minutes",
    "outline",
];
const QUALIFIERS: [&str; 12] = [
    "final", "v2", "signed", "revised", "Q1", "Q2", "Q3", "Q4", "2025", "2026", "copy", "approved",
];
const PLACES: [&str; 14] = [
    "lakeside", "harbour", "mountain", "old town", "forest", "coast", "garden", "river", "desert",
    "island", "market", "bridge", "valley", "sunset",
];
const ARTISTS: [&str; 10] = [
    "Paper Lanterns",
    "Northern Echo",
    "The Quiet Hours",
    "Velvet Compass",
    "Glass Harbor",
    "Amber Static",
    "Low Tide Radio",
    "Marble Kites",
    "Second Sunday",
    "Copper Field",
];
const SONGS: [&str; 16] = [
    "Morning Light",
    "Slow Train",
    "Paper Boats",
    "Night Drive",
    "Open Window",
    "Far Shore",
    "Little Fires",
    "Blue Hour",
    "Homecoming",
    "Static",
    "Wild Rye",
    "Afterglow",
    "North Road",
    "Long Way Round",
    "Small Hours",
    "Tidewater",
];
const PROJECTS: [&str; 8] = [
    "weather-station",
    "recipe-box",
    "budget-tracker",
    "photo-sorter",
    "garden-planner",
    "trail-maps",
    "book-club-site",
    "home-inventory",
];
const SOURCE_FILES: [&str; 14] = [
    "main", "config", "utils", "models", "routes", "store", "theme", "index", "api", "layout",
    "parser", "render", "state", "tests",
];
const APPS: [&str; 10] = [
    "Photo Studio",
    "Code Editor",
    "Mail Client",
    "Media Player",
    "Archive Manager",
    "Notes Desk",
    "Cloud Sync",
    "Disk Imager",
    "Font Manager",
    "3D Modeler",
];
const LIBS: [&str; 14] = [
    "core", "render", "codec", "network", "storage", "ui", "audio", "plugin", "update", "crash",
    "locale", "theme", "gpu", "script",
];

struct Gen {
    b: VolumeBuilder,
    rng: Rng,
    next_id: u64,
    now: u32,
}

impl Gen {
    fn new(root: &str, label: &str, seed: u64, now: u32) -> Gen {
        let mut meta = VolumeMeta::new(root, VolumeKind::NtfsMft);
        meta.label = label.into();
        meta.fs = "NTFS".into();
        meta.scanned_unix = now as u64;
        let mut b = VolumeBuilder::new(meta);
        b.set_root_frn(5);
        Gen {
            b,
            rng: Rng(seed),
            next_id: 100,
            now,
        }
    }

    fn dir(&mut self, parent: u64, name: &str) -> u64 {
        self.dir_attrs(parent, name, 0)
    }

    fn dir_attrs(&mut self, parent: u64, name: &str, extra: u32) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let age = self.rng.below(900 * 86_400) as u32;
        let meta = Meta {
            size: 0,
            mtime: self.now.saturating_sub(age / 3),
            ctime: self.now.saturating_sub(age),
        };
        self.b.push(id, parent, name, attr::DIRECTORY | extra, meta);
        id
    }

    /// Adds a file modified up to `max_age_days` ago.
    fn file(&mut self, parent: u64, name: &str, size: u64, max_age_days: u64) {
        self.file_attrs(parent, name, size, max_age_days, 0);
    }

    fn file_attrs(&mut self, parent: u64, name: &str, size: u64, max_age_days: u64, attrs: u32) {
        let id = self.next_id;
        self.next_id += 1;
        // Square the roll so recent files are more common than old ones.
        let roll = self.rng.below(10_000) as f64 / 10_000.0;
        let age =
            (roll * roll * max_age_days as f64 * 86_400.0) as u32 + self.rng.below(3600) as u32;
        let mtime = self.now.saturating_sub(age);
        let ctime = mtime.saturating_sub(self.rng.below(40 * 86_400) as u32);
        self.b
            .push(id, parent, name, attrs, Meta { size, mtime, ctime });
    }

    fn size(&mut self, min: u64, max: u64) -> u64 {
        min + self.rng.below(max - min)
    }

    fn documents(&mut self, parent: u64, count: usize) {
        const EXTS: [&str; 7] = ["docx", "xlsx", "pdf", "pptx", "txt", "md", "csv"];
        for i in 0..count {
            let topic = self.rng.pick(&TOPICS);
            let qual = self.rng.pick(&QUALIFIERS);
            let ext = self.rng.pick(&EXTS);
            let name = match i % 4 {
                0 => format!("{topic} {qual}.{ext}"),
                1 => format!(
                    "{}{} - {qual}.{ext}",
                    topic[..1].to_uppercase(),
                    &topic[1..]
                ),
                2 => format!("{topic}-{:04}.{ext}", 1000 + self.rng.below(9000)),
                _ => format!("{qual} {topic}.{ext}"),
            };
            let size = self.size(4_000, 6_000_000);
            self.file(parent, &name, size, 1200);
        }
    }

    fn photos(&mut self, parent: u64, count: usize) {
        for _ in 0..count {
            let place = self.rng.pick(&PLACES).replace(' ', "-");
            let n = self.rng.below(9000);
            let ext = if self.rng.below(12) == 0 {
                "png"
            } else {
                "jpg"
            };
            let name = format!("{place}-{n:04}.{ext}");
            let size = self.size(900_000, 9_000_000);
            self.file(parent, &name, size, 1500);
        }
    }
}

fn build_c(now: u32) -> VolumeBuilder {
    let mut g = Gen::new("C:", "System", 0x5EED_F00D_1234_5678, now);
    let users = g.dir(5, "Users");
    let alex = g.dir(users, "Alex");

    // Documents
    let docs = g.dir(alex, "Documents");
    g.file(docs, "Quarterly report 2026.xlsx", 482_112, 3);
    g.file(docs, "Quarterly report 2025.xlsx", 455_680, 400);
    g.file(docs, "Annual report draft.docx", 1_204_224, 1);
    g.file(docs, "Trip budget.xlsx", 38_912, 20);
    g.file(docs, "Résumé - Alex Morgan.pdf", 212_480, 90);
    g.file(docs, "Lease agreement signed.pdf", 2_310_144, 300);
    g.file(docs, "Reading list.md", 4_096, 12);
    g.file(docs, "report-template.dotx", 61_440, 700);
    for folder in [
        "Work",
        "Household",
        "Taxes",
        "Letters",
        "Recipes",
        "Manuals",
    ] {
        let d = g.dir(docs, folder);
        g.documents(d, 60);
        for year in ["2023", "2024", "2025", "2026"] {
            let y = g.dir(d, year);
            g.documents(y, 45);
        }
    }
    let reports = g.dir(docs, "Reports");
    for (i, month) in [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ]
    .iter()
    .enumerate()
    {
        g.file(
            reports,
            &format!("Monthly report {:02} {month}.pdf", i + 1),
            300_000 + i as u64 * 41_000,
            60 + i as u64 * 30,
        );
    }
    g.file(reports, "Sales report summary.pptx", 8_400_000, 6);
    g.file(reports, "Expense report.xlsx", 96_000, 2);
    g.file(reports, "Incident report 0412.docx", 54_000, 30);

    // Pictures
    let pics = g.dir(alex, "Pictures");
    for (album, n) in [
        ("2024 Coast trip", 220),
        ("2025 Mountain cabin", 260),
        ("2026 City weekend", 180),
        ("Family", 320),
        ("Garden", 140),
        ("Screenshots", 90),
        ("Wallpapers", 40),
    ] {
        let a = g.dir(pics, album);
        g.photos(a, n);
    }
    g.file(pics, "profile-picture.png", 184_320, 500);
    g.file(pics, "report-cover.png", 912_384, 4);

    // Music
    let music = g.dir(alex, "Music");
    for artist in ARTISTS {
        let a = g.dir(music, artist);
        for album in 1..=3 {
            let al = g.dir(a, &format!("Album {album}"));
            for (t, song) in SONGS.iter().enumerate().take(10 + album) {
                let ext = if album == 3 { "flac" } else { "mp3" };
                let size = if album == 3 {
                    g.size(18_000_000, 42_000_000)
                } else {
                    g.size(3_000_000, 11_000_000)
                };
                g.file(al, &format!("{:02} {song}.{ext}", t + 1), size, 2000);
            }
            g.file(al, "cover.jpg", 240_000, 2000);
        }
    }

    // Videos
    let videos = g.dir(alex, "Videos");
    for (name, gb10) in [
        ("Birthday 2025.mp4", 14),
        ("Coast trip highlights.mp4", 31),
        ("Cabin timelapse.mov", 22),
        ("Garden tour.mkv", 9),
        ("Screen recording 01.mp4", 4),
        ("Screen recording 02.mp4", 6),
        ("Wedding speech.mp4", 18),
        ("Report walkthrough.mp4", 3),
    ] {
        g.file(videos, name, gb10 * 107_374_182, 700);
    }

    // Downloads
    let downloads = g.dir(alex, "Downloads");
    for (name, size) in [
        ("photo-studio-setup-5.2.exe", 184_000_000u64),
        ("code-editor-1.92-x64.msi", 96_000_000),
        ("fonts-pack.zip", 48_000_000),
        ("city-map.pdf", 12_400_000),
        ("boarding-pass.pdf", 180_000),
        ("dataset-2026.csv", 310_000_000),
        ("backup-tool-portable.7z", 22_000_000),
        ("wallpaper-pack.rar", 140_000_000),
        ("receipt-88213.pdf", 64_000),
        ("driver-update.cab", 9_800_000),
        ("annual-report-2025.pdf", 6_200_000),
        ("linux-live.iso", 2_900_000_000),
    ] {
        g.file(downloads, name, size, 120);
    }
    g.documents(downloads, 80);

    // Projects
    let projects = g.dir(alex, "Projects");
    for project in PROJECTS {
        let p = g.dir(projects, project);
        g.file(p, "README.md", 3_200, 200);
        g.file(p, "LICENSE", 1_070, 800);
        g.file(p, ".gitignore", 220, 800);
        g.file(p, "package.json", 1_400, 90);
        let src = g.dir(p, "src");
        for f in SOURCE_FILES {
            let ext = ["ts", "tsx", "rs", "py", "css"][g.rng.below(5) as usize];
            let size = g.size(400, 48_000);
            g.file(src, &format!("{f}.{ext}"), size, 400);
        }
        let assets = g.dir(p, "assets");
        g.photos(assets, 12);
        let docs = g.dir(p, "docs");
        g.documents(docs, 10);
        let nm = g.dir(p, "node_modules");
        for lib in 0..40 {
            let l = g.dir(nm, &format!("pkg-{}-{lib:02}", LIBS[lib % LIBS.len()]));
            for f in ["index.js", "index.d.ts", "package.json", "README.md"] {
                let size = g.size(300, 90_000);
                g.file(l, f, size, 900);
            }
        }
    }

    // Desktop and hidden application data
    let desktop = g.dir(alex, "Desktop");
    g.file(desktop, "To do.txt", 812, 1);
    g.file(desktop, "Photo Studio.lnk", 1_420, 300);
    g.file(desktop, "Code Editor.lnk", 1_380, 250);
    g.file(desktop, "Report notes.txt", 2_048, 0);
    // Right-to-left names, so the demo shows joined Kurdish and Arabic text.
    let kurdish = g.dir(docs, "بەڵگەنامەکان");
    g.file(kurdish, "ڕاپۆرتی ساڵانە report 2026.pdf", 845_000, 2);
    g.file(kurdish, "گۆرانی کوردی.mp3", 5_300_000, 12);
    g.file(desktop, "تقرير الاجتماع.docx", 64_000, 1);
    let appdata = g.dir_attrs(alex, "AppData", attr::HIDDEN);
    let local = g.dir(appdata, "Local");
    for app in APPS {
        let a = g.dir(local, app);
        let cache = g.dir(a, "Cache");
        for i in 0..60 {
            let size = g.size(2_000, 4_000_000);
            g.file(cache, &format!("f_{i:06x}"), size, 30);
        }
        g.file(a, "settings.json", 5_200, 14);
        g.file(a, "log.txt", 840_000, 1);
    }

    // Programs
    let pf = g.dir(5, "Program Files");
    for app in APPS {
        let a = g.dir(pf, app);
        let exe = format!("{}.exe", app.replace(' ', ""));
        let size = g.size(2_000_000, 160_000_000);
        g.file(a, &exe, size, 300);
        g.file(a, "unins000.exe", 3_100_000, 300);
        g.file(a, "license.txt", 18_000, 900);
        for lib in LIBS {
            let size = g.size(80_000, 30_000_000);
            g.file(a, &format!("{lib}.dll"), size, 300);
        }
        let res = g.dir(a, "resources");
        for i in 0..40 {
            let size = g.size(1_000, 600_000);
            g.file(res, &format!("asset-{i:03}.pak"), size, 300);
        }
        let loc = g.dir(a, "locales");
        for l in [
            "en-US", "de", "fr", "es", "it", "ja", "ko", "pt-BR", "ar", "ku",
        ] {
            let size = g.size(40_000, 500_000);
            g.file(loc, &format!("{l}.pak"), size, 300);
        }
    }

    // System folder: invented component names only.
    let win = g.dir(5, "Windows");
    let sys32 = g.dir(win, "System32");
    for lib in LIBS {
        for n in 0..90 {
            let size = g.size(20_000, 9_000_000);
            g.file(sys32, &format!("{lib}svc{n:02}.dll"), size, 1400);
        }
    }
    let fonts = g.dir(win, "Fonts");
    for f in [
        "Lato",
        "Merriweather",
        "SourceSans",
        "Inconsolata",
        "Nunito",
        "Karla",
    ] {
        for w in ["Regular", "Bold", "Italic", "Light"] {
            let size = g.size(90_000, 700_000);
            g.file(fonts, &format!("{f}-{w}.ttf"), size, 1800);
        }
    }
    let temp = g.dir(win, "Temp");
    for i in 0..120 {
        let size = g.size(0, 2_000_000);
        g.file(temp, &format!("tmp{i:04X}.tmp"), size, 10);
    }
    g.dir(win, "Empty folder for updates");

    // The bulk of a system drive: components, caches, package stores.
    let components = g.dir(win, "Components");
    for d in 0..900usize {
        let lib = LIBS[d % LIBS.len()];
        let tag = g.rng.next() as u32;
        let folder = g.dir(components, &format!("{lib}-{tag:08x}"));
        for f in 0..380usize {
            let ext = ["dll", "mui", "cat", "manifest"][f % 4];
            let size = g.size(2_000, 4_000_000);
            let tag = g.rng.next() & 0xFF_FFFF;
            g.file(folder, &format!("{lib}{f:03}-{tag:06x}.{ext}"), size, 1800);
        }
    }
    let packages = g.dir(local, "Packages");
    for d in 0..500usize {
        let app = APPS[d % APPS.len()].replace(' ', "");
        let folder = g.dir(packages, &format!("{app}.{d:03}"));
        for f in 0..300usize {
            let ext = ["dat", "json", "bin", "idx"][f % 4];
            let size = g.size(200, 900_000);
            let tag = g.rng.next() & 0xFFFF_FFFF;
            g.file(folder, &format!("cache-{tag:08x}.{ext}"), size, 200);
        }
    }
    g.file_attrs(
        5,
        "pagefile.sys",
        8_589_934_592,
        1,
        attr::HIDDEN | attr::SYSTEM,
    );
    g.file_attrs(
        5,
        "hiberfil.sys",
        6_442_450_944,
        1,
        attr::HIDDEN | attr::SYSTEM,
    );
    g.b
}

fn build_d(now: u32) -> VolumeBuilder {
    let mut g = Gen::new("D:", "Data", 0x0DDB_A11C_0FFE_E123, now);
    let archive = g.dir(5, "Archive");
    for year in 2015..=2026 {
        let y = g.dir(archive, &year.to_string());
        for q in ["Q1", "Q2", "Q3", "Q4"] {
            let d = g.dir(y, q);
            g.documents(d, 70);
            g.file(
                d,
                &format!("{year} {q} report.pdf"),
                1_500_000 + (year as u64 % 7) * 210_000,
                (2027 - year as u64) * 365,
            );
        }
    }
    let media = g.dir(5, "Media");
    let raw = g.dir(media, "Raw photos");
    for shoot in 1..=200 {
        let s = g.dir(raw, &format!("Shoot {shoot:03}"));
        for n in 0..300 {
            let size = g.size(22_000_000, 48_000_000);
            g.file(s, &format!("IMG_{:05}.cr3", shoot * 300 + n), size, 2200);
        }
    }
    // Years of archived mail.
    let mail = g.dir(archive, "Mail");
    for year in 2012..=2026 {
        let y = g.dir(mail, &year.to_string());
        for month in 1..=12 {
            let m = g.dir(y, &format!("{year}-{month:02}"));
            for n in 0..900 {
                let size = g.size(2_000, 400_000);
                g.file(
                    m,
                    &format!("message-{n:05}.eml"),
                    size,
                    (2027 - year as u64) * 365,
                );
            }
        }
    }
    let films = g.dir(media, "Home videos");
    for n in 1..=40 {
        let size = g.size(400_000_000, 6_000_000_000);
        g.file(films, &format!("Tape {n:02} digitised.mkv"), size, 3000);
    }
    let backups = g.dir(5, "Backups");
    for n in 0..26 {
        let size = g.size(2_000_000_000, 40_000_000_000);
        g.file(
            backups,
            &format!("laptop-backup-2026-{:02}-{:02}.7z", n / 3 + 1, n % 27 + 1),
            size,
            330,
        );
    }
    g.file(backups, "backup report.txt", 12_000, 2);
    let vms = g.dir(5, "Virtual machines");
    for vm in ["test-lab", "build-agent", "old-desktop"] {
        let v = g.dir(vms, vm);
        g.file(v, &format!("{vm}.vhdx"), 64_424_509_440, 60);
        g.file(v, &format!("{vm}.vmcx"), 48_000, 60);
    }
    let books = g.dir(5, "Library");
    for n in 0..400 {
        let topic = g.rng.pick(&TOPICS);
        let place = g.rng.pick(&PLACES);
        let ext = ["epub", "pdf", "mobi"][g.rng.below(3) as usize];
        let size = g.size(300_000, 30_000_000);
        g.file(
            books,
            &format!("The {place} {topic} ({}).{ext}", 1950 + n % 76),
            size,
            2500,
        );
    }
    g.dir(5, "Scans to sort");
    g.b
}

/// The invented index. `now` is Unix seconds (dates are spread out before it).
pub fn index(now: u32) -> Index {
    let none = Excludes::default();
    Index {
        volumes: vec![
            build_c(now).finish(&none, true),
            build_d(now).finish(&none, true),
        ],
        epoch: 1,
    }
}

/// A large made-up index of about `entries` entries on one drive, for
/// measuring search speed without touching a real disk.
pub fn synthetic(entries: usize) -> Index {
    const WORDS: [&str; 16] = [
        "report", "invoice", "holiday", "budget", "notes", "draft", "final", "backup", "project",
        "summary", "photo", "song", "video", "archive", "index", "readme",
    ];
    const EXTS: [&str; 10] = [
        "txt", "pdf", "jpg", "png", "mp3", "docx", "xlsx", "zip", "rs", "dll",
    ];
    let mut b = VolumeBuilder::with_capacity(VolumeMeta::new("C:", VolumeKind::NtfsMft), entries);
    b.set_root_frn(5);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut dirs: Vec<u64> = vec![5];
    for (i, id) in (0..entries).zip(100u64..) {
        let r = rng.next();
        let parent = dirs[(r % dirs.len() as u64) as usize];
        if i % 12 == 0 {
            let name = format!("{} {}", WORDS[(r >> 8) as usize % 16], (r >> 16) % 5000);
            b.push(id, parent, &name, attr::DIRECTORY, Meta::default());
            if dirs.len() < 200_000 {
                dirs.push(id);
            }
        } else {
            let name = format!(
                "{}-{}-{:05}.{}",
                WORDS[(r >> 8) as usize % 16],
                WORDS[(r >> 12) as usize % 16],
                (r >> 20) % 100_000,
                EXTS[(r >> 40) as usize % 10]
            );
            b.push(
                id,
                parent,
                &name,
                0,
                Meta {
                    size: (r >> 30) % 50_000_000,
                    mtime: 1_600_000_000 + (r % 170_000_000) as u32,
                    ctime: 1_500_000_000,
                },
            );
        }
    }
    Index {
        volumes: vec![b.finish(&Excludes::default(), true)],
        epoch: 1,
    }
}

/// A status to go with [`index`], as if both drives were indexed and live.
pub fn status(index: &Index, now: u32) -> EngineStatus {
    let mut status = EngineStatus {
        elevated: true,
        last_save_unix: now as u64 - 140,
        ..Default::default()
    };
    for (i, v) in index.volumes.iter().enumerate() {
        let entries = v.live_count().saturating_sub(1);
        status.total_entries += entries;
        status.memory_bytes += v.memory_bytes();
        status.volumes.push(VolStatus {
            root: v.meta.root.clone(),
            label: v.meta.label.clone(),
            fs: v.meta.fs.clone(),
            kind: VolumeKind::NtfsMft,
            state: VolState::Ready,
            refreshing: false,
            entries,
            memory_bytes: v.memory_bytes(),
            live: true,
            last_update_unix: now as u64 - 3 - i as u64 * 40,
            scanned_unix: now as u64 - 5400,
            scan_ms: 1840 + i as u32 * 730,
            from_disk: true,
            note: String::new(),
        });
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_index_is_plausible_and_repeatable() {
        let a = index(1_800_000_000);
        let b = index(1_800_000_000);
        assert_eq!(a.total(), b.total());
        assert!(a.total() > 500_000, "{}", a.total());
        assert_eq!(a.volumes.len(), 2);
        let hit = a
            .find_path("C:\\Users\\Alex\\Documents\\Quarterly report 2026.xlsx")
            .expect("seeded file");
        let e = a.entry(hit).unwrap();
        assert_eq!(e.size, 482_112);
        assert!(e.mtime <= 1_800_000_000 && e.mtime > 1_700_000_000);
        assert!(a.find_path("D:\\Backups").is_some());
        // No orphans were dropped: every pushed entry hangs off a root.
        for v in &a.volumes {
            assert_eq!(v.garbage_ratio(), 0.0);
        }
        let s = status(&a, 1_800_000_000);
        assert_eq!(s.total_entries, a.total());
        assert!(s.volumes.iter().all(|v| v.live));
    }
}
