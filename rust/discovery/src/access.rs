//! Metadata-only admission. Reading a placeholder must never hydrate it.

use std::{fs, io, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryAccess {
    Local,
    LocalCloud,
    CloudUnavailable,
    Link,
    UnknownReparse,
}

#[cfg(any(windows, test))]
fn classify(attributes: u32, tag: u32) -> EntryAccess {
    const REPARSE: u32 = 0x400;
    const NEEDS_RECALL: u32 = 0x1000 | 0x40000 | 0x400000;
    if attributes & NEEDS_RECALL != 0 {
        return EntryAccess::CloudUnavailable;
    }
    if attributes & REPARSE == 0 {
        return EntryAccess::Local;
    }
    match tag {
        0xA000000C | 0xA0000003 => EntryAccess::Link,
        value if value & !0x0000F000 == 0x9000001A => EntryAccess::LocalCloud,
        _ => EntryAccess::UnknownReparse,
    }
}

#[cfg(windows)]
pub(crate) fn inspect(path: &Path) -> io::Result<EntryAccess> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW};
    // Exact-name enumeration returns both recall flags and the reparse tag,
    // without opening a data stream or traversing the directory.
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if handle == INVALID_HANDLE_VALUE {
        // Drive roots cannot be passed to FindFirstFile. They are ordinary local
        // roots; other errors retain their actual failure.
        if path.parent().is_none() && path.is_absolute() {
            let metadata = fs::symlink_metadata(path)?;
            use std::os::windows::fs::MetadataExt;
            return Ok(classify(metadata.file_attributes(), 0));
        }
        return Err(io::Error::last_os_error());
    }
    unsafe { FindClose(handle) };
    Ok(classify(data.dwFileAttributes, data.dwReserved0))
}

#[cfg(not(windows))]
pub(crate) fn inspect(path: &Path) -> io::Result<EntryAccess> {
    Ok(if fs::symlink_metadata(path)?.file_type().is_symlink() {
        EntryAccess::Link
    } else {
        EntryAccess::Local
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_downloaded_cloud_files_from_links_and_placeholders() {
        assert_eq!(classify(0x20, 0), EntryAccess::Local);
        for variant in 0..=15 {
            let tag = 0x9000001A | (variant << 12);
            assert_eq!(classify(0x80430, tag), EntryAccess::LocalCloud);
            for recall in [0x1000, 0x40000, 0x400000] {
                assert_eq!(classify(0x400 | recall, tag), EntryAccess::CloudUnavailable);
            }
        }
        assert_eq!(classify(0x400, 0xA000000C), EntryAccess::Link);
        assert_eq!(classify(0x400, 0xA0000003), EntryAccess::Link);
        assert_eq!(classify(0x400, 0x81234567), EntryAccess::UnknownReparse);
    }
}
