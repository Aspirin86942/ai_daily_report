use super::*;

/// Loads per-file source-guard and classifier provenance for `FileAuditV2`
/// assembly. `artifact_id` selects the current run's artifact rows.
pub(super) fn load_file_audits_v2(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    artifact_id: Option<i64>,
    metadata_json: Option<&str>,
) -> Result<Vec<FileAuditV2Source>, EvidenceAuditError> {
    #[derive(Default, serde::Deserialize)]
    struct PageAudits {
        #[serde(default)]
        pdf_page_audits: BTreeMap<String, ai_daily_scanner_contract::PdfPageAudit>,
        #[serde(default)]
        pdf_open_failures: BTreeMap<String, String>,
    }
    let mut audits = metadata_json
        .map(serde_json::from_str::<PageAudits>)
        .transpose()
        .map_err(|error| EvidenceAuditError::RunCorrupt(error.to_string()))?
        .unwrap_or_default();
    let mut statement = transaction.prepare(
        "SELECT fr.relative_path, fr.file_identity, fr.source_version,
                fi.source_guard_kind, fi.source_guard_sha256, fi.file_type, fi.size_bytes,
                fr.parse_status, fr.parser_backend, fr.worker_lane,
                fr.parse_cache_status, fr.cache_miss_reason,
                fr.truncated, fr.content_sha256, fr.parse_duration_ms,
                fr.failure_class, fr.fallback_backend, fr.fallback_reason_code,
                fr.error_code, fr.error_message, fr.error_retryable, fr.error_stage,
                fr.error_file_path, fr.error_backend,
                af.classifier_status, af.classifier_page_count,
                af.classifier_result_examined_pages, af.classifier_nominal_charged_pages,
                af.classifier_build, af.classifier_profile_hash,
                fx.parse_transport, fx.parse_attempt_count,
                fx.classification_status, fx.classification_page_count,
                fx.classification_cache_status, fx.classification_cache_miss_reason,
                fx.classification_result_examined_pages, fx.classification_run_inspected_pages,
                fx.classification_nominal_charged_pages, fx.classification_duration_ms,
                fx.classification_transport, fx.classification_attempt_count,
                fx.classifier_build, fx.classifier_profile_hash
         FROM scan_file_results fr
         LEFT JOIN file_inventory fi ON fi.file_identity = fr.file_identity
         LEFT JOIN context_artifact_files af
             ON af.file_identity = fr.file_identity AND af.artifact_id = ?2
         LEFT JOIN scan_file_execution fx
             ON fx.scan_run_id = fr.scan_run_id AND fx.file_identity = fr.file_identity
         WHERE fr.scan_run_id = ?1
         ORDER BY lower(fr.relative_path), fr.relative_path, fr.file_identity",
    )?;
    let mut query = statement.query([scan_run_id, artifact_id.unwrap_or(-1)])?;
    let mut files = Vec::new();
    while let Some(row) = query.next()? {
        let identity: String = row.get(1)?;
        let parse_status = parse_enum(&row.get::<_, String>(7)?, "file v2 parse_status")?;
        let backend: String = row.get(8)?;
        let final_diagnostic = match parse_status {
            ParseStatus::Error | ParseStatus::Timeout => Some(
                assemble_final_diagnostic(
                    row.get(18)?,
                    row.get(19)?,
                    row.get(20)?,
                    row.get(21)?,
                    row.get(22)?,
                    row.get(23)?,
                )?
                .ok_or_else(|| {
                    EvidenceAuditError::RunCorrupt(
                        "error/timeout file audit is missing its final diagnostic".into(),
                    )
                })?,
            ),
            ParseStatus::Success | ParseStatus::NotParsed => None,
        };
        let classifier = if empty_columns(row, 24..30)? {
            None
        } else {
            let reason = "classifier provenance columns are inconsistent";
            Some(PdfClassificationProvenanceV1 {
                status: parse_enum(&required::<String>(row, 24, reason)?, "classifier status")?,
                page_count: to_positive_u64(row.get(25)?, "classifier page_count")?,
                result_examined_pages: to_positive_u64(row.get(26)?, "classifier result pages")?,
                nominal_charged_pages: to_u64_opt(row.get(27)?, "classifier nominal pages")?,
                classifier_build: required(row, 28, reason)?,
                classifier_profile_hash: required(row, 29, reason)?,
            })
        };
        let (parse_transport, parse_attempt_count) = match (
            row.get::<_, Option<String>>(30)?,
            row.get::<_, Option<i64>>(31)?,
        ) {
            (Some(transport), Some(attempts)) => (
                Some(parse_enum(&transport, "file v2 parse_transport")?),
                Some(audit_count(attempts, "file v2 parse_attempt_count")?),
            ),
            (None, None) => (None, None),
            _ => {
                return Err(EvidenceAuditError::RunCorrupt(
                    "file execution provenance columns are inconsistent".into(),
                ))
            }
        };
        let classification_execution = if empty_columns(row, 32..44)? {
            None
        } else {
            let reason = "classification execution columns are inconsistent";
            let token = |index| required::<String>(row, index, reason);
            let count = |index, field| audit_count(required(row, index, reason)?, field);
            let audit = PdfClassificationAuditV1 {
                pdf_open_failure_key: audits.pdf_open_failures.remove(&identity),
                page_audit: audits.pdf_page_audits.remove(&identity),
                status: parse_enum(&token(32)?, "classification execution status")?,
                page_count: Nullable(to_positive_u64(
                    row.get(33)?,
                    "classification execution page_count",
                )?),
                classification_cache_status: parse_enum(
                    &token(34)?,
                    "classification execution cache_status",
                )?,
                classification_cache_miss_reason: token(35)?,
                result_examined_pages: Nullable(to_positive_u64(
                    row.get(36)?,
                    "classification execution result pages",
                )?),
                run_inspected_pages: Nullable(to_positive_u64(
                    row.get(37)?,
                    "classification execution run pages",
                )?),
                nominal_charged_pages: count(38, "classification execution nominal pages")?,
                duration_ms: count(39, "classification execution duration")?,
                transport: parse_enum(&token(40)?, "classification execution transport")?,
                attempt_count: count(41, "classification execution attempt count")?,
                classifier_build: token(42)?,
                classifier_profile_hash: token(43)?,
            };
            audit.validate().map_err(|message| {
                EvidenceAuditError::RunCorrupt(format!(
                    "classification execution audit is invalid: {message}"
                ))
            })?;
            Some(audit)
        };
        if backend == crate::parsers::document::OCR_BACKEND
            && classification_execution
                .as_ref()
                .and_then(PdfClassificationAuditV1::image_only_page_count)
                .is_none()
        {
            return Err(EvidenceAuditError::RunCorrupt(
                "PDF OCR is missing full-document image observations".into(),
            ));
        }
        files.push(FileAuditV2Source {
            relative_path: row.get(0)?,
            file_identity: identity,
            source_version: row.get(2)?,
            source_guard_kind: row.get(3)?,
            source_guard_sha256: row.get(4)?,
            file_type: row.get(5)?,
            size_bytes: to_u64_opt(Some(row.get(6)?), "file size_bytes")?,
            parse_status,
            parser_backend: backend,
            worker_lane: parse_enum(&row.get::<_, String>(9)?, "file v2 worker_lane")?,
            parse_cache_status: row.get(10)?,
            cache_miss_reason: row.get(11)?,
            truncated: parse_bool(row.get(12)?, "file v2 truncated")?,
            content_sha256: row.get(13)?,
            parse_duration_ms: to_u64_opt(Some(row.get(14)?), "file v2 parse_duration_ms")?,
            failure_class: row.get(15)?,
            fallback_backend: row.get(16)?,
            fallback_reason_code: row.get(17)?,
            parse_transport,
            parse_attempt_count,
            final_diagnostic,
            classifier,
            classification_execution,
        });
    }
    if !audits.pdf_open_failures.is_empty() {
        return Err(EvidenceAuditError::RunCorrupt(
            "orphan PDFium open failure".into(),
        ));
    }
    if !audits.pdf_page_audits.is_empty() {
        return Err(EvidenceAuditError::RunCorrupt(
            "orphan PDF page audit".into(),
        ));
    }
    Ok(files)
}

