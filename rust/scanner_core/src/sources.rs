//! Structured excerpts for evidence reports. No Markdown parsing or Python DB reads.

use std::{collections::HashMap, io::Read, path::Path, time::Instant};

use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheStatus, NormalizedScannerSettings, ParseStatus,
};
use chrono::{DateTime, Utc};
use serde::{de::value::StrDeserializer, Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    artifact::{ArtifactDraft, ArtifactFileRow},
    decision::ContextFileEvidence,
    source_guard::{source_guard_kind_from_text, SourceGuardObserver, SourceGuardV2},
    store::ScannerStore,
};

#[derive(Debug, Clone, Serialize)]
pub struct ScanSource {
    pub source_id: String,
    pub relative_path: String,
    pub extension: String,
    pub size_bytes: u64,
    pub modified_at_utc: String,
    pub observed_at_utc: String,
    /// Hash of the original file, distinct from the extracted text hash.
    pub content_sha256: Option<String>,
    pub excerpt_sha256: String,
    pub excerpt: String,
    pub parser_backend: String,
    pub worker_lane: AuditWorkerLane,
    pub parse_status: ParseStatus,
    pub parse_truncated: bool,
    pub excerpt_truncated: bool,
    pub coverage_limits: serde_json::Value,
    pub reason: Option<String>,
    #[serde(skip)]
    pub(crate) absolute_path: String,
    #[serde(skip)]
    pub(crate) guard: Option<SourceGuardV2>,
}

pub(crate) fn collect_sources(
    evidence: &[ContextFileEvidence],
    discovery: &[DiscoveredFileOut],
    profile: &NormalizedScannerSettings,
) -> Vec<ScanSource> {
    collect_indexed_sources(evidence, &files_by_identity(discovery), profile)
}

fn files_by_identity(discovery: &[DiscoveredFileOut]) -> HashMap<&str, &DiscoveredFileOut> {
    discovery
        .iter()
        .map(|f| (f.file_identity.as_str(), f))
        .collect()
}

