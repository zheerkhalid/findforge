//! Thin wrappers over the basic Win32 calls FindForge needs.

use std::ffi::c_void;
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// UTF-16, NUL-terminated.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Is this process running with administrator rights?
pub fn is_elevated() -> bool {
    // SAFETY: plain token query on our own process; the handle is closed.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// `ShellExecute` with a verb (`open`, `runas`, `properties`...).
pub fn shell_execute(verb: &str, file: &str, params: &str) -> Result<(), String> {
    let (v, f, p) = (wide(verb), wide(file), wide(params));
    // SAFETY: all strings are valid NUL-terminated UTF-16 that outlive the call.
    let r = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(v.as_ptr()),
            PCWSTR(f.as_ptr()),
            if params.is_empty() {
                PCWSTR::null()
            } else {
                PCWSTR(p.as_ptr())
            },
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if r.0 as usize > 32 {
        Ok(())
    } else {
        Err(format!(
            "Windows could not open \"{file}\" (code {}).",
            r.0 as usize
        ))
    }
}

/// Starts a new elevated copy of FindForge with the same arguments it would
/// need to continue (`params`). The caller should then close this one.
pub fn restart_as_admin(params: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    shell_execute("runas", &exe.to_string_lossy(), params)
}

/// Seconds to add to UTC to get local time, right now.
pub fn local_utc_offset() -> i32 {
    let mut tz = TIME_ZONE_INFORMATION::default();
    // SAFETY: plain out-parameter call.
    let id = unsafe { GetTimeZoneInformation(&mut tz) };
    let mut bias = tz.Bias;
    match id {
        1 => bias += tz.StandardBias,
        2 => bias += tz.DaylightBias,
        _ => {}
    }
    -bias * 60
}

/// Unix seconds, now.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `%LOCALAPPDATA%\FindForge`: settings and the saved index live here.
///
/// The `FINDFORGE_DATA` environment variable overrides the location (used
/// for testing, and for keeping a portable copy's data next to it).
pub fn app_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FINDFORGE_DATA").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("FindForge")
}

/// Working set of this process in bytes (0 if unknown).
pub fn process_memory() -> u64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    let mut c = PROCESS_MEMORY_COUNTERS::default();
    // SAFETY: plain out-parameter call on our own process.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut c,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if ok.is_ok() {
        c.WorkingSetSize as u64
    } else {
        0
    }
}

/// Makes `println!` work when a windowed program is started from a console.
pub fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: attaching to the parent's console is harmless if there is none.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_round_trip() {
        assert_eq!(from_wide(&wide("héllo")), "héllo");
        assert_eq!(wide("a"), vec![97, 0]);
    }

    #[test]
    fn offset_is_sane() {
        let o = local_utc_offset();
        assert!((-14 * 3600..=14 * 3600).contains(&o));
        assert!(now_unix() > 1_700_000_000);
    }
}
