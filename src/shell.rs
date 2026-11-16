//! Things done with files through the Windows shell: opening, revealing in
//! Explorer, the real Explorer context menu, dragging out, the clipboard, the
//! Recycle Bin, renaming, icons and type names.
//!
//! Functions that show a menu or run a drag must be called on the thread
//! that owns the window (the user-interface thread).

use crate::sys::{from_wide, shell_execute, wide};
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::Path;
use windows::core::{Interface, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    HBITMAP, HDC,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_FLAGS_AND_ATTRIBUTES,
};
use windows::Win32::System::Com::{CoInitializeEx, IDataObject, COINIT_APARTMENTTHREADED};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    OleFlushClipboard, OleInitialize, OleSetClipboard, DROPEFFECT_COPY, DROPEFFECT_LINK,
    DROPEFFECT_MOVE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, DefSubclassProc, IContextMenu, IContextMenu2, IContextMenu3,
    ILCreateFromPathW, ILFindLastID, ILFree, IShellFolder, IShellItemArray, RemoveWindowSubclass,
    SHBindToParent, SHCreateShellItemArrayFromIDLists, SHDoDragDrop, SHFileOperationW,
    SHGetFileInfoW, SHObjectProperties, SHOpenFolderAndSelectItems, SetWindowSubclass,
    CMF_CANRENAME, CMF_EXTENDEDVERBS, CMF_NORMAL, CMINVOKECOMMANDINFO, FOF_ALLOWUNDO,
    FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, GCS_VERBW, SHFILEINFOW,
    SHFILEOPSTRUCTW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_SMALLICON, SHGFI_TYPENAME,
    SHGFI_USEFILEATTRIBUTES, SHOP_FILEPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos, GetIconInfo, SetForegroundWindow,
    TrackPopupMenuEx, HICON, ICONINFO, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM,
    WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// COM / OLE must be initialised on every thread that uses the shell.
fn ensure_com() {
    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    DONE.with(|d| {
        if !d.get() {
            // SAFETY: initialising COM twice on a thread is allowed; the
            // window thread is already an STA (drag and drop needs OLE).
            unsafe {
                if OleInitialize(None).is_err() {
                    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                }
            }
            d.set(true);
        }
    });
}

// --------------------------------------------------------------------------------------
// Opening
// --------------------------------------------------------------------------------------

/// Opens a file with its default program, or a folder in Explorer.
pub fn open(path: &str) -> Result<(), String> {
    if !Path::new(path).exists() {
        return Err(format!("\"{path}\" no longer exists."));
    }
    shell_execute("open", path, "")
}

/// Shows the "Open with" chooser for a file.
pub fn open_with(path: &str) -> Result<(), String> {
    shell_execute("openas", path, "")
}

/// Opens the folder that contains `path` in Explorer with the item selected.
pub fn reveal(path: &str) -> Result<(), String> {
    ensure_com();
    let w = wide(path);
    // SAFETY: the id list is created and freed here.
    unsafe {
        let pidl = ILCreateFromPathW(PCWSTR(w.as_ptr()));
        if pidl.is_null() {
            return Err(format!("\"{path}\" no longer exists."));
        }
        let r = SHOpenFolderAndSelectItems(pidl, None, 0);
        ILFree(Some(pidl));
        r.map_err(|e| format!("Explorer could not show it: {e}"))
    }
}

/// Opens the Explorer properties sheet of a file or folder.
pub fn properties(window: isize, path: &str) -> Result<(), String> {
    ensure_com();
    let w = wide(path);
    // SAFETY: valid strings.
    let ok = unsafe {
        SHObjectProperties(
            hwnd(window),
            SHOP_FILEPATH,
            PCWSTR(w.as_ptr()),
            PCWSTR::null(),
        )
    };
    if ok.as_bool() {
        Ok(())
    } else {
        Err("Windows could not show the properties.".into())
    }
}

// --------------------------------------------------------------------------------------
// Clipboard
// --------------------------------------------------------------------------------------

