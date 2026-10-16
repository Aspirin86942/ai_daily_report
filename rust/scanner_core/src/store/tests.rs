use super::runs::compute_audit_size;
use super::*;
use crate::classifier::{ClassificationError, ParserRoute};
use crate::config::normalize_scanner_settings;
use crate::planner::{PlanAction, PlannedFile};
use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::{
    AdapterPaths, AuditWorkerLane, CacheMissReason, CacheStatus, ClassificationCacheStatus,
    ClassificationTransport, ContextAction, ContextDecision, DiagnosticStage, ParseStatus,
    ParseTransport, PdfClassificationAuditV1, PdfClassificationStatus, ReportMode, ScannerSettings,
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn cache_failure_diagnostics_expose_only_fixed_reason_codes() {
    for (detail, reason) in [
        (
            "Invalid column type Null at index: 2, name: PRIVATE_VALUE",
            "stored_column_type",
        ),
        (
            "UNIQUE constraint failed: context_artifacts.snapshot_key_sha256 PRIVATE_VALUE",
            "snapshot_key_unique",
        ),
        ("PRIVATE_VALUE", "unclassified"),
    ] {
        let diagnostic = StoreError::CacheWrite {
            detail: detail.to_string(),
        }
        .diagnostic(DiagnosticStage::Cache);
        assert_eq!(diagnostic.error_code, ErrorCode::CacheWriteFailed);
        assert_eq!(
            diagnostic.message,
            format!("scanner database transaction failed ({reason})")
        );
        assert!(!diagnostic.message.contains("PRIVATE_VALUE"));
    }
}

#[cfg(windows)]
#[test]
fn runtime_path_normalization_is_stable_across_file_creation() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let actual_parent = directory.path().join("MixedCaseParent");
    std::fs::create_dir(&actual_parent).expect("mixed-case parent");
    let database_path = directory
        .path()
        .join("mixedcaseparent")
        .join(SCAN_DB_FILENAME);

    let before_creation = normalize_runtime_path(&database_path.to_string_lossy());
    std::fs::File::create(&database_path).expect("database placeholder");
    let after_creation = normalize_runtime_path(&database_path.to_string_lossy());

    assert_eq!(before_creation, after_creation);
}
use tempfile::TempDir;

struct ExpiringClock {
    calls: AtomicUsize,
    expire_after: usize,
    expired_at_ms: u64,
}

impl ExpiringClock {
    fn new(expire_after: usize, expired_at_ms: u64) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            expire_after,
            expired_at_ms,
        }
    }

    fn reset(&self) {
        self.calls.store(0, Ordering::SeqCst);
    }
}

impl crate::deadline::Clock for ExpiringClock {
    fn now_ms(&self) -> u64 {
        if self.calls.fetch_add(1, Ordering::SeqCst) >= self.expire_after {
            self.expired_at_ms
        } else {
            0
        }
    }
}

struct DiagnosticBatchExpiringClock {
    expire_after_batches: usize,
    expired_at_ms: u64,
}

impl crate::deadline::Clock for DiagnosticBatchExpiringClock {
    fn now_ms(&self) -> u64 {
        let written = TEST_TERMINAL_DIAGNOSTIC_BATCHES_WRITTEN.with(|count| count.get());
        if written >= self.expire_after_batches {
            self.expired_at_ms
        } else {
            0
        }
    }
}

struct OpportunisticGcOvershootClock;

impl crate::deadline::Clock for OpportunisticGcOvershootClock {
    fn now_ms(&self) -> u64 {
        let statements = TEST_OPPORTUNISTIC_GC_STATEMENTS_EXECUTED.with(|count| count.get());
        if statements == 0 {
            0
        } else {
            10
        }
    }
}

struct Harness {
    _directory: TempDir,
    store: ScannerStore,
    request: BuildContextRequest,
    profile: NormalizedScannerSettings,
    canonical: CanonicalRequest,
    runtime: AttemptRuntime,
    db_path: PathBuf,
}

fn harness(request_id: &str) -> Harness {
    let directory = tempfile::tempdir().expect("temporary directory");
    let db_path = directory.path().join(SCAN_DB_FILENAME);
    let mut request: BuildContextRequest =
        serde_json::from_str(include_str!("../../tests/fixtures/request.json"))
            .expect("request fixture");
    request.request_id = request_id.to_string();
    request.work_dir = directory.path().to_string_lossy().to_string();
    request.scan_db_path = db_path.to_string_lossy().to_string();
    request.adapters = AdapterPaths {
        office_worker_path: directory
            .path()
            .join("office-worker.exe")
            .to_string_lossy()
            .to_string(),
        pdf_worker_path: directory
            .path()
            .join("python.exe")
            .to_string_lossy()
            .to_string(),
    };
    let profile = normalize_scanner_settings(&request.scanner_settings, request.report_mode)
        .expect("normalized scanner profile");
    let canonical =
        ScannerStore::canonicalize_request(&request, &profile).expect("canonical request");
    let runtime = AttemptRuntime::from_request(&request).unwrap();
    let store = ScannerStore::open(&db_path).expect("scanner store");
    Harness {
        _directory: directory,
        store,
        request,
        profile,
        canonical,
        runtime,
        db_path,
    }
}

#[test]
fn canonical_request_includes_compiled_engine_build_identity() {
    let harness = harness("00000000-0000-4000-8000-000000000000");
    let canonical: serde_json::Value =
        serde_json::from_str(&harness.canonical.json).expect("canonical request JSON");

    assert_eq!(
        canonical["scanner_settings"]["core_build_identity"],
        env!("AI_DAILY_ENGINE_BUILD")
    );
}

#[test]
fn opening_old_or_unknown_schema_never_modifies_database_bytes() {
    for version in [0, 1, 2, 4] {
        let directory = tempfile::tempdir().expect("temporary directory");
        let db_path = directory.path().join(SCAN_DB_FILENAME);
        let connection = rusqlite::Connection::open(&db_path).expect("database opens");
        connection
            .pragma_update(None, "user_version", version)
            .expect("schema version is seeded");
        drop(connection);

        let before = std::fs::read(&db_path).expect("database bytes before open");
        assert!(matches!(
            ScannerStore::open(&db_path),
            Err(StoreError::SchemaMismatch { found }) if found == version
        ));
        let after = std::fs::read(&db_path).expect("database bytes after rejected open");

        assert_eq!(before, after, "user_version={version} database changed");
        assert!(!db_path.with_extension("sqlite3-wal").exists());
        assert!(!db_path.with_extension("sqlite3-shm").exists());
    }
}

#[test]
fn fresh_v3_store_contains_no_compatibility_schema() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let db_path = directory.path().join(SCAN_DB_FILENAME);
    let store = ScannerStore::open(&db_path).expect("fresh v3 store");
    let forbidden: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
                 WHERE lower(name) LIKE '%migration%'
                    OR lower(name) LIKE '%compat%'
                    OR lower(name) LIKE '%one_shot%'
                    OR lower(name) LIKE '%legacy%'",
            [],
            |row| row.get(0),
        )
        .expect("schema can be inspected");

    assert_eq!(forbidden, 0);
    drop(store);
    ScannerStore::open(&db_path).expect("v3 store reopens");
}

fn started(outcome: BeginRunOutcome) -> ActiveRun {
    match outcome {
        BeginRunOutcome::Started(active) => active,
        BeginRunOutcome::Stored(_) => panic!("expected a new active run"),
    }
}

struct FixedClock(u64);

impl crate::deadline::Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0
    }
}

fn finalize_for_test(
    store: &mut ScannerStore,
    active: &ActiveRun,
    batch: &FinalizationBatch,
    now_ms: u64,
) -> Result<TerminalAuditTimings, StoreError> {
    store.prepare_inventory(&batch.inventory, active.scan_run_id, now_ms)?;
    let clock = FixedClock(0);
    let deadlines = crate::deadline::RunDeadlines::derive(3_600_000, &clock).unwrap();
    store.finalize(active, batch, now_ms, deadlines, &clock)
}

fn canonical_for_request_id(harness: &Harness, request_id: &str) -> CanonicalRequest {
    let mut request = harness.request.clone();
    request.request_id = request_id.to_string();
    ScannerStore::canonicalize_request(&request, &harness.profile).unwrap()
}

fn empty_summary() -> ContextSummary {
    ContextSummary {
        source_file_count: 0,
        success_count: 0,
        timeout_count: 0,
        included_file_count: 0,
        omitted_file_count: 0,
        error_file_count: 0,
        input_chars: 0,
        output_chars: 0,
        total_duration_ms: 0,
        discovery_duration_ms: 0,
        parse_duration_ms: 0,
        compression_duration_ms: 0,
    }
}

fn run_error() -> Diagnostic {
    Diagnostic {
        error_code: ErrorCode::WorkerHandshakeFailed,
        message: "worker handshake failed".to_string(),
        retryable: false,
        stage: DiagnosticStage::Process,
        file_path: Nullable(None),
        backend: Nullable(None),
    }
}

fn error_batch(active: &ActiveRun) -> FinalizationBatch {
    let error = run_error();
    let envelope = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: active.request_id().to_string(),
        engine_version: crate::engine_version().to_string(),
        engine_build: crate::ENGINE_BUILD_IDENTITY.to_string(),
        status: EngineStatus::Error,
        file_context: String::new(),
        summary: empty_summary(),
        scan_run_id: Nullable(Some(active.scan_run_id())),
        context_run_id: Nullable(None),
        warnings: Vec::new(),
        error: Nullable(Some(error.clone())),
    };
    FinalizationBatch {
        status: RunStatus::Error,
        envelope_json: canonical_envelope_json(&envelope).expect("canonical envelope"),
        inventory: Vec::new(),
        file_results: Vec::new(),
        diagnostics: vec![RunDiagnosticRecord {
            severity: DiagnosticSeverity::Error,
            diagnostic: error,
        }],
        stage_metrics: Vec::new(),
        extension_metrics: Vec::new(),
        context: None,
        artifact: None,
        snapshot_key: None,
        snapshot_hit: None,
        execution_metrics: None,
    }
}

fn diagnostic_audit_batch(active: &ActiveRun, warning_count: usize) -> FinalizationBatch {
    let mut batch = error_batch(active);
    let mut envelope: ContextEnvelope =
        serde_json::from_str(&batch.envelope_json).expect("error envelope");
    let warnings: Vec<Diagnostic> = (0..warning_count)
        .map(|index| Diagnostic {
            error_code: ErrorCode::CacheWriteFailed,
            message: format!("synthetic cache warning {index}"),
            retryable: true,
            stage: DiagnosticStage::Cache,
            file_path: Nullable(None),
            backend: Nullable(None),
        })
        .collect();
    envelope.warnings = warnings.clone();
    batch.envelope_json = canonical_envelope_json(&envelope).expect("canonical envelope");
    let error = batch
        .diagnostics
        .pop()
        .expect("error batch carries one error");
    batch.diagnostics = warnings
        .into_iter()
        .map(|diagnostic| RunDiagnosticRecord {
            severity: DiagnosticSeverity::Warning,
            diagnostic,
        })
        .chain(std::iter::once(error))
        .collect();
    batch
}

fn inventory_record(identity: &str, source_version: &str) -> InventoryRecord {
    let (mtime_ns, size_bytes) = inventory::parse_source_version(source_version).unwrap();
    InventoryRecord {
        file_identity: identity.to_string(),
        absolute_path: "C:\\fixture\\evidence.txt".to_string(),
        relative_path: "evidence.txt".to_string(),
        file_type: ".txt".to_string(),
        source_version: source_version.to_string(),
        size_bytes,
        mtime_ns,
        source_guard_kind: None,
        source_guard_sha256: None,
    }
}

