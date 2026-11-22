use daily_parsers::settings::{
    ContextProfile, DiscoveryProfile, ExecutionProfile, NormalizedScannerSettings,
    OfficeParseProfile, ParseProfile, PdfParseProfile, ReportMode, ScannerSettings,
    TextParseProfile, Validate, ADMISSION_POLICY_VERSION, CLASSIFIER_POLICY_VERSION,
};
use serde::{ser::SerializeStruct, Serialize, Serializer};
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
    if report_mode != ReportMode::Daily {
        return Err("only daily scan profiles are supported".into());
    }
    let worker_memory = raw.worker_rss_limit_bytes.unwrap_or(DEFAULT_WORKER_MEMORY);
    if !cfg!(windows) && worker_memory != DEFAULT_WORKER_MEMORY {
        return Err("an explicit worker_rss_limit_bytes requires Windows Job accounting".into());
    }
    let text_max_chars = raw.text_max_chars.unwrap_or(6_000);
    let document_excerpt_max_chars = raw.document_excerpt_max_chars.unwrap_or(text_max_chars);
    let default_head_bytes = raw.direct_text_max_bytes.unwrap_or(262_144);
    let global_max_chars = raw.context_max_chars.unwrap_or(500_000);
    let max_file_size_bytes = raw.max_file_size_mb.unwrap_or(50) * MIB;

    let settings = NormalizedScannerSettings {
        report_mode,
        discovery: DiscoveryProfile {
            allowed_extensions: canonical_strings(
                raw.allowed_extensions.as_deref(),
                DEFAULT_ALLOWED_EXTENSIONS,
            ),
            ignored_patterns: canonical_strings(
                raw.ignored_patterns.as_deref(),
                DEFAULT_IGNORED_PATTERNS,
            ),
            excluded_dirs: canonical_strings(raw.excluded_dirs.as_deref(), &[]),
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
                excel_max_sheets: raw.excel_max_sheets.unwrap_or(5),
                excel_max_rows: raw.excel_max_rows.unwrap_or(50),
                excel_max_columns: raw.excel_max_columns.unwrap_or(20),
                docx_max_paragraphs: raw.docx_max_paragraphs.unwrap_or(200),
                docx_max_tables: raw.docx_max_tables.unwrap_or(20),
                docx_table_max_rows: raw.docx_table_max_rows.unwrap_or(50),
                docx_table_max_cols: raw.docx_table_max_cols.unwrap_or(12),
                pptx_max_slides: raw.pptx_max_slides.unwrap_or(50),
                pptx_include_notes: raw.pptx_include_notes.unwrap_or(true),
                document_excerpt_max_chars,
            },
            pdf: PdfParseProfile {
                image_ocr: raw.pdf_image_ocr.filter(|ocr| ocr.enabled),
                selection_policy_version: CLASSIFIER_POLICY_VERSION.to_string(),
                backend: "rust_pdfium_v3".to_string(),
                max_pages: raw.pdf_max_pages.unwrap_or(5),
                excerpt_max_chars: document_excerpt_max_chars,
            },
        },
        context: ContextProfile { global_max_chars },
        admission_policy_version: ADMISSION_POLICY_VERSION.to_string(),
        classifier_policy_version: CLASSIFIER_POLICY_VERSION.to_string(),
        max_candidate_files: raw.max_candidate_files.unwrap_or(96),
        max_pdf_text_extractions: raw.max_pdf_text_extractions.unwrap_or(8),
        max_total_pdf_classification_pages: raw.max_total_pdf_classification_pages.unwrap_or(80),
        pdf_classification_timeout_ms: raw.pdf_classification_timeout_ms.unwrap_or(2_000),
        total_deadline_ms: raw.total_deadline_ms.unwrap_or(120_000).min(120_000),
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

// The v4 cache hashes the JSON bytes, including its retired context fields and field order.
#[derive(Serialize)]
#[serde(transparent)]
pub(super) struct SnapshotRules<'a>(
    #[serde(with = "SnapshotProfile")] pub &'a NormalizedScannerSettings,
);

#[derive(Serialize)]
#[serde(remote = "NormalizedScannerSettings")]
struct SnapshotProfile {
    report_mode: ReportMode,
    discovery: DiscoveryProfile,
    execution: ExecutionProfile,
    parse: ParseProfile,
    #[serde(serialize_with = "snapshot_context")]
    context: ContextProfile,
    admission_policy_version: String,
    classifier_policy_version: String,
    max_candidate_files: u64,
    max_pdf_text_extractions: u64,
    max_total_pdf_classification_pages: u64,
    pdf_classification_timeout_ms: u64,
    total_deadline_ms: u64,
    worker_max_requests: u64,
    worker_idle_ttl_ms: u64,
    worker_rss_limit_bytes: u64,
}

fn snapshot_context<S: Serializer>(
    profile: &ContextProfile,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut context = serializer.serialize_struct("ContextProfile", 8)?;
    context.serialize_field("profile_name", "daily_balanced_v1")?;
    context.serialize_field("global_max_chars", &profile.global_max_chars)?;
    context.serialize_field("per_file_max_chars", &100_000_u64)?;
    context.serialize_field("small_file_max_bytes", &65_536_u64)?;
    context.serialize_field("medium_file_max_bytes", &1_048_576_u64)?;
    context.serialize_field("large_file_max_bytes", &10_485_760_u64)?;
    context.serialize_field("priority_policy_version", "budget_nominal_v2")?;
    context.serialize_field("compression_policy_version", "markdown_context_v2")?;
    context.end()
}

fn canonical_strings(configured: Option<&[String]>, defaults: &[&str]) -> Vec<String> {
    let normalize = |value: &str| value.trim().to_owned();
    let mut values: Vec<_> = match configured {
        Some(values) => values.iter().map(|value| normalize(value)).collect(),
        None => defaults.iter().map(|value| normalize(value)).collect(),
    };
    values.sort_unstable();
    values.dedup();
    values
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
    fn daily_snapshot_rules_preserve_v4_bytes() {
        let raw = ScannerSettings {
            max_workers: Some(1),
            ..Default::default()
        };
        let profile = normalize_scanner_settings(&raw, ReportMode::Daily).unwrap();
        let bytes = serde_json::to_vec(&SnapshotRules(&profile)).unwrap();
        // Captured from the settings element of the v4 snapshot key at 244ebd7.
        assert_eq!(bytes.len(), 1793);
        assert_eq!(
            crate::scan::sha256_hex(&bytes),
            "d00dcb35660e8c41541675a62838798469a9dffa9086258815c30369f671404f"
        );
    }

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