/// Puts text on the clipboard.
pub fn copy_text(text: &str) -> Result<(), String> {
    let data = wide(text);
    // SAFETY: standard clipboard sequence; the memory is handed to the
    // clipboard on success.
    unsafe {
        OpenClipboard(HWND::default()).map_err(|e| format!("The clipboard is busy: {e}"))?;
        let result = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| e.to_string())?;
            let bytes = data.len() * 2;
            let mem: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| e.to_string())?;
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                return Err("Out of memory.".into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            let _ = GlobalUnlock(mem);
            // CF_UNICODETEXT
            SetClipboardData(13, HANDLE(mem.0)).map_err(|e| e.to_string())?;
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

struct IdLists(Vec<*mut ITEMIDLIST>);

impl IdLists {
    fn new(paths: &[String]) -> IdLists {
        let mut v = Vec::new();
        for p in paths {
            let w = wide(p);
            // SAFETY: valid string; freed in Drop.
            let pidl = unsafe { ILCreateFromPathW(PCWSTR(w.as_ptr())) };
            if !pidl.is_null() {
                v.push(pidl);
            }
        }
        IdLists(v)
    }

    fn as_const(&self) -> Vec<*const ITEMIDLIST> {
        self.0.iter().map(|p| *p as *const ITEMIDLIST).collect()
    }
}

impl Drop for IdLists {
    fn drop(&mut self) {
        for p in &self.0 {
            // SAFETY: created by ILCreateFromPathW.
            unsafe { ILFree(Some(*p)) };
        }
    }
}

fn data_object(paths: &[String]) -> Result<IDataObject, String> {
    ensure_com();
    let lists = IdLists::new(paths);
    if lists.0.is_empty() {
        return Err("These files no longer exist.".into());
    }
    // SAFETY: the id lists outlive the calls; COM objects are reference counted.
    unsafe {
        let array: IShellItemArray =
            SHCreateShellItemArrayFromIDLists(&lists.as_const()).map_err(|e| e.to_string())?;
        array
            .BindToHandler::<_, IDataObject>(None, &BHID_DataObject)
            .map_err(|e| e.to_string())
    }
}

/// Puts files on the clipboard so they can be pasted in Explorer (as a copy).
pub fn copy_files(paths: &[String]) -> Result<(), String> {
    let data = data_object(paths)?;
    // SAFETY: OLE is initialised on this thread by `data_object`.
    unsafe {
        OleSetClipboard(&data).map_err(|e| format!("The clipboard is busy: {e}"))?;
        let _ = OleFlushClipboard();
    }
    Ok(())
}

// --------------------------------------------------------------------------------------
// Drag out
// --------------------------------------------------------------------------------------

/// Starts dragging files out of the window; returns when the user drops them
/// or cancels. `Ok(true)` when they were dropped on something that took them.
/// Call while the mouse button is still down.
pub fn drag_out(window: isize, paths: &[String]) -> Result<bool, String> {
    let data = data_object(paths)?;
    // SAFETY: a modal drag loop on the window thread with a live data object.
    let effect = unsafe {
        SHDoDragDrop(
            hwnd(window),
            &data,
            None,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
        )
    };
    match effect {
        Ok(e) => Ok(e.0 != 0),
        Err(e) => Err(format!("The drag could not start: {e}")),
    }
}

// --------------------------------------------------------------------------------------
// The Explorer context menu
// --------------------------------------------------------------------------------------

thread_local! {
    static MENU_HANDLERS: RefCell<(Option<IContextMenu2>, Option<IContextMenu3>)> =
        const { RefCell::new((None, None)) };
}

unsafe extern "system" fn menu_subclass(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) {
        let handled = MENU_HANDLERS.with(|h| {
            let h = h.borrow();
            if let Some(cm3) = &h.1 {
                let mut result = LRESULT(0);
                if cm3
                    .HandleMenuMsg2(msg, wparam, lparam, Some(&mut result))
                    .is_ok()
                {
                    return Some(result);
                }
            } else if let Some(cm2) = &h.0 {
                if cm2.HandleMenuMsg(msg, wparam, lparam).is_ok() {
                    return Some(LRESULT(if msg == WM_INITMENUPOPUP { 0 } else { 1 }));
                }
            }
            None
        });
        if let Some(r) = handled {
            return r;
        }
    }
    DefSubclassProc(window, msg, wparam, lparam)
}

/// What the user chose in the Explorer context menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuChoice {
    /// Nothing (the menu was dismissed).
    None,
    /// A command was run by the shell; its verb when the handler names one
    /// (`delete`, `cut`, `properties`...).
    Invoked(String),
    /// "Rename" was chosen: the caller should show its own rename box,
    /// because the shell can only rename inside an Explorer window.
    Rename,
}

