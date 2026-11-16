//! Desktop integration: the global hotkey, the tray icon, "start with
//! Windows" and the single-instance handshake. A small hidden window on its
//! own thread receives the messages, so they arrive even while the main
//! window is hidden.

use crate::sys::wide;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut std::ffi::c_void)
}

// --------------------------------------------------------------------------------------
// Window helpers
// --------------------------------------------------------------------------------------

/// Shows the window (if hidden or minimised) and brings it to the front.
pub fn show_and_focus(raw: isize) {
    if raw == 0 {
        return;
    }
    // SAFETY: a window handle of this process; stale handles just fail.
    unsafe {
        if !IsWindowVisible(hwnd(raw)).as_bool() {
            let _ = ShowWindow(hwnd(raw), SW_SHOW);
        }
        if IsIconic(hwnd(raw)).as_bool() {
            let _ = ShowWindow(hwnd(raw), SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd(raw));
    }
}

/// Hides the window completely (it stays reachable from the tray / hotkey).
pub fn hide_window(raw: isize) {
    if raw == 0 {
        return;
    }
    // SAFETY: a window handle of this process.
    unsafe {
        let _ = ShowWindow(hwnd(raw), SW_HIDE);
    }
}

pub fn is_visible(raw: isize) -> bool {
    // SAFETY: a window handle of this process.
    raw != 0 && unsafe { IsWindowVisible(hwnd(raw)).as_bool() }
}

/// Is this window the one the user is working in right now?
pub fn is_foreground(raw: isize) -> bool {
    // SAFETY: no arguments.
    raw != 0 && unsafe { GetForegroundWindow() } == hwnd(raw)
}

// --------------------------------------------------------------------------------------
// Hotkey text
// --------------------------------------------------------------------------------------

/// A parsed hotkey: modifier flags and a virtual-key code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub vk: u32,
}

/// Parses text such as `Ctrl+Alt+F`, `Win+Shift+S` or `Ctrl+F9`. At least one
/// of Ctrl / Alt / Win is required so a plain letter can never be captured.
pub fn parse_hotkey(text: &str) -> Option<Hotkey> {
    let mut modifiers = 0u32;
    let mut vk = None;
    for part in text.split('+') {
        let p = part.trim().to_uppercase();
        match p.as_str() {
            "CTRL" | "CONTROL" => modifiers |= MOD_CONTROL.0,
            "ALT" => modifiers |= MOD_ALT.0,
            "SHIFT" => modifiers |= MOD_SHIFT.0,
            "WIN" | "WINDOWS" => modifiers |= MOD_WIN.0,
            "SPACE" if vk.is_none() => vk = Some(0x20),
            key => {
                if vk.is_some() {
                    return None;
                }
                let bytes = key.as_bytes();
                vk = Some(if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
                    u32::from(bytes[0])
                } else {
                    let n: u32 = key.strip_prefix('F')?.parse().ok()?;
                    if !(1..=24).contains(&n) {
                        return None;
                    }
                    0x70 + n - 1
                });
            }
        }
    }
    if modifiers & (MOD_CONTROL.0 | MOD_ALT.0 | MOD_WIN.0) == 0 {
        return None;
    }
    Some(Hotkey { modifiers, vk: vk? })
}

// --------------------------------------------------------------------------------------
// Background message window
// --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopEvent {
    /// The global hotkey was pressed (the window has already been shown).
    Hotkey,
    /// Another copy of the program was started, or the tray icon was clicked
    /// (the window has already been shown).
    Show,
    /// "Exit" was chosen in the tray menu.
    Exit,
    /// The hotkey could not be registered (another program owns it).
    HotkeyUnavailable,
}

const WM_TRAY: u32 = WM_APP + 1;
const WM_SET_HOTKEY: u32 = WM_APP + 2;
const WM_SET_TRAY: u32 = WM_APP + 3;
const WM_QUIT_THREAD: u32 = WM_APP + 4;
const WM_SHOW_MAIN: u32 = WM_APP + 5;
const HOTKEY_ID: i32 = 1;
const MENU_SHOW: usize = 1;
const MENU_EXIT: usize = 2;
const CLASS_NAME: PCWSTR = w!("FindForgeDesktopWindow");

struct State {
    tx: Sender<DesktopEvent>,
    wake: Box<dyn Fn() + Send>,
    main_window: isize,
    tray: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn emit(event: DesktopEvent) {
    if let Ok(guard) = STATE.lock() {
        if let Some(s) = guard.as_ref() {
            let _ = s.tx.send(event);
            (s.wake)();
        }
    }
}

unsafe fn tray_data(window: HWND) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    let module = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
    // Icon resource 1 is the application icon embedded by the build script.
    data.hIcon = LoadIconW(HINSTANCE(module.0), PCWSTR(std::ptr::without_provenance(1)))
        .or_else(|_| LoadIconW(HINSTANCE::default(), IDI_APPLICATION))
        .unwrap_or_default();
    for (i, c) in "FindForge".encode_utf16().enumerate() {
        data.szTip[i] = c;
    }
    data
}

