use ai_daily_scanner_contract::{
    v2_quota_defaults, ADMISSION_POLICY_VERSION, CLASSIFIER_POLICY_VERSION,
    COMPRESSION_POLICY_VERSION, ContextProfile, ContextProfileV2, DiscoveryProfile,
    ExecutionProfile, FallbackBackend, NormalizedScannerProfileV1, NormalizedScannerProfileV2,
    OfficeParseProfile, ParseProfile, PdfParseProfile, PRIORITY_POLICY_VERSION,
    RawScannerProfileV1, RawScannerProfileV2, ReportMode, SCANNER_PROFILE_V2_ONLY_FIELDS,
    ScannerProfile, TextParseProfile, Validate,
};
use std::collections::BTreeMap;

const DEFAULT_ALLOWED_EXTENSIONS: &[&str] = &[
    ".xlsx", ".xls", ".pptx", ".pdf", ".txt", ".md", ".docx", ".csv", ".json", ".log",
];
const DEFAULT_IGNORED_PATTERNS: &[&str] = &["~$*", "*.tmp"];
const MIB: u64 = 1024 * 1024;

pub fn normalize_scanner_profile(
    raw: &RawScannerProfileV1,
    report_mode: ReportMode,
) -> Result<NormalizedScannerProfileV1, String> {
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
        ReportMode::Daily => ("daily_balanced_v1", 50_000, 8_000),
        ReportMode::Weekly => ("weekly_balanced_v1", 50_000, 5_000),
        ReportMode::Monthly => ("monthly_balanced_v1", 60_000, 4_000),
    };
    let max_file_size_bytes = raw
        .max_file_size_mb
        .unwrap_or(50)
        .checked_mul(MIB)
        .ok_or_else(|| "max_file_size_mb overflows bytes".to_string())?;

    let profile = NormalizedScannerProfileV1 {
        schema_version: "normalized_scanner_profile_v1".to_string(),
        parser_profile_version: raw
            .parser_profile_version
            .clone()
            .unwrap_or_else(|| "v1".to_string()),
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
            aggregate_max_chars: raw.total_max_chars.unwrap_or(50_000),
            text: TextParseProfile {
                backend: "light_text_v1".to_string(),
                read_head_bytes: raw.direct_text_read_bytes.unwrap_or(default_head_bytes),
                read_tail_bytes: raw.log_tail_read_bytes.unwrap_or(262_144),
                max_chars: text_max_chars,
                excerpt_max_chars: raw.text_excerpt_max_chars.unwrap_or(text_max_chars),
            },
            office: OfficeParseProfile {
                primary_backend: raw
                    .office_parser_backend
                    .clone()
                    .unwrap_or_else(|| "rust_office_oxide_v1".to_string()),
                fallback_enabled: raw.office_parser_fallback_enabled.unwrap_or(true),
                fallback_order: raw.office_parser_fallback_order.clone().unwrap_or_else(|| {
                    vec![
                        FallbackBackend::PythonOfficeV1,
                        FallbackBackend::PythonSharepointTextV1,
                    ]
                }),
                fallback_after_timeout: raw.office_fallback_after_timeout.unwrap_or(false),
                fallback_policy_version: raw
                    .office_fallback_policy_version
                    .clone()
                    .unwrap_or_else(|| "hybrid_v1".to_string()),
                legacy_extensions_enabled: raw.office_legacy_extensions_enabled.unwrap_or(false),
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
                backend: raw
                    .pdf_parser_backend
                    .clone()
                    .unwrap_or_else(|| "pdf_text_v1".to_string()),
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
            priority_policy_version: "default_v1".to_string(),
            compression_policy_version: "markdown_context_v1".to_string(),
        },
    };
    profile.validate()?;
    Ok(profile)
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