/// Shows the real Explorer context menu for `paths` at the mouse position and
/// runs what the user picks. All items must be in the same folder; items in
/// other folders than the first are left out.
pub fn context_menu(window: isize, paths: &[String]) -> Result<MenuChoice, String> {
    ensure_com();
    let Some(first) = paths.first() else {
        return Ok(MenuChoice::None);
    };
    let folder = Path::new(first).parent().map(|p| p.to_path_buf());
    let same: Vec<String> = paths
        .iter()
        .filter(|p| Path::new(p.as_str()).parent().map(|x| x.to_path_buf()) == folder)
        .cloned()
        .collect();
    let lists = IdLists::new(&same);
    if lists.0.is_empty() {
        return Err(format!("\"{first}\" no longer exists."));
    }
    // SAFETY: id lists outlive every use; the menu and the subclass are
    // removed before returning; all of this runs on the window thread.
    unsafe {
        let parent: IShellFolder = SHBindToParent(lists.0[0], None).map_err(|e| e.to_string())?;
        let children: Vec<*const ITEMIDLIST> = lists
            .0
            .iter()
            .map(|p| ILFindLastID(*p) as *const ITEMIDLIST)
            .collect();
        let cm: IContextMenu = parent
            .GetUIObjectOf(hwnd(window), &children, None)
            .map_err(|e| e.to_string())?;
        let menu = CreatePopupMenu().map_err(|e| e.to_string())?;
        let mut flags = CMF_NORMAL | CMF_CANRENAME;
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            flags |= CMF_EXTENDEDVERBS;
        }
        const FIRST: u32 = 1;
        if let Err(e) = cm.QueryContextMenu(menu, 0, FIRST, 0x7FFF, flags) {
            let _ = DestroyMenu(menu);
            return Err(e.to_string());
        }
        MENU_HANDLERS.with(|h| {
            *h.borrow_mut() = (
                cm.cast::<IContextMenu2>().ok(),
                cm.cast::<IContextMenu3>().ok(),
            );
        });
        let _ = SetWindowSubclass(hwnd(window), Some(menu_subclass), 0xF1D0, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd(window));
        let picked = TrackPopupMenuEx(
            menu,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
            pt.x,
            pt.y,
            hwnd(window),
            None,
        )
        .0 as u32;
        let _ = RemoveWindowSubclass(hwnd(window), Some(menu_subclass), 0xF1D0);
        MENU_HANDLERS.with(|h| *h.borrow_mut() = (None, None));
        let mut choice = MenuChoice::None;
        if picked >= FIRST {
            let id = (picked - FIRST) as usize;
            let mut buf = [0u16; 128];
            let verb = if cm
                .GetCommandString(
                    id,
                    GCS_VERBW,
                    None,
                    PSTR(buf.as_mut_ptr() as *mut u8),
                    buf.len() as u32 - 1,
                )
                .is_ok()
            {
                from_wide(&buf).to_lowercase()
            } else {
                String::new()
            };
            if verb == "rename" {
                choice = MenuChoice::Rename;
            } else {
                let info = CMINVOKECOMMANDINFO {
                    cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
                    hwnd: hwnd(window),
                    // MAKEINTRESOURCE: the command by its offset.
                    lpVerb: PCSTR(id as *const u8),
                    nShow: SW_SHOWNORMAL.0,
                    ..Default::default()
                };
                // A failure here is usually the user cancelling a dialog.
                let _ = cm.InvokeCommand(&info);
                choice = MenuChoice::Invoked(verb);
            }
        }
        let _ = DestroyMenu(menu);
        Ok(choice)
    }
}

// --------------------------------------------------------------------------------------
// Changing files (only ever on an explicit user action)
// --------------------------------------------------------------------------------------

/// Sends files and folders to the Recycle Bin.
pub fn recycle(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    // SHFileOperation wants a double-NUL-terminated list of full paths.
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        if !Path::new(p).is_absolute() {
            return Err(format!("\"{p}\" is not a full path."));
        }
        from.extend(p.encode_utf16());
        from.push(0);
    }
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0) as u16,
        ..Default::default()
    };
    // SAFETY: the buffer outlives the call.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 {
        return Err(format!("The Recycle Bin refused it (code {code:#x})."));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err("The operation was cancelled.".into());
    }
    Ok(())
}

/// Checks a new file name; returns why it is not acceptable.
pub fn check_new_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("Type a name.".into());
    }
    if n != name {
        return Err("A name cannot start or end with a space.".into());
    }
    if let Some(c) = n
        .chars()
        .find(|c| "\\/:*?\"<>|".contains(*c) || (*c as u32) < 32)
    {
        return Err(if (c as u32) < 32 {
            "A name cannot contain control characters.".to_string()
        } else {
            format!("A name cannot contain {c}")
        });
    }
    if n.ends_with('.') {
        return Err("A name cannot end with a dot.".into());
    }
    if n.encode_utf16().count() > 255 {
        return Err("That name is too long.".into());
    }
    Ok(())
}

