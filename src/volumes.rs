//! Lists the drives of this PC.

use crate::sys::{from_wide, wide};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Optical,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    /// `C:`
    pub root: String,
    pub label: String,
    /// `NTFS`, `exFAT`, `FAT32`... Empty when the drive has no medium.
    pub fs: String,
    pub serial: u32,
    pub kind: DriveKind,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

impl VolumeInfo {
    pub fn is_ntfs(&self) -> bool {
        self.fs.eq_ignore_ascii_case("NTFS")
    }
}

/// Label, file system and serial number of the volume that holds `path`
/// (`C:\` or any folder path ending in a backslash that is a volume root).
pub fn volume_information(root_with_backslash: &str) -> Option<(String, String, u32)> {
    let w = wide(root_with_backslash);
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let mut serial = 0u32;
    // SAFETY: buffers are valid for the lengths passed.
    let ok = unsafe {
        GetVolumeInformationW(
            PCWSTR(w.as_ptr()),
            Some(&mut label),
            Some(&mut serial),
            None,
            None,
            Some(&mut fs),
        )
    };
    ok.ok()?;
    Some((from_wide(&label), from_wide(&fs), serial))
}

/// The volume root (`C:\`) of a full path, if it starts with a drive letter.
pub fn drive_root_of(path: &str) -> Option<String> {
    let b = path.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        Some(format!("{}:\\", (b[0] as char).to_ascii_uppercase()))
    } else {
        None
    }
}

/// All drive letters with a mounted volume.
pub fn list() -> Vec<VolumeInfo> {
    // SAFETY: no arguments.
    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let w = wide(&root);
        // SAFETY: valid NUL-terminated string.
        let kind = match unsafe { GetDriveTypeW(PCWSTR(w.as_ptr())) } {
            2 => DriveKind::Removable,
            3 => DriveKind::Fixed,
            4 => DriveKind::Network,
            5 => DriveKind::Optical,
            _ => DriveKind::Other,
        };
        let Some((label, fs, serial)) = volume_information(&root) else {
            continue;
        };
        let (mut total, mut free) = (0u64, 0u64);
        // SAFETY: plain out-parameters.
        unsafe {
            let _ =
                GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), None, Some(&mut total), Some(&mut free));
        }
        out.push(VolumeInfo {
            root: format!("{letter}:"),
            label,
            fs,
            serial,
            kind,
            total_bytes: total,
            free_bytes: free,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_the_system_drive() {
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let vols = list();
        let v = vols
            .iter()
            .find(|v| v.root.eq_ignore_ascii_case(&sys))
            .expect("system drive is listed");
        assert!(!v.fs.is_empty());
        assert!(v.total_bytes > 0);
    }

    #[test]
    fn drive_roots() {
        assert_eq!(drive_root_of("c:\\a\\b").as_deref(), Some("C:\\"));
        assert_eq!(drive_root_of("\\\\server\\share"), None);
        assert_eq!(drive_root_of(""), None);
    }
}
