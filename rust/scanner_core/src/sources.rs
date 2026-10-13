//! Structured excerpts for evidence reports. No Markdown parsing or Python DB reads.

use std::{collections::HashMap, io::Read, path::Path, time::Instant};

use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheStatus, NormalizedScannerSettings, ParseStatus,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    artifact::ArtifactDraft,
    decision::ContextFileEvidence,
    source_guard::{source_guard_kind_from_text, SourceGuardObserver, SourceGuardV2},
    store::{CacheLookup, ScannerStore},
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
    let files: HashMap<_, _> = discovery
        .iter()
        .map(|f| (f.file_identity.as_str(), f))
        .collect();
    let mut ordered: Vec<_> = evidence.iter().collect();
    ordered.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    let allocated = crate::source_allocation::allocate_excerpts_with_priority(
        &ordered,
        profile.parse.aggregate_max_chars as usize,
        &profile.parse.excerpt_priority_paths,
    );
    ordered
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
            let status = if item.parse_status == ParseStatus::NotParsed {
                "not_parsed"
            } else if item.parse_status != ParseStatus::Success {
                "parse_failed"
            } else if excerpt.is_empty()
                || selected
                    .audit
                    .as_ref()
                    .is_some_and(|a| a.usable_chars == Some(0))
            {
                "empty_excerpt"
            } else {
                "usable_excerpt"
            };
            audit["primary_status"] = serde_json::json!(status);
            audit["excerpt_ranges"] = serde_json::to_value(selected.ranges).unwrap();
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
                audit["primary_reason"] = serde_json::json!("empty_document");
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
        .collect()
}

/// Snapshot reuse is allowed only while its structured parse entries survive.
/// An evicted entry causes the ordinary parser/cache path to run again.
pub(crate) fn snapshot_sources(
    store: &ScannerStore,
    artifact: &ArtifactDraft,
    discovery: &[DiscoveredFileOut],
    profile: &NormalizedScannerSettings,
) -> Option<Vec<ScanSource>> {
    let files: HashMap<_, _> = discovery
        .iter()
        .map(|f| (f.file_identity.as_str(), f))
        .collect();
    let mut evidence = Vec::new();
    for row in &artifact.file_rows {
        let file = files.get(row.file_identity.as_str())?;
        let (content, evidence_audit) = if row.parser_backend == "pdf_metadata_v2" {
            (String::new(), None)
        } else if row.parse_status == ParseStatus::Success {
            match store
                .lookup_cache(
                    &row.file_identity,
                    &row.legacy_source_version,
                    row.source_guard_kind.as_deref()?,
                    row.source_guard_sha256.as_deref()?,
                    &row.parse_profile_hash,
                    true,
                )
                .ok()?
                .decode_payload(crate::store::cache::PARSE_CACHE_PAYLOAD_VERSION)
            {
                CacheLookup::Fresh(entry) if entry.content_sha256 == row.content_sha256 => {
                    (entry.content, entry.evidence_audit)
                }
                _ => return None,
            }
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
            worker_lane: serde_json::from_value(serde_json::Value::String(row.worker_lane.clone()))
                .ok()?,
            cache_status: CacheStatus::Fresh,
            parse_status: row.parse_status,
            truncated: row.truncated,
            error: None,
            reason: artifact
                .decision_rows
                .iter()
                .find(|d| d.file_identity == row.file_identity)
                .map(|d| d.reason.clone()),
        });
    }
    Some(collect_sources(&evidence, discovery, profile))
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
            if !metadata_matches_discovery(path, source) || !observer.verify(path, guard) {
                return None;
            }
            let hash = bounded_content_hash(path, root, max_bytes, deadline)?;
            (metadata_matches_discovery(path, source) && observer.verify(path, guard))
                .then_some(hash)
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
                audit["content_units"] = serde_json::json!([]);
                audit["excerpt_ranges"] = serde_json::json!([]);
                audit["primary_status"] = serde_json::json!("parse_failed");
            }
        }
    }
}

fn metadata_matches_discovery(path: &Path, source: &ScanSource) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(expected) = DateTime::parse_from_rfc3339(&source.modified_at_utc) else {
        return false;
    };
    metadata.len() == source.size_bytes && DateTime::<Utc>::from(modified) == expected
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
    let mut reader = std::io::BufReader::new(file).take(max_bytes.saturating_add(1));
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
    Some(
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}
