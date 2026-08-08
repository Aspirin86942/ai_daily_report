//! SourceGuardV2: engine-owned content identity for cache and snapshot.
//!
//! The legacy worker v1 wire keeps `source_version = mtime_ns:size` frozen, but
//! that guess collides on same-size, timestamp-preserving replacements. The v2
//! guard instead binds every cache/snapshot identity to filesystem identity
//! (file id + change time on Windows, inode + ctime on Unix) or, when the
//! metadata identity cannot be formed, to a full-content SHA-256. It never uses
//! a name heuristic or first/last sampling to fake a complete guard.
//!
//! Guard unavailable is a first-class result (`kind=unavailable,
//! guard_sha256=null`); callers fail closed with a retryable
//! `SOURCE_GUARD_UNAVAILABLE` file error and must not start cache/classifier/
//! parser work for that file.

pub use ai_daily_scanner_contract::SourceGuardKind;
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;

/// Domain separator for the canonical guard hash. Everything hashed after this
/// separator is a fixed field order of the identity.
const DOMAIN_SEPARATOR: &[u8] = b"source-guard-v2\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGuardV2 {
    pub kind: SourceGuardKind,
    pub guard_sha256: Option<String>,
}

impl SourceGuardV2 {
    /// Enforces the guard invariant: `unavailable` must have a null hash;
    /// every other kind must carry a 64-char lowercase hex SHA-256.
    pub fn validate(&self) -> Result<(), String> {
        match self.kind {
            SourceGuardKind::Unavailable => {
                if self.guard_sha256.is_some() {
                    return Err("unavailable source guard must have a null hash".to_string());
                }
            }
            _ => {
                let hash = self
                    .guard_sha256
                    .as_deref()
                    .ok_or_else(|| "source guard kind requires a sha256".to_string())?;
                if !is_sha256_hex(hash) {
                    return Err("source guard hash must be lowercase hex sha256".to_string());
                }
            }
        }
        Ok(())
    }
}

/// Computes the current content identity of the file at `path`.
///
/// The metadata identity is preferred. Any missing field/API or platform
/// unsupported/invalid-zero sentinel makes the metadata identity unavailable
/// and the guard falls back to the complete streamed SHA-256 of the source
/// bytes. If neither forms, the result is `Unavailable` with a null hash.
pub fn compute_source_guard(path: &Path) -> io::Result<SourceGuardV2> {
    #[cfg(windows)]
    {
        if let Some(hash) = windows_identity(path)? {
            let guard = SourceGuardV2 {
                kind: SourceGuardKind::WindowsFileIdChangeTimeV1,
                guard_sha256: Some(hash),
            };
            guard
                .validate()
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
            return Ok(guard);
        }
    }
    #[cfg(unix)]
    {
        if let Some(hash) = unix_identity(path)? {
            let guard = SourceGuardV2 {
                kind: SourceGuardKind::UnixInodeCtimeV1,
                guard_sha256: Some(hash),
            };
            guard
                .validate()
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
            return Ok(guard);
        }
    }
    // metadata guard 无法形成 → 完整流式 SHA-256（不以首尾采样冒充）
    match full_content_sha256(path) {
        Some(hash) => {
            let guard = SourceGuardV2 {
                kind: SourceGuardKind::ContentSha256V1,
                guard_sha256: Some(hash),
            };
            guard
                .validate()
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
            Ok(guard)
        }
        None => Ok(SourceGuardV2 {
            kind: SourceGuardKind::Unavailable,
            guard_sha256: None,
        }),
    }
}

/// Recomputes the guard for `path` and compares it against `expected`.
///
/// Returns false on any mismatch (including a file that became unavailable or
/// changed identity kind). Callers discard the just-obtained cache value or
/// worker result and treat the file as `SOURCE_VERSION_CHANGED`, even when the
/// legacy `source_version` text is unchanged.
pub fn verify_guard(path: &Path, expected: &SourceGuardV2) -> bool {
    match compute_source_guard(path) {
        Ok(actual) => actual == *expected,
        Err(_) => false,
    }
}

/// Streams the ENTIRE source bytes through SHA-256. Never samples only the head
/// and tail; the fallback is a complete content hash.
pub fn full_content_sha256(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut hasher = Sha256::new();
    let file = std::fs::File::open(path).ok()?;
    let mut reader = io::BufReader::new(file);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hex(&hasher.finalize()))
}

