use super::*;
use tempfile::tempdir;

#[test]
fn discovery_timeout_is_reported_by_the_active_scanner() {
    let root = tempdir().unwrap();
    let request: BuildContextRequest =
        serde_json::from_str(include_str!("../../tests/fixtures/request.json")).unwrap();
    let profile =
        normalize_scanner_settings(&request.scanner_settings, request.report_mode).unwrap();
    let error = discover_with_timeout(
        &root.path().join("missing"),
        &request,
        &profile,
        Duration::ZERO,
    )
    .unwrap_err();
    assert_eq!(error, discovery_timeout_diagnostic());
}

#[test]
fn committed_terminal_output_survives_restore_and_opportunistic_gc_failures() {
    let directory = tempdir().unwrap();
    let db_path = directory.path().join(crate::store::SCAN_DB_FILENAME);
    let mut store = ScannerStore::open(&db_path).unwrap();
    let timing = ActiveRunTiming::start(5_000).unwrap();
    let _ = crate::store::take_test_opportunistic_gc_calls();
    let envelope = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: "00000000-0000-4000-8000-000000000001".to_string(),
        engine_version: "test".to_string(),
        engine_build: "test".to_string(),
        status: EngineStatus::Ok,
        file_context: "committed".to_string(),
        summary: ContextSummary::default(),
        scan_run_id: Nullable(None),
        context_run_id: Nullable(None),
        warnings: Vec::new(),
        error: Nullable(None),
    };

    let output = complete_committed_terminal(
        &mut store,
        &timing,
        TerminalAuditTimings {
            busy_timeout_restore_failed: true,
            ..TerminalAuditTimings::default()
        },
        envelope.clone(),
        0,
    );
    assert_eq!(output.value.file_context, "committed");
    assert_eq!(output.exit_code, 0);
    assert_eq!(
        crate::store::take_test_opportunistic_gc_calls(),
        0,
        "restore failure must skip every post-commit store use"
    );

    let locker = rusqlite::Connection::open(&db_path).unwrap();
    locker.busy_timeout(Duration::from_millis(0)).unwrap();
    locker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let output = complete_committed_terminal(
        &mut store,
        &timing,
        TerminalAuditTimings::default(),
        envelope,
        7,
    );
    locker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(output.value.file_context, "committed");
    assert_eq!(output.exit_code, 7);
    assert_eq!(crate::store::take_test_opportunistic_gc_calls(), 1);
}

#[test]
fn local_scanner_build_uses_a_deterministic_source_fingerprint() {
    let build = engine_identity().engine_build;
    if let Some(ci_build) =
        option_env!("AI_DAILY_BUILD_ID").filter(|value| !value.trim().is_empty())
    {
        assert_eq!(build, ci_build);
    } else {
        let digest = build
            .strip_prefix("sha256-source-v1:")
            .expect("local build must use the source hash prefix");
        assert_eq!(digest.len(), 64);
        assert!(digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
    }
}

#[test]
fn db_parent_probe_does_not_create_the_scan_database() {
    let directory = tempdir().expect("temporary directory should be created");
    let scan_db = directory.path().join("scan-index-v2.sqlite3");

    probe_scan_db_parent(scan_db.to_str().expect("path should be UTF-8"))
        .expect("temporary directory should be writable");

    assert!(!scan_db.exists());
    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("temporary directory should remain readable")
            .count(),
        0
    );
}

