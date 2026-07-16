use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::NormalizedScannerProfileV1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserRoute {
    LightText,
    RustOffice,
    RustXlsx,
    Pdf,
    PythonOffice,
    PythonSharepointText,
}

impl ParserRoute {
    pub fn backend(self) -> &'static str {
        match self {
            Self::LightText => "light_text_v1",
            Self::RustOffice => "rust_office_oxide_v1",
            Self::RustXlsx => "rust_xlsx_bounded_v1",
            Self::Pdf => "pdf_text_v1",
            Self::PythonOffice => "python_office_v1",
            Self::PythonSharepointText => "python_sharepoint_text_v1",
        }
    }

    pub fn worker_lane(self) -> &'static str {
        match self {
            Self::LightText => "rust_core",
            Self::RustOffice | Self::RustXlsx => "rust_office_process",
            Self::Pdf | Self::PythonOffice | Self::PythonSharepointText => {
                "python_document_process"
            }
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
    profile: &NormalizedScannerProfileV1,
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
        ".xls" => Ok(ParserRoute::PythonOffice),
        ".pdf" if profile.parse.pdf.backend == "pdf_text_v1" => Ok(ParserRoute::Pdf),
        ".pdf" => Err(ClassificationError::UnsupportedBackend),
        ".doc" | ".ppt" if profile.parse.office.legacy_extensions_enabled => {
            Ok(ParserRoute::PythonSharepointText)
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
        "rust_office_oxide_v1" => Ok(rust_route),
        "python_office_v1" => Ok(ParserRoute::PythonOffice),
        _ => Err(ClassificationError::UnsupportedBackend),
    }
}
