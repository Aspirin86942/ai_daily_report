use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, path::Path};

mod excerpt;
mod worker;
pub use worker::{is_valid_text_char, worker_hello};

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
}

impl PdfReader {
    /// Bind only the pinned DLL beside this worker. No system-library search.
    pub fn open(program_dir: &Path) -> Result<Self, PdfError> {
        let library = program_dir.join("pdfium.dll");
        let expected: serde_json::Value =
            serde_json::from_str(BUILD_INPUTS).expect("validated build manifest");
        let digest: String = Sha256::digest(std::fs::read(&library)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if Some(digest.as_str()) != expected["pdfium"]["dll_sha256"].as_str() {
            return Err(PdfError::LibraryIdentity);
        }
        Ok(Self {
            pdfium: Pdfium::new(Pdfium::bind_to_library(library)?),
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
