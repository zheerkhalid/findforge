# Changelog

All notable changes to FindForge are listed here, newest first.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the version numbers follow [Semantic Versioning](https://semver.org/).

## [1.1.0] - 2026-11-24

### Added

- **Look inside files.** Add `content:invoice` (or `content:"total due"`) to a search and FindForge opens the plain-text files that match the rest of the search and keeps the ones that contain the text. It only happens when you ask for it, so ordinary searches stay instant. It also works from the command line: `FindForge.exe -s "content:invoice ext:txt;csv"`.
- While files are being read the status bar says so, and afterwards it tells how many files were looked into and how many were skipped. The index keeps following your drives in the meantime.
- The preview pane marks the text that `content:` found.
- **Matches are highlighted in Kurdish, Arabic, Persian and Hebrew names**, just as in other names.
- **Open with…** in the actions menu (the ⋯ button), to pick the program a result opens in.
- The window remembers its size, and whether it was maximized, for the next start.

### Changed

- Searches that take more than a second show their time in seconds instead of thousands of milliseconds.
- Right-to-left names are no longer drawn again while you resize a column, unless they have to be cut shorter.
- The packaging script works with a build folder outside the project (`CARGO_TARGET_DIR`).

### Fixed

- **Kurdish and Arabic text typed in the search box is now shown joined and in the right order.** The caret, the selection and mouse clicks land on the right letters. The same goes for the rename box, the filter and bookmark boxes and the lists in Settings.

## [1.0.1] - 2026-11-17

### Fixed

- Names and paths in Kurdish, Arabic, Persian, Hebrew and other right-to-left scripts were drawn left to right with the letters apart. They are now drawn by Windows' own text engine: letters joined, and mixed names ordered the way File Explorer shows them, in the results, the path column and the preview pane.

## [1.0.0] - 2026-11-17

First release.

### Added

- A live index of every file and folder name on your drives, saved to disk so the next start is instant.
- Results while you type, with the matched part of each name highlighted; sortable columns and a column chooser.
- A search language: AND, OR, NOT, phrases, wildcards, regular expressions, and filters by extension, size, date, type, folder, drive, depth, name length, duplicates, empty folders and attributes.
- Ready-made filters (Audio, Video, Pictures, Documents, Compressed, Executables, Folders) and your own.
- Actions on results: open, open the containing folder, copy the file, name or path, rename, move to the Recycle Bin, properties, the Explorer right-click menu, and dragging results into other programs.
- A preview pane for pictures and text files.
- Bookmarks, search history, and export of the results to CSV or text.
- A global hotkey, a tray icon, start with Windows, and "keep running when the window is closed".
- A command line: `FindForge.exe -s "query"` prints results as paths or CSV.
- Two ways to index: a parallel walk of the folders with live updates (no administrator rights needed), and a faster read of the NTFS file table and change journal when run as administrator.
- Dark and light themes; an installer and a portable version.

[1.1.0]: https://github.com/zheerkhalid/findforge/releases/tag/v1.1.0
[1.0.1]: https://github.com/zheerkhalid/findforge/releases/tag/v1.0.1
[1.0.0]: https://github.com/zheerkhalid/findforge/releases/tag/v1.0.0
