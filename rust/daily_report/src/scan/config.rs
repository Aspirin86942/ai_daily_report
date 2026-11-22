use daily_parsers::settings::{
    quota_defaults, ContextProfile, DiscoveryProfile, ExecutionProfile, NormalizedScannerSettings,
    OfficeParseProfile, ParseProfile, PdfParseProfile, ReportMode, ScannerSettings,
    TextParseProfile, Validate, ADMISSION_POLICY_VERSION, CLASSIFIER_POLICY_VERSION,
    COMPRESSION_POLICY_VERSION, PRIORITY_POLICY_VERSION,
};
use std::collections::BTreeMap;

const DEFAULT_ALLOWED_EXTENSIONS: &[&str] = &[
    ".xlsx", ".xls", ".pptx", ".pdf", ".txt", ".md", ".docx", ".csv", ".json", ".log",
];
const DEFAULT_IGNORED_PATTERNS: &[&str] = &["~$*", "*.tmp"];
const MIB: u64 = 1024 * 1024;
const DEFAULT_WORKER_MEMORY: u64 = 512 * MIB;

/// Merge caller-controlled leaves with the current compiled scanner policy.
/// Routing and backend identities are deliberately not part of
/// the public settings surface.
pub fn normalize_scanner_settings(
    raw: &ScannerSettings,
    report_mode: ReportMode,
) -> Result<NormalizedScannerSettings, String> {
    raw.validate()?;
    let worker_memory = raw.worker_rss_limit_bytes.unwrap_or(DEFAULT_WORKER_MEMORY);
    if !cfg!(windows) && worker_memory != DEFAULT_WORKER_MEMORY {
        return Err("an explicit worker_rss_limit_bytes requires Windows Job accounting".into());
    }
    let limit = |daily: Option<u64>, summary: Option<u64>, daily_default, summary_default| {
        if report_mode == ReportMode::Daily {
            daily.unwrap_or(daily_default)
        } else {
            summary.unwrap_or(summary_default)
        }
    };
    let text_max_chars = limit(raw.text_max_chars, raw.summary_text_max_chars, 6_000, 2_000);
    let document_excerpt_max_chars = limit(
        raw.document_excerpt_max_chars,
        raw.summary_document_excerpt_max_chars,
        text_max_chars,
        text_max_chars,
    );
    let default_head_bytes = raw.direct_text_max_bytes.unwrap_or(262_144);
    let global_max_chars = raw.context_max_chars.unwrap_or(500_000);
    let max_file_size_bytes = raw.max_file_size_mb.unwrap_or(50) * MIB;
    let (
        default_max_candidate_files,
        default_max_total_pdf_classification_pages,
        default_max_pdf_text_extractions,
        default_total_deadline_ms,
    ) = quota_defaults(report_mode);

    let settings = NormalizedScannerSettings {
        report_mode,
        discovery: DiscoveryProfile {
            allowed_extensions: canonical_strings(
                raw.allowed_extensions.as_deref(),
                DEFAULT_ALLOWED_EXTENSIONS,
                "allowed_extensions",
            )?,
            ignored_patterns: canonical_strings(
                raw.ignored_patterns.as_deref(),
                DEFAULT_IGNORED_PATTERNS,
                "ignored_patterns",
            )?,
            excluded_dirs: canonical_strings(raw.excluded_dirs.as_deref(), &[], "excluded_dirs")?,
            follow_symlinks: raw.follow_symlinks.unwrap_or(false),
            cloud_files_policy: raw
                .cloud_files_policy
                .clone()
                .unwrap_or_else(|| "local_only".to_string()),
        },
        execution: ExecutionProfile {
            max_workers: raw.max_workers.unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(1, |count| count.get().min(4)) as u64
            }),
            max_file_size_bytes,
            discovery_timeout_ms: raw.discovery_timeout_seconds.unwrap_or(30) * 1_000,
            file_timeout_ms: raw.file_timeout_seconds.unwrap_or(30) * 1_000,
            file_timeout_by_extension_ms: normalize_timeout_map(
                raw.file_timeout_by_extension.as_ref(),
            )?,
        },
        parse: ParseProfile {
            cache_payload_version: 1,
            excerpt_policy_version: super::allocation::EXCERPT_POLICY_VERSION.into(),
            excerpt_priority_paths: {
                let mut paths: Vec<_> = raw
                    .excerpt_priority_paths
                    .iter()
                    .flatten()
                    .map(|path| path.replace('\\', "/"))
                    .collect();
                paths.sort_unstable();
                paths.dedup();
                paths
            },
            aggregate_max_chars: raw.total_max_chars.unwrap_or(500_000),
            text: TextParseProfile {
                csv_encodings: raw
                    .csv_encodings
                    .clone()
                    .unwrap_or_else(daily_parsers::settings::default_csv_encodings),
                backend: "light_text_v2".to_string(),
                read_head_bytes: raw.direct_text_read_bytes.unwrap_or(default_head_bytes),
                read_tail_bytes: raw.log_tail_read_bytes.unwrap_or(262_144),
                max_chars: text_max_chars,
                excerpt_max_chars: raw.text_excerpt_max_chars.unwrap_or(text_max_chars),
            },
            office: OfficeParseProfile {
                primary_backend: "rust_office_oxide_v2".to_string(),
                legacy_extensions_enabled: raw.legacy_office_enabled.unwrap_or(false),
                excel_max_sheets: limit(raw.excel_max_sheets, raw.summary_excel_max_sheets, 5, 2),
                excel_max_rows: limit(raw.excel_max_rows, raw.summary_excel_max_rows, 50, 10),
                excel_max_columns: limit(
                    raw.excel_max_columns,
                    raw.summary_excel_max_columns,
                    20,
                    12,
                ),
                docx_max_paragraphs: limit(
                    raw.docx_max_paragraphs,
                    raw.summary_docx_max_paragraphs,
                    200,
                    80,
                ),
                docx_max_tables: limit(raw.docx_max_tables, raw.summary_docx_max_tables, 20, 8),
                docx_table_max_rows: limit(
                    raw.docx_table_max_rows,
                    raw.summary_docx_table_max_rows,
                    50,
                    20,
                ),
                docx_table_max_cols: limit(
                    raw.docx_table_max_cols,
                    raw.summary_docx_table_max_cols,
                    12,
                    8,
                ),
                pptx_max_slides: limit(raw.pptx_max_slides, raw.summary_pptx_max_slides, 50, 15),
                pptx_include_notes: raw.pptx_include_notes.unwrap_or(true),
                document_excerpt_max_chars,
            },
            pdf: PdfParseProfile {
                image_ocr: raw.pdf_image_ocr.filter(|ocr| ocr.enabled),
                selection_policy_version: CLASSIFIER_POLICY_VERSION.to_string(),
                backend: "rust_pdfium_v3".to_string(),
                max_pages: limit(raw.pdf_max_pages, raw.summary_pdf_max_pages, 5, 2),
                excerpt_max_chars: document_excerpt_max_chars,
            },
        },
        context: ContextProfile {
            profile_name: report_mode.compression_profile().as_str().to_owned(),
            global_max_chars,
            per_file_max_chars: daily_parsers::settings::CONTEXT_PER_FILE_MAX_CHARS,
            small_file_max_bytes: 65_536,
            medium_file_max_bytes: 1_048_576,
            large_file_max_bytes: 10_485_760,
            priority_policy_version: PRIORITY_POLICY_VERSION.to_string(),
            compression_policy_version: COMPRESSION_POLICY_VERSION.to_string(),
        },
        admission_policy_version: ADMISSION_POLICY_VERSION.to_string(),
        classifier_policy_version: CLASSIFIER_POLICY_VERSION.to_string(),
        max_candidate_files: raw
            .max_candidate_files
            .unwrap_or(default_max_candidate_files),
        max_pdf_text_extractions: raw
            .max_pdf_text_extractions
            .unwrap_or(default_max_pdf_text_extractions),
        max_total_pdf_classification_pages: raw
            .max_total_pdf_classification_pages
            .unwrap_or(default_max_total_pdf_classification_pages),
        pdf_classification_timeout_ms: raw.pdf_classification_timeout_ms.unwrap_or(2_000),
        total_deadline_ms: raw
            .total_deadline_ms
            .unwrap_or(if report_mode == ReportMode::Daily {
                120_000
            } else {
                default_total_deadline_ms
            })
            .min(120_000),
        worker_max_requests: raw.worker_max_requests.unwrap_or(128),
        worker_idle_ttl_ms: raw.worker_idle_ttl_ms.unwrap_or(30_000),
        worker_rss_limit_bytes: worker_memory,
    };
    // The raw limits and compiled defaults define the budgets. Check the fields
    // whose text normalization can change validity, including extension keys.
    settings.discovery.validate()?;
    settings.execution.validate()?;
    Ok(settings)
}

