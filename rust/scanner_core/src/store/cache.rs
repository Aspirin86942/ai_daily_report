//! Route-aware successful parse cache, deterministic profile fingerprints, and
//! the success-only PDF classification cache (spec Part 3/4).

use ai_daily_scanner_contract::{
    CacheMissReason, CacheRetentionPolicy, NormalizedScannerSettings, Validate,
};
use ai_daily_worker_contract::ParsedContentAudit;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::classifier::ParserRoute;

pub const PARSE_PROFILE_HASH_ALGORITHM: &str = "sha256-parse-profile-v1";
pub const PARSE_CACHE_PAYLOAD_VERSION: u64 = 1;
pub const CLASSIFIER_PROFILE_HASH_ALGORITHM: &str = "sha256-classifier-profile-v1";
const CLASSIFIER_DOMAIN: &[u8] = b"classifier-profile-v1\0";

// ---------------------------------------------------------------------------
// cache_retention_v1 固定硬上限（spec Part 4 表）。这些常量由
// These limits are compiled policy; changing one requires a new policy version.
// ---------------------------------------------------------------------------

pub const PARSE_CACHE_MAX_BYTES: i64 = 1_073_741_824; // 1 GiB
pub const CLASSIFICATION_CACHE_MAX_BYTES: i64 = 134_217_728; // 128 MiB
pub const CONTEXT_ARTIFACTS_MAX_BYTES: i64 = 536_870_912; // 512 MiB
pub const TERMINAL_AUDIT_MAX_BYTES: i64 = 2_147_483_648; // 2 GiB
pub const TERMINAL_RUN_MAX_COUNT: i64 = 500;
pub const TERMINAL_RUN_MAX_AGE_DAYS: i64 = 90;
pub const OPPORTUNISTIC_GC_BUDGET_MS: u64 = 10;