unsafe fn set_tray(window: HWND, on: bool) {
    let was = STATE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.tray))
        .unwrap_or(false);
    if on == was {
        return;
    }
    let data = tray_data(window);
    let _ = Shell_NotifyIconW(if on { NIM_ADD } else { NIM_DELETE }, &data);
    if let Ok(mut g) = STATE.lock() {
        if let Some(s) = g.as_mut() {
            s.tray = on;
        }
    }
}

unsafe fn tray_menu(window: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("Show FindForge"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("Exit"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Needed so the menu closes when the user clicks elsewhere.
    let _ = SetForegroundWindow(window);
    let choice = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        window,
        None,
    );
    let _ = DestroyMenu(menu);
    match choice.0 as usize {
        MENU_SHOW => show_main(DesktopEvent::Show),
        MENU_EXIT => {
            // The main window must be awake to process the exit.
            let main = main_window();
            if main != 0 {
                let _ = ShowWindow(hwnd(main), SW_SHOWNOACTIVATE);
            }
            emit(DesktopEvent::Exit);
        }
        _ => {}
    }
}

fn main_window() -> isize {
    STATE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.main_window))
        .unwrap_or(0)
}

fn show_main(event: DesktopEvent) {
    show_and_focus(main_window());
    emit(event);
}

unsafe extern "system" fn wnd_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            let main = main_window();
            if is_visible(main) && is_foreground(main) {
                // Pressing the hotkey again puts the window away.
                if STATE
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|s| s.tray))
                    .unwrap_or(false)
                {
                    hide_window(main);
                } else {
                    let _ = ShowWindow(hwnd(main), SW_MINIMIZE);
                }
            } else {
                show_main(DesktopEvent::Hotkey);
            }
            LRESULT(0)
        }
        WM_SHOW_MAIN => {
            show_main(DesktopEvent::Show);
            LRESULT(0)
        }
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_main(DesktopEvent::Show),
                WM_RBUTTONUP | WM_CONTEXTMENU => tray_menu(window),
                _ => {}
            }
            LRESULT(0)
        }
        WM_SET_HOTKEY => {
            let _ = UnregisterHotKey(window, HOTKEY_ID);
            if lparam.0 != 0
                && RegisterHotKey(
                    window,
                    HOTKEY_ID,
                    HOT_KEY_MODIFIERS(wparam.0 as u32) | MOD_NOREPEAT,
                    lparam.0 as u32,
                )
                .is_err()
            {
                emit(DesktopEvent::HotkeyUnavailable);
            }
            LRESULT(0)
        }
        WM_SET_TRAY => {
            set_tray(window, wparam.0 != 0);
            LRESULT(0)
        }
        WM_QUIT_THREAD => {
            let _ = UnregisterHotKey(window, HOTKEY_ID);
            set_tray(window, false);
            let _ = DestroyWindow(window);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, msg, wparam, lparam),
    }
}

/// Handle to the background message window. Dropping it removes the hotkey
/// and the tray icon.
pub struct Desktop {
    window: isize,
}

impl Desktop {
    /// Starts the background thread. `wake` is called after every event so
    /// the user interface runs and picks it up.
    pub fn start(
        main_window: isize,
        tx: Sender<DesktopEvent>,
        wake: Box<dyn Fn() + Send>,
    ) -> Option<Desktop> {
        if let Ok(mut g) = STATE.lock() {
            *g = Some(State {
                tx,
                wake,
                main_window,
                tray: false,
            });
        }
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<isize>();
        std::thread::Builder::new()
            .name("findforge-desktop".into())
            .spawn(move || {
                // SAFETY: a standard window class, window and message loop,
                // all owned by this thread.
                unsafe {
                    let module = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
                    let class = WNDCLASSW {
                        lpfnWndProc: Some(wnd_proc),
                        hInstance: HINSTANCE(module.0),
                        lpszClassName: CLASS_NAME,
                        ..Default::default()
                    };
                    RegisterClassW(&class);
                    let window = CreateWindowExW(
                        WS_EX_TOOLWINDOW,
                        CLASS_NAME,
                        w!("FindForge"),
                        WS_POPUP,
                        0,
                        0,
                        0,
                        0,
                        HWND::default(),
                        HMENU::default(),
                        HINSTANCE(module.0),
                        None,
                    );
                    let Ok(window) = window else {
                        let _ = ready_tx.send(0);
                        return;
                    };
                    let _ = ready_tx.send(window.0 as isize);
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            })
            .ok()?;
        let window = ready_rx.recv().ok().filter(|w| *w != 0)?;
        Some(Desktop { window })
    }

    fn post(&self, msg: u32, wparam: usize, lparam: isize) {
        // SAFETY: posting to our own window; a stale handle just fails.
        unsafe {
            let _ = PostMessageW(hwnd(self.window), msg, WPARAM(wparam), LPARAM(lparam));
        }
    }

    /// Registers (or with `None`, removes) the global hotkey. If it is taken,
    /// [`DesktopEvent::HotkeyUnavailable`] arrives shortly after.
    pub fn set_hotkey(&self, hotkey: Option<Hotkey>) {
        match hotkey {
            Some(h) => self.post(WM_SET_HOTKEY, h.modifiers as usize, h.vk as isize),
            None => self.post(WM_SET_HOTKEY, 0, 0),
        }
    }

    pub fn set_tray(&self, on: bool) {
        self.post(WM_SET_TRAY, usize::from(on), 0);
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        self.post(WM_QUIT_THREAD, 0, 0);
    }
}

// --------------------------------------------------------------------------------------
// Single instance
// --------------------------------------------------------------------------------------

/// Exists for as long as this is the running copy of FindForge.
pub struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Tries to become the running copy without disturbing another one.
fn try_claim() -> Option<InstanceGuard> {
    // SAFETY: plain Win32 calls with valid strings.
    unsafe {
        let handle = CreateMutexW(None, false, w!("FindForge.Running")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return None;
        }
        Some(InstanceGuard(handle))
    }
}

/// Claims the "running copy" role. Returns `None` when another copy already
/// has it; that copy is asked to show its window.
///
/// With `wait`, the claim is retried for that long first: a copy that was
/// just told to restart with other rights needs a moment to exit.
pub fn single_instance(wait: Option<std::time::Duration>) -> Option<InstanceGuard> {
    let started = std::time::Instant::now();
    loop {
        if let Some(guard) = try_claim() {
            return Some(guard);
        }
        match wait {
            Some(limit) if started.elapsed() < limit => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            _ => break,
        }
    }
    // SAFETY: plain Win32 calls with valid strings.
    unsafe {
        if let Ok(other) = FindWindowW(CLASS_NAME, PCWSTR::null()) {
            if !other.is_invalid() {
                let _ = PostMessageW(other, WM_SHOW_MAIN, WPARAM(0), LPARAM(0));
            }
        }
    }
    None
}

// --------------------------------------------------------------------------------------
// Start with Windows
// --------------------------------------------------------------------------------------

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "FindForge";

/// The command stored in the Run key for this executable.
fn autostart_command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" --tray", exe.display()))
}