fn collect_indexed_sources(
    evidence: &[ContextFileEvidence],
    files: &HashMap<&str, &DiscoveredFileOut>,
    profile: &NormalizedScannerSettings,
) -> Vec<ScanSource> {
    let mut ordered: Vec<_> = evidence.iter().collect();
    let is_ocr =
        |item: &ContextFileEvidence| item.parser_backend == crate::parsers::document::OCR_BACKEND;
    ordered.sort_by(|a, b| (is_ocr(a), &a.relative_path).cmp(&(is_ocr(b), &b.relative_path)));
    let native = ordered.partition_point(|item| !is_ocr(item));
    let allocate = |items: &[&ContextFileEvidence], cap| {
        crate::source_allocation::allocate_excerpts_with_priority(
            items,
            cap,
            &profile.parse.excerpt_priority_paths,
        )
    };
    let budget = profile.parse.aggregate_max_chars as usize;
    let mut allocated = allocate(&ordered[..native], budget);
    if native < ordered.len() {
        // Optional OCR may use only the excerpt budget left by ordinary reading.
        let used = allocated
            .iter()
            .map(|excerpt| excerpt.text.chars().count())
            .sum::<usize>();
        allocated.extend(allocate(&ordered[native..], budget.saturating_sub(used)));
    }
    let mut sources: Vec<_> = ordered
        .into_iter()
        .zip(allocated)
        .filter_map(|(item, selected)| {
            let file = files.get(item.file_identity.as_str())?;
            let relative_path = item.relative_path.replace('\\', "/");
            let excerpt = selected.text;
            let ns: i64 = file
                .source_version
                .strip_prefix("mtime_ns=")?
                .split(':')
                .next()?
                .parse()
                .ok()?;
            let modified_at_utc = DateTime::<Utc>::from_timestamp_nanos(ns).to_rfc3339();
            let reason = if selected.truncated {
                Some("excerpt_character_budget".to_string())
            } else {
                item.reason.clone()
            };
            let mut limits =
                serde_json::to_value(&profile.parse).expect("serializable parse profile");
            let mut audit = selected
                .audit
                .as_ref()
                .map(|a| serde_json::to_value(a).unwrap())
                .unwrap_or_else(|| serde_json::json!({"version": 1}));
            let status = match item.parse_status {
                ParseStatus::NotParsed => "not_parsed",
                ParseStatus::Error | ParseStatus::Timeout => "parse_failed",
                ParseStatus::Success
                    if excerpt.is_empty()
                        || selected
                            .audit
                            .as_ref()
                            .is_some_and(|a| a.usable_chars == Some(0)) =>
                {
                    "empty_excerpt"
                }
                ParseStatus::Success => "usable_excerpt",
            };
            audit["primary_status"] = serde_json::json!(status);
            if selected.ranges.iter().flatten().any(|range| {
                range.unit == ai_daily_worker_contract::SourceRangeUnit::PdfPage
                    && range.label.as_deref() == Some(crate::parsers::document::OCR_RANGE_LABEL)
            }) {
                audit["text_unverified"] = true.into();
            }
            if let Some(
                reason @ ("pdf_image_ocr_file_quota_exhausted"
                | "pdf_image_ocr_page_quota_exhausted"
                | "pdf_image_ocr_timeout"
                | "pdf_image_ocr_failed"),
            ) = reason.as_deref()
            {
                audit["primary_reason"] = reason.into();
                audit["secondary_tags"] = serde_json::json!([reason.to_ascii_uppercase()]);
            }
            if reason.as_deref() == Some("file_size_policy") {
                audit["primary_reason"] = serde_json::json!("file_size_policy");
                audit["max_file_size_bytes"] = profile.execution.max_file_size_bytes.into();
            } else if item.error.as_ref().is_some_and(|error| {
                error.message == ai_daily_worker_contract::OFFICE_CONTENT_MISSING_MESSAGE
            }) {
                audit["primary_reason"] = serde_json::json!("office_content_missing");
            }
            audit["excerpt_ranges"] = serde_json::to_value(selected.ranges).unwrap();
            if !selected.omitted_ranges.is_empty() {
                audit["omitted_ranges"] = serde_json::json!(selected.omitted_ranges);
            }
            if audit["content_units"].is_array() {
                audit["excerpt_block_ends"] = serde_json::json!(selected.block_ends);
                audit["excerpt_block_context"] = serde_json::json!(selected.block_context);
            }
            audit["parser_version"] = serde_json::json!(item.parser_backend);
            audit["algorithm_versions"] = serde_json::json!({
                "cache_payload": "1", "excerpt": crate::source_allocation::EXCERPT_POLICY_VERSION,
                "admission": profile.admission_policy_version,
                "pdf_selection": profile.parse.pdf.selection_policy_version });
            if status == "empty_excerpt"
                && item.content.is_empty()
                && !item.truncated
                && item.evidence_audit.is_some()
            {
                audit["primary_reason"] = if is_ocr(item) {
                    "pdf_image_ocr_empty"
                } else {
                    "empty_document"
                }
                .into();
            }
            if is_ocr(item) {
                audit["algorithm_versions"]["pdf_image_ocr"] =
                    crate::parsers::document::OCR_BACKEND.into();
                audit["algorithm_versions"]["parser_content_identity"] =
                    crate::PARSER_CONTENT_IDENTITY.into();
            }
            limits["evidence_audit"] = audit;
            Some(ScanSource {
                source_id: format!("src_{}", crate::store::sha256_hex(relative_path.as_bytes())),
                relative_path,
                extension: item.extension.clone(),
                size_bytes: file.size_bytes,
                modified_at_utc,
                observed_at_utc: Utc::now().to_rfc3339(),
                content_sha256: None,
                excerpt_sha256: crate::store::sha256_hex(excerpt.as_bytes()),
                excerpt,
                parser_backend: item.parser_backend.clone(),
                worker_lane: item.worker_lane,
                parse_status: item.parse_status,
                parse_truncated: item.truncated,
                excerpt_truncated: selected.truncated,
                coverage_limits: limits,
                reason,
                absolute_path: item.absolute_path.clone(),
                guard: file
                    .source_guard_kind
                    .as_deref()
                    .and_then(source_guard_kind_from_text)
                    .map(|kind| SourceGuardV2 {
                        kind,
                        guard_sha256: file.source_guard_sha256.clone(),
                    }),
            })
        })
        .collect();
    if native < evidence.len() {
        sources.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    }
    sources
}

/// Snapshot reuse is allowed only while its structured parse entries survive.
/// An evicted entry causes the ordinary parser/cache path to run again.
pub(crate) fn snapshot_sources(
    store: &ScannerStore,
    artifact: &ArtifactDraft,
    page_audits: &std::collections::BTreeMap<String, ai_daily_scanner_contract::PdfPageAudit>,
    discovery: &[DiscoveredFileOut],
    profile: &NormalizedScannerSettings,
) -> Option<Vec<ScanSource>> {
    let files = files_by_identity(discovery);
    // Keep the first matching decision, including for historical inputs.
    let reasons: HashMap<_, _> = artifact
        .decision_rows
        .iter()
        .rev()
        .map(|row| (row.file_identity.as_str(), &row.reason))
        .collect();
    let mut entries = store.lookup_snapshot_cache(
        artifact
            .file_rows
            .iter()
            .filter(|row| has_parse_payload(row)),
    )?;
    let mut evidence = Vec::with_capacity(artifact.file_rows.len());
    for row in &artifact.file_rows {
        let file = files.get(row.file_identity.as_str())?;
        if row.parser_backend == crate::parsers::document::OCR_BACKEND {
            // Reclassify if retention removed the proof needed to replay this OCR result.
            let classifier = row.classifier.as_ref()?;
            if classifier.status
                != ai_daily_scanner_contract::PdfClassificationStatus::NoTextInParseWindow
                || classifier.page_count != classifier.result_examined_pages
                || !page_audits
                    .get(&row.file_identity)?
                    .all_pages_are_images(classifier.page_count?)
            {
                return None;
            }
        }
        let (content, evidence_audit) = if has_parse_payload(row) {
            let entry = entries.remove(&row.file_identity)?;
            if entry.content_sha256 != row.content_sha256
                || entry.parser_backend != row.parser_backend
                || entry.worker_lane != row.worker_lane
            {
                return None;
            }
            (entry.content, entry.evidence_audit)
        } else {
            (String::new(), None)
        };
        evidence.push(ContextFileEvidence {
            evidence_audit,
            file_identity: row.file_identity.clone(),
            absolute_path: file.path.clone(),
            relative_path: row.relative_path.clone(),
            extension: file.extension.clone(),
            size_bytes: Some(file.size_bytes),
            content,
            parser_backend: row.parser_backend.clone(),
            worker_lane: AuditWorkerLane::deserialize(
                StrDeserializer::<serde::de::value::Error>::new(&row.worker_lane),
            )
            .ok()?,
            cache_status: CacheStatus::Fresh,
            parse_status: row.parse_status,
            truncated: row.truncated,
            error: None,
            reason: reasons
                .get(row.file_identity.as_str())
                .map(|reason| (*reason).clone()),
        });
    }
    Some(collect_indexed_sources(&evidence, &files, profile))
}