#[test]
fn post_outcome_rejection_commits_a_replayable_minimal_error() {
    let directory = tempdir().expect("temporary directory should be created");
    let scan_db = directory.path().join(crate::store::SCAN_DB_FILENAME);
    let mut request: BuildContextRequest =
        serde_json::from_str(include_str!("../../tests/fixtures/request.json"))
            .expect("request fixture");
    request.request_id = "00000000-0000-4000-8000-000000000001".to_string();
    request.work_dir = directory.path().to_string_lossy().into_owned();
    request.scan_db_path = scan_db.to_string_lossy().into_owned();
    request.adapters.office_worker_path = directory
        .path()
        .join("office-worker.exe")
        .to_string_lossy()
        .into_owned();
    request.adapters.pdf_worker_path = directory
        .path()
        .join("pdf.exe")
        .to_string_lossy()
        .into_owned();

    let profile = normalize_scanner_settings(&request.scanner_settings, request.report_mode)
        .expect("normalized scanner profile");
    let canonical =
        ScannerStore::canonicalize_request(&request, &profile).expect("canonical request");
    let runtime = AttemptRuntime::from_request(&request).expect("attempt runtime");
    let mut store = ScannerStore::open(&scan_db).expect("scanner store");
    let now_ms = current_time_millis().expect("current time");
    let active = match store
        .begin_run(&request.request_id, &canonical, &runtime, now_ms)
        .expect("begin run")
    {
        BeginRunOutcome::Started(active) => active,
        BeginRunOutcome::Stored(_) => panic!("expected a new active run"),
    };
    let office = WorkerFingerprint {
        contract: "ai_daily_worker_v3".to_string(),
        version: "0.1.0".to_string(),
        build: "office-build".to_string(),
    };
    let pdf = WorkerFingerprint {
        contract: "ai_daily_worker_v3".to_string(),
        version: "0.1.0".to_string(),
        build: "pdf-build".to_string(),
    };
    store
        .record_worker_fingerprints(&active, Some(&office), Some(&pdf), now_ms)
        .expect("worker fingerprints");
    let invalid_outcome = FinalizationBatch {
        status: RunStatus::Success,
        envelope_json: "{}".to_string(),
        inventory: Vec::new(),
        file_results: Vec::new(),
        diagnostics: Vec::new(),
        stage_metrics: Vec::new(),
        extension_metrics: Vec::new(),
        context: None,
        artifact: None,
        snapshot_key: None,
        snapshot_hit: None,
        execution_metrics: None,
    };

    let timing = ActiveRunTiming::start(10_000).expect("test deadline");
    let output = persist_post_outcome_failure_if_invalid(
        &request,
        &engine_identity(),
        &mut store,
        &active,
        &invalid_outcome,
        0,
        0,
        &WorkerRssTracker::default(),
        &timing,
    )
    .expect("invalid outcome must be converted");
    assert_eq!(output.exit_code, 1);

    let stored = store
        .load_terminal_envelope(active.scan_run_id())
        .unwrap_or_else(|error| {
            panic!(
                "committed terminal failure must be replayable: {error}; output={:?}",
                output.value.status
            )
        });
    assert_eq!(stored.envelope.scan_run_id.0, Some(active.scan_run_id()));
    assert_eq!(
        stored.envelope.context_run_id.0,
        Some(active.context_run_id())
    );
}

