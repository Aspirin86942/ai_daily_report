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

/// Reuse the directory enumeration. Only a Windows reparse point needs an
/// exact-name lookup to distinguish a local cloud entry from a link.
pub(crate) fn inspect_entry(entry: &fs::DirEntry) -> io::Result<(EntryAccess, fs::Metadata)> {
    let metadata = entry.metadata()?;
    #[cfg(windows)]
    let access = {
        use std::os::windows::fs::MetadataExt;
        match classify(metadata.file_attributes(), 0) {
            EntryAccess::UnknownReparse => inspect(&entry.path())?,
            ordinary => ordinary,
        }
    };
    #[cfg(not(windows))]
    let access = if metadata.file_type().is_symlink() {
        EntryAccess::Link
    } else {
        EntryAccess::Local
    };
    Ok((access, metadata))
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
pub(super) fn current_write_time(path: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{FARPROC, HMODULE};

    // FILE_STAT_INFORMATION as documented for FileStatByNameInfo.
    #[repr(C)]
    #[derive(Default)]
    struct FileStat {
        file_id: i64,
        created: i64,
        accessed: i64,
        written: i64,
        changed: i64,
        allocation_size: i64,
        end_of_file: i64,
        attributes: u32,
        reparse_tag: u32,
        links: u32,
        effective_access: u32,
    }
    type Query = unsafe extern "system" fn(*const u16, i32, *mut c_void, u32) -> i32;
    static QUERY: OnceLock<Option<Query>> = OnceLock::new();
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> HMODULE;
        fn GetProcAddress(module: HMODULE, name: *const u8) -> FARPROC;
    }
    let query = QUERY
        .get_or_init(|| {
            // Kernel32 remains loaded for the lifetime of this process.
            unsafe {
                let handle = GetModuleHandleW(windows_sys::w!("kernel32.dll"));
                if handle.is_null() {
                    return None;
                }
                let address = GetProcAddress(handle, c"GetFileInformationByName".as_ptr().cast())?;
                Some(std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    Query,
                >(address))
            }
        })
        .as_ref()?;
    let path = wide_path(path);
    let mut stat = FileStat::default();
    // Both arguments point to initialized buffers for the duration of the call.
    let ok = unsafe {
        query(
            path.as_ptr(),
            0,
            (&mut stat as *mut FileStat).cast(),
            size_of::<FileStat>() as u32,
        )
    };
    if ok == 0
        || classify(stat.attributes, stat.reparse_tag) != EntryAccess::Local
        || stat.attributes & 0x10 != 0
        || stat.links == 0
    {
        return None;
    }
    const WINDOWS_EPOCH_OFFSET: i64 = 116_444_736_000_000_000;
    let ticks = stat.written.checked_sub(WINDOWS_EPOCH_OFFSET)?;
    if ticks < 0 {
        return None;
    }
    chrono::DateTime::from_timestamp(ticks / 10_000_000, (ticks % 10_000_000) as u32 * 100)
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let path = path.as_os_str();
    let mut wide = Vec::with_capacity(path.len() + 1);
    wide.extend(path.encode_wide());
    wide.push(0);
    wide
}

#[cfg(windows)]
pub(super) fn fresh_metadata(path: &Path) -> io::Result<fs::Metadata> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    // Keep access metadata-only, without a directory-cache fallback.
    fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?
        .metadata()
}

#[cfg(windows)]
pub(crate) fn inspect(path: &Path) -> io::Result<EntryAccess> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW};
    // FindFirstFile does not accept drive roots. Inspect them in one call.
    if path.parent().is_none() && path.is_absolute() {
        use std::os::windows::fs::MetadataExt;
        return Ok(classify(fs::symlink_metadata(path)?.file_attributes(), 0));
    }
    // Exact-name enumeration returns both recall flags and the reparse tag,
    // without opening a data stream or traversing the directory.
    let wide = wide_path(path);
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if handle == INVALID_HANDLE_VALUE {
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