/// Renames a file or folder in place; returns the new full path. Refuses to
/// overwrite another file.
pub fn rename(path: &str, new_name: &str) -> Result<String, String> {
    check_new_name(new_name)?;
    let old = Path::new(path);
    let parent = old
        .parent()
        .ok_or_else(|| "A drive cannot be renamed here.".to_string())?;
    let new = parent.join(new_name);
    let same_file = old
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase() == new_name.to_lowercase())
        .unwrap_or(false);
    if new.exists() && !same_file {
        return Err(format!("\"{new_name}\" already exists in this folder."));
    }
    std::fs::rename(old, &new).map_err(|e| format!("Could not rename: {e}"))?;
    Ok(new.to_string_lossy().into_owned())
}

// --------------------------------------------------------------------------------------
// Icons and type names
// --------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct IconImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn file_info(
    path_or_ext: &str,
    is_dir: bool,
    use_attributes: bool,
    flags: windows::Win32::UI::Shell::SHGFI_FLAGS,
) -> Option<SHFILEINFOW> {
    ensure_com();
    let w = wide(path_or_ext);
    let mut info = SHFILEINFOW::default();
    let attrs: FILE_FLAGS_AND_ATTRIBUTES = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let flags = if use_attributes {
        flags | SHGFI_USEFILEATTRIBUTES
    } else {
        flags
    };
    // SAFETY: valid string and out-structure.
    let r = unsafe {
        SHGetFileInfoW(
            PCWSTR(w.as_ptr()),
            attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        )
    };
    (r != 0).then_some(info)
}