fn success_file_result(
    identity: &str,
    source_version: &str,
    profile_hash: &str,
    content: &str,
) -> FileResultRecord {
    FileResultRecord {
        file_identity: identity.to_string(),
        relative_path: "evidence.txt".to_string(),
        source_version: source_version.to_string(),
        parse_profile_hash: profile_hash.to_string(),
        cache_status: CacheStatus::Miss,
        cache_miss_reason: CacheMissReason::NewFile,
        parse_status: ParseStatus::Success,
        parser_backend: "light_text_v2".to_string(),
        worker_lane: AuditWorkerLane::RustCore,
        truncated: false,
        content_sha256: sha256_hex(content.as_bytes()),
        primary_duration_ms: 3,
        fallback_duration_ms: 0,
        parse_duration_ms: 3,
        failure_class: String::new(),
        fallback_backend: String::new(),
        fallback_reason_code: String::new(),
        parse_transport: ParseTransport::RustInProcess,
        parse_attempt_count: 1,
        pdf_classification: None,
        error: None,
    }
}

#[test]
fn file_result_validation_rejects_impossible_execution_provenance() {
    let source = "mtime_ns=100:size=5";
    let profile_hash = "a".repeat(64);

    let mut fresh_with_execution = success_file_result("file-a", source, &profile_hash, "hello");
    fresh_with_execution.cache_status = CacheStatus::Fresh;
    fresh_with_execution.cache_miss_reason = CacheMissReason::None;
    fresh_with_execution.parse_transport = ParseTransport::OneShot;
    assert!(fresh_with_execution.validate().is_err());

    let mut miss_without_execution = success_file_result("file-a", source, &profile_hash, "hello");
    miss_without_execution.parse_transport = ParseTransport::NotApplicable;
    miss_without_execution.parse_attempt_count = 0;
    miss_without_execution.primary_duration_ms = 0;
    miss_without_execution.parse_duration_ms = 0;
    assert!(miss_without_execution.validate().is_err());
}

#[test]
fn audit_size_counts_the_execution_child_payload() {
    let source = "mtime_ns=100:size=5";
    let profile_hash = "a".repeat(64);
    let mut batch = error_batch(&ActiveRun {
        scan_run_id: 1,
        attempt_number: 1,
        owner_id: "owner".to_string(),
        request_id: "00000000-0000-4000-8000-000000000099".to_string(),
    });
    batch.file_results = vec![success_file_result(
        "file-a",
        source,
        &profile_hash,
        "hello",
    )];
    let parse_only_size = compute_audit_size(&batch);
    batch.file_results[0].pdf_classification = Some(PdfClassificationAuditV1 {
        page_audit: None,
        status: PdfClassificationStatus::TextInParseWindow,
        page_count: Nullable(Some(2)),
        classification_cache_status: ClassificationCacheStatus::Miss,
        classification_cache_miss_reason: "classifier_identity_changed".to_string(),
        result_examined_pages: Nullable(Some(1)),
        run_inspected_pages: Nullable(Some(1)),
        nominal_charged_pages: 2,
        duration_ms: 3,
        transport: ClassificationTransport::Session,
        attempt_count: 1,
        classifier_build: "b".repeat(64),
        classifier_profile_hash: "c".repeat(64),
    });

    assert!(
        compute_audit_size(&batch) > parse_only_size,
        "classification execution child fields must contribute to audit_size_bytes"
    );
}

fn cache_record(
    identity: &str,
    source_version: &str,
    profile_hash: &str,
    content: &str,
) -> CacheWriteRecord {
    CacheWriteRecord {
        evidence_audit: None,
        cache_payload_version: 0,
        file_identity: identity.to_string(),
        source_version: source_version.to_string(),
        source_guard_kind: "content_sha256_v1".to_string(),
        source_guard_sha256: "0".repeat(64),
        parse_profile_hash: profile_hash.to_string(),
        content: content.to_string(),
        content_sha256: sha256_hex(content.as_bytes()),
        parser_backend: "light_text_v2".to_string(),
        worker_lane: "rust_core".to_string(),
        truncated: false,
        worker_contract_version: "ai_daily_context_v1".to_string(),
        worker_version: crate::engine_version().to_string(),
        worker_build: crate::ENGINE_BUILD_IDENTITY.to_string(),
    }
}

fn insert_orphan_artifact(store: &ScannerStore, created_at_ms: i64) {
    store
        .connection
        .execute(
            "INSERT INTO context_artifacts(
                    snapshot_eligible, snapshot_key_sha256, snapshot_key_json,
                    final_context, context_sha256, semantic_summary_json,
                    artifact_size_bytes, created_at_ms, last_accessed_bucket
                 ) VALUES (0, NULL, NULL, '', ?1, '{}', 1, ?2, '1970-01-01')",
            params!["0".repeat(64), created_at_ms],
        )
        .unwrap();
}

fn record_both_workers(store: &mut ScannerStore, active: &ActiveRun, now_ms: u64) {
    let office = WorkerFingerprint {
        contract: "ai_daily_worker_v3".to_string(),
        version: "0.1.0".to_string(),
        build: "office-build".to_string(),
    };
    let python = WorkerFingerprint {
        contract: "ai_daily_worker_v3".to_string(),
        version: "0.1.0".to_string(),
        build: "python-build".to_string(),
    };
    store
        .record_worker_fingerprints(active, Some(&office), Some(&python), now_ms)
        .unwrap();
}

fn success_batch(
    active: &ActiveRun,
    identity: &str,
    source_version: &str,
    profile_hash: &str,
    content: &str,
) -> FinalizationBatch {
    let summary = ContextSummary {
        source_file_count: 1,
        success_count: 1,
        timeout_count: 0,
        included_file_count: 1,
        omitted_file_count: 0,
        error_file_count: 0,
        input_chars: content.chars().count() as u64,
        output_chars: content.chars().count() as u64,
        total_duration_ms: 8,
        discovery_duration_ms: 2,
        parse_duration_ms: 3,
        compression_duration_ms: 1,
    };
    let envelope = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: active.request_id().to_string(),
        engine_version: crate::engine_version().to_string(),
        engine_build: crate::ENGINE_BUILD_IDENTITY.to_string(),
        status: EngineStatus::Ok,
        file_context: content.to_string(),
        summary: summary.clone(),
        scan_run_id: Nullable(Some(active.scan_run_id())),
        context_run_id: Nullable(Some(active.context_run_id())),
        warnings: Vec::new(),
        error: Nullable(None),
    };
    let artifact = crate::artifact::ArtifactDraft::new(
        false,
        content.to_string(),
        crate::artifact::SemanticSummary {
            source_file_count: 1,
            success_count: 1,
            timeout_count: 0,
            included_file_count: 1,
            omitted_file_count: 0,
            error_file_count: 0,
            input_chars: content.chars().count() as u64,
            output_chars: content.chars().count() as u64,
            reserved_chars: content.chars().count() as u64,
            rendered_chars: content.chars().count() as u64,
        },
        Vec::new(),
        Vec::new(),
    )
    .expect("ineligible payload artifact");
    FinalizationBatch {
        status: RunStatus::Success,
        envelope_json: canonical_envelope_json(&envelope).expect("canonical envelope"),
        inventory: vec![inventory_record(identity, source_version)],
        file_results: vec![success_file_result(
            identity,
            source_version,
            profile_hash,
            content,
        )],
        diagnostics: Vec::new(),
        stage_metrics: vec![
            StageMetric {
                stage: StageName::Discovery,
                item_count: 1,
                duration_ms: 2,
            },
            StageMetric {
                stage: StageName::Cache,
                item_count: 1,
                duration_ms: 1,
            },
            StageMetric {
                stage: StageName::Parse,
                item_count: 1,
                duration_ms: 3,
            },
            StageMetric {
                stage: StageName::Context,
                item_count: 1,
                duration_ms: 1,
            },
        ],
        extension_metrics: vec![ExtensionMetric {
            extension: ".txt".to_string(),
            file_count: 1,
            parse_duration_ms: 3,
            success_count: 1,
            error_count: 0,
            timeout_count: 0,
        }],
        context: Some(ContextRunRecord {
            context_profile_hash: "b".repeat(64),
            status: RunStatus::Success,
            final_context: content.to_string(),
            context_sha256: sha256_hex(content.as_bytes()),
            summary,
            decisions: vec![ContextDecisionRecord {
                file_identity: identity.to_string(),
                decision: ContextDecision {
                    relative_path: "evidence.txt".to_string(),
                    action: ContextAction::Keep,
                    reason: "fits budgets".to_string(),
                    priority: 1,
                    input_chars: content.chars().count() as u64,
                    output_chars: content.chars().count() as u64,
                    truncated: false,
                    error_code: String::new(),
                },
            }],
        }),
        artifact: Some(artifact),
        snapshot_key: None,
        snapshot_hit: None,
        execution_metrics: None,
    }
}

#[test]
fn canonical_request_excludes_ids_runtime_paths_and_builds() {
    let first = harness("00000000-0000-4000-8000-000000000001");
    let mut changed = first.request.clone();
    changed.request_id = "00000000-0000-4000-8000-000000000002".to_string();
    changed.scan_db_path = first
        ._directory
        .path()
        .join("other.sqlite3")
        .to_string_lossy()
        .to_string();
    changed.adapters.office_worker_path = first
        ._directory
        .path()
        .join("changed-worker.exe")
        .to_string_lossy()
        .to_string();
    let equivalent = ScannerStore::canonicalize_request(&changed, &first.profile).unwrap();

    assert_eq!(equivalent.json, first.canonical.json);
    assert_eq!(equivalent.hash, first.canonical.hash);
    assert!(!first.canonical.json.contains("request_id"));
    assert!(!first.canonical.json.contains("scan_db_path"));
    assert!(!first.canonical.json.contains("adapters"));
    assert!(first.canonical.json.contains("context_profile"));
}

#[test]
fn worker_count_does_not_change_parse_profile_hash() {
    let harness = harness("00000000-0000-4000-8000-000000000003");
    let route = RouteStackFingerprint::text("build-a").unwrap();
    let original = parse_profile_hash(1, &route, &harness.profile).unwrap();
    let mut changed = harness.profile.clone();
    changed.execution.max_workers = 1;
    let changed = parse_profile_hash(1, &route, &changed).unwrap();
    assert_eq!(original, changed);
}

#[test]
fn cache_aware_plan_consumes_pure_planner_output_without_reclassification() {
    let harness = harness("00000000-0000-4000-8000-000000000019");
    let file = DiscoveredFileOut {
        file_identity: "file-a".to_string(),
        path: "C:\\fixture\\evidence.txt".to_string(),
        extension: ".txt".to_string(),
        modified_at: "2026-07-16T00:00:00+08:00".to_string(),
        size_bytes: 5,
        source_version: "mtime_ns=100:size=5".to_string(),
        source_guard_kind: None,
        source_guard_sha256: None,
    };
    let stacks = RouteStackFingerprints {
        text_like: RouteStackFingerprint::text("engine-build").unwrap(),
        modern_office: RouteStackFingerprint::modern_office(
            "engine-build",
            "ai_daily_worker_v3",
            "office-build",
        )
        .unwrap(),
        pdf: RouteStackFingerprint::pdf("engine-build", "ai_daily_worker_v3", "python-build")
            .unwrap(),
    };
    let entries = harness
        .store
        .attach_cache_evidence(
            vec![
                PlannedFile {
                    file: file.clone(),
                    action: PlanAction::Parse(ParserRoute::LightText),
                    timeout_ms: 30_000,
                },
                PlannedFile {
                    file,
                    action: PlanAction::Reject(ClassificationError::FileTooLarge),
                    timeout_ms: 30_000,
                },
            ],
            &harness.profile,
            &stacks,
        )
        .unwrap();

    assert!(entries[0].parse_profile_hash.is_some());
    assert_eq!(
        entries[0].cache_lookup,
        Some(CacheLookup::Miss(CacheMissReason::NewFile))
    );
    assert_eq!(
        entries[1].planned.action,
        PlanAction::Reject(ClassificationError::FileTooLarge)
    );
    assert!(entries[1].parse_profile_hash.is_none());
    assert!(entries[1].cache_lookup.is_none());
}