#[test]
fn discovery_produces_a_source_guard_for_every_file() {
    let directory = tempdir().expect("temporary directory should be created");
    let path = directory.path().join("evidence.txt");
    std::fs::write(&path, "AAAA").expect("fixture file should be written");
    let mut files = vec![DiscoveredFileOut {
        file_identity: "bootstrap:evidence".to_string(),
        path: path.to_string_lossy().into_owned(),
        extension: ".txt".to_string(),
        modified_at: "2026-07-16T00:00:00+08:00".to_string(),
        size_bytes: 4,
        source_version: "mtime_ns=1:size=4".to_string(),
        source_guard_kind: None,
        source_guard_sha256: None,
    }];

    let observer = crate::source_guard::SourceGuardObserver::default();
    attach_source_guards(&mut files, &observer);

    // Every discovered file must carry a guard. Unavailable stays fail-closed
    // with a null hash; any other kind must carry a 64-char sha256.
    let kind = files[0]
        .source_guard_kind
        .as_deref()
        .expect("discovery must produce a guard kind");
    match kind {
        "unavailable" => {
            assert!(files[0].source_guard_sha256.is_none());
        }
        _ => {
            let hash = files[0]
                .source_guard_sha256
                .as_deref()
                .expect("guard kind requires a hash");
            assert_eq!(hash.len(), 64);
            assert!(hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')));
        }
    }
}

#[test]
fn guard_attachment_metrics_flow_into_engine_error_metrics() {
    let directory = tempdir().unwrap();
    let missing = directory.path().join("missing.txt");
    let observer = crate::source_guard::SourceGuardObserver::default();
    let mut files = vec![guarded_discovery_file(&missing)];

    attach_source_guards(&mut files, &observer);
    let observations = observer.metrics();
    assert_eq!(observations.content_hash_file_count, 1);
    assert_eq!(observations.unavailable_file_count, 1);
    assert_eq!(observations.bytes_read, 0);

    let metrics = base_execution_metrics(1, 2, 1, observations, Some(0));
    assert_eq!(metrics.source_guard_content_hash_file_count, 1);
    assert_eq!(metrics.source_guard_unavailable_count, 1);
    assert_eq!(metrics.source_guard_bytes_read, 0);
}

fn guarded_discovery_file(path: &Path) -> DiscoveredFileOut {
    DiscoveredFileOut {
        file_identity: "fixture:evidence".to_string(),
        path: path.to_string_lossy().into_owned(),
        extension: ".txt".to_string(),
        modified_at: "2026-07-16T00:00:00+08:00".to_string(),
        size_bytes: 4,
        source_version: "mtime_ns=1:size=4".to_string(),
        source_guard_kind: None,
        source_guard_sha256: None,
    }
}

fn artifact_for_discovery(file: &DiscoveredFileOut) -> ArtifactDraft {
    ArtifactDraft {
        snapshot_eligible: true,
        final_context: "fixture".to_string(),
        context_sha256: crate::artifact::sha256_hex(b"fixture"),
        semantic_summary: SemanticSummary {
            source_file_count: 1,
            success_count: 1,
            timeout_count: 0,
            included_file_count: 1,
            omitted_file_count: 0,
            error_file_count: 0,
            input_chars: 7,
            output_chars: 7,
            reserved_chars: 7,
            rendered_chars: 7,
        },
        file_rows: vec![ArtifactFileRow {
            file_identity: file.file_identity.clone(),
            relative_path: "evidence.txt".to_string(),
            legacy_source_version: file.source_version.clone(),
            source_guard_kind: file.source_guard_kind.clone(),
            source_guard_sha256: file.source_guard_sha256.clone(),
            parse_profile_hash: "a".repeat(64),
            parse_status: ParseStatus::Success,
            parser_backend: "light_text_v2".to_string(),
            worker_lane: "rust_core".to_string(),
            truncated: false,
            content_sha256: crate::artifact::sha256_hex(b"fixture"),
            classifier: None,
        }],
        decision_rows: Vec::new(),
    }
}

#[test]
fn snapshot_precheck_reuses_the_discovery_snapshot() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("precheck.txt");
    std::fs::write(&path, "AAAA").unwrap();
    let observer = crate::source_guard::SourceGuardObserver::default();
    let mut files = vec![guarded_discovery_file(&path)];
    attach_source_guards(&mut files, &observer);

    // 放款政策：同一 run 内复用瞬时快照，discovery 之后的文件漂移不再触发
    // 二次读取，precheck 始终基于 discovery 快照判定为「当前」。
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(&path, "BBBB").unwrap();
    std::thread::sleep(Duration::from_millis(50));

    assert!(
        discovery_guards_are_current(&files, &observer),
        "precheck reuses the discovery-time snapshot and must not re-read"
    );
}

#[test]
fn snapshot_postcheck_reuses_the_discovery_snapshot() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("postcheck.txt");
    std::fs::write(&path, "AAAA").unwrap();
    let observer = crate::source_guard::SourceGuardObserver::default();
    let mut files = vec![guarded_discovery_file(&path)];
    attach_source_guards(&mut files, &observer);
    let artifact = artifact_for_discovery(&files[0]);

    // 放款政策：artifact 与 discovery 的 guard 仍按记录比对（跨 run 缓存身份），
    // 但同一 run 内的文件漂移不再二次读取，postcheck 复用 discovery 快照。
    std::thread::sleep(Duration::from_millis(50));
    std::fs::write(&path, "BBBB").unwrap();
    std::thread::sleep(Duration::from_millis(50));

    assert!(
        artifact_guards_are_current(&artifact, &files, &observer),
        "postcheck reuses the discovery-time snapshot and must not re-read"
    );
}