fn canonical_strings(
    configured: Option<&[String]>,
    defaults: &[&str],
    field: &str,
) -> Result<Vec<String>, String> {
    let mut canonical: Vec<String> = match configured {
        Some(values) => values.iter().map(|value| value.trim().to_owned()).collect(),
        None => defaults
            .iter()
            .map(|value| value.trim().to_owned())
            .collect(),
    };
    if canonical.iter().any(String::is_empty) {
        return Err(format!("{field} contains an empty value"));
    }
    canonical.sort_unstable();
    canonical.dedup();
    Ok(canonical)
}

fn normalize_timeout_map(
    configured: Option<&BTreeMap<String, u64>>,
) -> Result<BTreeMap<String, u64>, String> {
    let Some(source) = configured else {
        return Ok(BTreeMap::from([
            (".pdf".into(), 45_000),
            (".xls".into(), 60_000),
            (".xlsx".into(), 60_000),
        ]));
    };
    let normalized: BTreeMap<_, _> = source
        .iter()
        .map(|(extension, seconds)| (extension.trim().to_owned(), seconds * 1_000))
        .collect();
    if normalized.len() != source.len() {
        return Err("file_timeout_by_extension contains duplicate canonical keys".into());
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalization_rejects_invalid_or_colliding_canonical_extensions() {
        let cases = [
            (
                json!({"allowed_extensions": [". "]}),
                "allowed_extensions must be a lowercase extension",
            ),
            (
                json!({"file_timeout_by_extension": {". ": 1}}),
                "file_timeout_by_extension_ms key must be a lowercase extension",
            ),
            (
                json!({"file_timeout_by_extension": {".pdf": 1, ".pdf ": 2}}),
                "file_timeout_by_extension contains duplicate canonical keys",
            ),
        ];
        for (value, message) in cases {
            let raw: ScannerSettings = serde_json::from_value(value).unwrap();
            raw.validate().unwrap();
            assert_eq!(
                normalize_scanner_settings(&raw, ReportMode::Daily).unwrap_err(),
                message
            );
        }
    }
}