#[test]
fn live_lease_returns_structured_same_and_different_request_errors() {
    let mut first = harness("00000000-0000-4000-8000-000000000004");
    let active = started(
        first
            .store
            .begin_run(
                &first.request.request_id,
                &first.canonical,
                &first.runtime,
                100_000,
            )
            .unwrap(),
    );
    let mut second = ScannerStore::open(&first.db_path).unwrap();

    assert_eq!(
        second
            .begin_run(
                &first.request.request_id,
                &first.canonical,
                &first.runtime,
                100_001
            )
            .unwrap_err(),
        StoreError::RequestInProgress
    );
    assert_eq!(
        second
            .begin_run(
                "00000000-0000-4000-8000-000000000005",
                &canonical_for_request_id(&first, "00000000-0000-4000-8000-000000000005",),
                &first.runtime,
                100_001,
            )
            .unwrap_err(),
        StoreError::ScanAlreadyRunning
    );
    let attempts: i64 = first
        .store
        .connection
        .query_row(
            "SELECT count(*) FROM scan_run_attempts WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(attempts, 1);
}

#[test]
fn heartbeat_prevents_reclaim_but_expired_lease_is_recovered() {
    let mut first = harness("00000000-0000-4000-8000-000000000006");
    let active = started(
        first
            .store
            .begin_run(
                &first.request.request_id,
                &first.canonical,
                &first.runtime,
                100_000,
            )
            .unwrap(),
    );
    first.store.heartbeat(&active, 115_000).unwrap();
    let mut second = ScannerStore::open(&first.db_path).unwrap();
    assert_eq!(
        second
            .begin_run(
                "00000000-0000-4000-8000-000000000007",
                &canonical_for_request_id(&first, "00000000-0000-4000-8000-000000000007",),
                &first.runtime,
                130_000,
            )
            .unwrap_err(),
        StoreError::ScanAlreadyRunning
    );

    let recovered = started(
        second
            .begin_run(
                "00000000-0000-4000-8000-000000000007",
                &canonical_for_request_id(&first, "00000000-0000-4000-8000-000000000007"),
                &first.runtime,
                135_001,
            )
            .unwrap(),
    );
    assert_ne!(recovered.scan_run_id(), active.scan_run_id());
    let first_status: String = second
        .connection
        .query_row(
            "SELECT status FROM scan_runs WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(first_status, "abandoned");
}

#[test]
fn explicit_abandon_atomically_releases_the_lease_and_allows_retry() {
    let mut harness = harness("00000000-0000-4000-8000-000000000108");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1_000,
            )
            .expect("begin run"),
    );

    harness
        .store
        .abandon_active_run(&active, 1_001)
        .expect("explicit abandon");

    let state: (String, String, i64) = harness
        .store
        .connection
        .query_row(
            "SELECT r.status, a.status,
                        (SELECT count(*) FROM engine_lease)
                 FROM scan_runs r
                 JOIN scan_run_attempts a ON a.scan_run_id=r.scan_run_id
                 WHERE r.scan_run_id=?1 AND a.attempt_number=1",
            [active.scan_run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("abandoned state");
    assert_eq!(state, ("abandoned".to_string(), "abandoned".to_string(), 0));
    let restored_busy_timeout_ms: i64 = harness
        .store
        .connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("busy timeout should remain readable");
    assert_eq!(restored_busy_timeout_ms, BUSY_TIMEOUT_MS as i64);

    let retried = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1_002,
            )
            .expect("retry abandoned run"),
    );
    assert_eq!(retried.scan_run_id(), active.scan_run_id());
    assert_eq!(retried.attempt_number(), 2);
}

#[test]
fn abandon_cleanup_is_zero_wait_atomic_and_commit_survives_restore_failure() {
    let mut harness = harness("00000000-0000-4000-8000-000000000211");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    let locker = rusqlite::Connection::open(&harness.db_path).unwrap();
    locker.busy_timeout(Duration::from_millis(0)).unwrap();
    locker.execute_batch("BEGIN IMMEDIATE").unwrap();

    assert!(matches!(
        harness.store.abandon_active_run(&active, 2),
        Err(StoreError::CacheWrite { .. })
    ));
    let (status, lease_count): (String, i64) = harness
        .store
        .connection
        .query_row(
            "SELECT status, (SELECT count(*) FROM engine_lease) FROM scan_runs
                 WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "running");
    assert_eq!(lease_count, 1, "busy cleanup cannot delete only the lease");
    locker.execute_batch("ROLLBACK").unwrap();

    TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE.with(|flag| flag.set(true));
    let committed = harness.store.abandon_active_run(&active, 3);
    TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE.with(|flag| flag.set(false));
    committed.expect("abandon COMMIT is authoritative despite restore failure");
    let (status, lease_count): (String, i64) = harness
        .store
        .connection
        .query_row(
            "SELECT status, (SELECT count(*) FROM engine_lease) FROM scan_runs
                 WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "abandoned");
    assert_eq!(lease_count, 0);
}

#[test]
fn abandoned_same_request_reuses_row_appends_attempt_and_clears_lease_on_finish() {
    let mut first = harness("00000000-0000-4000-8000-000000000008");
    let active = started(
        first
            .store
            .begin_run(
                &first.request.request_id,
                &first.canonical,
                &first.runtime,
                1_000,
            )
            .unwrap(),
    );
    let mut recovered_store = ScannerStore::open(&first.db_path).unwrap();
    let recovered = started(
        recovered_store
            .begin_run(
                &first.request.request_id,
                &first.canonical,
                &first.runtime,
                1_000 + LEASE_GRACE_MS + 1,
            )
            .unwrap(),
    );
    assert_eq!(recovered.scan_run_id(), active.scan_run_id());
    assert_eq!(recovered.attempt_number(), 2);
    let batch = error_batch(&recovered);
    finalize_for_test(&mut recovered_store, &recovered, &batch, 30_000).unwrap();
    let lease_count: i64 = recovered_store
        .connection
        .query_row("SELECT count(*) FROM engine_lease", [], |row| row.get(0))
        .unwrap();
    assert_eq!(lease_count, 0);
}

#[test]
fn new_run_finalizes_after_another_run_was_abandoned_without_audit_size() {
    let mut harness = harness("00000000-0000-4000-8000-000000000312");
    let old = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1_000,
            )
            .unwrap(),
    );
    harness.store.abandon_active_run(&old, 1_001).unwrap();
    let audit_size: Option<i64> = harness
        .store
        .connection
        .query_row(
            "SELECT audit_size_bytes FROM scan_runs WHERE scan_run_id=?1",
            [old.scan_run_id() as i64],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit_size, None);

    let next_id = "00000000-0000-4000-8000-000000000313";
    let canonical = canonical_for_request_id(&harness, next_id);
    let active = started(
        harness
            .store
            .begin_run(next_id, &canonical, &harness.runtime, 1_002)
            .unwrap(),
    );
    let batch = error_batch(&active);
    finalize_for_test(&mut harness.store, &active, &batch, 1_003).unwrap();
    assert_eq!(
        harness
            .store
            .load_terminal_envelope(active.scan_run_id())
            .unwrap()
            .envelope_json,
        batch.envelope_json
    );
    let old_status: String = harness
        .store
        .connection
        .query_row(
            "SELECT status FROM scan_runs WHERE scan_run_id=?1",
            [old.scan_run_id() as i64],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_status, "abandoned");
}

#[test]
fn expired_owner_cannot_revive_its_own_stale_lease() {
    let mut harness = harness("00000000-0000-4000-8000-000000000018");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1_000,
            )
            .unwrap(),
    );

    assert_eq!(
        harness
            .store
            .heartbeat(&active, 1_000 + LEASE_GRACE_MS + 1)
            .unwrap_err(),
        StoreError::LeaseLost
    );
}

#[test]
fn terminal_retry_is_byte_identical_after_runtime_build_changes_and_conflicts_reject() {
    let mut harness = harness("00000000-0000-4000-8000-000000000009");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    let batch = error_batch(&active);
    let expected = batch.envelope_json.clone();
    finalize_for_test(&mut harness.store, &active, &batch, 2).unwrap();

    let mut changed_identity = crate::identity::engine_identity();
    changed_identity.engine_build = "engine-build-that-did-not-exist-on-first-run".to_string();
    let changed_runtime =
        AttemptRuntime::from_request_with_identity(&harness.request, &changed_identity).unwrap();
    let stored = harness
        .store
        .begin_run(
            &harness.request.request_id,
            &harness.canonical,
            &changed_runtime,
            3,
        )
        .unwrap();
    let BeginRunOutcome::Stored(stored) = stored else {
        panic!("terminal retry must return stored bytes")
    };
    assert_eq!(stored.envelope_json.as_bytes(), expected.as_bytes());

    let mut changed_request = harness.request.clone();
    changed_request.end_date = "2099-12-31".to_string();
    let changed_profile = normalize_scanner_settings(
        &changed_request.scanner_settings,
        changed_request.report_mode,
    )
    .unwrap();
    let conflicting =
        ScannerStore::canonicalize_request(&changed_request, &changed_profile).unwrap();
    assert_eq!(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &conflicting,
                &changed_runtime,
                4,
            )
            .unwrap_err(),
        StoreError::RequestIdConflict
    );
}

#[test]
fn replay_rebuilds_success_envelope_from_metadata_and_artifact() {
    // spec Part 5.1: final_context is stored ONCE in the artifact; the
    // persisted scan_runs JSON is body-free, and idempotent replay rebuilds
    // the full ContextEnvelope from metadata + summary + artifact.
    let mut harness = harness("00000000-0000-4000-8000-000000000023");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let batch = success_batch(
        &active,
        "file-a",
        "mtime_ns=100:size=5",
        &"a".repeat(64),
        "hello",
    );
    let expected = batch.envelope_json.clone();
    assert!(
        expected.contains("hello"),
        "original envelope carries the body"
    );
    finalize_for_test(&mut harness.store, &active, &batch, 2).unwrap();

    let stored_json: String = harness
        .store
        .connection
        .query_row(
            "SELECT final_envelope_json FROM scan_runs WHERE scan_run_id=?1",
            [active.scan_run_id() as i64],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !stored_json.contains("hello"),
        "scan_runs JSON must not duplicate the body"
    );
    let value: serde_json::Value = serde_json::from_str(&stored_json).unwrap();
    assert!(
        value.get("file_context").is_none(),
        "body-free metadata must not carry file_context"
    );

    // Idempotent replay rebuilds the full envelope byte-for-byte.
    let stored = harness
        .store
        .begin_run(
            &harness.request.request_id,
            &harness.canonical,
            &harness.runtime,
            3,
        )
        .unwrap();
    let BeginRunOutcome::Stored(stored) = stored else {
        panic!("terminal retry must return stored bytes")
    };
    assert_eq!(
        stored.envelope_json.as_bytes(),
        expected.as_bytes(),
        "rebuilt envelope must match the original byte-for-byte"
    );
    assert!(
        stored.envelope.file_context.contains("hello"),
        "rebuilt envelope must recover the body from the artifact"
    );
}