#[test]
fn snapshot_postcheck_rejects_artifact_guard_row_disagreement() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("artifact-row.txt");
    std::fs::write(&path, "AAAA").unwrap();
    let observer = crate::source_guard::SourceGuardObserver::default();
    let mut files = vec![guarded_discovery_file(&path)];
    attach_source_guards(&mut files, &observer);
    let mut artifact = artifact_for_discovery(&files[0]);
    artifact.file_rows[0].source_guard_kind = Some("unknown_guard".to_string());
    assert!(!artifact_guards_are_current(&artifact, &files, &observer));
    artifact.file_rows[0].source_guard_kind = files[0].source_guard_kind.clone();
    artifact.file_rows[0].source_guard_sha256 = Some("f".repeat(64));

    assert!(!artifact_guards_are_current(&artifact, &files, &observer));
    artifact.file_rows.clear();
    assert!(!artifact_guards_are_current(&artifact, &files, &observer));
}

#[test]
fn snapshot_reports_the_live_handshake_peak_rss() {
    let observations = crate::source_guard::SourceGuardObservationMetrics {
        content_hash_file_count: 1,
        unavailable_file_count: 2,
        bytes_read: 3,
    };
    let metrics =
        assemble_snapshot_execution_metrics(4, observations, 0, 0, 1, 2, 3, 4, Some(64 * 1024));
    assert_eq!(metrics.discovery_observed_file_count, 4);
    assert_eq!(metrics.source_guard_content_hash_file_count, 1);
    assert_eq!(metrics.source_guard_unavailable_count, 2);
    assert_eq!(metrics.source_guard_bytes_read, 3);
    assert_eq!(metrics.peak_worker_rss_bytes.0, Some(64 * 1024));
}

fn successful_outcome_for_rss_test() -> BudgetedScanOutcome {
    BudgetedScanOutcome {
        scan_run_id: 1,
        terminal_intent: TerminalIntent::Success,
        inventory: Vec::new(),
        file_results: Vec::new(),
        sources: Vec::new(),
        parse_cache_receipts: Vec::new(),
        classification_cache_receipts: Vec::new(),
        classifications: std::collections::BTreeMap::new(),
        diagnostics: Vec::new(),
        stage_metrics: Vec::new(),
        extension_metrics: Vec::new(),
        context: Some(ContextRunRecord {
            context_profile_hash: "a".repeat(64),
            status: RunStatus::Success,
            final_context: String::new(),
            context_sha256: crate::store::sha256_hex(b""),
            summary: ContextSummary::default(),
            decisions: Vec::new(),
        }),
        execution_metrics: crate::scheduler::ExecutionMetrics::default(),
    }
}

#[test]
fn unavailable_worker_rss_adds_warning_without_changing_success() {
    let mut outcome = successful_outcome_for_rss_test();

    apply_worker_rss_observation(&mut outcome, None);

    assert_eq!(outcome.terminal_intent, TerminalIntent::Success);
    assert_eq!(
        outcome.context.as_ref().map(|context| context.status),
        Some(RunStatus::Success)
    );
    assert_eq!(outcome.diagnostics.len(), 1);
    assert_eq!(outcome.diagnostics[0].severity, DiagnosticSeverity::Warning);
    assert_eq!(
        outcome.diagnostics[0].diagnostic.error_code,
        ErrorCode::WorkerRssUnavailable
    );
    assert_eq!(
        outcome.diagnostics[0].diagnostic.stage,
        DiagnosticStage::Process
    );
}

#[test]
fn observed_worker_rss_keeps_success_without_warning() {
    for peak_worker_rss_bytes in [0, 64 * 1024 * 1024] {
        let mut outcome = successful_outcome_for_rss_test();

        apply_worker_rss_observation(&mut outcome, Some(peak_worker_rss_bytes));

        assert_eq!(outcome.terminal_intent, TerminalIntent::Success);
        assert_eq!(
            outcome.context.as_ref().map(|context| context.status),
            Some(RunStatus::Success)
        );
        assert!(outcome.diagnostics.is_empty());
    }
}