fn icon_from_info(info: SHFILEINFOW) -> Option<IconImage> {
    if info.hIcon.is_invalid() {
        return None;
    }
    // SAFETY: the icon handle came from SHGetFileInfo and is destroyed here.
    unsafe {
        let img = icon_to_rgba(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        img
    }
}

/// The icon Windows shows for a kind of file, found from the extension alone
/// (no disk access; safe for files that no longer exist). `ext` is without
/// the dot; folders ignore it. `large` = 32 px instead of 16 px.
pub fn icon_for_type(ext: &str, is_dir: bool, large: bool) -> Option<IconImage> {
    let size = if large {
        SHGFI_LARGEICON
    } else {
        SHGFI_SMALLICON
    };
    let name = if is_dir {
        "folder".to_string()
    } else if ext.is_empty() {
        "file".to_string()
    } else {
        format!("file.{ext}")
    };
    icon_from_info(file_info(&name, is_dir, true, SHGFI_ICON | size)?)
}

/// The icon of one particular file (programs, shortcuts and icon files have
/// their own). Touches the disk: call from a background thread.
pub fn icon_for_path(path: &str, large: bool) -> Option<IconImage> {
    let size = if large {
        SHGFI_LARGEICON
    } else {
        SHGFI_SMALLICON
    };
    icon_from_info(file_info(path, false, false, SHGFI_ICON | size)?)
}

/// Extensions whose icon differs from file to file.
pub fn has_own_icon(ext: &str) -> bool {
    matches!(
        ext,
        "exe" | "lnk" | "ico" | "cur" | "ani" | "scr" | "url" | "msc" | "appref-ms"
    )
}

/// The type name Explorer shows ("Text Document", "File folder").
pub fn type_name(ext: &str, is_dir: bool) -> String {
    let name = if is_dir {
        "folder".to_string()
    } else if ext.is_empty() {
        "file".to_string()
    } else {
        format!("file.{ext}")
    };
    let found = file_info(&name, is_dir, true, SHGFI_TYPENAME)
        .map(|i| from_wide(&i.szTypeName))
        .unwrap_or_default();
    if !found.is_empty() {
        found
    } else if is_dir {
        "File folder".into()
    } else if ext.is_empty() {
        "File".into()
    } else {
        format!("{} File", ext.to_uppercase())
    }
}

unsafe fn bitmap_bits(dc: HDC, bmp: HBITMAP, w: i32, h: i32) -> Option<Vec<u8>> {
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(
        dc,
        bmp,
        0,
        h as u32,
        Some(buf.as_mut_ptr() as *mut c_void),
        &mut bi,
        DIB_RGB_COLORS,
    );
    (lines != 0).then_some(buf)
}

unsafe fn icon_to_rgba(icon: HICON) -> Option<IconImage> {
    let mut info = ICONINFO::default();
    GetIconInfo(icon, &mut info).ok()?;
    let dc = GetDC(HWND::default());
    let mut result = None;
    if !info.hbmColor.is_invalid() {
        // Find the bitmap size by asking for its header.
        let mut probe = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if GetDIBits(dc, info.hbmColor, 0, 0, None, &mut probe, DIB_RGB_COLORS) != 0 {
            let (w, h) = (probe.bmiHeader.biWidth, probe.bmiHeader.biHeight.abs());
            if w > 0 && h > 0 && w <= 512 && h <= 512 {
                if let Some(mut px) = bitmap_bits(dc, info.hbmColor, w, h) {
                    // BGRA -> RGBA
                    for p in px.as_chunks_mut::<4>().0 {
                        p.swap(0, 2);
                    }
                    // Old icons have no alpha channel: take transparency from the mask.
                    if px.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
                        let mask = if info.hbmMask.is_invalid() {
                            None
                        } else {
                            bitmap_bits(dc, info.hbmMask, w, h)
                        };
                        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                            let transparent = mask.as_ref().map(|m| m[i * 4] != 0).unwrap_or(false);
                            p[3] = if transparent { 0 } else { 255 };
                        }
                    }
                    result = Some(IconImage {
                        width: w as u32,
                        height: h as u32,
                        rgba: px,
                    });
                }
            }
        }
    }
    ReleaseDC(HWND::default(), dc);
    if !info.hbmColor.is_invalid() {
        let _ = DeleteObject(info.hbmColor);
    }
    if !info.hbmMask.is_invalid() {
        let _ = DeleteObject(info.hbmMask);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("findforge-shell-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn new_names_are_checked() {
        assert!(check_new_name("report.txt").is_ok());
        assert!(check_new_name("naïve file (2).tar.gz").is_ok());
        for bad in [
            "",
            "  ",
            "a/b",
            "a\\b",
            "a:b",
            "what?",
            "st*r",
            "\"q\"",
            "a<b",
            "a|b",
            "dot.",
            " lead",
            "trail ",
            "tab\tname",
        ] {
            assert!(check_new_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_new_name(&"x".repeat(256)).is_err());
    }

    #[test]
    fn rename_in_a_temp_folder() {
        let d = temp_dir("rename");
        let a = d.join("a.txt");
        std::fs::write(&a, "one").unwrap();
        std::fs::write(d.join("b.txt"), "two").unwrap();
        let a = a.to_string_lossy().into_owned();
        // Refuses to overwrite and refuses bad names.
        assert!(rename(&a, "b.txt").is_err());
        assert!(rename(&a, "x/y").is_err());
        assert_eq!(std::fs::read_to_string(d.join("b.txt")).unwrap(), "two");
        let new = rename(&a, "c.txt").unwrap();
        assert!(new.ends_with("c.txt"));
        assert!(!Path::new(&a).exists());
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "one");
        // Changing only the case is allowed.
        let upper = rename(&new, "C.TXT").unwrap();
        assert!(upper.ends_with("C.TXT"));
        assert!(rename(&a, "gone.txt").is_err(), "the source must exist");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    #[ignore = "leaves a small temp file in the Recycle Bin; run with --ignored"]
    fn recycle_in_a_temp_folder() {
        let d = temp_dir("recycle");
        let f = d.join("to-bin.txt");
        std::fs::write(&f, "bye").unwrap();
        assert!(recycle(&["relative.txt".to_string()]).is_err());
        recycle(&[f.to_string_lossy().into_owned()]).unwrap();
        assert!(!f.exists());
        assert!(recycle(&[]).is_ok());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn icons_and_type_names() {
        let folder = icon_for_type("", true, false).expect("folder icon");
        assert_eq!(folder.rgba.len() as u32, folder.width * folder.height * 4);
        assert!(folder.rgba.as_chunks::<4>().0.iter().any(|p| p[3] > 0));
        let txt = icon_for_type("txt", false, true).expect("text icon");
        assert!(txt.width >= 16);
        assert!(!type_name("txt", false).is_empty());
        assert!(!type_name("", true).is_empty());
        assert!(type_name("zzqqxx", false).to_lowercase().contains("zzqqxx"));
        assert!(has_own_icon("exe") && !has_own_icon("txt"));
    }

    #[test]
    fn missing_files_are_reported() {
        assert!(open("C:\\findforge-missing\\nothing.txt").is_err());
        assert!(reveal("C:\\findforge-missing\\nothing.txt").is_err());
        assert!(context_menu(0, &[]).is_ok());
    }
}
