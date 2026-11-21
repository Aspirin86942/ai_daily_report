use ai_daily_scanner_contract::{AuditWorkerLane, NormalizedScannerSettings};
use ai_daily_worker_contract::{ParserBackend, WorkerLane};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParserRoute {
    LightText,
    RustOffice,
    RustXlsx,
    Pdf,
    PdfImageOcr { page_count: u64, max_pages: u64 },
    RustXls,
    RustLegacyOffice,
}

impl ParserRoute {
    pub(crate) const fn worker_backend(self) -> Option<ParserBackend> {
        match self {
            Self::LightText => None,
            Self::RustOffice => Some(ParserBackend::RustOfficeOxideV2),
            Self::RustXlsx => Some(ParserBackend::RustXlsxBoundedV2),
            Self::RustXls => Some(ParserBackend::RustXlsCalamineV3),
            Self::RustLegacyOffice => Some(ParserBackend::RustLegacyOfficeV3),
            Self::Pdf | Self::PdfImageOcr { .. } => Some(ParserBackend::RustPdfiumV3),
        }
    }

    pub const fn backend(self) -> &'static str {
        if matches!(self, Self::PdfImageOcr { .. }) {
            return crate::parsers::document::OCR_BACKEND;
        }
        match self.worker_backend() {
            Some(backend) => backend.as_str(),
            None => "light_text_v2",
        }
    }

    pub const fn worker_lane(self) -> &'static str {
        match self.worker_backend() {
            Some(backend) => match backend.lane() {
                WorkerLane::RustOfficeProcessV3 => AuditWorkerLane::RustOfficeProcessV3.as_str(),
                WorkerLane::RustPdfProcessV3 => AuditWorkerLane::RustPdfProcessV3.as_str(),
            },
            None => AuditWorkerLane::RustCore.as_str(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassificationError {
    UnsupportedExtension,
    UnsupportedBackend,
    LegacyExtensionDisabled,
}

pub(crate) fn classify_extension(
    extension: &str,
    profile: &NormalizedScannerSettings,
) -> Result<ParserRoute, ClassificationError> {
    match extension {
        ".txt" | ".md" | ".csv" | ".json" | ".log" => Ok(ParserRoute::LightText),
        ".xlsx" => {
            classify_modern_office(&profile.parse.office.primary_backend, ParserRoute::RustXlsx)
        }
        ".docx" | ".pptx" => classify_modern_office(
            &profile.parse.office.primary_backend,
            ParserRoute::RustOffice,
        ),
        ".xls" => Ok(ParserRoute::RustXls),
        ".pdf" if profile.parse.pdf.backend == "rust_pdfium_v3" => Ok(ParserRoute::Pdf),
        ".pdf" => Err(ClassificationError::UnsupportedBackend),
        ".doc" | ".ppt" if profile.parse.office.legacy_extensions_enabled => {
            Ok(ParserRoute::RustLegacyOffice)
        }
        ".doc" | ".ppt" => Err(ClassificationError::LegacyExtensionDisabled),
        _ => Err(ClassificationError::UnsupportedExtension),
    }
}

fn classify_modern_office(
    primary_backend: &str,
    rust_route: ParserRoute,
) -> Result<ParserRoute, ClassificationError> {
    match primary_backend {
        "rust_office_oxide_v2" => Ok(rust_route),
        _ => Err(ClassificationError::UnsupportedBackend),
    }
}