/// Is FindForge set to start when the user signs in?
pub fn autostart_enabled() -> bool {
    let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
    // SAFETY: valid strings; the key is closed.
    unsafe {
        let mut h = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            0,
            KEY_QUERY_VALUE,
            &mut h,
        )
        .is_err()
        {
            return false;
        }
        let found = RegQueryValueExW(h, PCWSTR(value.as_ptr()), None, None, None, None).is_ok();
        let _ = RegCloseKey(h);
        found
    }
}

/// Adds or removes the per-user "start with Windows" entry.
pub fn set_autostart(on: bool) -> Result<(), String> {
    let (key, value) = (wide(RUN_KEY), wide(RUN_VALUE));
    // SAFETY: valid strings and buffers; the key is closed.
    unsafe {
        let mut h = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut h,
            None,
        )
        .ok()
        .map_err(|e| format!("Could not open the startup list: {e}"))?;
        let result = if on {
            let cmd = autostart_command().ok_or("Could not find the program path.")?;
            let data = wide(&cmd);
            let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
            RegSetValueExW(h, PCWSTR(value.as_ptr()), 0, REG_SZ, Some(bytes))
                .ok()
                .map_err(|e| format!("Could not add the startup entry: {e}"))
        } else {
            let r = RegDeleteValueW(h, PCWSTR(value.as_ptr()));
            // Already absent is fine.
            if r.is_ok() || r.0 == 2 {
                Ok(())
            } else {
                Err(format!(
                    "Could not remove the startup entry (code {}).",
                    r.0
                ))
            }
        };
        let _ = RegCloseKey(h);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_copy_claims_the_role() {
        // The test process itself plays "the running copy".
        let first = try_claim();
        if first.is_some() {
            assert!(try_claim().is_none(), "a second claim must fail");
            assert!(single_instance(Some(std::time::Duration::from_millis(250))).is_none());
            drop(first);
            let again = try_claim();
            assert!(
                again.is_some(),
                "the role is free again after the guard is dropped"
            );
        }
        // (If a real FindForge is running on this PC the role is taken:
        // nothing to check then, and nothing is disturbed.)
    }

    #[test]
    fn hotkeys() {
        assert_eq!(
            parse_hotkey("Ctrl+Alt+F"),
            Some(Hotkey {
                modifiers: MOD_CONTROL.0 | MOD_ALT.0,
                vk: b'F' as u32
            })
        );
        assert_eq!(
            parse_hotkey(" win + shift + f9 "),
            Some(Hotkey {
                modifiers: MOD_WIN.0 | MOD_SHIFT.0,
                vk: 0x78
            })
        );
        assert_eq!(parse_hotkey("Alt+Space").map(|h| h.vk), Some(0x20));
        assert_eq!(parse_hotkey("F"), None, "a modifier is required");
        assert_eq!(parse_hotkey("Shift+F"), None, "Shift alone is not enough");
        assert_eq!(parse_hotkey("Ctrl+"), None);
        assert_eq!(parse_hotkey("Ctrl+A+B"), None);
        assert_eq!(parse_hotkey("Ctrl+F25"), None);
        assert_eq!(parse_hotkey(""), None);
    }
}
