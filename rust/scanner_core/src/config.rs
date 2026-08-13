use ai_daily_scanner_contract::{
    quota_defaults, ContextProfile, DiscoveryProfile, ExecutionProfile, FallbackBackend,
    NormalizedScannerSettings, OfficeParseProfile, ParseProfile, PdfParseProfile, ReportMode,
    ScannerSettings, TextParseProfile, Validate, ADMISSION_POLICY_VERSION,
    CLASSIFIER_POLICY_VERSION, COMPRESSION_POLICY_VERSION, PRIORITY_POLICY_VERSION,
};
use std::collections::BTreeMap;

const DEFAULT_ALLOWED_EXTENSIONS: &[&str] = &[
    ".xlsx", ".xls", ".pptx", ".pdf", ".txt", ".md", ".docx", ".csv", ".json", ".log",
];
const DEFAULT_IGNORED_PATTERNS: &[&str] = &["~$*", "*.tmp"];
const MIB: u64 = 1024 * 1024;

/// Merge caller-controlled leaves with the current compiled scanner policy.
/// Routing, backend identities and fallback order are deliberately not part of
/// the public settings surface.
pub fn normalize_scanner_settings(
    raw: &ScannerSettings,
    report_mode: ReportMode,
) -> Result<NormalizedScannerSettings, String> {
    raw.validate()?;
    let summary_mode = !matches!(report_mode, ReportMode::Daily);
    let text_max_chars = if summary_mode {
        raw.summary_text_max_chars.unwrap_or(2_000)
    } else {
        raw.text_max_chars.unwrap_or(6_000)
    };
    let default_head_bytes = raw.direct_text_max_bytes.unwrap_or(262_144);
    let document_excerpt_max_chars = if summary_mode {
        raw.summary_document_excerpt_max_chars
            .unwrap_or(text_max_chars)
    } else {
        raw.document_excerpt_max_chars.unwrap_or(text_max_chars)
    };
    let (
        excel_max_sheets,
        excel_max_rows,
        excel_max_columns,
        docx_max_paragraphs,
        docx_max_tables,
        docx_table_max_rows,
        docx_table_max_cols,
        pptx_max_slides,
        pdf_max_pages,
    ) = if summary_mode {
        (
            raw.summary_excel_max_sheets.unwrap_or(2),
            raw.summary_excel_max_rows.unwrap_or(10),
            raw.summary_excel_max_columns.unwrap_or(12),
            raw.summary_docx_max_paragraphs.unwrap_or(80),
            raw.summary_docx_max_tables.unwrap_or(8),
            raw.summary_docx_table_max_rows.unwrap_or(20),
            raw.summary_docx_table_max_cols.unwrap_or(8),
            raw.summary_pptx_max_slides.unwrap_or(15),
            raw.summary_pdf_max_pages.unwrap_or(2),
        )
    } else {
        (
            raw.excel_max_sheets.unwrap_or(5),
            raw.excel_max_rows.unwrap_or(50),
            raw.excel_max_columns.unwrap_or(20),
            raw.docx_max_paragraphs.unwrap_or(200),
            raw.docx_max_tables.unwrap_or(20),
            raw.docx_table_max_rows.unwrap_or(50),
            raw.docx_table_max_cols.unwrap_or(12),
            raw.pptx_max_slides.unwrap_or(50),
            raw.pdf_max_pages.unwrap_or(5),
        )
    };
    let (profile_name, global_max_chars, per_file_max_chars) = match report_mode {
        ReportMode::Daily => ("daily_balanced_v1", 500_000, 100_000),
        ReportMode::Weekly => ("weekly_balanced_v1", 500_000, 100_000),
        ReportMode::Monthly => ("monthly_balanced_v1", 500_000, 100_000),
    };
    let max_file_size_bytes = raw
        .max_file_size_mb
        .unwrap_or(50)
        .checked_mul(MIB)
        .ok_or_else(|| "max_file_size_mb overflows bytes".to_string())?;
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
                raw.allowed_extensions.as_ref(),
                DEFAULT_ALLOWED_EXTENSIONS,
                "allowed_extensions",
            )?,
            ignored_patterns: canonical_strings(
                raw.ignored_patterns.as_ref(),
                DEFAULT_IGNORED_PATTERNS,
                "ignored_patterns",
            )?,
            excluded_dirs: canonical_strings(raw.excluded_dirs.as_ref(), &[], "excluded_dirs")?,
        },
        execution: ExecutionProfile {
            max_workers: raw.max_workers.unwrap_or(4),
            max_file_size_bytes,
            discovery_timeout_ms: seconds_to_millis(
                raw.discovery_timeout_seconds.unwrap_or(30),
                "discovery_timeout_seconds",
            )?,
            file_timeout_ms: seconds_to_millis(
                raw.file_timeout_seconds.unwrap_or(30),
                "file_timeout_seconds",
            )?,
            file_timeout_by_extension_ms: normalize_timeout_map(
                raw.file_timeout_by_extension.as_ref(),
            )?,
        },
        parse: ParseProfile {
            aggregate_max_chars: raw.total_max_chars.unwrap_or(500_000),
            text: TextParseProfile {
                backend: "light_text_v2".to_string(),
                read_head_bytes: raw.direct_text_read_bytes.unwrap_or(default_head_bytes),
                read_tail_bytes: raw.log_tail_read_bytes.unwrap_or(262_144),
                max_chars: text_max_chars,
                excerpt_max_chars: raw.text_excerpt_max_chars.unwrap_or(text_max_chars),
            },
            office: OfficeParseProfile {
                primary_backend: "rust_office_oxide_v2".to_string(),
                fallback_enabled: true,
                fallback_order: vec![
                    FallbackBackend::PythonOfficeV2,
                    FallbackBackend::PythonSharepointTextV2,
                ],
                fallback_after_timeout: raw.fallback_after_timeout.unwrap_or(false),
                fallback_policy_version: "worker_v2_fixed_routing".to_string(),
                legacy_extensions_enabled: raw.legacy_office_enabled.unwrap_or(false),
                excel_max_sheets,
                excel_max_rows,
                excel_max_columns,
                docx_max_paragraphs,
                docx_max_tables,
                docx_table_max_rows,
                docx_table_max_cols,
                pptx_max_slides,
                pptx_include_notes: raw.pptx_include_notes.unwrap_or(true),
                document_excerpt_max_chars,
            },
            pdf: PdfParseProfile {
                backend: "python_pdf_text_v2".to_string(),
                max_pages: pdf_max_pages,
                excerpt_max_chars: document_excerpt_max_chars,
            },
        },
        context: ContextProfile {
            profile_name: profile_name.to_string(),
            global_max_chars,
            per_file_max_chars,
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
        total_deadline_ms: raw.total_deadline_ms.unwrap_or(default_total_deadline_ms),
        worker_max_requests: raw.worker_max_requests.unwrap_or(128),
        worker_idle_ttl_ms: raw.worker_idle_ttl_ms.unwrap_or(30_000),
        worker_rss_limit_bytes: raw.worker_rss_limit_bytes.unwrap_or(512 * 1024 * 1024),
    };
    settings.validate()?;
    Ok(settings)
}