fn has_parse_payload(row: &ArtifactFileRow) -> bool {
    row.parse_status == ParseStatus::Success && row.parser_backend != "pdf_metadata_v2"
}

pub(crate) fn fingerprint_sources(
    sources: &mut [ScanSource],
    root: &Path,
    observer: &SourceGuardObserver,
    max_bytes: u64,
    deadline: Instant,
) {
    for source in sources {
        if source.parse_status != ParseStatus::Success || source.excerpt.is_empty() {
            continue;
        }
        let path = Path::new(&source.absolute_path);
        let hash = source.guard.as_ref().and_then(|guard| {
            let modified = DateTime::parse_from_rfc3339(&source.modified_at_utc).ok()?;
            let matches = || {
                std::fs::metadata(path).is_ok_and(|metadata| {
                    metadata.len() == source.size_bytes
                        && metadata
                            .modified()
                            .is_ok_and(|time| DateTime::<Utc>::from(time) == modified)
                }) && observer.verify(path, guard)
            };
            if !matches() {
                return None;
            }
            let hash = bounded_content_hash(path, root, max_bytes, deadline)?;
            matches().then_some(hash)
        });
        if let Some(hash) = hash {
            source.content_sha256 = Some(hash);
        } else {
            source.parse_status = ParseStatus::Error;
            source.reason = Some("source_unstable_or_fingerprint_unavailable".to_string());
            source.excerpt.clear();
            source.excerpt_sha256 = crate::store::sha256_hex(b"");
            if let Some(audit) = source.coverage_limits.get_mut("evidence_audit") {
                audit["usable_chars"] = serde_json::json!(0);
                for field in [
                    "content_units",
                    "excerpt_ranges",
                    "excerpt_block_ends",
                    "excerpt_block_context",
                ] {
                    audit[field] = serde_json::json!([]);
                }
                if let Some(fields) = audit.as_object_mut() {
                    fields.remove("omitted_ranges");
                }
                audit["primary_status"] = serde_json::json!("parse_failed");
            }
        }
    }
}

fn bounded_content_hash(
    path: &Path,
    root: &Path,
    max_bytes: u64,
    deadline: Instant,
) -> Option<String> {
    if Instant::now() >= deadline || !ai_daily_discovery::local_candidate_is_readable(path, root) {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > max_bytes {
        return None;
    }
    let mut reader = file.take(max_bytes.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0u8; 65536];
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let read = reader.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        if bytes > max_bytes {
            return None;
        }
        hasher.update(&buffer[..read]);
    }
    Some(ai_daily_worker_contract::archive::hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn short_text_audit_drops_omitted_ranges_when_source_verification_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = ScanSource {
            source_id: "synthetic".into(),
            relative_path: "note.md".into(),
            extension: ".md".into(),
            size_bytes: 24,
            modified_at_utc: Utc::now().to_rfc3339(),
            observed_at_utc: Utc::now().to_rfc3339(),
            content_sha256: None,
            excerpt_sha256: crate::store::sha256_hex(b"fact"),
            excerpt: "fact".into(),
            parser_backend: "light_text_v2".into(),
            worker_lane: AuditWorkerLane::RustCore,
            parse_status: ParseStatus::Success,
            parse_truncated: true,
            excerpt_truncated: false,
            coverage_limits: json!({"evidence_audit":{"omitted_ranges":[
                {"unit":"byte","start":4,"end":24,"label":"unread"}]}}),
            reason: None,
            absolute_path: dir.path().join("note.md").to_string_lossy().into_owned(),
            guard: None,
        };
        fingerprint_sources(
            std::slice::from_mut(&mut source),
            dir.path(),
            &SourceGuardObserver::default(),
            100,
            Instant::now(),
        );
        assert_eq!(source.parse_status, ParseStatus::Error);
        assert_eq!(
            source.coverage_limits["evidence_audit"]["primary_status"],
            "parse_failed"
        );
        assert!(source.coverage_limits["evidence_audit"]
            .get("omitted_ranges")
            .is_none());
    }
}
