//! NTFS compression preserves ordinary reads, seek offsets and evidence hashes.
use std::{fs, io, path::Path};

pub fn allocated_bytes(path: &Path) -> io::Result<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::{GetLastError, SetLastError},
            Storage::FileSystem::GetCompressedFileSizeW,
        };
        let wide: Vec<_> = fs::canonicalize(path)?
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut high = 0;
        unsafe {
            SetLastError(0);
            let low = GetCompressedFileSizeW(wide.as_ptr(), &mut high);
            if low == u32::MAX && GetLastError() != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((u64::from(high) << 32) | u64::from(low))
        }
    }
    #[cfg(not(windows))]
    {
        Ok(fs::metadata(path)?.len())
    }
}

pub fn compress(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::{fs::MetadataExt, fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::{
            Storage::FileSystem::{FILE_ATTRIBUTE_COMPRESSED, FILE_FLAG_BACKUP_SEMANTICS},
            System::{Ioctl::FSCTL_SET_COMPRESSION, IO::DeviceIoControl},
        };
        if fs::metadata(path)?.file_attributes() & FILE_ATTRIBUTE_COMPRESSED != 0 {
            return Ok(());
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let format: u16 = 1; // COMPRESSION_FORMAT_DEFAULT (LZNT1).
        let mut returned = 0;
        let success = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_SET_COMPRESSION,
                (&format as *const u16).cast(),
                std::mem::size_of_val(&format) as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "NTFS compression requires Windows",
        ))
    }
}