#[test]
fn snapshot_preserves_the_short_refill_classification_window() {
    let file = guarded_discovery_file(Path::new("/work/short.pdf"));
    let mut artifact = artifact_for_discovery(&file);
    let provenance = PdfClassificationProvenanceV1 {
        status: ai_daily_scanner_contract::PdfClassificationStatus::NoTextInParseWindow,
        page_count: Some(10),
        result_examined_pages: Some(2),
        nominal_charged_pages: 2,
        classifier_build: "a".repeat(64),
        classifier_profile_hash: "b".repeat(64),
    };
    artifact.file_rows[0].parser_backend = "pdf_metadata_v2".to_string();
    artifact.file_rows[0].classifier = Some(provenance.clone());
    let mut outcome = successful_outcome_for_rss_test();
    outcome.inventory =
        vec![InventoryRecord::from_discovered(&file, "short.pdf".to_string()).unwrap()];
    outcome.file_results = snapshot_file_results(&artifact, None);
    artifact.file_rows = artifact_file_rows(&outcome);
    assert_eq!(artifact.file_rows[0].classifier, Some(provenance));
    let restored = snapshot_file_results(&artifact, None);
    restored[0]
        .validate_for_persistence(true)
        .expect("short window remains valid on snapshot reuse");
    assert_eq!(
        restored[0]
            .pdf_classification
            .as_ref()
            .unwrap()
            .run_inspected_pages
            .0,
        Some(0)
    );
}

#[test]
fn snapshot_not_classified_by_budget_stays_not_eligible() {
    let classifier = PdfClassificationProvenanceV1 {
        status: ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget,
        page_count: None,
        result_examined_pages: Some(0),
        nominal_charged_pages: 0,
        classifier_build: "a".repeat(64),
        classifier_profile_hash: "b".repeat(64),
    };
    let artifact = ArtifactDraft::new(
        true,
        String::new(),
        SemanticSummary {
            source_file_count: 1,
            success_count: 0,
            timeout_count: 0,
            included_file_count: 0,
            omitted_file_count: 1,
            error_file_count: 0,
            input_chars: 0,
            output_chars: 0,
            reserved_chars: 0,
            rendered_chars: 0,
        },
        vec![ArtifactFileRow {
            file_identity: "fixture:budget.pdf".to_string(),
            relative_path: "budget.pdf".to_string(),
            legacy_source_version: "mtime_ns=1:size=1".to_string(),
            source_guard_kind: Some("content_sha256_v1".to_string()),
            source_guard_sha256: Some("c".repeat(64)),
            parse_profile_hash: "d".repeat(64),
            parse_status: ParseStatus::NotParsed,
            parser_backend: "not_parsed".to_string(),
            worker_lane: "not_parsed".to_string(),
            truncated: false,
            content_sha256: crate::store::sha256_hex(b""),
            classifier: Some(classifier),
        }],
        vec![ArtifactDecisionRow {
            file_identity: "fixture:budget.pdf".to_string(),
            relative_path: "budget.pdf".to_string(),
            action: ai_daily_scanner_contract::ContextAction::Omit,
            reason: "pdf_classification_page_quota_exhausted".to_string(),
            priority: 0,
            input_chars: 0,
            output_chars: 0,
            truncated: false,
            error_code: String::new(),
        }],
    )
    .expect("eligible artifact");

    let rows = snapshot_file_results(&artifact, None);
    let audit = rows[0]
        .pdf_classification
        .as_ref()
        .expect("snapshot classification audit");
    assert_eq!(
        audit.classification_cache_status,
        ai_daily_scanner_contract::ClassificationCacheStatus::NotEligible
    );
    assert_eq!(
        audit.transport,
        ai_daily_scanner_contract::ClassificationTransport::NotApplicable
    );
    assert_eq!(audit.attempt_count, 0);

    // Historical eligible artifacts created before zero-execution
    // provenance was stored can be reconstructed only from the frozen
    // quota decision and the current preflight classifier identity.
    let mut historical = artifact.clone();
    historical.file_rows[0].classifier = None;
    let identity = ClassifierIdentity {
        contract: CLASSIFIER_CONTRACT_VERSION.to_string(),
        build: "a".repeat(64),
        profile_hash: "b".repeat(64),
    };
    let rows = snapshot_file_results(&historical, Some(&identity));
    let audit = rows[0]
        .pdf_classification
        .as_ref()
        .expect("quota decision permits controlled reconstruction");
    assert_eq!(
        audit.status,
        ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget
    );

    historical.decision_rows[0].reason = "semantic_file_quota_exhausted".to_string();
    let rows = snapshot_file_results(&historical, Some(&identity));
    assert!(
        rows[0].pdf_classification.is_none(),
        "a generic unclassified PDF must never be guessed as budget-excluded"
    );
}
