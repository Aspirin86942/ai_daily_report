//! Route-aware successful parse cache, deterministic profile fingerprints, and
//! the success-only PDF classification cache (spec Part 3/4).

use ai_daily_scanner_contract::{
    CacheMissReason, NormalizedScannerProfileV1, NormalizedScannerProfileV2, Validate,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::classifier::ParserRoute;
use crate::planner::PlannedFile;

pub const PARSE_PROFILE_HASH_ALGORITHM: &str = "sha256-parse-profile-v1";
pub const CLASSIFIER_PROFILE_HASH_ALGORITHM: &str = "sha256-classifier-profile-v1";
const CLASSIFIER_DOMAIN: &[u8] = b"classifier-profile-v1\0";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteStackFingerprint {
    route: String,
    members: Vec<RouteStackMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct RouteStackMember {
    component: String,
    contract: String,
    build: String,
}

impl RouteStackFingerprint {
    pub fn text(engine_build: &str) -> Result<Self, String> {
        Self::from_members("text_like", vec![RouteStackMember::engine(engine_build)])
    }

    pub fn modern_office(
        engine_build: &str,
        office_contract: &str,
        office_build: &str,
        python_fallback: Option<(&str, &str)>,
    ) -> Result<Self, String> {
        let mut members = vec![
            RouteStackMember::engine(engine_build),
            RouteStackMember::worker("office_worker", office_contract, office_build),
        ];
        if let Some((contract, build)) = python_fallback {
            members.push(RouteStackMember::worker("python_worker", contract, build));
        }
        Self::from_members("modern_office", members)
    }

    pub fn python_document(
        engine_build: &str,
        python_contract: &str,
        python_build: &str,
    ) -> Result<Self, String> {
        Self::from_members(
            "python_document",
            vec![
                RouteStackMember::engine(engine_build),
                RouteStackMember::worker("python_worker", python_contract, python_build),
            ],
        )
    }

    fn from_members(route: &str, members: Vec<RouteStackMember>) -> Result<Self, String> {
        if route.is_empty()
            || route.chars().count() > 1_024
            || members.is_empty()
            || members.iter().any(|member| {
                member.component.is_empty()
                    || member.component.chars().count() > 1_024
                    || member.contract.is_empty()
                    || member.contract.chars().count() > 1_024
                    || member.build.is_empty()
                    || member.build.chars().count() > 4_096
            })
        {
            return Err("route stack fingerprint is incomplete".to_string());
        }
        let unique_components: std::collections::HashSet<&str> = members
            .iter()
            .map(|member| member.component.as_str())
            .collect();
        if unique_components.len() != members.len() {
            return Err("route stack fingerprint components must be unique".to_string());
        }
        Ok(Self {
            route: route.to_string(),
            members,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteStackFingerprints {
    pub text_like: RouteStackFingerprint,
    pub modern_office: RouteStackFingerprint,
    pub python_document: RouteStackFingerprint,
}

impl RouteStackFingerprints {
    pub(crate) fn for_route(&self, route: ParserRoute) -> &RouteStackFingerprint {
        match route {
            ParserRoute::LightText => &self.text_like,
            ParserRoute::RustOffice | ParserRoute::RustXlsx => &self.modern_office,
            ParserRoute::Pdf | ParserRoute::PythonOffice | ParserRoute::PythonSharepointText => {
                &self.python_document
            }
        }
    }
}

impl RouteStackMember {
    fn engine(build: &str) -> Self {
        Self::worker("engine", "ai_daily_context_v1", build)
    }

    fn worker(component: &str, contract: &str, build: &str) -> Self {
        Self {
            component: component.to_string(),
            contract: contract.to_string(),
            build: build.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheWriteRecord {
    pub file_identity: String,
    pub source_version: String,
    pub parse_profile_hash: String,
    pub content: String,
    pub content_sha256: String,
    pub parser_backend: String,
    pub worker_lane: String,
    pub truncated: bool,
    pub worker_contract_version: String,
    pub worker_version: String,
    pub worker_build: String,
}

impl CacheWriteRecord {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.file_identity.is_empty()
            || self.file_identity.chars().count() > 4_096
            || self.source_version.is_empty()
            || super::inventory::parse_source_version(&self.source_version).is_err()
            || !super::inventory::is_sha256(&self.parse_profile_hash)
            || !super::inventory::is_sha256(&self.content_sha256)
            || sha256_hex(self.content.as_bytes()) != self.content_sha256
            || self.parser_backend.is_empty()
            || self.parser_backend.chars().count() > 1_024
            || self.worker_lane.is_empty()
            || self.worker_lane.chars().count() > 1_024
            || self.worker_contract_version.is_empty()
            || self.worker_contract_version.chars().count() > 1_024
            || self.worker_version.is_empty()
            || self.worker_version.chars().count() > 1_024
            || self.worker_build.is_empty()
            || self.worker_build.chars().count() > 4_096
        {
            return Err("invalid successful cache record".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub content: String,
    pub content_sha256: String,
    pub parser_backend: String,
    pub worker_lane: String,
    pub truncated: bool,
    pub worker_contract_version: String,
    pub worker_version: String,
    pub worker_build: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup {
    Fresh(CacheEntry),
    Miss(CacheMissReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheAwarePlanEntry {
    pub planned: PlannedFile,
    pub parse_profile_hash: Option<String>,
    pub cache_lookup: Option<CacheLookup>,
}

#[derive(Serialize)]
struct ParseProfileFingerprint<'a> {
    protocol_version: u64,
    route_stack: &'a RouteStackFingerprint,
    parser_profile_version: &'a str,
    max_file_size_bytes: u64,
    default_timeout_ms: u64,
    timeout_by_extension_ms: &'a std::collections::BTreeMap<String, u64>,
    parse: &'a ai_daily_scanner_contract::ParseProfile,
}

pub fn parse_profile_hash(
    protocol_version: u64,
    route_stack: &RouteStackFingerprint,
    profile: &NormalizedScannerProfileV1,
) -> Result<String, String> {
    if protocol_version != 1 {
        return Err("parse profile hash protocol must be v1".to_string());
    }
    profile.validate()?;
    let input = ParseProfileFingerprint {
        protocol_version,
        route_stack,
        parser_profile_version: &profile.parser_profile_version,
        max_file_size_bytes: profile.execution.max_file_size_bytes,
        default_timeout_ms: profile.execution.file_timeout_ms,
        timeout_by_extension_ms: &profile.execution.file_timeout_by_extension_ms,
        parse: &profile.parse,
    };
    let canonical_json = serde_json::to_vec(&input).map_err(|error| error.to_string())?;
    Ok(domain_hash(b"parse-profile-v1\0", &canonical_json))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_bytes(&digest)
}

pub(crate) fn domain_hash(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    hex_bytes(&hasher.finalize())
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

pub(crate) fn lookup_cache(
    connection: &Connection,
    file_identity: &str,
    source_version: &str,
    parse_profile_hash: &str,
    inventory_existed_before: bool,
) -> rusqlite::Result<CacheLookup> {
    let fresh = connection
        .query_row(
            "SELECT content, content_sha256, parser_backend, worker_lane, truncated,
                    worker_contract_version, worker_version, worker_build
             FROM parse_cache
             WHERE file_identity=?1 AND source_version=?2 AND parse_profile_hash=?3",
            params![file_identity, source_version, parse_profile_hash],
            |row| {
                Ok(CacheEntry {
                    content: row.get(0)?,
                    content_sha256: row.get(1)?,
                    parser_backend: row.get(2)?,
                    worker_lane: row.get(3)?,
                    truncated: row.get::<_, i64>(4)? != 0,
                    worker_contract_version: row.get(5)?,
                    worker_version: row.get(6)?,
                    worker_build: row.get(7)?,
                })
            },
        )
        .optional()?;
    if let Some(entry) = fresh {
        if sha256_hex(entry.content.as_bytes()) == entry.content_sha256 {
            return Ok(CacheLookup::Fresh(entry));
        }
        // Corrupt cache bytes are never returned as successful content.
        return Ok(CacheLookup::Miss(CacheMissReason::ErrorCache));
    }

    let exact_uncached_result: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM scan_file_results
            WHERE file_identity=?1 AND source_version=?2 AND parse_profile_hash=?3
         )",
        params![file_identity, source_version, parse_profile_hash],
        |row| row.get(0),
    )?;
    if exact_uncached_result {
        return Ok(CacheLookup::Miss(CacheMissReason::ErrorCache));
    }

    let same_source: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM parse_cache WHERE file_identity=?1 AND source_version=?2
            UNION ALL
            SELECT 1 FROM scan_file_results WHERE file_identity=?1 AND source_version=?2
         )",
        params![file_identity, source_version],
        |row| row.get(0),
    )?;
    if same_source {
        return Ok(CacheLookup::Miss(CacheMissReason::ParserProfileChanged));
    }

    // spec Part 4 miss-reason tree steps 4-5: the global `file_inventory` is
    // upserted by `prepare_inventory` BEFORE the lookup, so it cannot decide
    // "same identity" (the current round's rows would make every new file look
    // changed). Identity changes come only from prior cache/result rows, and the
    // `entry_absent_or_evicted` / `new_file` branch uses the pre-round existence
    // flag reported by `prepare_inventory`.
    let same_identity: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM parse_cache WHERE file_identity=?1
            UNION ALL
            SELECT 1 FROM scan_file_results WHERE file_identity=?1
         )",
        [file_identity],
        |row| row.get(0),
    )?;
    if same_identity {
        return Ok(CacheLookup::Miss(CacheMissReason::SourceVersionChanged));
    }

    if inventory_existed_before {
        // v1 lossy projection of the v2 `entry_absent_or_evicted` reason
        // (spec Part 5: entry_absent_or_evicted can only project as new_file).
        Ok(CacheLookup::Miss(CacheMissReason::NewFile))
    } else {
        Ok(CacheLookup::Miss(CacheMissReason::NewFile))
    }
}

pub(crate) fn write_success_cache(
    transaction: &Transaction<'_>,
    cached_at_ms: i64,
    records: &[CacheWriteRecord],
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO parse_cache(
            file_identity, source_version, parse_profile_hash, content,
            content_sha256, parser_backend, worker_lane, truncated,
            worker_contract_version, worker_version, worker_build, cached_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(file_identity, source_version, parse_profile_hash) DO UPDATE SET
            content=excluded.content,
            content_sha256=excluded.content_sha256,
            parser_backend=excluded.parser_backend,
            worker_lane=excluded.worker_lane,
            truncated=excluded.truncated,
            worker_contract_version=excluded.worker_contract_version,
            worker_version=excluded.worker_version,
            worker_build=excluded.worker_build,
            cached_at_ms=excluded.cached_at_ms",
    )?;
    for record in records {
        statement.execute(params![
            record.file_identity,
            record.source_version,
            record.parse_profile_hash,
            record.content,
            record.content_sha256,
            record.parser_backend,
            record.worker_lane,
            i64::from(record.truncated),
            record.worker_contract_version,
            record.worker_version,
            record.worker_build,
            cached_at_ms,
        ])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// classification cache（spec Part 3.2/4）：无负缓存，只写 text/no-text
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationCacheEntry {
    pub status: String,
    pub page_count: u64,
    pub result_examined_pages: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassificationCacheMissReason {
    NewFile,
    SourceVersionChanged,
    ClassifierIdentityChanged,
    EntryAbsentOrEvicted,
}

impl ClassificationCacheMissReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NewFile => "new_file",
            Self::SourceVersionChanged => "source_version_changed",
            Self::ClassifierIdentityChanged => "classifier_identity_changed",
            Self::EntryAbsentOrEvicted => "entry_absent_or_evicted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassificationCacheLookup {
    Fresh(ClassificationCacheEntry),
    Miss(ClassificationCacheMissReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationCacheWriteRecord {
    pub file_identity: String,
    pub source_version: String,
    pub source_guard_kind: String,
    pub source_guard_sha256: String,
    pub classifier_profile_hash: String,
    pub classifier_build: String,
    pub status: String,
    pub page_count: u64,
    pub result_examined_pages: u64,
}

impl ClassificationCacheWriteRecord {
    // T2-4 Scheduler consumes the write path; until then only tests exercise it.
    #[allow(dead_code)]
    pub(crate) fn validate(&self) -> Result<(), String> {
        let is_sha256 =
            |value: &str| value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if self.file_identity.is_empty()
            || self.file_identity.chars().count() > 4_096
            || super::inventory::parse_source_version(&self.source_version).is_err()
            || !matches!(
                self.source_guard_kind.as_str(),
                "windows_file_id_change_time_v1" | "unix_inode_ctime_v1" | "content_sha256_v1"
            )
            || !is_sha256(&self.source_guard_sha256)
            || !is_sha256(&self.classifier_profile_hash)
            || !is_sha256(&self.classifier_build)
            || !matches!(
                self.status.as_str(),
                "text_in_parse_window" | "no_text_in_parse_window"
            )
            || self.page_count == 0
            || self.result_examined_pages == 0
        {
            return Err("invalid classification cache record".to_string());
        }
        Ok(())
    }
}

/// `classifier_profile_hash` canonical payload（spec Part 3.2）：policy version
/// + `pdf_max_pages` + `pdf_classification_timeout_ms`；不含全局 quota/session。
#[derive(Serialize)]
struct ClassifierProfileFingerprint<'a> {
    policy_version: &'a str,
    pdf_max_pages: u64,
    pdf_classification_timeout_ms: u64,
}

pub fn classifier_profile_hash(profile: &NormalizedScannerProfileV2) -> Result<String, String> {
    profile.validate()?;
    let input = ClassifierProfileFingerprint {
        policy_version: &profile.classifier_policy_version,
        pdf_max_pages: profile.parse.pdf.max_pages,
        pdf_classification_timeout_ms: profile.pdf_classification_timeout_ms,
    };
    let canonical_json = serde_json::to_vec(&input).map_err(|error| error.to_string())?;
    Ok(domain_hash(CLASSIFIER_DOMAIN, &canonical_json))
}

/// 类型化 classification cache lookup。key 固定为
/// `file_identity + source_version + SourceGuardV2 + classifier_profile_hash +
/// classifier_build`（spec Part 3.2）。`inventory_existed_before` 表示
/// `prepare_inventory` 报告该 inventory 在本轮前已存在，用于区分
/// `entry_absent_or_evicted` 与 `new_file`。
// T2-4 Scheduler consumes the lookup path; until then only tests exercise it.
#[allow(dead_code)]
pub(crate) fn lookup_classification_cache(
    connection: &Connection,
    file_identity: &str,
    source_version: &str,
    source_guard_kind: &str,
    source_guard_sha256: &str,
    classifier_profile_hash: &str,
    classifier_build: &str,
    inventory_existed_before: bool,
) -> rusqlite::Result<ClassificationCacheLookup> {
    let fresh = connection
        .query_row(
            "SELECT status, page_count, result_examined_pages
             FROM classification_cache
             WHERE file_identity=?1 AND source_version=?2
               AND source_guard_kind=?3 AND source_guard_sha256=?4
               AND classifier_profile_hash=?5 AND classifier_build=?6",
            params![
                file_identity,
                source_version,
                source_guard_kind,
                source_guard_sha256,
                classifier_profile_hash,
                classifier_build
            ],
            |row| {
                Ok(ClassificationCacheEntry {
                    status: row.get(0)?,
                    page_count: row.get::<_, i64>(1)? as u64,
                    result_examined_pages: row.get::<_, i64>(2)? as u64,
                })
            },
        )
        .optional()?;
    if let Some(entry) = fresh {
        if matches!(
            entry.status.as_str(),
            "text_in_parse_window" | "no_text_in_parse_window"
        ) && entry.page_count > 0
        {
            return Ok(ClassificationCacheLookup::Fresh(entry));
        }
    }

    let same_identity_and_guard: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM classification_cache
            WHERE file_identity=?1 AND source_version=?2
              AND source_guard_kind=?3 AND source_guard_sha256=?4
         )",
        params![file_identity, source_version, source_guard_kind, source_guard_sha256],
        |row| row.get(0),
    )?;
    if same_identity_and_guard {
        return Ok(ClassificationCacheLookup::Miss(
            ClassificationCacheMissReason::ClassifierIdentityChanged,
        ));
    }

    let same_identity: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM classification_cache WHERE file_identity=?1
         )",
        [file_identity],
        |row| row.get(0),
    )?;
    if same_identity {
        return Ok(ClassificationCacheLookup::Miss(
            ClassificationCacheMissReason::SourceVersionChanged,
        ));
    }

    if inventory_existed_before {
        Ok(ClassificationCacheLookup::Miss(
            ClassificationCacheMissReason::EntryAbsentOrEvicted,
        ))
    } else {
        Ok(ClassificationCacheLookup::Miss(
            ClassificationCacheMissReason::NewFile,
        ))
    }
}

/// 只写成功 text/no-text 缓存（spec Part 3.2：无负缓存）。entry_size_bytes
/// 是本 Store 计算的逻辑 payload 字节数；`last_accessed_bucket` 使用当前 UTC
/// 日期，`generation_rank` 默认 1。
// T2-4 Scheduler consumes the write path; until then only tests exercise it.
#[allow(dead_code)]
pub(crate) fn write_success_classification_cache(
    transaction: &Transaction<'_>,
    cached_at_ms: i64,
    records: &[ClassificationCacheWriteRecord],
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO classification_cache(
            file_identity, source_version, source_guard_kind, source_guard_sha256,
            classifier_profile_hash, classifier_build, status, page_count,
            result_examined_pages, cached_at_ms, entry_size_bytes, generation_rank,
            last_accessed_bucket
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12)
         ON CONFLICT(file_identity, source_version, source_guard_kind, source_guard_sha256,
                     classifier_profile_hash, classifier_build) DO UPDATE SET
            status=excluded.status,
            page_count=excluded.page_count,
            result_examined_pages=excluded.result_examined_pages,
            cached_at_ms=excluded.cached_at_ms,
            entry_size_bytes=excluded.entry_size_bytes,
            last_accessed_bucket=excluded.last_accessed_bucket",
    )?;
    for record in records {
        statement.execute(params![
            record.file_identity,
            record.source_version,
            record.source_guard_kind,
            record.source_guard_sha256,
            record.classifier_profile_hash,
            record.classifier_build,
            record.status,
            record.page_count as i64,
            record.result_examined_pages as i64,
            cached_at_ms,
            classification_entry_size_bytes(record),
            utc_date_bucket(),
        ])?;
    }
    Ok(())
}

// T2-4 Scheduler consumes the classification cache write path; until then only
// tests exercise these helpers.
#[allow(dead_code)]
fn classification_entry_size_bytes(record: &ClassificationCacheWriteRecord) -> i64 {
    let text_bytes: usize = [
        record.file_identity.as_str(),
        record.source_version.as_str(),
        record.source_guard_kind.as_str(),
        record.source_guard_sha256.as_str(),
        record.classifier_profile_hash.as_str(),
        record.classifier_build.as_str(),
        record.status.as_str(),
    ]
    .iter()
    .map(|value| value.len())
    .sum();
    (text_bytes + 8 + 8) as i64
}

// T2-4 Scheduler consumes the classification cache write path; until then only
// tests exercise this helper.
#[allow(dead_code)]
fn utc_date_bucket() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    // 自 1970-01-01 起的天数；每天 UTC 边界翻转。
    let days = seconds / 86_400;
    const DAYS_FROM_0_TO_1970: i64 = 719_468;
    let z = days as i64 + DAYS_FROM_0_TO_1970;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{normalize_scanner_profile, normalize_scanner_profile_v2};
    use crate::store::schema::V2_DDL;
    use ai_daily_scanner_contract::{
        RawScannerProfileV1, RawScannerProfileV2, ReportMode, ScannerProfile,
    };

    fn raw_profile() -> RawScannerProfileV1 {
        serde_json::from_value(serde_json::json!({"schema_version": "scanner_profile_v1"}))
            .expect("minimal raw profile")
    }

    fn v2_profile(mode: ReportMode) -> NormalizedScannerProfileV2 {
        let raw: RawScannerProfileV2 = serde_json::from_value(serde_json::json!({
            "schema_version": "scanner_profile_v2"
        }))
        .expect("minimal v2 raw profile");
        normalize_scanner_profile_v2(&ScannerProfile::V2(raw), mode).expect("normalized v2 profile")
    }

    #[test]
    fn parse_profile_hash_changes_for_every_content_semantic_input() {
        let profile = normalize_scanner_profile(&raw_profile(), ReportMode::Daily)
            .expect("normalized profile");
        let route = RouteStackFingerprint::text("build-a").unwrap();
        let base = parse_profile_hash(1, &route, &profile).expect("hash");

        let mut timeout = profile.clone();
        timeout.execution.file_timeout_ms += 1_000;
        let mut extension_timeout = profile.clone();
        extension_timeout
            .execution
            .file_timeout_by_extension_ms
            .insert(".txt".to_string(), 2_000);
        let mut guard = profile.clone();
        guard.execution.max_file_size_bytes += 1;
        let mut parse = profile.clone();
        parse.parse.text.max_chars += 1;
        let mut parser_profile_version = profile.clone();
        parser_profile_version.parser_profile_version = "v2".to_string();
        let mut backend = profile.clone();
        backend.parse.office.primary_backend = "different_office_backend_v1".to_string();
        let mut fallback = profile.clone();
        fallback.parse.office.fallback_after_timeout = true;
        let mut fallback_order = profile.clone();
        fallback_order.parse.office.fallback_order.reverse();
        let changed_route = RouteStackFingerprint::text("build-b").unwrap();
        let changed_backend_route = RouteStackFingerprint::modern_office(
            "build-a",
            "ai_daily_worker_v1",
            "office-build",
            None,
        )
        .unwrap();

        assert_ne!(base, parse_profile_hash(1, &route, &timeout).unwrap());
        assert_ne!(
            base,
            parse_profile_hash(1, &route, &extension_timeout).unwrap()
        );
        assert_ne!(base, parse_profile_hash(1, &route, &guard).unwrap());
        assert_ne!(base, parse_profile_hash(1, &route, &parse).unwrap());
        assert_ne!(
            base,
            parse_profile_hash(1, &route, &parser_profile_version).unwrap()
        );
        assert_ne!(base, parse_profile_hash(1, &route, &backend).unwrap());
        assert_ne!(base, parse_profile_hash(1, &route, &fallback).unwrap());
        assert_ne!(
            base,
            parse_profile_hash(1, &route, &fallback_order).unwrap()
        );
        assert_ne!(
            base,
            parse_profile_hash(1, &changed_route, &profile).unwrap()
        );
        assert_ne!(
            base,
            parse_profile_hash(1, &changed_backend_route, &profile).unwrap()
        );
        assert!(parse_profile_hash(2, &route, &profile).is_err());
    }

    #[test]
    fn route_stack_hash_includes_each_applicable_worker_contract_and_build() {
        let profile = normalize_scanner_profile(&raw_profile(), ReportMode::Daily)
            .expect("normalized profile");
        let without_fallback = RouteStackFingerprint::modern_office(
            "engine-build",
            "ai_daily_worker_v1",
            "office-build",
            None,
        )
        .unwrap();
        let with_fallback = RouteStackFingerprint::modern_office(
            "engine-build",
            "ai_daily_worker_v1",
            "office-build",
            Some(("ai_daily_worker_v1", "python-build")),
        )
        .unwrap();
        let changed_python = RouteStackFingerprint::modern_office(
            "engine-build",
            "ai_daily_worker_v1",
            "office-build",
            Some(("ai_daily_worker_v1", "python-build-2")),
        )
        .unwrap();
        let changed_contract = RouteStackFingerprint::modern_office(
            "engine-build",
            "ai_daily_worker_v2",
            "office-build",
            Some(("ai_daily_worker_v1", "python-build")),
        )
        .unwrap();

        let base = parse_profile_hash(1, &without_fallback, &profile).unwrap();
        assert_ne!(
            base,
            parse_profile_hash(1, &with_fallback, &profile).unwrap()
        );
        assert_ne!(
            parse_profile_hash(1, &with_fallback, &profile).unwrap(),
            parse_profile_hash(1, &changed_python, &profile).unwrap()
        );
        assert_ne!(
            parse_profile_hash(1, &with_fallback, &profile).unwrap(),
            parse_profile_hash(1, &changed_contract, &profile).unwrap()
        );
    }

    // -----------------------------------------------------------------------
    // classification cache（spec Part 3.2/4）
    // -----------------------------------------------------------------------

    fn fresh_v2_db() -> rusqlite::Connection {
        let connection = rusqlite::Connection::open_in_memory().expect("in-memory db");
        connection
            .execute_batch(V2_DDL)
            .expect("v2 schema builds");
        connection
    }

    fn insert_inventory(connection: &Connection, file_identity: &str, source_version: &str) {
        connection
            .execute(
                "INSERT INTO file_inventory(
                    file_identity, absolute_path, relative_path, file_type,
                    source_version, size_bytes, mtime_ns, last_seen_at_ms
                 ) VALUES (?1, ?2, ?3, '.pdf', ?4, 1, 1, 1)",
                params![
                    file_identity,
                    format!("C:\\{file_identity}.pdf"),
                    format!("{file_identity}.pdf"),
                    source_version
                ],
            )
            .expect("inventory row inserts");
    }

    fn cache_record(
        file_identity: &str,
        source_version: &str,
        status: &str,
        classifier_build: &str,
    ) -> ClassificationCacheWriteRecord {
        ClassificationCacheWriteRecord {
            file_identity: file_identity.to_string(),
            source_version: source_version.to_string(),
            source_guard_kind: "windows_file_id_change_time_v1".to_string(),
            source_guard_sha256: "b".repeat(64),
            classifier_profile_hash: "c".repeat(64),
            classifier_build: classifier_build.to_string(),
            status: status.to_string(),
            page_count: 5,
            result_examined_pages: 5,
        }
    }

    #[test]
    fn classifier_profile_hash_changes_for_every_classification_semantic_input() {
        let profile = v2_profile(ReportMode::Daily);
        let base = classifier_profile_hash(&profile).expect("hash");

        let mut timeout = profile.clone();
        timeout.pdf_classification_timeout_ms += 1_000;
        let mut pages = profile.clone();
        pages.parse.pdf.max_pages += 1;
        let report_mode = v2_profile(ReportMode::Monthly);

        assert_ne!(base, classifier_profile_hash(&timeout).unwrap());
        assert_ne!(base, classifier_profile_hash(&pages).unwrap());
        assert_ne!(base, classifier_profile_hash(&report_mode).unwrap());
        // 同一 profile 稳定
        assert_eq!(base, classifier_profile_hash(&profile).unwrap());
        // 与全局 quota/session 无关
        let mut quota = profile.clone();
        quota.max_total_pdf_classification_pages += 1;
        let mut session = profile.clone();
        session.session_concurrency += 1;
        let mut deadline = profile.clone();
        deadline.total_deadline_ms += 1_000;
        assert_eq!(base, classifier_profile_hash(&quota).unwrap());
        assert_eq!(base, classifier_profile_hash(&session).unwrap());
        assert_eq!(base, classifier_profile_hash(&deadline).unwrap());
    }

    #[test]
    fn classification_cache_write_then_lookup_is_fresh() {
        let mut connection = fresh_v2_db();
        insert_inventory(&connection, "fixture:a", "mtime_ns=1:size=2");
        let transaction = connection.transaction().expect("tx");
        write_success_classification_cache(
            &transaction,
            1_000,
            &[cache_record("fixture:a", "mtime_ns=1:size=2", "text_in_parse_window", &"a".repeat(64))],
        )
        .expect("classification cache write");
        transaction.commit().expect("commit");

        let lookup = lookup_classification_cache(
            &connection,
            "fixture:a",
            "mtime_ns=1:size=2",
            "windows_file_id_change_time_v1",
            &"b".repeat(64),
            &"c".repeat(64),
            &"a".repeat(64),
            false,
        )
        .expect("lookup");
        match lookup {
            ClassificationCacheLookup::Fresh(entry) => {
                assert_eq!(entry.status, "text_in_parse_window");
                assert_eq!(entry.page_count, 5);
                assert_eq!(entry.result_examined_pages, 5);
            }
            other => panic!("expected fresh, got {other:?}"),
        }
    }

    #[test]
    fn classification_cache_miss_reason_tree_is_frozen() {
        let mut connection = fresh_v2_db();
        insert_inventory(&connection, "fixture:a", "mtime_ns=1:size=2");
        insert_inventory(&connection, "fixture:b", "mtime_ns=5:size=6");
        let transaction = connection.transaction().expect("tx");
        write_success_classification_cache(
            &transaction,
            1_000,
            &[cache_record("fixture:a", "mtime_ns=1:size=2", "no_text_in_parse_window", &"a".repeat(64))],
        )
        .expect("classification cache write");
        transaction.commit().expect("commit");

        // 同一 identity+source_version+guard，classifier build 变化
        let lookup = lookup_classification_cache(
            &connection,
            "fixture:a",
            "mtime_ns=1:size=2",
            "windows_file_id_change_time_v1",
            &"b".repeat(64),
            &"c".repeat(64),
            &"d".repeat(64),
            false,
        )
        .expect("lookup");
        assert_eq!(
            lookup,
            ClassificationCacheLookup::Miss(ClassificationCacheMissReason::ClassifierIdentityChanged)
        );

        // 同一 file_identity，source_version 变化
        let lookup = lookup_classification_cache(
            &connection,
            "fixture:a",
            "mtime_ns=9:size=2",
            "windows_file_id_change_time_v1",
            &"b".repeat(64),
            &"c".repeat(64),
            &"a".repeat(64),
            false,
        )
        .expect("lookup");
        assert_eq!(
            lookup,
            ClassificationCacheLookup::Miss(ClassificationCacheMissReason::SourceVersionChanged)
        );

        // 无任何缓存行的新文件
        let lookup = lookup_classification_cache(
            &connection,
            "fixture:b",
            "mtime_ns=5:size=6",
            "windows_file_id_change_time_v1",
            &"b".repeat(64),
            &"c".repeat(64),
            &"a".repeat(64),
            false,
        )
        .expect("lookup");
        assert_eq!(
            lookup,
            ClassificationCacheLookup::Miss(ClassificationCacheMissReason::NewFile)
        );

        // inventory 本轮前已存在 -> entry_absent_or_evicted
        let lookup = lookup_classification_cache(
            &connection,
            "fixture:b",
            "mtime_ns=5:size=6",
            "windows_file_id_change_time_v1",
            &"b".repeat(64),
            &"c".repeat(64),
            &"a".repeat(64),
            true,
        )
        .expect("lookup");
        assert_eq!(
            lookup,
            ClassificationCacheLookup::Miss(ClassificationCacheMissReason::EntryAbsentOrEvicted)
        );
    }

    #[test]
    fn classification_cache_has_no_negative_cache() {
        // unknown/error 永不写分类缓存
        assert!(
            cache_record("fixture:a", "mtime_ns=1:size=2", "unknown", &"a".repeat(64))
                .validate()
                .is_err()
        );
        assert!(
            cache_record("fixture:a", "mtime_ns=1:size=2", "error", &"a".repeat(64))
                .validate()
                .is_err()
        );
        assert!(
            cache_record("fixture:a", "mtime_ns=1:size=2", "text_in_parse_window", &"a".repeat(64))
                .validate()
                .is_ok()
        );
    }
}
