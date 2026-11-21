use super::{diagnostic, parse_error, PdfError, WorkerDiagnostic};
use pdfium_render::prelude::PdfiumError;
use std::{
    fs::{self, File, Metadata},
    time::UNIX_EPOCH,
};

pub(super) struct OpenSource {
    pub file: File,
    pub stamp: SourceStamp,
}

impl OpenSource {
    pub fn open(path: &str) -> Result<Self, Box<WorkerDiagnostic>> {
        let file = File::open(path).map_err(|error| {
            if fs::metadata(path).is_ok() {
                parse_error(path, &PdfError::Parse(PdfiumError::IoError(error)))
            } else {
                diagnostic(path, "PARSER_START_FAILED", true)
            }
        })?;
        let stamp = SourceStamp::from_file(path, &file)?;
        Ok(Self { file, stamp })
    }
}

#[derive(PartialEq, Eq)]
pub(super) struct SourceStamp {
    pub version: String,
    pub bytes: u64,
    identity: Option<FileIdentity>,
}

impl SourceStamp {
    fn from_file(path: &str, file: &File) -> Result<Self, Box<WorkerDiagnostic>> {
        let metadata = file
            .metadata()
            .map_err(|_| diagnostic(path, "PARSER_START_FAILED", true))?;
        let (version, bytes) = version(path, &metadata)?;
        let identity = metadata
            .is_file()
            .then(|| file_identity(file, &metadata))
            .flatten();
        Ok(Self {
            version,
            bytes,
            identity,
        })
    }

    pub fn reusable(&self) -> bool {
        self.identity.is_some()
    }

    pub fn check(&self, path: &str) -> Result<(), Box<WorkerDiagnostic>> {
        let unchanged = if self.reusable() {
            let file =
                File::open(path).map_err(|_| diagnostic(path, "PARSER_START_FAILED", true))?;
            *self == Self::from_file(path, &file)?
        } else {
            self.version == source(path)?.0
        };
        if unchanged {
            Ok(())
        } else {
            Err(diagnostic(path, "SOURCE_VERSION_CHANGED", false))
        }
    }
}

pub(super) fn source(path: &str) -> Result<(String, u64), Box<WorkerDiagnostic>> {
    let metadata = fs::metadata(path).map_err(|_| diagnostic(path, "PARSER_START_FAILED", true))?;
    version(path, &metadata)
}

fn version(path: &str, metadata: &Metadata) -> Result<(String, u64), Box<WorkerDiagnostic>> {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .ok_or_else(|| diagnostic(path, "SOURCE_VERSION_CHANGED", false))?;
    Ok((
        ai_daily_discovery::build_source_version(modified.as_nanos(), metadata.len()),
        metadata.len(),
    ))
}

#[cfg(windows)]
#[derive(PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    file_id: [u8; 16],
    changed: i64,
}

#[cfg(windows)]
fn file_identity(file: &File, _: &Metadata) -> Option<FileIdentity> {
    use std::{ffi::c_void, mem::size_of_val, os::windows::io::AsRawHandle};

    #[repr(C)]
    #[derive(Default)]
    struct FileIdInfo {
        volume: u64,
        file_id: [u8; 16],
    }

    #[repr(C)]
    #[derive(Default)]
    struct FileBasicInfo {
        created: i64,
        accessed: i64,
        written: i64,
        changed: i64,
        attributes: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut c_void,
            class: i32,
            buffer: *mut c_void,
            bytes: u32,
        ) -> i32;
    }

    let mut id = FileIdInfo::default();
    let mut basic = FileBasicInfo::default();
    const FILE_BASIC_INFO: i32 = 0;
    const FILE_ID_INFO: i32 = 18;
    // The live File owns the handle. Both output buffers have the specified Win32 C layout.
    let known = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FILE_ID_INFO,
            (&raw mut id).cast(),
            size_of_val(&id) as u32,
        ) != 0
            && GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FILE_BASIC_INFO,
                (&raw mut basic).cast(),
                size_of_val(&basic) as u32,
            ) != 0
    };
    (known && id.volume != 0 && id.file_id != [0; 16] && basic.changed > 0).then_some(
        FileIdentity {
            volume: id.volume,
            file_id: id.file_id,
            changed: basic.changed,
        },
    )
}

#[cfg(unix)]
#[derive(PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    changed: (i64, i64),
}

#[cfg(unix)]
fn file_identity(_: &File, metadata: &Metadata) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    (metadata.ino() != 0 && metadata.ctime() != 0).then_some(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

#[cfg(not(any(unix, windows)))]
type FileIdentity = ();

#[cfg(not(any(unix, windows)))]
fn file_identity(_: &File, _: &Metadata) -> Option<FileIdentity> {
    None
}
