use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::NormalizedScannerSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserRoute {
    LightText,
    RustOffice,
    RustXlsx,
    Pdf,
    RustXls,
    RustLegacyOffice,
}

impl ParserRoute {
    pub fn backend(self) -> &'static str {
        match self {
            Self::LightText => "light_text_v2",
            Self::RustOffice => "rust_office_oxide_v2",
            Self::RustXlsx => "rust_xlsx_bounded_v2",
            Self::Pdf => "rust_pdfium_v3",
            Self::RustXls => "rust_xls_calamine_v3",
            Self::RustLegacyOffice => "rust_legacy_office_v3",
        }
    }

    pub fn worker_lane(self) -> &'static str {
        match self {
            Self::LightText => "rust_core",
            Self::RustOffice | Self::RustXlsx | Self::RustXls | Self::RustLegacyOffice => {
                "rust_office_process_v3"
            }
            Self::Pdf => "rust_pdf_process_v3",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassificationError {
    FileTooLarge,
    UnsupportedExtension,
    UnsupportedBackend,
    LegacyExtensionDisabled,
}

pub fn classify_candidate(
    file: &DiscoveredFileOut,
    profile: &NormalizedScannerSettings,
) -> Result<ParserRoute, ClassificationError> {
    if file.size_bytes > profile.execution.max_file_size_bytes {
        return Err(ClassificationError::FileTooLarge);
    }
    if !profile
        .discovery
        .allowed_extensions
        .iter()
        .any(|extension| extension == &file.extension)
    {
        return Err(ClassificationError::UnsupportedExtension);
    }

    match file.extension.as_str() {
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