fn canonical_strings(
    configured: Option<&Vec<String>>,
    defaults: &[&str],
    field: &str,
) -> Result<Vec<String>, String> {
    let values = configured
        .cloned()
        .unwrap_or_else(|| defaults.iter().map(|value| (*value).to_string()).collect());
    let mut canonical = Vec::with_capacity(values.len());
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(format!("{field} contains an empty value"));
        }
        canonical.push(trimmed.to_string());
    }
    canonical.sort();
    canonical.dedup();
    Ok(canonical)
}

fn normalize_timeout_map(
    configured: Option<&BTreeMap<String, u64>>,
) -> Result<BTreeMap<String, u64>, String> {
    let defaults = BTreeMap::from([
        (".pdf".to_string(), 45_u64),
        (".xls".to_string(), 60_u64),
        (".xlsx".to_string(), 60_u64),
    ]);
    let source = configured.unwrap_or(&defaults);
    let mut normalized = BTreeMap::new();
    for (extension, seconds) in source {
        let key = extension.trim().to_string();
        let milliseconds = seconds_to_millis(*seconds, "file_timeout_by_extension value")?;
        if normalized.insert(key, milliseconds).is_some() {
            return Err("file_timeout_by_extension contains duplicate canonical keys".to_string());
        }
    }
    Ok(normalized)
}

fn seconds_to_millis(seconds: u64, field: &str) -> Result<u64, String> {
    seconds
        .checked_mul(1_000)
        .ok_or_else(|| format!("{field} overflows milliseconds"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_pool_defaults_follow_max_workers_and_keep_lifecycle_limits() {
        let raw: ScannerSettings = serde_json::from_value(serde_json::json!({
            "max_workers": 6,
            "worker_max_requests": 5,
            "worker_idle_ttl_ms": 2_000,
            "worker_rss_limit_bytes": 67_108_864
        }))
        .expect("settings decode");
        let normalized =
            normalize_scanner_settings(&raw, ReportMode::Daily).expect("settings normalize");

        assert_eq!(normalized.execution.max_workers, 6);
        assert_eq!(normalized.worker_max_requests, 5);
        assert_eq!(normalized.worker_idle_ttl_ms, 2_000);
        assert_eq!(normalized.worker_rss_limit_bytes, 67_108_864);
    }
}
