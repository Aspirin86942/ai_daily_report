use super::sha256_hex;
use daily_parsers::{
    discovery::{build_source_version, local_candidate_is_readable, DiscoveredFileOut},
    hex,
    pdf_wire::file_identity_sha256,
    settings::SourceGuardKind,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, Metadata},
    io::{self, Read},
    path::Path,
    time::{Instant, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SourceGuard {
    pub kind: SourceGuardKind,
    pub sha256: String,
}

pub(super) fn observe(
    file: &DiscoveredFileOut,
    root: &Path,
    deadline: Instant,
) -> io::Result<SourceGuard> {
    let path = Path::new(&file.path);
    check_deadline(deadline)?;
    if !local_candidate_is_readable(path, root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "source is unavailable locally or outside the scan root",
        ));
    }
    let metadata_file = open_metadata(path)?;
    verify_version(&metadata_file.metadata()?, file)?;
    if let Some((kind, sha256)) = file_identity_sha256(&metadata_file) {
        return Ok(SourceGuard { kind, sha256 });
    }
    let stream = File::open(path)?;
    let sha256 = hash_file(stream, file, None, deadline)?;
    Ok(SourceGuard {
        kind: SourceGuardKind::ContentSha256V1,
        sha256,
    })
}

pub(super) fn source_sha256(
    file: &DiscoveredFileOut,
    guard: &SourceGuard,
    root: &Path,
    deadline: Instant,
) -> io::Result<String> {
    if guard.kind == SourceGuardKind::ContentSha256V1 {
        return Ok(guard.sha256.clone());
    }
    check_deadline(deadline)?;
    let path = Path::new(&file.path);
    if !local_candidate_is_readable(path, root) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    hash_file(File::open(path)?, file, Some(guard), deadline)
}

fn hash_file(
    mut stream: File,
    file: &DiscoveredFileOut,
    expected: Option<&SourceGuard>,
    deadline: Instant,
) -> io::Result<String> {
    let before = stream.metadata()?;
    verify_version(&before, file)?;
    let before_identity = file_identity_sha256(&stream);
    if expected.is_some_and(|expected| {
        before_identity
            .as_ref()
            .is_none_or(|(kind, hash)| *kind != expected.kind || hash != &expected.sha256)
    }) {
        return Err(source_changed());
    }
    let mut reader = (&mut stream).take(file.size_bytes.saturating_add(1));
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut total = 0;
    loop {
        check_deadline(deadline)?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        hash.update(&buffer[..count]);
    }
    verify_version(&stream.metadata()?, file)?;
    if total != file.size_bytes || before_identity != file_identity_sha256(&stream) {
        return Err(source_changed());
    }
    Ok(hex(&hash.finalize()))
}

fn verify_version(metadata: &Metadata, file: &DiscoveredFileOut) -> io::Result<()> {
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    let current = build_source_version(modified.as_nanos(), metadata.len());
    if metadata.is_file() && current == file.source_version {
        Ok(())
    } else {
        Err(source_changed())
    }
}

pub(super) fn source_changed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "source changed during the scan")
}

pub(super) fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(())
    }
}

pub(super) fn file_source_id(relative_path: &str) -> String {
    format!("src_{}", sha256_hex(relative_path.as_bytes()))
}

#[cfg(windows)]
fn open_metadata(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_READ_ATTRIBUTES;

    File::options().access_mode(FILE_READ_ATTRIBUTES).open(path)
}

#[cfg(not(windows))]
fn open_metadata(path: &Path) -> io::Result<File> {
    File::open(path)
}