/// Stable wire text for a guard kind, matching the `file_inventory` CHECK and
/// the classification_cache CHECK literals exactly.
pub fn source_guard_kind_text(kind: SourceGuardKind) -> &'static str {
    match kind {
        SourceGuardKind::WindowsFileIdChangeTimeV1 => "windows_file_id_change_time_v1",
        SourceGuardKind::UnixInodeCtimeV1 => "unix_inode_ctime_v1",
        SourceGuardKind::ContentSha256V1 => "content_sha256_v1",
        SourceGuardKind::Unavailable => "unavailable",
    }
}

/// Parses a guard-kind wire literal. Returns None for unknown text so callers
/// fail closed rather than guessing.
pub fn source_guard_kind_from_text(value: &str) -> Option<SourceGuardKind> {
    match value {
        "windows_file_id_change_time_v1" => Some(SourceGuardKind::WindowsFileIdChangeTimeV1),
        "unix_inode_ctime_v1" => Some(SourceGuardKind::UnixInodeCtimeV1),
        "content_sha256_v1" => Some(SourceGuardKind::ContentSha256V1),
        "unavailable" => Some(SourceGuardKind::Unavailable),
        _ => None,
    }
}

/// Windows identity from ONE opened handle: canonical volume serial,
/// 128-bit file id (FILE_ID_INFO), size, last-write time and change time, in
/// that fixed field order under the domain separator. Any missing API or
/// platform invalid-zero sentinel returns Ok(None) so the caller falls back to
/// the full-content hash.
#[cfg(windows)]
#[allow(non_snake_case)]
fn windows_identity(path: &Path) -> io::Result<Option<String>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileBasicInfo, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_ID_INFO,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // No handle at all: metadata identity cannot be formed.
        return Ok(None);
    }
    let result = (|| {
        let mut by_handle: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(handle, &mut by_handle) } == 0 {
            return Ok(None);
        }
        let volume_serial = by_handle.dwVolumeSerialNumber;
        let size = ((by_handle.nFileSizeHigh as u64) << 32) | by_handle.nFileSizeLow as u64;
        if volume_serial == 0 {
            return Ok(None);
        }

        let mut file_id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
        let id_ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                &mut file_id as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        if id_ok == 0 {
            return Ok(None);
        }
        let identifier = file_id.FileId.Identifier;
        if identifier == [0u8; 16] {
            return Ok(None);
        }

        let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        let basic_ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                &mut basic as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        if basic_ok == 0 {
            return Ok(None);
        }
        let last_write_time = basic.LastWriteTime as u64;
        let change_time = basic.ChangeTime as u64;
        if last_write_time == 0 || change_time == 0 {
            return Ok(None);
        }

        let mut hasher = Sha256::new();
        hasher.update(DOMAIN_SEPARATOR);
        hasher.update(volume_serial.to_le_bytes());
        hasher.update(identifier);
        hasher.update(size.to_le_bytes());
        hasher.update(last_write_time.to_le_bytes());
        hasher.update(change_time.to_le_bytes());
        Ok(Some(hex(&hasher.finalize())))
    })();
    unsafe { CloseHandle(handle) };
    result
}

/// Unix identity: device/inode/size/mtime_ns/ctime_ns under the domain
/// separator. Any invalid-zero sentinel returns Ok(None) so the caller falls
/// back to the full-content hash.
#[cfg(unix)]
fn unix_identity(path: &Path) -> io::Result<Option<String>> {
    use std::os::unix::fs::MetadataExt;

    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let device = metadata.dev();
    let inode = metadata.ino();
    let size = metadata.size();
    let mtime_ns = (metadata.mtime() as u128) * 1_000_000_000 + metadata.mtime_nsec() as u128;
    let ctime_ns = (metadata.ctime() as u128) * 1_000_000_000 + metadata.ctime_nsec() as u128;
    if device == 0 || inode == 0 || mtime_ns == 0 || ctime_ns == 0 {
        return Ok(None);
    }

    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_SEPARATOR);
    hasher.update(device.to_le_bytes());
    hasher.update(inode.to_le_bytes());
    hasher.update(size.to_le_bytes());
    hasher.update(mtime_ns.to_le_bytes());
    hasher.update(ctime_ns.to_le_bytes());
    Ok(Some(hex(&hasher.finalize())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