/// Normalize a tagged union scanner profile (v1 or v2) into the fully-required
/// canonical v2 profile. v1 requests are converted to a raw v2 profile first,
/// so the v2-only leaves fall back to the frozen report-mode default table
/// (spec Part 8.1). PDF page defaults stay daily=5, weekly/monthly=2.
pub fn normalize_scanner_profile_v2(
    profile: &ScannerProfile,
    report_mode: ReportMode,
) -> Result<NormalizedScannerProfileV2, String> {
    profile.validate()?;
    match profile {
        ScannerProfile::V1(raw) => {
            let raw_v2 = raw_v1_to_v2(raw)?;
            normalize_scanner_profile_v2_raw(&raw_v2, report_mode)
        }
        ScannerProfile::V2(raw) => normalize_scanner_profile_v2_raw(raw, report_mode),
    }
}

/// Normalize a raw scanner profile v2 into the fully-required canonical v2
/// profile. The v2 path merges raw leaves with the frozen report-mode default
/// table (spec Part 8.1) and keeps the existing PDF page defaults (daily=5,
/// weekly/monthly=2).
fn normalize_scanner_profile_v2_raw(
    raw: &RawScannerProfileV2,
    report_mode: ReportMode,
) -> Result<NormalizedScannerProfileV2, String> {
    let cpu_count = std::thread::available_parallelism()
        .map(|value| value.get() as u64)
        .unwrap_or(1);
    normalize_scanner_profile_v2_raw_with_parallelism(raw, report_mode, cpu_count)
}

fn normalize_scanner_profile_v2_raw_with_parallelism(
    raw: &RawScannerProfileV2,
    report_mode: ReportMode,
    cpu_count: u64,
) -> Result<NormalizedScannerProfileV2, String> {
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
        ReportMode::Daily => ("daily_balanced_v1", 50_000, 8_000),
        ReportMode::Weekly => ("weekly_balanced_v1", 50_000, 5_000),
        ReportMode::Monthly => ("monthly_balanced_v1", 60_000, 4_000),
    };
    let max_file_size_bytes = raw
        .max_file_size_mb
        .unwrap_or(50)
        .checked_mul(MIB)
        .ok_or_else(|| "max_file_size_mb overflows bytes".to_string())?;
    let (default_max_candidate_files, default_max_total_pdf_classification_pages, default_max_pdf_text_extractions, default_total_deadline_ms) =
        v2_quota_defaults(report_mode);
    let cpu_count = cpu_count.max(1);
    let max_workers = raw.max_workers.unwrap_or(cpu_count.min(4));
    let session_concurrency = raw
        .session_concurrency
        .unwrap_or(max_workers.min(4).min(cpu_count));

    let profile = NormalizedScannerProfileV2 {
        schema_version: "normalized_scanner_profile_v2".to_string(),
        parser_profile_version: raw
            .parser_profile_version
            .clone()
            .unwrap_or_else(|| "v1".to_string()),
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
            max_workers,
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
            aggregate_max_chars: raw.total_max_chars.unwrap_or(50_000),
            text: TextParseProfile {
                backend: "light_text_v1".to_string(),
                read_head_bytes: raw.direct_text_read_bytes.unwrap_or(default_head_bytes),
                read_tail_bytes: raw.log_tail_read_bytes.unwrap_or(262_144),
                max_chars: text_max_chars,
                excerpt_max_chars: raw.text_excerpt_max_chars.unwrap_or(text_max_chars),
            },
            office: OfficeParseProfile {
                primary_backend: raw
                    .office_parser_backend
                    .clone()
                    .unwrap_or_else(|| "rust_office_oxide_v1".to_string()),
                fallback_enabled: raw.office_parser_fallback_enabled.unwrap_or(true),
                fallback_order: raw.office_parser_fallback_order.clone().unwrap_or_else(|| {
                    vec![
                        FallbackBackend::PythonOfficeV1,
                        FallbackBackend::PythonSharepointTextV1,
                    ]
                }),
                fallback_after_timeout: raw.office_fallback_after_timeout.unwrap_or(false),
                fallback_policy_version: raw
                    .office_fallback_policy_version
                    .clone()
                    .unwrap_or_else(|| "hybrid_v1".to_string()),
                legacy_extensions_enabled: raw.office_legacy_extensions_enabled.unwrap_or(false),
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
                backend: raw
                    .pdf_parser_backend
                    .clone()
                    .unwrap_or_else(|| "pdf_text_v1".to_string()),
                max_pages: pdf_max_pages,
                excerpt_max_chars: document_excerpt_max_chars,
            },
        },
        context: ContextProfileV2 {
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
        max_candidate_files: raw.max_candidate_files.unwrap_or(default_max_candidate_files),
        max_pdf_text_extractions: raw
            .max_pdf_text_extractions
            .unwrap_or(default_max_pdf_text_extractions),
        max_total_pdf_classification_pages: raw
            .max_total_pdf_classification_pages
            .unwrap_or(default_max_total_pdf_classification_pages),
        pdf_classification_timeout_ms: raw.pdf_classification_timeout_ms.unwrap_or(2_000),
        total_deadline_ms: raw.total_deadline_ms.unwrap_or(default_total_deadline_ms),
        session_concurrency,
        max_requests_per_session: raw.max_requests_per_session.unwrap_or(128),
        session_idle_ttl_ms: raw.session_idle_ttl_ms.unwrap_or(30_000),
        session_rss_limit_bytes: raw
            .session_rss_limit_bytes
            .unwrap_or(512 * 1024 * 1024),
    };
    profile.validate()?;
    Ok(profile)
}