fn empty_columns(
    row: &rusqlite::Row<'_>,
    range: std::ops::Range<usize>,
) -> Result<bool, EvidenceAuditError> {
    for index in range {
        if row.get_ref(index)? != rusqlite::types::ValueRef::Null {
            return Ok(false);
        }
    }
    Ok(true)
}

fn required<T: rusqlite::types::FromSql>(
    row: &rusqlite::Row<'_>,
    index: usize,
    reason: &str,
) -> Result<T, EvidenceAuditError> {
    row.get::<_, Option<T>>(index)?
        .ok_or_else(|| EvidenceAuditError::RunCorrupt(reason.into()))
}

fn audit_count(value: i64, field: &str) -> Result<u64, EvidenceAuditError> {
    u64::try_from(value).map_err(|_| EvidenceAuditError::RunCorrupt(format!("{field} is negative")))
}

/// Rebuilds the nullable `final_diagnostic` from the persisted error columns.
fn assemble_final_diagnostic(
    error_code: Option<String>,
    error_message: Option<String>,
    error_retryable: Option<i64>,
    error_stage: Option<String>,
    error_file_path: Option<String>,
    error_backend: Option<String>,
) -> Result<Option<Diagnostic>, EvidenceAuditError> {
    match (error_code, error_message, error_retryable, error_stage) {
        (None, None, None, None) => Ok(None),
        (Some(code), Some(message), Some(retryable), Some(stage)) => {
            let diagnostic = Diagnostic {
                error_code: parse_enum(&code, "final diagnostic error_code")?,
                message,
                retryable: parse_bool(retryable, "diagnostic retryable")?,
                stage: parse_enum(&stage, "final diagnostic stage")?,
                file_path: Nullable(error_file_path),
                backend: Nullable(error_backend),
            };
            diagnostic.validate().map_err(|_| {
                EvidenceAuditError::RunCorrupt("final diagnostic is invalid".to_string())
            })?;
            Ok(Some(diagnostic))
        }
        _ => Err(EvidenceAuditError::RunCorrupt(
            "persisted final diagnostic is partially populated".to_string(),
        )),
    }
}
