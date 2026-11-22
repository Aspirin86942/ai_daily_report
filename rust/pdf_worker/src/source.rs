use crate::{check_deadline, PdfError};
use daily_parsers::{
    discovery::build_source_version,
    pdf_wire::{file_identity_sha256, reader_sha256, InspectAndExtract, PdfSourceGuard},
    settings::SourceGuardKind,
};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, UNIX_EPOCH},
};

pub(super) struct OpenSource {
    pub(super) file: File,
    guard: PdfSourceGuard,
}

impl OpenSource {
    pub(super) fn open(request: &InspectAndExtract, deadline: Instant) -> Result<Self, PdfError> {
        check_deadline(deadline)?;
        let expected = request.expected_source_guard.as_ref();
        let source = Self::open_with_kind(request, deadline, expected.map(|guard| guard.kind))?;
        if expected.is_some_and(|expected| *expected != source.guard) {
            return Err(PdfError::SourceChanged);
        }
        check_deadline(deadline)?;
        Ok(source)
    }

    fn open_with_kind(
        request: &InspectAndExtract,
        deadline: Instant,
        kind: Option<SourceGuardKind>,
    ) -> Result<Self, PdfError> {
        let path = Path::new(&request.parse.file_path);
        if kind == Some(SourceGuardKind::ContentSha256V1) {
            return Self::open_and_hash(path, request, deadline);
        }
        let file = File::open(path).map_err(|_| PdfError::SourceIo)?;
        check_version(&file, request)?;
        match file_identity_sha256(&file) {
            Some((kind, sha256)) => Ok(Self {
                file,
                guard: PdfSourceGuard { kind, sha256 },
            }),
            None if kind.is_none() => Self::open_and_hash(path, request, deadline),
            None => Err(PdfError::SourceChanged),
        }
    }

    fn open_and_hash(
        path: &Path,
        request: &InspectAndExtract,
        deadline: Instant,
    ) -> Result<Self, PdfError> {
        let mut options = File::options();
        options.read(true);
        // Without metadata proof, deny writes and replacement while hashing and reading.
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 1);
        let file = options.open(path).map_err(|_| PdfError::SourceIo)?;
        let bytes = check_version(&file, request)?;
        let state = ReadState::new(deadline, None);
        let sha256 = reader_sha256(state.input(&file, false).take(bytes + 1));
        state.check()?;
        let sha256 = sha256.map_err(|_| PdfError::SourceIo)?;
        check_version(&file, request)?;
        Ok(Self {
            file,
            guard: PdfSourceGuard {
                kind: SourceGuardKind::ContentSha256V1,
                sha256,
            },
        })
    }

    pub(super) fn verify(
        &self,
        request: &InspectAndExtract,
        deadline: Instant,
    ) -> Result<(), PdfError> {
        check_deadline(deadline)?;
        let current = Self::open_with_kind(request, deadline, Some(self.guard.kind))?;
        if self.guard != current.guard {
            return Err(PdfError::SourceChanged);
        }
        check_deadline(deadline)
    }
}

fn check_version(file: &File, request: &InspectAndExtract) -> Result<u64, PdfError> {
    let metadata = file.metadata().map_err(|_| PdfError::SourceIo)?;
    if !metadata.is_file() {
        return Err(PdfError::SourceIo);
    }
    let bytes = metadata.len();
    let modified = metadata
        .modified()
        .map_err(|_| PdfError::SourceIo)?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PdfError::SourceChanged)?;
    if request.parse.expected_source_version != build_source_version(modified.as_nanos(), bytes) {
        return Err(PdfError::SourceChanged);
    }
    // FPDF_FILEACCESS stores the length as an unsigned long, 32 bits on Windows.
    if bytes > request.parse.max_file_size_bytes || bytes > u64::from(u32::MAX) {
        return Err(PdfError::TooLarge);
    }
    Ok(bytes)
}

pub(super) struct ReadState {
    deadline: Instant,
    inspection: Option<(Instant, AtomicBool)>,
    failed: AtomicBool,
}

impl ReadState {
    pub(super) fn new(deadline: Instant, inspection: Option<Instant>) -> Self {
        Self {
            deadline,
            inspection: inspection.map(|deadline| (deadline, AtomicBool::new(false))),
            failed: AtomicBool::new(false),
        }
    }

    pub(super) fn finish_inspection(&self) {
        if let Some((_, finished)) = &self.inspection {
            finished.store(true, Ordering::Relaxed);
        }
    }

    fn deadline(&self) -> Instant {
        self.inspection
            .as_ref()
            .filter(|(_, finished)| !finished.load(Ordering::Relaxed))
            .map_or(self.deadline, |(deadline, _)| *deadline)
    }

    pub(super) fn input<'a>(&'a self, file: &'a File, full_read: bool) -> SourceInput<'a> {
        SourceInput {
            file,
            state: self,
            full_read,
        }
    }

    pub(super) fn check(&self) -> Result<(), PdfError> {
        check_deadline(self.deadline())?;
        if self.failed.load(Ordering::Relaxed) {
            return Err(PdfError::SourceIo);
        }
        Ok(())
    }
}

pub(super) struct SourceInput<'a> {
    file: &'a File,
    state: &'a ReadState,
    // PDFium requests fixed byte ranges. Hashing stops normally at EOF.
    full_read: bool,
}

impl Read for SourceInput<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0;
        while read < buf.len() {
            if Instant::now() >= self.state.deadline() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.file.read(&mut buf[read..]) {
                Ok(0) => {
                    if self.full_read {
                        self.state.failed.store(true, Ordering::Relaxed);
                    }
                    break;
                }
                Ok(n) => read += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.state.failed.store(true, Ordering::Relaxed);
                    return Err(error);
                }
            }
        }
        Ok(read)
    }
}

impl Seek for SourceInput<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if Instant::now() >= self.state.deadline() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        self.file
            .seek(position)
            .inspect_err(|_| self.state.failed.store(true, Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn inspection_has_its_own_budget_until_extraction() {
        let started = Instant::now();
        let reading = ReadState::new(started + Duration::from_secs(30), Some(started));
        assert!(matches!(reading.check(), Err(PdfError::Timeout)));
        reading.finish_inspection();
        assert!(reading.check().is_ok());
    }

    #[test]
    fn short_reads_fail_native_callbacks_and_end_hash_reads_normally() {
        use std::io::Write;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"pdf").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        for full_read in [true, false] {
            file.rewind().unwrap();
            let state = ReadState::new(deadline, None);
            let mut bytes = [0; 8];
            assert_eq!(state.input(&file, full_read).read(&mut bytes).unwrap(), 3);
            assert_eq!(&bytes[..3], b"pdf");
            assert_eq!(matches!(state.check(), Err(PdfError::SourceIo)), full_read);
        }
    }
}
