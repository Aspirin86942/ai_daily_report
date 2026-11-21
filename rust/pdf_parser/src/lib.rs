use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
};

mod excerpt;
mod ocr;
mod worker;
pub use ai_daily_worker_contract::OCR_RANGE_LABEL;
pub use worker::{is_valid_text_char, worker_hello, PdfSession};

pub const BUILD_INPUTS: &str = include_str!("../../build-inputs.json");

#[derive(Debug, thiserror::Error)]
pub enum PdfError {
    #[error("PDFIUM_MISSING: {0}")]
    LibraryMissing(#[from] std::io::Error),
    #[error("PDFIUM_VERSION_MISMATCH")]
    LibraryIdentity,
    #[error("PDF_PARSE_FAILED: {0}")]
    Parse(#[from] PdfiumError),
    #[error("PDF_PAGES_INVALID")]
    Pages,
    #[error("PDF_OCR_PAGE_LIMIT")]
    OcrLimits,
    #[error("PDF_OCR_PAGE_NOT_IMAGE_ONLY")]
    OcrIneligible,
    #[error("PDF_OCR_UNAVAILABLE")]
    OcrUnavailable,
    #[error("PDF_OCR_MODEL_MISSING")]
    OcrModelMissing,
    #[error("PDF_OCR_MODEL_MISMATCH")]
    OcrModelMismatch,
    #[error("PDF_OCR_TIMEOUT")]
    OcrTimeout,
    #[error("PDF_OCR_FAILED")]
    OcrFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageText {
    pub page: u32,
    pub text: String,
    pub chars_total: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PdfText {
    pub pages_total: u32,
    pub pages: Vec<PageText>,
    pub truncated: bool,
}

pub struct PdfReader {
    pdfium: Pdfium,
    program_dir: PathBuf,
}

impl PdfReader {
    /// Bind only the pinned DLL beside this worker. No system-library search.
    pub fn open(program_dir: &Path) -> Result<Self, PdfError> {
        let library = program_dir.join("pdfium.dll").canonicalize()?;
        let mut options = File::options();
        options.read(true);
        // FILE_SHARE_READ prevents writes and replacement between hashing and loading.
        #[cfg(windows)]
        std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 1);
        let mut file = options.open(&library)?;
        let expected: serde_json::Value =
            serde_json::from_str(BUILD_INPUTS).expect("validated build manifest");
        let digest = file_sha256(&mut file)?;
        if Some(digest.as_str()) != expected["pdfium"]["dll_sha256"].as_str() {
            return Err(PdfError::LibraryIdentity);
        }
        let bindings = Pdfium::bind_to_library(&library)?;
        drop(file);
        Ok(Self {
            pdfium: Pdfium::new(bindings),
            program_dir: program_dir.canonicalize()?,
        })
    }

    pub fn read(
        &self,
        path: &Path,
        selected_pages: Option<&[u32]>,
        max_pages: u32,
        max_chars: usize,
    ) -> Result<PdfText, PdfError> {
        if max_pages == 0 || max_chars == 0 {
            return Err(PdfError::Pages);
        }
        let document = self.pdfium.load_pdf_from_file(path, None)?;
        let (total, pages) = page_numbers(&document, selected_pages, max_pages)?;
        excerpt::read(&document, total, &pages, max_chars)
    }

    pub fn visit_pages(
        &self,
        path: &Path,
        selected_pages: Option<&[u32]>,
        max_pages: u32,
        max_chars: usize,
        mut visit: impl FnMut(PageText) -> bool,
    ) -> Result<(u32, bool), PdfError> {
        let result = self.read(path, selected_pages, max_pages, max_chars)?;
        let stopped = result.pages.into_iter().any(|page| !visit(page));
        Ok((result.pages_total, result.truncated || stopped))
    }

    pub(crate) fn visit_archive_pages(
        &self,
        path: &Path,
        mut visit: impl FnMut(u32, &str, Option<bool>) -> bool,
    ) -> Result<(u32, bool), PdfError> {
        let document = self.pdfium.load_pdf_from_file(path, None)?;
        let total = u32::try_from(document.pages().len()).map_err(|_| PdfError::Pages)?;
        if total == 0 {
            return Err(PdfError::Pages);
        }
        for number in 1..=total {
            let page = document.pages().get((number - 1) as PdfPageIndex)?;
            let text = page.text()?.all().replace("\r\n", "\n").replace('\r', "\n");
            if !visit(number, &text, page_images(&page)) {
                return Ok((total, true));
            }
        }
        Ok((total, false))
    }
}

fn file_sha256(file: &mut File) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(ai_daily_worker_contract::archive::hex(&hash.finalize())),
            Ok(read) => hash.update(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    ai_daily_worker_contract::archive::hex(&Sha256::digest(bytes))
}

fn page_numbers(
    document: &PdfDocument<'_>,
    selected: Option<&[u32]>,
    max_pages: u32,
) -> Result<(u32, Vec<u32>), PdfError> {
    let total = u32::try_from(document.pages().len()).map_err(|_| PdfError::Pages)?;
    let pages = selected
        .map(<[u32]>::to_vec)
        .unwrap_or_else(|| (1..=total.min(max_pages)).collect());
    if pages.is_empty()
        || pages.len() > max_pages as usize
        || pages.iter().any(|page| *page == 0 || *page > total)
        || pages.iter().copied().collect::<BTreeSet<_>>().len() != pages.len()
    {
        return Err(PdfError::Pages);
    }
    Ok((total, pages))
}

pub(crate) fn page_images(page: &PdfPage<'_>) -> Option<bool> {
    let mut found = Some(false);
    for (index, object) in page.objects().iter().enumerate() {
        if index >= 2048 {
            return None;
        }
        if object.as_image_object().is_some() {
            return Some(true);
        }
        if object.as_x_object_form_object().is_some() {
            found = None;
        }
    }
    found
}