/// v1 raw → v2 raw 投影（v2 是 v1 的严格超集，缺省的 v2-only 叶子在归一化
/// 时由 report-mode 冻结默认表填充）。
fn raw_v1_to_v2(raw: &RawScannerProfileV1) -> Result<RawScannerProfileV2, String> {
    let mut value = serde_json::to_value(raw).map_err(|error| error.to_string())?;
    value["schema_version"] = serde_json::json!("scanner_profile_v2");
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// v2 raw → v1 raw 投影：删除 v2-only 叶子并改回 v1 schema_version。
/// 仅用于 Plan 2 T4 接线前的生产路径，保证 v1 请求行为不变。
fn raw_v2_to_v1(raw: &RawScannerProfileV2) -> Result<RawScannerProfileV1, String> {
    let mut value = serde_json::to_value(raw).map_err(|error| error.to_string())?;
    value["schema_version"] = serde_json::json!("scanner_profile_v1");
    if let Some(object) = value.as_object_mut() {
        for field in SCANNER_PROFILE_V2_ONLY_FIELDS {
            object.remove(*field);
        }
    }
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// 生产 build-context 路径的归一化入口。v1 请求走原有 v1 归一化（行为不变）；
/// v2 请求在 Plan 2 T4 接线前投影为 v1，保持现有生产行为，不消费 v2-only 叶子。
pub fn normalize_scanner_profile_for_request(
    profile: &ScannerProfile,
    report_mode: ReportMode,
) -> Result<NormalizedScannerProfileV1, String> {
    profile.validate()?;
    match profile {
        ScannerProfile::V1(raw) => normalize_scanner_profile(raw, report_mode),
        ScannerProfile::V2(raw) => {
            let v1 = raw_v2_to_v1(raw)?;
            normalize_scanner_profile(&v1, report_mode)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_aware_defaults_preserve_explicit_worker_values() {
        let cases = [
            (0_u64, None, None, 1_u64, 1_u64),
            (2_u64, None, None, 2_u64, 2_u64),
            (16, None, None, 4, 4),
            (2, Some(6), None, 6, 2),
            (2, None, Some(5), 2, 5),
            (2, Some(6), Some(5), 6, 5),
        ];

        for (cpu_count, max_workers, session_concurrency, expected_workers, expected_session) in cases
        {
            let mut value = serde_json::json!({
                "schema_version": "scanner_profile_v2"
            });
            if let Some(max_workers) = max_workers {
                value["max_workers"] = serde_json::json!(max_workers);
            }
            if let Some(session_concurrency) = session_concurrency {
                value["session_concurrency"] = serde_json::json!(session_concurrency);
            }
            let raw: RawScannerProfileV2 =
                serde_json::from_value(value).expect("test profile must decode");
            let normalized = normalize_scanner_profile_v2_raw_with_parallelism(
                &raw,
                ReportMode::Daily,
                cpu_count,
            )
            .expect("test profile must normalize");

            assert_eq!(normalized.execution.max_workers, expected_workers);
            assert_eq!(normalized.session_concurrency, expected_session);
        }
    }
}
