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
    let mut remaining = profile.parse.aggregate_max_chars as usize;
    ordered
        .into_iter()
        .filter_map(|item| {
            let file = files.get(item.file_identity.as_str())?;
            let relative_path = item.relative_path.replace('\\', "/");
            let full_chars = item.content.chars().count();
            let take = full_chars.min(remaining);
            let excerpt: String = item.content.chars().take(take).collect();
            remaining -= take;
            let ns: i64 = file
                .source_version
                .strip_prefix("mtime_ns=")?
                .split(':')
                .next()?
                .parse()
                .ok()?;
            let modified_at_utc = DateTime::<Utc>::from_timestamp_nanos(ns).to_rfc3339();
            let reason = if take < full_chars {
                Some("excerpt_character_budget".to_string())
            } else {
                item.reason.clone()
            };
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
                excerpt_truncated: take < full_chars,
                coverage_limits: serde_json::to_value(&profile.parse)
                    .expect("serializable parse profile"),
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
        let content = if row.parse_status == ParseStatus::Success {
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
            {
                CacheLookup::Fresh(entry) if entry.content_sha256 == row.content_sha256 => {
                    entry.content
                }
                _ => return None,
            }
        } else {
            String::new()
        };
        evidence.push(ContextFileEvidence {
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