#[test]
fn error_run_with_file_rows_produces_complete_evidence() {
    // spec Part 2.3: a committed Error run with file rows must carry a
    // context_runs row (artifact_id=NULL) whose summary reconciles with the
    // persisted file/decision/stage rows, so current evidence is complete
    // (previously: "error envelope has unexpected context rows" RUN_CORRUPT).
    let mut harness = harness("00000000-0000-4000-8000-000000000024");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let source = "mtime_ns=100:size=5";
    let profile_hash = "a".repeat(64);
    let file_error = Diagnostic {
        error_code: ErrorCode::ParserFailed,
        message: "parse failed".to_string(),
        retryable: false,
        stage: DiagnosticStage::Parse,
        file_path: Nullable(Some("C:\\fixture\\evidence.txt".to_string())),
        backend: Nullable(Some("light_text_v2".to_string())),
    };
    let summary = ContextSummary {
        source_file_count: 1,
        success_count: 0,
        timeout_count: 0,
        included_file_count: 0,
        omitted_file_count: 0,
        error_file_count: 1,
        input_chars: 5,
        output_chars: 0,
        total_duration_ms: 3,
        discovery_duration_ms: 2,
        parse_duration_ms: 1,
        compression_duration_ms: 0,
    };
    let context = ContextRunRecord {
        context_profile_hash: "0".repeat(64),
        status: RunStatus::Error,
        final_context: String::new(),
        context_sha256: sha256_hex(b""),
        summary: summary.clone(),
        decisions: vec![ContextDecisionRecord {
            file_identity: "file-a".to_string(),
            decision: ContextDecision {
                relative_path: "evidence.txt".to_string(),
                action: ContextAction::Error,
                reason: "parse_error".to_string(),
                priority: 0,
                input_chars: 5,
                output_chars: 0,
                truncated: false,
                error_code: "PARSER_FAILED".to_string(),
            },
        }],
    };
    let envelope = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: active.request_id().to_string(),
        engine_version: crate::engine_version().to_string(),
        engine_build: crate::ENGINE_BUILD_IDENTITY.to_string(),
        status: EngineStatus::Error,
        file_context: String::new(),
        summary: summary.clone(),
        scan_run_id: Nullable(Some(active.scan_run_id())),
        context_run_id: Nullable(Some(active.context_run_id())),
        warnings: Vec::new(),
        error: Nullable(Some(file_error.clone())),
    };
    let mut inventory = inventory_record("file-a", source);
    inventory.source_guard_kind = Some("content_sha256_v1".to_string());
    inventory.source_guard_sha256 = Some("a".repeat(64));
    let batch = FinalizationBatch {
        status: RunStatus::Error,
        envelope_json: canonical_envelope_json(&envelope).expect("canonical envelope"),
        inventory: vec![inventory],
        file_results: vec![FileResultRecord {
            file_identity: "file-a".to_string(),
            relative_path: "evidence.txt".to_string(),
            source_version: source.to_string(),
            parse_profile_hash: profile_hash,
            cache_status: CacheStatus::Miss,
            cache_miss_reason: CacheMissReason::NewFile,
            parse_status: ParseStatus::Error,
            parser_backend: "light_text_v2".to_string(),
            worker_lane: AuditWorkerLane::RustCore,
            truncated: false,
            content_sha256: sha256_hex(b""),
            primary_duration_ms: 1,
            fallback_duration_ms: 0,
            parse_duration_ms: 1,
            failure_class: "parser_failed".to_string(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: ParseTransport::RustInProcess,
            parse_attempt_count: 1,
            pdf_classification: None,
            error: Some(file_error),
        }],
        diagnostics: vec![RunDiagnosticRecord {
            severity: DiagnosticSeverity::Error,
            diagnostic: Diagnostic {
                error_code: ErrorCode::ParserFailed,
                message: "parse failed".to_string(),
                retryable: false,
                stage: DiagnosticStage::Parse,
                file_path: Nullable(Some("C:\\fixture\\evidence.txt".to_string())),
                backend: Nullable(Some("light_text_v2".to_string())),
            },
        }],
        stage_metrics: vec![
            StageMetric {
                stage: StageName::Discovery,
                item_count: 1,
                duration_ms: 2,
            },
            StageMetric {
                stage: StageName::Cache,
                item_count: 1,
                duration_ms: 0,
            },
            StageMetric {
                stage: StageName::Parse,
                item_count: 1,
                duration_ms: 1,
            },
            StageMetric {
                stage: StageName::Context,
                item_count: 1,
                duration_ms: 0,
            },
        ],
        extension_metrics: vec![ExtensionMetric {
            extension: ".txt".to_string(),
            file_count: 1,
            parse_duration_ms: 1,
            success_count: 0,
            error_count: 1,
            timeout_count: 0,
        }],
        context: Some(context),
        artifact: None,
        snapshot_key: None,
        snapshot_hit: None,
        execution_metrics: Some(ai_daily_scanner_contract::ExecutionMetricsV2 {
            discovery_observed_file_count: 1,
            refill_audit: Some(ai_daily_scanner_contract::RefillAudit {
                policy_version: "budget_refill_v3".into(),
                stop_reason: "complete".into(),
                max_rounds: 2,
                candidate_cap: 96,
                classification_page_cap: 80,
                extraction_cap: 8,
                context_char_cap: 500_000,
                candidate_slots_used: 0,
                classification_pages_held: 0,
                extraction_slots_used: 0,
                refunded_chars: 0,
                refunded_classification_pages: 0,
                rounds: Vec::new(),
            }),
            source_guard_content_hash_file_count: 0,
            source_guard_unavailable_count: 0,
            source_guard_bytes_read: 0,
            candidate_file_count: 0,
            admitted_file_count: 0,
            classification_slot_count: 0,
            confirmed_run_inspected_pages_total: 0,
            unobserved_classification_attempt_count: 0,
            nominal_charged_pages_total: 0,
            extraction_slot_count: 0,
            pdfplumber_invocations: 0,
            snapshot_hit: false,
            parse_cache_lookup_count: 0,
            classification_cache_lookup_count: 0,
            parse_cache_all_hit: Nullable(None),
            classification_cache_all_hit: Nullable(None),
            stage_deadline_exhausted_count: 0,
            session_restart_count: 0,
            session_fallback_count: 0,
            classify_attempt_count: 0,
            parse_attempt_count: 0,
            reserved_chars: 0,
            rendered_chars: 0,
            worker_handshake_ms: 5,
            discovery_ms: 2,
            snapshot_lookup_ms: 0,
            current_run_audit_write_ms: 0,
            terminal_precommit_ms: 0,
            deadline_precommit_elapsed_ms: 0,
            envelope_rebuild_ms: 0,
            terminal_rows_written: 0,
            peak_worker_rss_bytes: Nullable(None),
        }),
    };
    finalize_for_test(&mut harness.store, &active, &batch, 2).expect("error finalize");

    // The context_runs row must exist with artifact_id=NULL and status=error.
    let (context_run_id, status, artifact_id): (i64, String, Option<i64>) = harness
        .store
        .connection
        .query_row(
            "SELECT context_run_id, status, artifact_id
                 FROM context_runs WHERE scan_run_id=?1",
            [active.scan_run_id() as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(context_run_id, active.context_run_id() as i64);
    assert_eq!(status, "error");
    assert!(
        artifact_id.is_none(),
        "error run must reference no artifact"
    );

    let snapshot = harness
        .store
        .load_evidence(active.scan_run_id())
        .expect("evidence must load for an error run with file rows");
    assert_eq!(snapshot.run_status, RunStatus::Error);

    let evidence = crate::evidence::assemble_scanner_evidence(
        active.request_id(),
        active.scan_run_id(),
        &snapshot,
    )
    .expect("scanner evidence must assemble");
    assert_eq!(evidence.artifact_id.0, None, "error run has no artifact");
    assert_eq!(evidence.execution_metrics.worker_handshake_ms, 5);
    assert_eq!(evidence.execution_metrics.discovery_ms, 2);
    harness.store.connection.execute(
            "UPDATE scan_runs SET final_envelope_metadata_json=json_set(final_envelope_metadata_json,
                '$.pdf_page_audits', json('{\"invented\":{\"policy_version\":\"pdf_text_presence_v2\",\"checked_pages\":[1],\"observations\":null}}')) WHERE scan_run_id=?1",
            [active.scan_run_id() as i64]).unwrap();
    assert!(
        harness.store.load_evidence(active.scan_run_id()).is_err(),
        "a persisted page audit must belong to an observed PDF source"
    );
    harness.store.connection.execute(
            "UPDATE scan_runs SET final_envelope_metadata_json=json_remove(final_envelope_metadata_json,
                '$.pdf_page_audits') WHERE scan_run_id=?1", [active.scan_run_id() as i64]).unwrap();
    assert_eq!(
        evidence
            .execution_metrics
            .refill_audit
            .as_ref()
            .unwrap()
            .stop_reason,
        "complete"
    );

    let rows_written: i64 = harness
        .store
        .connection
        .query_row(
            "SELECT terminal_rows_written FROM scan_execution_metrics WHERE scan_run_id=?1",
            [active.scan_run_id() as i64],
            |row| row.get(0),
        )
        .expect("terminal row count");
    assert_eq!(
        rows_written, 13,
        "terminal rows include scan_file_execution but exclude global inventory"
    );

    // Missing per-file execution provenance fails closed.
    harness
        .store
        .connection
        .execute(
            "DELETE FROM scan_file_execution WHERE scan_run_id=?1",
            [active.scan_run_id() as i64],
        )
        .expect("simulate pre-amendment full-v2 run");
    let historical = harness
        .store
        .load_evidence(active.scan_run_id())
        .expect("persisted rows remain readable");
    assert_eq!(historical.files_v2[0].parse_transport, None);
    assert_eq!(historical.files_v2[0].parse_attempt_count, None);

    assert!(
        crate::evidence::assemble_scanner_evidence(
            active.request_id(),
            active.scan_run_id(),
            &historical,
        )
        .is_err(),
        "current evidence must fail closed when provenance is unavailable"
    );
    harness.store.connection.execute(
            "UPDATE scan_runs SET final_envelope_metadata_json=json_set(final_envelope_metadata_json,
                '$.refill_audit.candidate_slots_used', 97) WHERE scan_run_id=?1",
            [active.scan_run_id() as i64]).unwrap();
    assert!(
        harness.store.load_evidence(active.scan_run_id()).is_err(),
        "over-budget persisted refill audit must fail closed"
    );
}