pub fn cache_retention_policy() -> CacheRetentionPolicy {
    CacheRetentionPolicy {
        policy_version: "cache_retention_v1".to_string(),
        parse_cache_max_bytes: PARSE_CACHE_MAX_BYTES as u64,
        classification_cache_max_bytes: CLASSIFICATION_CACHE_MAX_BYTES as u64,
        context_artifacts_max_bytes: CONTEXT_ARTIFACTS_MAX_BYTES as u64,
        terminal_audit_max_bytes: TERMINAL_AUDIT_MAX_BYTES as u64,
        terminal_run_max_count: TERMINAL_RUN_MAX_COUNT as u64,
        terminal_run_max_age_days: TERMINAL_RUN_MAX_AGE_DAYS as u64,
        opportunistic_gc_budget_ms: OPPORTUNISTIC_GC_BUDGET_MS,
    }
}

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
    ) -> Result<Self, String> {
        Self::from_members(
            "office",
            vec![
                RouteStackMember::engine(engine_build),
                RouteStackMember::worker("office_worker", office_contract, office_build),
            ],
        )
    }

    pub fn pdf(engine_build: &str, pdf_contract: &str, pdf_build: &str) -> Result<Self, String> {
        Self::from_members(
            "pdf",
            vec![
                RouteStackMember::engine(engine_build),
                RouteStackMember::worker("pdf_worker", pdf_contract, pdf_build),
            ],
        )
    }

    fn from_members(route: &str, members: Vec<RouteStackMember>) -> Result<Self, String> {
        if !valid_text(route, 1_024)
            || members.is_empty()
            || members.iter().any(|member| {
                !valid_text(&member.component, 1_024)
                    || !valid_text(&member.contract, 1_024)
                    || !valid_text(&member.build, 4_096)
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
    pub pdf: RouteStackFingerprint,
}

impl RouteStackFingerprints {
    pub(crate) fn for_route(&self, route: ParserRoute) -> &RouteStackFingerprint {
        match route {
            ParserRoute::LightText => &self.text_like,
            ParserRoute::RustOffice
            | ParserRoute::RustXlsx
            | ParserRoute::RustXls
            | ParserRoute::RustLegacyOffice => &self.modern_office,
            ParserRoute::Pdf | ParserRoute::PdfImageOcr { .. } => &self.pdf,
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
    /// Engine-owned SourceGuardV2 kind (spec SourceGuard v2). The parse cache key
    /// is `file_identity + source_version + guard + parse_profile_hash`, so a
    /// same-size + preserved-mtime content replacement forces a miss.
    pub source_guard_kind: String,
    pub source_guard_sha256: String,
    pub parse_profile_hash: String,
    pub content: String,
    pub content_sha256: String,
    pub cache_payload_version: u64,
    pub evidence_audit: Option<ParsedContentAudit>,
    pub parser_backend: String,
    pub worker_lane: String,
    pub truncated: bool,
    pub worker_contract_version: String,
    pub worker_version: String,
    pub worker_build: String,
}

fn valid_text(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.chars().count() <= limit
}

fn valid_cache_source(identity: &str, version: &str, kind: &str, guard: &str) -> bool {
    valid_text(identity, 4_096)
        && super::inventory::parse_source_version(version).is_ok()
        && super::valid_guard_wire(kind, guard)
}

fn valid_parse_content(content: &str, hash: &str, worker_fields: [&str; 4], build: &str) -> bool {
    super::inventory::is_sha256(hash)
        && sha256_hex(content.as_bytes()) == hash
        && worker_fields
            .into_iter()
            .all(|field| valid_text(field, 1_024))
        && valid_text(build, 4_096)
}

impl CacheWriteRecord {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.cache_payload_version > PARSE_CACHE_PAYLOAD_VERSION {
            return Err("unsupported parse cache payload version".into());
        }
        if let Some(audit) = &self.evidence_audit {
            audit.validate(&self.content)?;
        }
        if !valid_cache_source(
            &self.file_identity,
            &self.source_version,
            &self.source_guard_kind,
            &self.source_guard_sha256,
        ) || !super::inventory::is_sha256(&self.parse_profile_hash)
            || !valid_parse_content(
                &self.content,
                &self.content_sha256,
                [
                    &self.parser_backend,
                    &self.worker_lane,
                    &self.worker_contract_version,
                    &self.worker_version,
                ],
                &self.worker_build,
            )
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
    pub evidence_audit: Option<ParsedContentAudit>,
    pub parser_backend: String,
    pub worker_lane: String,
    pub truncated: bool,
    pub worker_contract_version: String,
    pub worker_version: String,
    pub worker_build: String,
}

#[cfg(test)]
thread_local! {
    static TEST_OUTER_CACHE_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl CacheEntry {
    fn is_valid(&self) -> bool {
        #[cfg(test)]
        TEST_OUTER_CACHE_VALIDATIONS.with(|count| count.set(count.get() + 1));
        valid_parse_content(
            &self.content,
            &self.content_sha256,
            [
                &self.parser_backend,
                &self.worker_lane,
                &self.worker_contract_version,
                &self.worker_version,
            ],
            &self.worker_build,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
// Hits are bounded by the run's candidate cap. Keep the decoded entry inline so
// the hot cache path does not allocate an additional box for each source.
#[allow(clippy::large_enum_variant)]
pub enum CacheLookup {
    Fresh(CacheEntry),
    Miss(CacheMissReason),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParseCachePayload {
    version: u64,
    text: String,
    text_sha256: String,
    evidence_audit: ParsedContentAudit,
}

impl CacheLookup {
    /// The caller selects the format from its parse profile; business JSON is never sniffed.
    pub fn decode_payload(self, version: u64) -> Self {
        if version == 0 {
            return self;
        }
        let Self::Fresh(entry) = self else {
            return self;
        };
        let miss = Self::Miss(CacheMissReason::EntryAbsentOrEvicted);
        if !entry.is_valid() {
            return miss;
        }
        entry
            .decode_authenticated_payload(version)
            .map_or(miss, Self::Fresh)
    }
}

impl CacheEntry {
    // Callers must verify the stored content digest and parser fields first.
    fn decode_authenticated_payload(mut self, version: u64) -> Option<Self> {
        let payload: ParseCachePayload = serde_json::from_str(&self.content).ok()?;
        if payload.version != version
            || version != PARSE_CACHE_PAYLOAD_VERSION
            || sha256_hex(payload.text.as_bytes()) != payload.text_sha256
            || payload.evidence_audit.validate(&payload.text).is_err()
            || (self.parser_backend == crate::parsers::document::OCR_BACKEND
                && !crate::parsers::document::valid_ocr_ranges(
                    &payload.text,
                    &payload.evidence_audit,
                    None,
                ))
        {
            return None;
        }
        self.content = payload.text;
        self.content_sha256 = payload.text_sha256;
        self.evidence_audit = Some(payload.evidence_audit);
        Some(self)
    }
}

fn stored_content(record: &CacheWriteRecord) -> (String, String) {
    if record.cache_payload_version == 0 {
        return (record.content.clone(), record.content_sha256.clone());
    }
    let content = serde_json::to_string(&ParseCachePayload {
        version: record.cache_payload_version,
        text: record.content.clone(),
        text_sha256: record.content_sha256.clone(),
        evidence_audit: record.evidence_audit.clone().unwrap_or_default(),
    })
    .expect("serializable parse cache payload");
    let hash = sha256_hex(content.as_bytes());
    (content, hash)
}

#[derive(Serialize)]
struct ParseProfileFingerprint<'a> {
    cache_payload_version: u64,
    excerpt_policy_version: &'static str,
    protocol_version: u64,
    route_stack: &'a RouteStackFingerprint,
    core_build_identity: &'a str,
    max_file_size_bytes: u64,
    default_timeout_ms: u64,
    timeout_by_extension_ms: &'a std::collections::BTreeMap<String, u64>,
    parse: &'a ai_daily_scanner_contract::ParseProfile,
}

pub fn parse_profile_hash(
    protocol_version: u64,
    route_stack: &RouteStackFingerprint,
    profile: &NormalizedScannerSettings,
) -> Result<String, String> {
    if protocol_version != 1 {
        return Err("parse profile hash protocol must be v1".to_string());
    }
    profile.validate()?;
    let canonical_json = parse_profile_fingerprint_json(protocol_version, route_stack, profile)?;
    Ok(domain_hash(b"parse-profile-v1\0", &canonical_json))
}

fn parse_profile_fingerprint_json(
    protocol_version: u64,
    route_stack: &RouteStackFingerprint,
    profile: &NormalizedScannerSettings,
) -> Result<Vec<u8>, String> {
    // Native text extraction is independent of the optional image OCR policy.
    let mut native;
    let parse = if profile.parse.pdf.image_ocr.is_some() {
        native = profile.parse.clone();
        native.pdf.image_ocr = None;
        &native
    } else {
        &profile.parse
    };
    let input = ParseProfileFingerprint {
        cache_payload_version: PARSE_CACHE_PAYLOAD_VERSION,
        excerpt_policy_version: crate::source_allocation::EXCERPT_POLICY_VERSION,
        protocol_version,
        route_stack,
        core_build_identity: crate::PARSER_CONTENT_IDENTITY,
        max_file_size_bytes: profile.execution.max_file_size_bytes,
        default_timeout_ms: profile.execution.file_timeout_ms,
        timeout_by_extension_ms: &profile.execution.file_timeout_by_extension_ms,
        parse,
    };
    serde_json::to_vec(&input).map_err(|error| error.to_string())
}

pub use crate::artifact::sha256_hex;

pub(crate) fn domain_hash(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    ai_daily_worker_contract::archive::hex(&hasher.finalize())
}

#[cfg(test)]
fn over_cap_error(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::OperationAborted,
            extended_code: 0,
        },
        Some(message),
    )
}

#[derive(Debug)]
pub(crate) enum CacheMutationError {
    Sqlite(rusqlite::Error),
    Record(String),
    DeadlineExhausted,
}

impl From<rusqlite::Error> for CacheMutationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl CacheMutationError {
    #[cfg(test)]
    fn into_sqlite(self) -> rusqlite::Error {
        match self {
            Self::Sqlite(error) => error,
            Self::Record(detail) => over_cap_error(detail),
            Self::DeadlineExhausted => {
                over_cap_error("cache mutation deadline exhausted".to_string())
            }
        }
    }
}

fn ensure_mutation_deadline(deadline_open: &dyn Fn() -> bool) -> Result<(), CacheMutationError> {
    if deadline_open() {
        Ok(())
    } else {
        Err(CacheMutationError::DeadlineExhausted)
    }
}

#[derive(Debug)]
pub(crate) enum CacheLookupReadError {
    Sqlite(rusqlite::Error),
    Corrupt(&'static str),
}

impl From<rusqlite::Error> for CacheLookupReadError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// `last_accessed_bucket` 使用 UTC 日期（spec Part 4），同一天内同一行最多更新
/// 一次。该函数把 epoch ms 归一化为 `YYYY-MM-DD` UTC。
pub(crate) fn date_bucket_for_ms(ms: i64) -> String {
    let seconds = ms.div_euclid(1_000);
    // 自 1970-01-01 起的天数；每天 UTC 边界翻转。
    let days = seconds.div_euclid(86_400);
    const DAYS_FROM_0_TO_1970: i64 = 719_468;
    let z = days + DAYS_FROM_0_TO_1970;
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

/// Only the two schema-owned cache tables construct this descriptor.
struct CacheTable {
    name: &'static str,
    select_victim: &'static str,
    delete_victim: &'static str,
}

enum CachedSource {
    Same,
    Changed,
    Absent,
}

impl CacheTable {
    fn cached_source(
        &self,
        connection: &Connection,
        source: [&str; 4],
    ) -> rusqlite::Result<CachedSource> {
        let table = self.name;
        let same: bool = connection.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE file_identity=?1 AND source_version=?2
                      AND source_guard_kind=?3 AND source_guard_sha256=?4)"
            ),
            source,
            |row| row.get(0),
        )?;
        if same {
            return Ok(CachedSource::Same);
        }
        let exists: bool = connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE file_identity=?1)"),
            [source[0]],
            |row| row.get(0),
        )?;
        Ok(if exists {
            CachedSource::Changed
        } else {
            CachedSource::Absent
        })
    }

    fn needs_touch(
        &self,
        connection: &Connection,
        identities: &[String],
        bucket: &str,
        deadline_open: &dyn Fn() -> bool,
    ) -> Result<bool, CacheMutationError> {
        for chunk in identities.chunks(super::CACHE_TRANSACTION_BATCH_SIZE) {
            ensure_mutation_deadline(deadline_open)?;
            let table = self.name;
            let placeholders = super::sql_placeholders(chunk.len());
            let sql = format!(
                "SELECT EXISTS(SELECT 1 FROM {table}
                 WHERE last_accessed_bucket <> ?1 AND file_identity IN ({placeholders}))"
            );
            let params = std::iter::once(bucket).chain(chunk.iter().map(String::as_str));
            let needs_touch = connection
                .prepare_cached(&sql)?
                .query_row(rusqlite::params_from_iter(params), |row| row.get(0))?;
            if needs_touch {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn touch(
        &self,
        transaction: &Transaction<'_>,
        identities: &[String],
        bucket: &str,
        deadline_open: &dyn Fn() -> bool,
    ) -> Result<usize, CacheMutationError> {
        if identities.is_empty() {
            return Ok(0);
        }
        ensure_mutation_deadline(deadline_open)?;
        let mut statement = transaction.prepare_cached(&format!(
            "UPDATE {} SET last_accessed_bucket=?2 WHERE file_identity=?1 AND last_accessed_bucket<>?2", self.name
        ))?;
        let mut updated = 0;
        for identity in identities {
            ensure_mutation_deadline(deadline_open)?;
            updated += statement.execute(params![identity, bucket])?;
        }
        Ok(updated)
    }

    fn evict(
        &self,
        transaction: &Transaction<'_>,
        cap: i64,
        params: impl rusqlite::Params + Copy,
        deadline_open: &dyn Fn() -> bool,
    ) -> Result<(), CacheMutationError> {
        ensure_mutation_deadline(deadline_open)?;
        let mut total: i64 = transaction.query_row(
            &format!(
                "SELECT COALESCE(SUM(entry_size_bytes), 0) FROM {}",
                self.name
            ),
            [],
            |row| row.get(0),
        )?;
        while total > cap {
            ensure_mutation_deadline(deadline_open)?;
            let victim = transaction
                .query_row(self.select_victim, params, |row| {
                    // Both queries return the complete text primary key followed by the stored size.
                    let size_column = row.as_ref().column_count() - 1;
                    let key = (0..size_column)
                        .map(|index| row.get::<_, String>(index))
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok((key, row.get::<_, i64>(size_column)?))
                })
                .optional()?;
            let Some((key, size)) = victim else { break };
            ensure_mutation_deadline(deadline_open)?;
            if transaction.execute(self.delete_victim, rusqlite::params_from_iter(key))? != 1 {
                break;
            }
            total = total.saturating_sub(size);
        }
        Ok(())
    }
}

pub(super) fn cache_access_needs_touch(
    connection: &Connection,
    parse_hits: &[String],
    classification_hits: &[String],
    bucket: &str,
    deadline_open: &dyn Fn() -> bool,
) -> Result<bool, CacheMutationError> {
    Ok(
        parse::TABLE.needs_touch(connection, parse_hits, bucket, deadline_open)?
            || classification::TABLE.needs_touch(
                connection,
                classification_hits,
                bucket,
                deadline_open,
            )?,
    )
}

pub(crate) fn touch_parse_cache_access_with_check(
    transaction: &Transaction<'_>,
    identities: &[String],
    bucket: &str,
    deadline_open: &dyn Fn() -> bool,
) -> Result<usize, CacheMutationError> {
    parse::TABLE.touch(transaction, identities, bucket, deadline_open)
}

pub(crate) fn touch_classification_cache_access_with_check(
    transaction: &Transaction<'_>,
    identities: &[String],
    bucket: &str,
    deadline_open: &dyn Fn() -> bool,
) -> Result<usize, CacheMutationError> {
    classification::TABLE.touch(transaction, identities, bucket, deadline_open)
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;

mod parse;
#[cfg(test)]
use parse::evict_parse_cache;
pub(super) use parse::lookup_snapshot_cache;
pub(crate) use parse::{lookup_cache, write_success_cache_with_check};
#[cfg(test)]
pub(crate) use parse::{parse_entry_size_bytes, write_success_cache};
mod classification;
#[cfg(test)]
use classification::evict_classification_cache;
#[cfg(test)]
pub(crate) use classification::write_success_classification_cache;
pub use classification::{
    classifier_profile_hash, ClassificationCacheEntry, ClassificationCacheLookup,
    ClassificationCacheMissReason, ClassificationCacheWriteRecord,
};
pub(crate) use classification::{
    lookup_classification_cache, write_success_classification_cache_with_check,
};
