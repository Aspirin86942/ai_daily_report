//! Route-aware successful parse cache and deterministic profile fingerprints.

use ai_daily_scanner_contract::{CacheMissReason, NormalizedScannerProfileV1, Validate};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::classifier::ParserRoute;
use crate::planner::PlannedFile;

pub const PARSE_PROFILE_HASH_ALGORITHM: &str = "sha256-parse-profile-v1";

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

    let same_identity: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM file_inventory WHERE file_identity=?1
            UNION ALL
            SELECT 1 FROM scan_file_results WHERE file_identity=?1
         )",
        [file_identity],
        |row| row.get(0),
    )?;
    if same_identity {
        return Ok(CacheLookup::Miss(CacheMissReason::SourceVersionChanged));
    }

    Ok(CacheLookup::Miss(CacheMissReason::NewFile))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::normalize_scanner_profile;
    use ai_daily_scanner_contract::{RawScannerProfileV1, ReportMode};

    fn raw_profile() -> RawScannerProfileV1 {
        serde_json::from_value(serde_json::json!({"schema_version": "scanner_profile_v1"}))
            .expect("minimal raw profile")
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
}