#[test]
fn zero_file_error_run_produces_complete_evidence() {
    // spec Part 2.3 regression: a zero-file scheduler Error outcome
    // (source-file ceiling, BUDGET_MODEL_MISMATCH, enforced-render mismatch)
    // commits a context_runs row with 4 reconciling zero stage metrics, so
    // current evidence succeeds (previously `stages.len() != 4` RUN_CORRUPT).
    let mut harness = harness("00000000-0000-4000-8000-000000000025");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let error = Diagnostic {
        error_code: ErrorCode::SourceFileLimitExceeded,
        message: "discovery observed 1000001 source files, exceeding the engine ceiling of 1000000"
            .to_string(),
        retryable: false,
        stage: DiagnosticStage::Discovery,
        file_path: Nullable(None),
        backend: Nullable(None),
    };
    let summary = empty_summary();
    let context = ContextRunRecord {
        context_profile_hash: "0".repeat(64),
        status: RunStatus::Error,
        final_context: String::new(),
        context_sha256: sha256_hex(b""),
        summary: summary.clone(),
        decisions: Vec::new(),
    };
    let envelope = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: active.request_id().to_string(),
        engine_version: crate::engine_version().to_string(),
        engine_build: crate::ENGINE_BUILD_IDENTITY.to_string(),
        status: EngineStatus::Error,
        file_context: String::new(),
        summary,
        scan_run_id: Nullable(Some(active.scan_run_id())),
        context_run_id: Nullable(Some(active.context_run_id())),
        warnings: Vec::new(),
        error: Nullable(Some(error.clone())),
    };
    let batch = FinalizationBatch {
        status: RunStatus::Error,
        envelope_json: canonical_envelope_json(&envelope).expect("canonical envelope"),
        inventory: Vec::new(),
        file_results: Vec::new(),
        diagnostics: vec![RunDiagnosticRecord {
            severity: DiagnosticSeverity::Error,
            diagnostic: error,
        }],
        stage_metrics: vec![
            StageMetric {
                stage: StageName::Discovery,
                item_count: 0,
                duration_ms: 0,
            },
            StageMetric {
                stage: StageName::Cache,
                item_count: 0,
                duration_ms: 0,
            },
            StageMetric {
                stage: StageName::Parse,
                item_count: 0,
                duration_ms: 0,
            },
            StageMetric {
                stage: StageName::Context,
                item_count: 0,
                duration_ms: 0,
            },
        ],
        extension_metrics: Vec::new(),
        context: Some(context),
        artifact: None,
        snapshot_key: None,
        snapshot_hit: None,
        execution_metrics: Some(ai_daily_scanner_contract::ExecutionMetricsV2 {
            discovery_observed_file_count: 1_000_001,
            refill_audit: None,
            source_guard_content_hash_file_count: 0,
            source_guard_unavailable_count: 0,
            source_guard_bytes_read: 0,
            candidate_file_count: 0,
            admitted_file_count: 0,
            classification_slot_count: 0,
            confirmed_run_inspected_pages_total: 0,
            unobserved_classification_attempt_count: 0,
            nominal_charged_pages_total: 0,
            extraction_slot_count: 0,
            pdfplumber_invocations: 0,
            snapshot_hit: false,
            parse_cache_lookup_count: 0,
            classification_cache_lookup_count: 0,
            parse_cache_all_hit: Nullable(None),
            classification_cache_all_hit: Nullable(None),
            stage_deadline_exhausted_count: 0,
            session_restart_count: 0,
            session_fallback_count: 0,
            classify_attempt_count: 0,
            parse_attempt_count: 0,
            reserved_chars: 0,
            rendered_chars: 0,
            worker_handshake_ms: 5,
            discovery_ms: 2,
            snapshot_lookup_ms: 0,
            current_run_audit_write_ms: 0,
            terminal_precommit_ms: 0,
            deadline_precommit_elapsed_ms: 0,
            envelope_rebuild_ms: 0,
            terminal_rows_written: 0,
            peak_worker_rss_bytes: Nullable(None),
        }),
    };
    finalize_for_test(&mut harness.store, &active, &batch, 2).expect("zero-file error finalize");

    let (context_run_id, status, artifact_id): (i64, String, Option<i64>) = harness
        .store
        .connection
        .query_row(
            "SELECT context_run_id, status, artifact_id
                 FROM context_runs WHERE scan_run_id=?1",
            [active.scan_run_id() as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(context_run_id, active.context_run_id() as i64);
    assert_eq!(status, "error");
    assert!(artifact_id.is_none());

    let snapshot = harness
        .store
        .load_evidence(active.scan_run_id())
        .expect("evidence must load for a zero-file error run");
    assert_eq!(snapshot.run_status, RunStatus::Error);
    assert_eq!(
        snapshot.stage_metrics.len(),
        4,
        "zero-file error must persist 4 stage rows"
    );

    let evidence = crate::evidence::assemble_scanner_evidence(
        active.request_id(),
        active.scan_run_id(),
        &snapshot,
    )
    .expect("scanner evidence must assemble");
    assert_eq!(
        evidence.execution_metrics.discovery_observed_file_count,
        1_000_001
    );
}

#[test]
fn cache_miss_hit_source_profile_and_build_invalidation_are_explicit() {
    let mut harness = harness("00000000-0000-4000-8000-000000000010");
    let source = "mtime_ns=100:size=5";
    let route = RouteStackFingerprint::text("build-a").unwrap();
    let profile_hash = parse_profile_hash(1, &route, &harness.profile).unwrap();
    assert_eq!(
        harness
            .store
            .lookup_cache(
                "file-a",
                source,
                "content_sha256_v1",
                &"0".repeat(64),
                &profile_hash,
                false
            )
            .unwrap(),
        CacheLookup::Miss(CacheMissReason::NewFile)
    );
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let batch = success_batch(&active, "file-a", source, &profile_hash, "hello");
    harness
        .store
        .prepare_inventory(&batch.inventory, active.scan_run_id, 2)
        .unwrap();
    harness
        .store
        .write_success_parse_cache(&[cache_record("file-a", source, &profile_hash, "hello")], 2)
        .unwrap();
    finalize_for_test(&mut harness.store, &active, &batch, 2).unwrap();
    assert!(matches!(
        harness
            .store
            .lookup_cache(
                "file-a",
                source,
                "content_sha256_v1",
                &"0".repeat(64),
                &profile_hash,
                false
            )
            .unwrap(),
        CacheLookup::Fresh(_)
    ));
    assert_eq!(
        harness
            .store
            .lookup_cache(
                "file-a",
                "mtime_ns=101:size=6",
                "content_sha256_v1",
                &"0".repeat(64),
                &profile_hash,
                false
            )
            .unwrap(),
        CacheLookup::Miss(CacheMissReason::SourceVersionChanged)
    );
    assert_eq!(
        harness
            .store
            .lookup_cache(
                "file-a",
                "mtime_ns=100:size=6",
                "content_sha256_v1",
                &"0".repeat(64),
                &profile_hash,
                false
            )
            .unwrap(),
        CacheLookup::Miss(CacheMissReason::SourceVersionChanged)
    );
    assert_eq!(
        harness
            .store
            .lookup_cache(
                "file-a",
                "mtime_ns=101:size=5",
                "content_sha256_v1",
                &"0".repeat(64),
                &profile_hash,
                false
            )
            .unwrap(),
        CacheLookup::Miss(CacheMissReason::SourceVersionChanged)
    );

    let changed_build = RouteStackFingerprint::text("build-b").unwrap();
    let changed_hash = parse_profile_hash(1, &changed_build, &harness.profile).unwrap();
    assert_eq!(
        harness
            .store
            .lookup_cache(
                "file-a",
                source,
                "content_sha256_v1",
                &"0".repeat(64),
                &changed_hash,
                false
            )
            .unwrap(),
        CacheLookup::Miss(CacheMissReason::ParserIdentityChanged)
    );
}

#[test]
fn batch_cache_lookup_matches_scalar_across_chunk_boundary_and_rejects_corruption() {
    let mut harness = harness("00000000-0000-4000-8000-000000000030");
    let source = "mtime_ns=100:size=5";
    let changed_source = "mtime_ns=101:size=5";
    let profile_hash = "a".repeat(64);
    let changed_profile_hash = "b".repeat(64);
    let zero_guard = "0".repeat(64);

    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    harness
        .store
        .prepare_inventory(
            &[
                inventory_record("file-fresh", source),
                inventory_record("file-error", source),
                inventory_record("file-profile", source),
                inventory_record("file-source", source),
            ],
            i64::try_from(active.scan_run_id()).unwrap(),
            1,
        )
        .unwrap();

    harness
        .store
        .write_success_parse_cache(
            &[
                cache_record("file-fresh", source, &profile_hash, "fresh"),
                cache_record("file-error", source, &profile_hash, "corrupt"),
                cache_record("file-profile", source, &profile_hash, "profile"),
                cache_record("file-source", source, &profile_hash, "source"),
            ],
            1,
        )
        .unwrap();
    harness
        .store
        .connection
        .execute(
            "UPDATE parse_cache SET content_sha256=?1 WHERE file_identity=?2",
            params![&zero_guard, "file-error"],
        )
        .unwrap();

    let mut cases = (0..cache::CACHE_LOOKUP_BATCH_SIZE)
        .map(|index| {
            (
                format!("file-new-{index}"),
                source.to_string(),
                profile_hash.clone(),
                false,
            )
        })
        .collect::<Vec<_>>();
    let special_start = cases.len();
    cases.extend([
        (
            "file-fresh".to_string(),
            source.to_string(),
            profile_hash.clone(),
            false,
        ),
        (
            "file-profile".to_string(),
            source.to_string(),
            changed_profile_hash,
            false,
        ),
        (
            "file-source".to_string(),
            changed_source.to_string(),
            profile_hash.clone(),
            false,
        ),
        (
            "file-evicted".to_string(),
            source.to_string(),
            profile_hash.clone(),
            true,
        ),
        (
            "file-new".to_string(),
            source.to_string(),
            profile_hash,
            false,
        ),
    ]);
    let keys = cases
        .iter()
        .map(
            |(file_identity, source_version, parse_profile_hash, existed)| cache::CacheLookupKey {
                file_identity,
                source_version,
                source_guard_kind: "content_sha256_v1",
                source_guard_sha256: &zero_guard,
                parse_profile_hash,
                inventory_existed_before: *existed,
            },
        )
        .collect::<Vec<_>>();

    let scalar = keys
        .iter()
        .map(|key| {
            harness.store.lookup_cache(
                key.file_identity,
                key.source_version,
                key.source_guard_kind,
                key.source_guard_sha256,
                key.parse_profile_hash,
                key.inventory_existed_before,
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let batch = harness.store.lookup_cache_batch(&keys).unwrap();

    assert_eq!(batch, scalar);
    assert!(matches!(batch[special_start], CacheLookup::Fresh(_)));
    assert_eq!(
        batch[special_start + 1],
        CacheLookup::Miss(CacheMissReason::ParserIdentityChanged)
    );
    assert_eq!(
        batch[special_start + 2],
        CacheLookup::Miss(CacheMissReason::SourceVersionChanged)
    );
    assert_eq!(
        batch[special_start + 3],
        CacheLookup::Miss(CacheMissReason::EntryAbsentOrEvicted)
    );
    assert_eq!(
        batch[special_start + 4],
        CacheLookup::Miss(CacheMissReason::NewFile)
    );

    let scalar_error = harness
        .store
        .lookup_cache(
            "file-error",
            source,
            "content_sha256_v1",
            &zero_guard,
            &"a".repeat(64),
            false,
        )
        .expect_err("a corrupt exact row must fail the run-level lookup");
    assert!(matches!(scalar_error, StoreError::RunCorrupt(_)));

    let corrupt_profile_hash = "a".repeat(64);
    let corrupt_key = cache::CacheLookupKey {
        file_identity: "file-error",
        source_version: source,
        source_guard_kind: "content_sha256_v1",
        source_guard_sha256: &zero_guard,
        parse_profile_hash: &corrupt_profile_hash,
        inventory_existed_before: false,
    };
    let batch_error = harness
        .store
        .lookup_cache_batch(&[corrupt_key])
        .expect_err("batch lookup must not downgrade exact-row corruption to a miss");
    assert!(matches!(batch_error, StoreError::RunCorrupt(_)));

    harness
        .store
        .connection
        .execute(
            "UPDATE parse_cache
                 SET content_sha256=?1, parser_backend=''
                 WHERE file_identity='file-error'",
            [sha256_hex(b"corrupt")],
        )
        .unwrap();
    let value_error = harness
        .store
        .lookup_cache(
            "file-error",
            source,
            "content_sha256_v1",
            &zero_guard,
            &"a".repeat(64),
            false,
        )
        .expect_err("invalid exact-row values must fail the run-level lookup");
    assert!(matches!(value_error, StoreError::RunCorrupt(_)));
}

#[test]
fn exact_classification_cache_corruption_fails_closed() {
    let mut harness = harness("00000000-0000-4000-8000-000000000031");
    let source = "mtime_ns=100:size=5";
    let guard = "0".repeat(64);
    let classifier_profile_hash = "c".repeat(64);
    let classifier_build = "a".repeat(64);
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    harness
        .store
        .prepare_inventory(
            &[inventory_record("file-classification-corrupt", source)],
            i64::try_from(active.scan_run_id()).unwrap(),
            1,
        )
        .unwrap();
    harness
        .store
        .write_success_classification_cache(
            &[ClassificationCacheWriteRecord {
                file_identity: "file-classification-corrupt".to_string(),
                source_version: source.to_string(),
                source_guard_kind: "content_sha256_v1".to_string(),
                source_guard_sha256: guard.clone(),
                classifier_profile_hash: classifier_profile_hash.clone(),
                classifier_build: classifier_build.clone(),
                status: "text_in_parse_window".to_string(),
                page_count: 5,
                result_examined_pages: 1,
            }],
            1,
        )
        .unwrap();
    harness
        .store
        .connection
        .execute(
            "UPDATE classification_cache
                 SET result_examined_pages=0
                 WHERE file_identity='file-classification-corrupt'",
            [],
        )
        .unwrap();

    let error = harness
        .store
        .lookup_classification_cache(
            "file-classification-corrupt",
            source,
            "content_sha256_v1",
            &guard,
            &classifier_profile_hash,
            &classifier_build,
            false,
        )
        .expect_err("exact classification cache corruption must fail the run");
    assert!(matches!(error, StoreError::RunCorrupt(_)));
}

#[test]
fn parse_cache_key_is_source_guard_bound() {
    // spec R3-29: a same-size + preserved-mtime content replacement keeps the
    // legacy source_version unchanged, so the SourceGuardV2 must be part of the
    // parse cache key. Changing only the guard forces a miss, never a Fresh hit.
    let mut harness = harness("00000000-0000-4000-8000-000000000099");
    let source = "mtime_ns=100:size=5";
    let profile_hash = "b".repeat(64);
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let batch = success_batch(&active, "file-a", source, &profile_hash, "hello");
    harness
        .store
        .prepare_inventory(&batch.inventory, active.scan_run_id, 2)
        .unwrap();
    harness
        .store
        .write_success_parse_cache(&[cache_record("file-a", source, &profile_hash, "hello")], 2)
        .unwrap();
    finalize_for_test(&mut harness.store, &active, &batch, 2).unwrap();
    // Same identity + source_version + profile, SAME guard -> Fresh.
    assert!(matches!(
        harness
            .store
            .lookup_cache(
                "file-a",
                source,
                "content_sha256_v1",
                &"0".repeat(64),
                &profile_hash,
                false,
            )
            .unwrap(),
        CacheLookup::Fresh(_)
    ));
    // Same identity + source_version + profile, DIFFERENT guard -> miss.
    assert_eq!(
        harness
            .store
            .lookup_cache(
                "file-a",
                source,
                "content_sha256_v1",
                &"1".repeat(64),
                &profile_hash,
                false,
            )
            .unwrap(),
        CacheLookup::Miss(CacheMissReason::SourceVersionChanged)
    );
    // A fresh write with the new guard lands under its own key and hits.
    let mut second = cache_record("file-a", source, &profile_hash, "changed");
    second.source_guard_sha256 = "1".repeat(64);
    harness
        .store
        .write_success_parse_cache(&[second], 5)
        .unwrap();
    assert!(matches!(
        harness
            .store
            .lookup_cache(
                "file-a",
                source,
                "content_sha256_v1",
                &"1".repeat(64),
                &profile_hash,
                false,
            )
            .unwrap(),
        CacheLookup::Fresh(_)
    ));
}

#[test]
fn one_file_source_change_invalidates_exactly_one_cache_entry() {
    let mut harness = harness("00000000-0000-4000-8000-000000000011");
    let source = "mtime_ns=100:size=5";
    let profile_hash = "a".repeat(64);
    let first = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &first, 1);
    let first_batch = success_batch(&first, "file-a", source, &profile_hash, "hello");
    harness
        .store
        .prepare_inventory(&first_batch.inventory, first.scan_run_id, 2)
        .unwrap();
    harness
        .store
        .write_success_parse_cache(&[cache_record("file-a", source, &profile_hash, "hello")], 2)
        .unwrap();
    finalize_for_test(&mut harness.store, &first, &first_batch, 2).unwrap();
    let second_request_id = "00000000-0000-4000-8000-000000000016";
    let second_canonical = canonical_for_request_id(&harness, second_request_id);
    let second = started(
        harness
            .store
            .begin_run(second_request_id, &second_canonical, &harness.runtime, 3)
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &second, 3);
    let second_batch = success_batch(&second, "file-b", source, &profile_hash, "world");
    harness
        .store
        .prepare_inventory(&second_batch.inventory, second.scan_run_id, 4)
        .unwrap();
    harness
        .store
        .write_success_parse_cache(&[cache_record("file-b", source, &profile_hash, "world")], 4)
        .unwrap();
    finalize_for_test(&mut harness.store, &second, &second_batch, 4).unwrap();

    let lookups = [
        harness
            .store
            .lookup_cache(
                "file-a",
                "mtime_ns=101:size=6",
                "content_sha256_v1",
                &"0".repeat(64),
                &profile_hash,
                false,
            )
            .unwrap(),
        harness
            .store
            .lookup_cache(
                "file-b",
                source,
                "content_sha256_v1",
                &"0".repeat(64),
                &profile_hash,
                false,
            )
            .unwrap(),
    ];
    assert_eq!(
        lookups
            .iter()
            .filter(|lookup| matches!(lookup, CacheLookup::Miss(_)))
            .count(),
        1
    );
    assert_eq!(
        lookups
            .iter()
            .filter(|lookup| matches!(lookup, CacheLookup::Fresh(_)))
            .count(),
        1
    );
}

#[test]
fn error_results_are_never_success_cache_hits() {
    let mut harness = harness("00000000-0000-4000-8000-000000000012");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let source = "mtime_ns=100:size=5";
    let profile_hash = "a".repeat(64);
    let diagnostic = Diagnostic {
        error_code: ErrorCode::ParserFailed,
        message: "parse failed".to_string(),
        retryable: false,
        stage: DiagnosticStage::Parse,
        file_path: Nullable(Some("C:\\fixture\\evidence.txt".to_string())),
        backend: Nullable(Some("light_text_v2".to_string())),
    };
    let mut batch = error_batch(&active);
    batch.inventory.push(inventory_record("file-a", source));
    batch.file_results.push(FileResultRecord {
        file_identity: "file-a".to_string(),
        relative_path: "evidence.txt".to_string(),
        source_version: source.to_string(),
        parse_profile_hash: profile_hash.clone(),
        cache_status: CacheStatus::Miss,
        cache_miss_reason: CacheMissReason::NewFile,
        parse_status: ParseStatus::Error,
        parser_backend: "light_text_v2".to_string(),
        worker_lane: AuditWorkerLane::RustCore,
        truncated: false,
        content_sha256: sha256_hex(b""),
        primary_duration_ms: 1,
        fallback_duration_ms: 0,
        parse_duration_ms: 1,
        failure_class: "parser_failed".to_string(),
        fallback_backend: String::new(),
        fallback_reason_code: String::new(),
        parse_transport: ParseTransport::RustInProcess,
        parse_attempt_count: 1,
        pdf_classification: None,
        error: Some(diagnostic),
    });
    finalize_for_test(&mut harness.store, &active, &batch, 2).unwrap();
    // v2 spec Part 4: no negative cache — an Error result row is per-run
    // audit, not a cache entry, so the guard-bound cache lookup reports a
    // new-file miss rather than the legacy `error_cache` literal.
    assert_eq!(
        harness
            .store
            .lookup_cache(
                "file-a",
                source,
                "content_sha256_v1",
                &"0".repeat(64),
                &profile_hash,
                false
            )
            .unwrap(),
        CacheLookup::Miss(CacheMissReason::NewFile)
    );
}

#[test]
fn finalize_persists_inventory_source_guard_columns() {
    let mut harness = harness("00000000-0000-4000-8000-000000000021");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let source = "mtime_ns=100:size=5";
    let mut batch = error_batch(&active);
    let mut record = inventory_record("file-a", source);
    record.source_guard_kind = Some("content_sha256_v1".to_string());
    record.source_guard_sha256 = Some("a".repeat(64));
    batch.inventory = vec![record];
    finalize_for_test(&mut harness.store, &active, &batch, 2).unwrap();

    let connection = rusqlite::Connection::open(&harness.db_path).unwrap();
    let (kind, hash): (Option<String>, Option<String>) = connection
        .query_row(
            "SELECT source_guard_kind, source_guard_sha256
                 FROM file_inventory WHERE file_identity=?1",
            ["file-a"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(kind.as_deref(), Some("content_sha256_v1"));
    assert_eq!(hash.as_deref(), Some("a".repeat(64).as_str()));
}

#[test]
fn finalize_persists_unavailable_inventory_guard_with_null_hash() {
    let mut harness = harness("00000000-0000-4000-8000-000000000022");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let source = "mtime_ns=100:size=5";
    let mut batch = error_batch(&active);
    let mut record = inventory_record("file-b", source);
    record.source_guard_kind = Some("unavailable".to_string());
    record.source_guard_sha256 = None;
    batch.inventory = vec![record];
    finalize_for_test(&mut harness.store, &active, &batch, 2).unwrap();

    let connection = rusqlite::Connection::open(&harness.db_path).unwrap();
    let (kind, hash): (Option<String>, Option<String>) = connection
        .query_row(
            "SELECT source_guard_kind, source_guard_sha256
                 FROM file_inventory WHERE file_identity=?1",
            ["file-b"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(kind.as_deref(), Some("unavailable"));
    assert_eq!(hash, None, "unavailable guard must persist a null hash");
}

#[test]
fn not_parsed_rows_write_not_applicable_with_empty_miss_reason() {
    // spec Part 5.2/5.3 regression (real-corpus RUN_CORRUPT): a NotParsed row
    // that performed a lookup (cache_status=Miss, reason=new_file) must be
    // persisted with parse_cache_status=not_applicable and an EMPTY
    // cache_miss_reason. FileAuditV2::validate fail-closes any non-miss row
    // that carries a non-empty reason.
    let mut harness = harness("00000000-0000-4000-8000-000000000023");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let source = "mtime_ns=100:size=5";
    let profile_hash = "a".repeat(64);
    let mut batch = error_batch(&active);
    batch.inventory.push(inventory_record("file-a", source));
    batch.file_results.push(FileResultRecord {
        file_identity: "file-a".to_string(),
        relative_path: "evidence.txt".to_string(),
        source_version: source.to_string(),
        parse_profile_hash: profile_hash.clone(),
        cache_status: CacheStatus::Miss,
        cache_miss_reason: CacheMissReason::NewFile,
        parse_status: ParseStatus::NotParsed,
        parser_backend: "not_parsed".to_string(),
        worker_lane: AuditWorkerLane::NotParsed,
        truncated: false,
        content_sha256: sha256_hex(b""),
        primary_duration_ms: 0,
        fallback_duration_ms: 0,
        parse_duration_ms: 0,
        failure_class: String::new(),
        fallback_backend: String::new(),
        fallback_reason_code: String::new(),
        parse_transport: ParseTransport::NotApplicable,
        parse_attempt_count: 0,
        pdf_classification: None,
        error: None,
    });
    finalize_for_test(&mut harness.store, &active, &batch, 2).unwrap();

    let connection = rusqlite::Connection::open(&harness.db_path).unwrap();
    let (parse_cache_status, cache_miss_reason): (Option<String>, String) = connection
        .query_row(
            "SELECT parse_cache_status, cache_miss_reason
                 FROM scan_file_results WHERE file_identity=?1",
            ["file-a"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(parse_cache_status.as_deref(), Some("not_applicable"));
    assert_eq!(
        cache_miss_reason, "",
        "not_applicable rows must carry an empty miss reason"
    );
}

#[test]
fn failed_terminal_transaction_preserves_independent_inventory_only() {
    let mut harness = harness("00000000-0000-4000-8000-000000000013");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let source = "mtime_ns=100:size=5";
    let profile_hash = "a".repeat(64);
    let batch = success_batch(&active, "file-a", source, &profile_hash, "hello");
    harness
        .store
        .connection
        .execute_batch(
            "CREATE TRIGGER inject_finalization_failure
                 BEFORE INSERT ON context_runs
                 BEGIN
                    SELECT RAISE(ABORT, 'injected finalization failure');
                 END;",
        )
        .unwrap();

    assert!(matches!(
        finalize_for_test(&mut harness.store, &active, &batch, 2),
        Err(StoreError::CacheWrite { .. })
    ));
    for table in [
        "file_inventory",
        "parse_cache",
        "scan_file_results",
        "context_runs",
    ] {
        let count: i64 = harness
            .store
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        let expected = i64::from(table == "file_inventory");
        assert_eq!(count, expected, "{table}");
    }
    let status: String = harness
        .store
        .connection
        .query_row(
            "SELECT status FROM scan_runs WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "running");
}

#[test]
fn absolute_deadline_expiry_never_commits_a_terminal_row() {
    let mut harness = harness("00000000-0000-4000-8000-000000000113");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    let batch = error_batch(&active);
    let clock = ExpiringClock::new(3, 5_000);
    let deadlines = crate::deadline::RunDeadlines::derive(5_000, &clock).unwrap();
    clock.reset();

    assert!(matches!(
        harness
            .store
            .finalize(&active, &batch, 2, deadlines, &clock),
        Err(StoreError::AbsoluteDeadlineExhausted)
    ));
    let status: String = harness
        .store
        .connection
        .query_row(
            "SELECT status FROM scan_runs WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "running");
    let diagnostics: i64 = harness
        .store
        .connection
        .query_row(
            "SELECT count(*) FROM run_diagnostics WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(diagnostics, 0);
}

#[test]
fn terminal_audit_batches_commit_completely_at_small_and_large_scales() {
    for (suffix, warning_count) in [(201_u64, 1_usize), (202, 1_000), (203, 10_000)] {
        let request_id = format!("00000000-0000-4000-8000-{suffix:012}");
        let mut harness = harness(&request_id);
        let active = started(
            harness
                .store
                .begin_run(
                    &harness.request.request_id,
                    &harness.canonical,
                    &harness.runtime,
                    1,
                )
                .unwrap(),
        );
        let batch = diagnostic_audit_batch(&active, warning_count);
        let clock = FixedClock(0);
        let deadlines = crate::deadline::RunDeadlines::derive(3_600_000, &clock).unwrap();

        harness
            .store
            .finalize(&active, &batch, 2, deadlines, &clock)
            .expect("complete terminal audit commit");

        let (status, diagnostic_count): (String, i64) = harness
            .store
            .connection
            .query_row(
                "SELECT status, (SELECT count(*) FROM run_diagnostics WHERE scan_run_id=?1)
                     FROM scan_runs WHERE scan_run_id=?1",
                [active.scan_run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "error");
        assert_eq!(diagnostic_count, warning_count as i64 + 1);
    }
}

#[test]
fn absolute_deadline_after_one_open_transaction_batch_rolls_back_every_terminal_row() {
    let mut harness = harness("00000000-0000-4000-8000-000000000204");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    let batch = diagnostic_audit_batch(&active, 1_000);
    TEST_TERMINAL_DIAGNOSTIC_BATCHES_WRITTEN.with(|count| count.set(0));
    let clock = DiagnosticBatchExpiringClock {
        expire_after_batches: 1,
        expired_at_ms: 5_000,
    };
    let deadlines = crate::deadline::RunDeadlines::derive(5_000, &clock).unwrap();

    let result = harness
        .store
        .finalize(&active, &batch, 2, deadlines, &clock);
    let observed_batches = TEST_TERMINAL_DIAGNOSTIC_BATCHES_WRITTEN.with(|count| count.replace(0));
    assert_eq!(result, Err(StoreError::AbsoluteDeadlineExhausted));
    assert_eq!(observed_batches, 1, "one SQL batch executed before expiry");

    let (status, diagnostic_count): (String, i64) = harness
        .store
        .connection
        .query_row(
            "SELECT status, (SELECT count(*) FROM run_diagnostics WHERE scan_run_id=?1)
                 FROM scan_runs WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "running");
    assert_eq!(
        diagnostic_count, 0,
        "the open terminal transaction rolled back"
    );
}

#[test]
fn prepare_inventory_deadline_after_first_batch_preserves_only_the_global_prefix() {
    let mut harness = harness("00000000-0000-4000-8000-000000000205");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    let records: Vec<InventoryRecord> = (0..257)
        .map(|index| inventory_record(&format!("file-{index:03}"), "mtime_ns=100:size=5"))
        .collect();
    let clock = ExpiringClock::new(517, 3_000);
    let deadlines = crate::deadline::RunDeadlines::derive(5_000, &clock).unwrap();
    clock.reset();

    assert_eq!(
        harness.store.prepare_inventory_with_deadline(
            &records,
            active.scan_run_id,
            2,
            deadlines,
            &clock,
        ),
        Err(StoreError::WorkDeadlineExhausted)
    );
    let persisted: i64 = harness
        .store
        .connection
        .query_row("SELECT count(*) FROM file_inventory", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        persisted, 256,
        "only the committed global prefix may remain"
    );
    let final_identity_present: bool = harness
        .store
        .connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM file_inventory WHERE file_identity='file-256')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !final_identity_present,
        "no completed inventory receipt was returned"
    );
}

#[test]
fn cache_commit_remains_authoritative_when_work_deadline_expires_on_return() {
    let mut harness = harness("00000000-0000-4000-8000-000000000206");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    let records = vec![
        inventory_record("file-a", "mtime_ns=100:size=5"),
        inventory_record("file-b", "mtime_ns=100:size=5"),
    ];
    harness
        .store
        .prepare_inventory(&records, active.scan_run_id, 1)
        .unwrap();
    let first = cache_record("file-a", "mtime_ns=100:size=5", &"a".repeat(64), "a");
    let second = cache_record("file-b", "mtime_ns=100:size=5", &"a".repeat(64), "b");
    let clock = ExpiringClock::new(8, 3_000);
    let deadlines = crate::deadline::RunDeadlines::derive(5_000, &clock).unwrap();
    clock.reset();

    harness
        .store
        .write_success_parse_cache_with_deadline(&[first], 2, deadlines, &clock)
        .expect("the first cache COMMIT is the receipt linearization point");
    assert_eq!(deadlines.remaining_to_work_deadline(&clock), 0);
    assert_eq!(
        harness
            .store
            .write_success_parse_cache_with_deadline(&[second], 3, deadlines, &clock),
        Err(StoreError::WorkDeadlineExhausted)
    );
    let persisted: Vec<String> = harness
        .store
        .connection
        .prepare("SELECT file_identity FROM parse_cache ORDER BY file_identity")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(persisted, vec!["file-a".to_string()]);
}

#[test]
fn busy_timeout_restore_failure_preserves_commit_and_surfaces_rollback_failure() {
    let mut committed = harness("00000000-0000-4000-8000-000000000207");
    let active = started(
        committed
            .store
            .begin_run(
                &committed.request.request_id,
                &committed.canonical,
                &committed.runtime,
                1,
            )
            .unwrap(),
    );
    let batch = error_batch(&active);
    let clock = FixedClock(0);
    let deadlines = crate::deadline::RunDeadlines::derive(5_000, &clock).unwrap();
    TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE.with(|flag| flag.set(true));
    let committed_result = committed
        .store
        .finalize(&active, &batch, 2, deadlines, &clock);
    TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE.with(|flag| flag.set(false));
    let timings = committed_result.expect("terminal COMMIT remains authoritative");
    assert!(timings.busy_timeout_restore_failed);
    let status: String = committed
        .store
        .connection
        .query_row(
            "SELECT status FROM scan_runs WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status, "error");

    let mut rolled_back = harness("00000000-0000-4000-8000-000000000208");
    let active = started(
        rolled_back
            .store
            .begin_run(
                &rolled_back.request.request_id,
                &rolled_back.canonical,
                &rolled_back.runtime,
                1,
            )
            .unwrap(),
    );
    rolled_back
        .store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_terminal_diagnostic
                 BEFORE INSERT ON run_diagnostics
                 BEGIN SELECT RAISE(ABORT, 'injected terminal failure'); END;",
        )
        .unwrap();
    let batch = error_batch(&active);
    TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE.with(|flag| flag.set(true));
    let failed_result = rolled_back
        .store
        .finalize(&active, &batch, 2, deadlines, &clock);
    TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE.with(|flag| flag.set(false));
    assert!(matches!(
        failed_result,
        Err(StoreError::BusyTimeoutRestore { .. })
    ));
    let (status, diagnostics): (String, i64) = rolled_back
        .store
        .connection
        .query_row(
            "SELECT status, (SELECT count(*) FROM run_diagnostics WHERE scan_run_id=?1)
                 FROM scan_runs WHERE scan_run_id=?1",
            [active.scan_run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "running");
    assert_eq!(diagnostics, 0);
}

#[test]
fn terminal_finalization_never_rewrites_an_independent_cache_receipt() {
    let mut harness = harness("00000000-0000-4000-8000-000000000209");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1);
    let source = "mtime_ns=100:size=5";
    let profile_hash = "a".repeat(64);
    let batch = success_batch(&active, "file-a", source, &profile_hash, "terminal");
    harness
        .store
        .prepare_inventory(&batch.inventory, active.scan_run_id, 2)
        .unwrap();
    harness
        .store
        .write_success_parse_cache(
            &[cache_record("file-a", source, &profile_hash, "cached")],
            7,
        )
        .unwrap();

    finalize_for_test(&mut harness.store, &active, &batch, 10).unwrap();
    let (content, cached_at_ms): (String, i64) = harness
        .store
        .connection
        .query_row(
            "SELECT content, cached_at_ms FROM parse_cache WHERE file_identity='file-a'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(content, "cached");
    assert_eq!(cached_at_ms, 7);
}

#[test]
fn opportunistic_gc_success_busy_overshoot_and_restore_failure_are_isolated() {
    let mut harness = harness("00000000-0000-4000-8000-000000000210");
    insert_orphan_artifact(&harness.store, 1);
    let locker = rusqlite::Connection::open(&harness.db_path).unwrap();
    locker.busy_timeout(Duration::from_millis(0)).unwrap();
    locker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let fixed = FixedClock(0);
    assert!(matches!(
        harness.store.run_opportunistic_gc(100, 10, &fixed),
        Err(StoreError::CacheWrite { .. })
    ));
    locker.execute_batch("ROLLBACK").unwrap();
    let count_after_busy: i64 = harness
        .store
        .connection
        .query_row("SELECT count(*) FROM context_artifacts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count_after_busy, 1);

    harness
        .store
        .run_opportunistic_gc(100, 10, &fixed)
        .expect("bounded GC succeeds after the lock clears");
    let count_after_success: i64 = harness
        .store
        .connection
        .query_row("SELECT count(*) FROM context_artifacts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count_after_success, 0);

    insert_orphan_artifact(&harness.store, 2);
    TEST_OPPORTUNISTIC_GC_STATEMENTS_EXECUTED.with(|count| count.set(0));
    harness
        .store
        .run_opportunistic_gc(100, 10, &OpportunisticGcOvershootClock)
        .expect("budget overshoot only rolls back optional GC");
    let overshoot_statements =
        TEST_OPPORTUNISTIC_GC_STATEMENTS_EXECUTED.with(|count| count.replace(0));
    let count_after_overshoot: i64 = harness
        .store
        .connection
        .query_row("SELECT count(*) FROM context_artifacts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(overshoot_statements, 1);
    assert_eq!(
        count_after_overshoot, 1,
        "overshooting GC transaction rolled back"
    );

    TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE.with(|flag| flag.set(true));
    let restore_failure = harness.store.run_opportunistic_gc(100, 10, &fixed);
    TEST_FORCE_BUSY_TIMEOUT_RESTORE_FAILURE.with(|flag| flag.set(false));
    assert!(matches!(
        restore_failure,
        Err(StoreError::BusyTimeoutRestore { .. })
    ));
    let count_after_restore_failure: i64 = harness
        .store
        .connection
        .query_row("SELECT count(*) FROM context_artifacts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        count_after_restore_failure, 0,
        "the GC COMMIT remains authoritative even when restore fails"
    );
}

#[test]
fn successful_finalization_requires_both_preflight_fingerprints() {
    let mut harness = harness("00000000-0000-4000-8000-000000000017");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    let batch = success_batch(
        &active,
        "file-a",
        "mtime_ns=100:size=5",
        &"a".repeat(64),
        "hello",
    );

    assert!(matches!(
        finalize_for_test(&mut harness.store, &active, &batch, 2),
        Err(StoreError::RunCorrupt(_))
    ));
}

#[test]
fn final_envelope_cannot_change_engine_identity_after_run_start() {
    let mut harness = harness("00000000-0000-4000-8000-000000000020");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    let mut batch = error_batch(&active);
    let mut envelope: ContextEnvelope = serde_json::from_str(&batch.envelope_json).unwrap();
    envelope.engine_build = "changed-after-start".to_string();
    batch.envelope_json = canonical_envelope_json(&envelope).unwrap();

    assert!(matches!(
        finalize_for_test(&mut harness.store, &active, &batch, 2),
        Err(StoreError::RunCorrupt(_))
    ));
}

#[test]
fn database_lock_maps_to_a_structured_retryable_error() {
    let harness = harness("00000000-0000-4000-8000-000000000014");
    let mut second = ScannerStore::open(&harness.db_path).unwrap();
    harness
        .store
        .connection
        .execute_batch("BEGIN IMMEDIATE")
        .unwrap();
    let error = second
        .begin_run(
            &harness.request.request_id,
            &harness.canonical,
            &harness.runtime,
            1,
        )
        .unwrap_err();
    harness.store.connection.execute_batch("ROLLBACK").unwrap();

    assert!(matches!(error, StoreError::CacheWrite { .. }));
    assert_eq!(error.error_code(), ErrorCode::CacheWriteFailed);
    assert!(error.retryable());
}

#[test]
fn worker_fingerprints_and_run_diagnostics_round_trip() {
    let mut harness = harness("00000000-0000-4000-8000-000000000015");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1,
            )
            .unwrap(),
    );
    let office = WorkerFingerprint {
        contract: "ai_daily_worker_v3".to_string(),
        version: "0.1.0".to_string(),
        build: "office-build".to_string(),
    };
    harness
        .store
        .record_worker_fingerprints(&active, Some(&office), None, 2)
        .unwrap();
    let batch = error_batch(&active);
    let expected_diagnostics = batch.diagnostics.clone();
    finalize_for_test(&mut harness.store, &active, &batch, 3).unwrap();

    let fingerprints: (Option<String>, Option<String>) = harness
        .store
        .connection
        .query_row(
            "SELECT office_worker_build, pdf_worker_build FROM scan_run_attempts
                 WHERE scan_run_id=?1 AND attempt_number=1",
            [active.scan_run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(fingerprints, (Some("office-build".to_string()), None));
    assert_eq!(
        harness
            .store
            .load_diagnostics(active.scan_run_id())
            .unwrap(),
        expected_diagnostics
    );
    assert_eq!(
        harness
            .store
            .load_terminal_envelope(active.scan_run_id())
            .unwrap()
            .envelope_json,
        batch.envelope_json
    );
}

#[test]
fn evidence_returns_stable_rows_and_rejects_tampering() {
    let mut harness = harness("00000000-0000-4000-8000-000000000019");
    let active = started(
        harness
            .store
            .begin_run(
                &harness.request.request_id,
                &harness.canonical,
                &harness.runtime,
                1_000,
            )
            .unwrap(),
    );
    record_both_workers(&mut harness.store, &active, 1_001);
    let batch = success_batch(
        &active,
        "file-a",
        "mtime_ns=100:size=5",
        &"a".repeat(64),
        "hello",
    );
    finalize_for_test(&mut harness.store, &active, &batch, 1_010).unwrap();

    let snapshot = harness
        .store
        .load_evidence(active.scan_run_id())
        .expect("evidence snapshot");
    assert_eq!(snapshot.run_status, RunStatus::Success);
    assert_eq!(snapshot.context_run_id, Some(active.context_run_id()));
    assert_eq!(snapshot.summary.source_file_count, 1);
    assert_eq!(snapshot.stage_metrics.len(), 4);
    assert_eq!(snapshot.files_v2.len(), 1);
    assert_eq!(snapshot.decisions.len(), 1);

    harness
        .store
        .connection
        .execute(
            "UPDATE context_decisions SET relative_path='tampered.txt'
                 WHERE context_run_id=?1",
            [active.context_run_id() as i64],
        )
        .unwrap();
    let corrupt = harness
        .store
        .load_evidence(active.scan_run_id())
        .expect_err("decision identity/path mismatch is corrupt");
    assert!(matches!(
        corrupt.error,
        crate::context_audit::EvidenceAuditError::RunCorrupt(_)
    ));
}

#[test]
fn db_filename_guard_never_opens_the_legacy_database() {
    let directory = tempfile::tempdir().unwrap();
    let legacy = directory.path().join("scan_index.sqlite3");
    assert!(matches!(
        ScannerStore::open(&legacy),
        Err(StoreError::InvalidRequest(_))
    ));
    assert!(!legacy.exists());
}

#[test]
fn minimal_profile_fixture_remains_deserializable_for_store_tests() {
    let raw: ScannerSettings = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(normalize_scanner_settings(&raw, ReportMode::Daily).is_ok());
}

#[test]
fn retention_gc_for_current_run_trims_to_run_count_cap() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let db_path = directory.path().join(SCAN_DB_FILENAME);
    let mut connection = rusqlite::Connection::open(&db_path).unwrap();
    connection.execute_batch(schema::V3_DDL).unwrap();
    connection.pragma_update(None, "user_version", 3).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    // 520 aged terminal runs (each tiny audit).
    for index in 0..520 {
        connection
            .execute(
                "INSERT INTO scan_runs(
                        request_id, canonical_request_json, request_hash_algorithm, request_hash,
                        owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                        finished_at_ms, final_envelope_json, audit_size_bytes
                     ) VALUES (?1, '{}', 'sha256-request-v1', ?2, 'owner', 'error',
                                1, 1, 1, ?3, '{}', 100)",
                params![
                    format!("aged-{index}"),
                    "0".repeat(64),
                    1_000 + index as i64,
                ],
            )
            .unwrap();
    }
    connection
        .execute(
            "INSERT INTO scan_runs(
                    request_id, canonical_request_json, request_hash_algorithm, request_hash,
                    owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                    finished_at_ms, final_envelope_json, audit_size_bytes
                 ) VALUES ('current', '{}', 'sha256-request-v1', ?1, 'owner', 'running',
                            2, 2, 2, NULL, NULL, 0)",
            ["1".repeat(64)],
        )
        .unwrap();
    let current_run_id = connection.last_insert_rowid();
    let protected_runs = std::collections::HashSet::from([current_run_id]);
    let plan = prepare_finalization_retention_plan(
        &connection,
        current_run_id,
        &protected_runs,
        &HashSet::new(),
        None,
        1_000_000,
        100,
        &|| Ok(()),
    )
    .expect("retention planning succeeds before the transaction");
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    transaction
        .execute(
            "UPDATE scan_runs SET status='error', finished_at_ms=2,
                                      final_envelope_json='{}', audit_size_bytes=100
                 WHERE scan_run_id=?1 AND status='running'",
            [current_run_id],
        )
        .unwrap();
    apply_terminal_retention_plan(&transaction, &plan, &protected_runs, &|| Ok(()))
        .expect("prepared retention applies");
    let count: i64 = transaction
        .query_row(
            "SELECT count(*) FROM scan_runs
                 WHERE status IN ('success', 'partial', 'error', 'abandoned')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, cache::TERMINAL_RUN_MAX_COUNT);
    transaction.commit().unwrap();
}

#[test]
fn artifact_retention_plan_preselects_orphan_before_transaction() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let db_path = directory.path().join(SCAN_DB_FILENAME);
    let mut store = ScannerStore::open(&db_path).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO scan_runs(
                    request_id, canonical_request_json, request_hash_algorithm, request_hash,
                    owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                    audit_size_bytes
                 ) VALUES ('current', '{}', 'sha256-request-v1', ?1, 'owner', 'running',
                            2, 2, 2, 0)",
            ["1".repeat(64)],
        )
        .unwrap();
    let current_run_id = store.connection.last_insert_rowid();
    store
        .connection
        .execute(
            "INSERT INTO context_artifacts(
                    snapshot_eligible, snapshot_key_sha256, snapshot_key_json,
                    final_context, context_sha256, semantic_summary_json,
                    artifact_size_bytes, created_at_ms, last_accessed_bucket
                 ) VALUES (0, NULL, NULL, '', ?1, '{}', ?2, 1, '1970-01-01')",
            params!["0".repeat(64), cache::CONTEXT_ARTIFACTS_MAX_BYTES],
        )
        .unwrap();
    let orphan_artifact_id = store.connection.last_insert_rowid();
    let protected_runs = HashSet::from([current_run_id]);
    let plan = prepare_finalization_retention_plan(
        &store.connection,
        current_run_id,
        &protected_runs,
        &HashSet::new(),
        Some(1),
        100,
        0,
        &|| Ok(()),
    )
    .expect("artifact victim is selected before the transaction");
    match plan.artifact_room.as_slice() {
        [PreparedRetentionDelete::Artifacts(ids)] => {
            assert_eq!(ids.as_slice(), [orphan_artifact_id]);
        }
        other => panic!("unexpected artifact retention plan: {other:?}"),
    }

    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    apply_artifact_retention_plan(
        &transaction,
        &plan,
        &protected_runs,
        &HashSet::new(),
        &|| Ok(()),
    )
    .expect("prepared orphan deletion applies");
    let remaining_bytes: i64 = transaction
        .query_row(
            "SELECT COALESCE(SUM(artifact_size_bytes), 0) FROM context_artifacts",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(remaining_bytes.saturating_add(1) <= cache::CONTEXT_ARTIFACTS_MAX_BYTES);
    transaction.commit().unwrap();
}

#[test]
fn artifact_retention_apply_fails_closed_when_prepared_victim_changes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let db_path = directory.path().join(SCAN_DB_FILENAME);
    let mut store = ScannerStore::open(&db_path).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO context_artifacts(
                    snapshot_eligible, snapshot_key_sha256, snapshot_key_json,
                    final_context, context_sha256, semantic_summary_json,
                    artifact_size_bytes, created_at_ms, last_accessed_bucket
                 ) VALUES (0, NULL, NULL, '', ?1, '{}', ?2, 1, '1970-01-01')",
            params!["0".repeat(64), cache::CONTEXT_ARTIFACTS_MAX_BYTES],
        )
        .unwrap();
    let orphan_artifact_id = store.connection.last_insert_rowid();
    let plan = prepare_finalization_retention_plan(
        &store.connection,
        1,
        &HashSet::from([1]),
        &HashSet::new(),
        Some(1),
        100,
        0,
        &|| Ok(()),
    )
    .expect("artifact victim is selected before the transaction");
    store
        .connection
        .execute(
            "DELETE FROM context_artifacts WHERE artifact_id=?1",
            [orphan_artifact_id],
        )
        .unwrap();

    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let error = apply_artifact_retention_plan(
        &transaction,
        &plan,
        &HashSet::from([1]),
        &HashSet::new(),
        &|| Ok(()),
    )
    .expect_err("changed prepared victim must fail closed");
    assert!(matches!(error, StoreError::RunCorrupt(_)));
    transaction.rollback().unwrap();
}

#[test]
fn retention_gc_for_current_run_fails_closed_when_record_exceeds_cap() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let db_path = directory.path().join(SCAN_DB_FILENAME);
    let connection = rusqlite::Connection::open(&db_path).unwrap();
    connection.execute_batch(schema::V3_DDL).unwrap();
    connection.pragma_update(None, "user_version", 3).unwrap();
    connection
        .pragma_update(None, "foreign_keys", true)
        .unwrap();
    // 当前 record 自身超 2 GiB → fail closed。
    let protected_runs = std::collections::HashSet::from([1]);
    let error = prepare_finalization_retention_plan(
        &connection,
        1,
        &protected_runs,
        &HashSet::new(),
        None,
        1_000_000,
        cache::TERMINAL_AUDIT_MAX_BYTES + 1,
        &|| Ok(()),
    )
    .expect_err("current audit over cap must fail closed");
    assert!(matches!(error, StoreError::RunCorrupt(_)));
}
