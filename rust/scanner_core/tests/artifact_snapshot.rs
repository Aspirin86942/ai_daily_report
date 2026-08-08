//! Artifact relational model, envelope rebuild, and snapshot identity tests
//! (spec Part 5.1/5.4). The artifact module is the domain model Task 2's
//! snapshot-hit finalization consumes; these tests pin the module's own
//! invariants first.

use ai_daily_discovery::{DiscoveredFileOut, DiscoveryIssue, DiscoveryIssueKind};
use ai_daily_scanner_contract::{
    BuildContextRequest, ContextAction, ContextSummary, EngineStatus, NormalizedScannerProfileV2,
    Nullable, ParseStatus, ReportMode, ScannerProfile, Validate,
};
use ai_daily_scanner_core::artifact::{
    rebuild_envelope, snapshot_key, snapshot_key_parts, ArtifactDecisionRow, ArtifactDraft,
    ArtifactFileRow, ClassifierIdentity, PdfClassificationProvenanceV1, SemanticSummary,
};
use ai_daily_scanner_core::config::normalize_scanner_profile_v2;
use ai_daily_scanner_core::scheduler::WorkerIdentities;
use sha2::Digest;

const REQUEST_ID_A: &str = "11111111-1111-4111-8111-111111111111";
const REQUEST_ID_B: &str = "22222222-2222-4222-8222-222222222222";
const ENGINE_BUILD_A: &str = "engine-build-a";
const ENGINE_BUILD_B: &str = "engine-build-b";

fn raw_profile() -> serde_json::Value {
    serde_json::json!({ "schema_version": "scanner_profile_v2" })
}

fn v2_profile(mode: ReportMode) -> NormalizedScannerProfileV2 {
    let raw: ai_daily_scanner_contract::RawScannerProfileV2 =
        serde_json::from_value(raw_profile()).expect("minimal v2 raw profile");
    normalize_scanner_profile_v2(&ScannerProfile::V2(raw), mode).expect("normalized v2 profile")
}

fn request(mode: ReportMode, request_id: &str) -> BuildContextRequest {
    let profile = raw_profile();
    serde_json::from_value(serde_json::json!({
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": request_id,
        "work_dir": "C:\\scanner-fixtures\\工作 目录",
        "start_date": "2026-07-14",
        "end_date": "2026-07-15",
        "report_mode": serde_json::Value::String(
            match mode {
                ReportMode::Daily => "daily".into(),
                ReportMode::Weekly => "weekly".into(),
                ReportMode::Monthly => "monthly".into(),
            }
        ),
        "compression_profile": null,
        "scan_db_path": "C:\\scanner-fixtures\\state\\scan-index-v2.sqlite3",
        "scanner_profile": profile,
        "adapters": {
            "office_worker_path": "C:\\scanner-fixtures\\bin\\ai-daily-office-parser.exe",
            "python_executable": "C:\\scanner-fixtures\\venv\\Scripts\\python.exe",
            "python_module_root": "C:\\scanner-fixtures\\repo",
            "python_document_worker_module": "src.workers.document_parser_worker"
        }
    }))
    .expect("request should decode")
}

fn workers(office_build: &str, python_build: &str) -> WorkerIdentities {
    WorkerIdentities {
        office_contract: Some("ai_daily_worker_v1".to_string()),
        office_version: Some("1.0".to_string()),
        office_build: Some(office_build.to_string()),
        python_contract: Some("ai_daily_worker_v1".to_string()),
        python_version: Some("1.0".to_string()),
        python_build: Some(python_build.to_string()),
        classifier_build: Some(python_build.to_string()),
    }
}

fn classifier(build: &str) -> ClassifierIdentity {
    ClassifierIdentity {
        contract: "ai_daily_pdf_classifier_v1".to_string(),
        build: build.to_string(),
        profile_hash: "0".repeat(64),
    }
}

fn discovery_file(identity: &str) -> DiscoveredFileOut {
    DiscoveredFileOut {
        file_identity: identity.to_string(),
        path: format!("C:\\work\\{identity}"),
        extension: ".txt".to_string(),
        modified_at: "2026-08-08T12:00:00.000000".to_string(),
        size_bytes: 5,
        source_version: format!("mtime_ns=1:size={}", identity.len()),
        source_guard_kind: Some("content_sha256_v1".to_string()),
        source_guard_sha256: Some("0".repeat(64)),
    }
}

fn issue(path: &str) -> DiscoveryIssue {
    DiscoveryIssue {
        kind: DiscoveryIssueKind::Metadata,
        path: Some(path.to_string()),
        message: "cannot stat candidate".to_string(),
    }
}

// ---------------------------------------------------------------------------
// snapshot_key
// ---------------------------------------------------------------------------

#[test]
fn snapshot_key_changes_when_report_mode_changes() {
    let daily = v2_profile(ReportMode::Daily);
    let weekly = v2_profile(ReportMode::Weekly);
    let k1 = snapshot_key(
        &request(ReportMode::Daily, REQUEST_ID_A),
        &[],
        &[],
        &daily,
        ENGINE_BUILD_A,
        &workers("office-a", "python-a"),
        &classifier("classifier-a"),
    )
    .unwrap();
    let k2 = snapshot_key(
        &request(ReportMode::Weekly, REQUEST_ID_A),
        &[],
        &[],
        &weekly,
        ENGINE_BUILD_A,
        &workers("office-a", "python-a"),
        &classifier("classifier-a"),
    )
    .unwrap();
    assert_ne!(k1, k2, "report_mode must change the snapshot key");
}

#[test]
fn snapshot_key_changes_when_engine_build_changes() {
    let profile = v2_profile(ReportMode::Daily);
    let request = request(ReportMode::Daily, REQUEST_ID_A);
    let base = snapshot_key(&request, &[], &[], &profile, ENGINE_BUILD_A, &workers("o", "p"), &classifier("c"))
        .unwrap();
    let changed = snapshot_key(&request, &[], &[], &profile, ENGINE_BUILD_B, &workers("o", "p"), &classifier("c"))
        .unwrap();
    assert_ne!(base, changed, "engine build must change the snapshot key");
}

#[test]
fn snapshot_key_changes_when_discovery_rows_change() {
    let profile = v2_profile(ReportMode::Daily);
    let request = request(ReportMode::Daily, REQUEST_ID_A);
    let base = snapshot_key(&request, &[], &[], &profile, ENGINE_BUILD_A, &workers("o", "p"), &classifier("c"))
        .unwrap();
    let changed = snapshot_key(
        &request,
        &[discovery_file("a.txt")],
        &[],
        &profile,
        ENGINE_BUILD_A,
        &workers("o", "p"),
        &classifier("c"),
    )
    .unwrap();
    assert_ne!(base, changed, "discovery rows must change the snapshot key");
}

#[test]
fn snapshot_key_changes_when_discovery_issues_change() {
    let profile = v2_profile(ReportMode::Daily);
    let request = request(ReportMode::Daily, REQUEST_ID_A);
    let base = snapshot_key(&request, &[], &[], &profile, ENGINE_BUILD_A, &workers("o", "p"), &classifier("c"))
        .unwrap();
    let changed = snapshot_key(
        &request,
        &[],
        &[issue("C:\\work\\unreadable.txt")],
        &profile,
        ENGINE_BUILD_A,
        &workers("o", "p"),
        &classifier("c"),
    )
    .unwrap();
    assert_ne!(base, changed, "discovery issues must change the snapshot key");
}

#[test]
fn snapshot_key_changes_when_worker_identity_changes() {
    let profile = v2_profile(ReportMode::Daily);
    let request = request(ReportMode::Daily, REQUEST_ID_A);
    let base = snapshot_key(&request, &[], &[], &profile, ENGINE_BUILD_A, &workers("office-a", "python-a"), &classifier("c"))
        .unwrap();
    let changed = snapshot_key(&request, &[], &[], &profile, ENGINE_BUILD_A, &workers("office-b", "python-a"), &classifier("c"))
        .unwrap();
    assert_ne!(base, changed, "route-stack worker build must change the snapshot key");
}

#[test]
fn snapshot_key_changes_when_classifier_identity_changes() {
    let profile = v2_profile(ReportMode::Daily);
    let request = request(ReportMode::Daily, REQUEST_ID_A);
    let base = snapshot_key(&request, &[], &[], &profile, ENGINE_BUILD_A, &workers("o", "p"), &classifier("classifier-a"))
        .unwrap();
    let changed = snapshot_key(&request, &[], &[], &profile, ENGINE_BUILD_A, &workers("o", "p"), &classifier("classifier-b"))
        .unwrap();
    assert_ne!(base, changed, "classifier build must change the snapshot key");
}

#[test]
fn snapshot_key_omits_request_id() {
    let profile = v2_profile(ReportMode::Daily);
    let request_a = request(ReportMode::Daily, REQUEST_ID_A);
    let request_b = request(ReportMode::Daily, REQUEST_ID_B);
    let k1 = snapshot_key(&request_a, &[], &[], &profile, ENGINE_BUILD_A, &workers("o", "p"), &classifier("c"))
        .unwrap();
    let k2 = snapshot_key(&request_b, &[], &[], &profile, ENGINE_BUILD_A, &workers("o", "p"), &classifier("c"))
        .unwrap();
    assert_eq!(k1, k2, "the snapshot key must not depend on request_id");
}

#[test]
fn snapshot_key_includes_source_guard_identity() {
    let profile = v2_profile(ReportMode::Daily);
    let request = request(ReportMode::Daily, REQUEST_ID_A);
    let mut file = discovery_file("a.txt");
    let base = snapshot_key(&request, &[file.clone()], &[], &profile, ENGINE_BUILD_A, &workers("o", "p"), &classifier("c"))
        .unwrap();
    file.source_guard_sha256 = Some("1".repeat(64));
    let changed = snapshot_key(&request, &[file], &[], &profile, ENGINE_BUILD_A, &workers("o", "p"), &classifier("c"))
        .unwrap();
    assert_ne!(base, changed, "SourceGuardV2 must enter the snapshot key");
}

#[test]
fn snapshot_key_parts_hash_is_domain_separated_sha256_of_canonical_json() {
    let profile = v2_profile(ReportMode::Daily);
    let request = request(ReportMode::Daily, REQUEST_ID_A);
    let parts = snapshot_key_parts(
        &request,
        &[discovery_file("a.txt")],
        &[],
        &profile,
        ENGINE_BUILD_A,
        &workers("o", "p"),
        &classifier("c"),
    )
    .unwrap();
    assert_eq!(parts.sha256.len(), 64, "snapshot key must be a SHA-256 hex");
    let bytes = parts.canonical_json.as_bytes();
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"snapshot-key-v1\0");
    hasher.update(bytes);
    let expected = hex(&hasher.finalize());
    assert_eq!(parts.sha256, expected, "hash must be the domain-separated SHA-256");
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

// ---------------------------------------------------------------------------
// ArtifactDraft construction invariants
// ---------------------------------------------------------------------------

fn semantic_summary(source_file_count: u64) -> SemanticSummary {
    SemanticSummary {
        source_file_count,
        success_count: source_file_count,
        timeout_count: 0,
        included_file_count: source_file_count,
        omitted_file_count: 0,
        error_file_count: 0,
        input_chars: 42,
        output_chars: 42,
        reserved_chars: 512,
        rendered_chars: 42,
    }
}

fn file_row(identity: &str) -> ArtifactFileRow {
    ArtifactFileRow {
        file_identity: identity.to_string(),
        relative_path: identity.to_string(),
        legacy_source_version: "mtime_ns=1:size=5".to_string(),
        source_guard_kind: Some("content_sha256_v1".to_string()),
        source_guard_sha256: Some("0".repeat(64)),
        parse_profile_hash: "1".repeat(64),
        parse_status: ParseStatus::Success,
        parser_backend: "rust_xlsx_bounded_v1".to_string(),
        worker_lane: "rust_core".to_string(),
        truncated: false,
        content_sha256: "2".repeat(64),
        classifier: Some(PdfClassificationProvenanceV1 {
            status: ai_daily_scanner_contract::PdfClassificationStatus::TextInParseWindow,
            page_count: Some(5),
            result_examined_pages: Some(3),
            nominal_charged_pages: 5,
            classifier_build: "3".repeat(64),
            classifier_profile_hash: "4".repeat(64),
        }),
    }
}

fn decision_row(identity: &str) -> ArtifactDecisionRow {
    ArtifactDecisionRow {
        file_identity: identity.to_string(),
        relative_path: identity.to_string(),
        action: ContextAction::Keep,
        reason: "small_file_keep".to_string(),
        priority: 1,
        input_chars: 42,
        output_chars: 42,
        truncated: false,
        error_code: String::new(),
    }
}

#[test]
fn eligible_draft_requires_one_file_and_decision_row_per_source_file() {
    let summary = semantic_summary(1);
    ArtifactDraft::new(
        true,
        "# 文件证据上下文\n".to_string(),
        summary.clone(),
        vec![file_row("a.txt")],
        vec![decision_row("a.txt")],
    )
    .expect("eligible draft with one row per source file must construct");

    let missing_file = ArtifactDraft::new(
        true,
        "# 文件证据上下文\n".to_string(),
        summary.clone(),
        vec![],
        vec![decision_row("a.txt")],
    );
    assert!(missing_file.is_err(), "eligible artifact must carry a file row per source file");

    let missing_decision = ArtifactDraft::new(
        true,
        "# 文件证据上下文\n".to_string(),
        summary.clone(),
        vec![file_row("a.txt")],
        vec![],
    );
    assert!(missing_decision.is_err(), "eligible artifact must carry a decision row per source file");

    let mismatched = ArtifactDraft::new(
        true,
        "# 文件证据上下文\n".to_string(),
        summary.clone(),
        vec![file_row("a.txt"), file_row("b.txt")],
        vec![decision_row("a.txt"), decision_row("b.txt")],
    );
    assert!(mismatched.is_err(), "eligible artifact row counts must equal source_file_count");
}

#[test]
fn ineligible_draft_must_not_carry_file_or_decision_rows() {
    ArtifactDraft::new(
        false,
        "# 文件证据上下文\n".to_string(),
        semantic_summary(1),
        vec![],
        vec![],
    )
    .expect("ineligible payload draft without rows must construct");

    let with_rows = ArtifactDraft::new(
        false,
        "# 文件证据上下文\n".to_string(),
        semantic_summary(1),
        vec![file_row("a.txt")],
        vec![],
    );
    assert!(with_rows.is_err(), "ineligible artifact must never carry file rows");
}

#[test]
fn draft_context_sha256_equals_sha256_of_final_context() {
    let final_context = "# 文件证据上下文\n".to_string();
    let draft = ArtifactDraft::new(
        true,
        final_context.clone(),
        semantic_summary(1),
        vec![file_row("a.txt")],
        vec![decision_row("a.txt")],
    )
    .expect("draft should construct");
    let expected = hex(&{
        let mut hasher = sha2::Sha256::new();
        hasher.update(final_context.as_bytes());
        hasher.finalize()
    });
    assert_eq!(draft.context_sha256, expected);
}

// ---------------------------------------------------------------------------
// rebuild_envelope
// ---------------------------------------------------------------------------

fn summary_for_envelope() -> ContextSummary {
    ContextSummary {
        source_file_count: 1,
        success_count: 1,
        timeout_count: 0,
        included_file_count: 1,
        omitted_file_count: 0,
        error_file_count: 0,
        input_chars: 7,
        output_chars: 7,
        total_duration_ms: 4,
        discovery_duration_ms: 1,
        parse_duration_ms: 2,
        compression_duration_ms: 1,
    }
}

#[test]
fn rebuilt_envelope_validates_and_omits_file_context_from_scan_runs() {
    let metadata = serde_json::json!({
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": REQUEST_ID_A,
        "engine_version": "test",
        "engine_build": ENGINE_BUILD_A,
        "status": "ok",
        "scan_run_id": 1,
        "context_run_id": 1,
        "warnings": [],
        "error": null
    });
    let current_summary = summary_for_envelope();
    let draft = ArtifactDraft::new(
        true,
        "# 文件证据上下文\n".to_string(),
        semantic_summary(1),
        vec![file_row("a.txt")],
        vec![decision_row("a.txt")],
    )
    .expect("eligible draft");
    let envelope = rebuild_envelope(&metadata, &current_summary, Some(&draft))
        .expect("rebuild must succeed");
    envelope
        .validate()
        .expect("rebuilt envelope must re-validate as ContextEnvelope v1");
    assert_eq!(envelope.file_context, draft.final_context);
    assert_eq!(envelope.summary, current_summary);
    assert_eq!(envelope.status, EngineStatus::Ok);
    assert_eq!(envelope.scan_run_id, Nullable(Some(1)));
    assert_eq!(envelope.context_run_id, Nullable(Some(1)));
}

#[test]
fn rebuild_envelope_for_error_run_uses_empty_context_without_artifact() {
    let metadata = serde_json::json!({
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": REQUEST_ID_A,
        "engine_version": "test",
        "engine_build": ENGINE_BUILD_A,
        "status": "error",
        "scan_run_id": 1,
        "context_run_id": null,
        "warnings": [],
        "error": {
            "error_code": "PARSER_FAILED",
            "message": "scanner could not start",
            "retryable": false,
            "stage": "parse",
            "file_path": null,
            "backend": null
        }
    });
    let envelope = rebuild_envelope(&metadata, &summary_for_envelope(), None)
        .expect("error run rebuild must succeed");
    envelope
        .validate()
        .expect("rebuilt error envelope must validate");
    assert_eq!(envelope.file_context, "");
    assert_eq!(envelope.status, EngineStatus::Error);
}

#[test]
fn rebuild_envelope_rejects_missing_small_fields() {
    let metadata = serde_json::json!({ "status": "ok" });
    let result = rebuild_envelope(&metadata, &summary_for_envelope(), None);
    assert!(result.is_err(), "missing contract/engine fields must fail closed");
}

#[test]
fn rebuild_envelope_rejects_a_context_that_violates_status_invariants() {
    // Success envelope rebuilt with an empty context must fail validation.
    let metadata = serde_json::json!({
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": REQUEST_ID_A,
        "engine_version": "test",
        "engine_build": ENGINE_BUILD_A,
        "status": "ok",
        "scan_run_id": 1,
        "context_run_id": 1,
        "warnings": [],
        "error": null
    });
    let result = rebuild_envelope(&metadata, &summary_for_envelope(), None);
    assert!(result.is_err(), "ok run rebuilt with empty context must fail");
}

// ---------------------------------------------------------------------------
// schema: context_runs snapshot relationship CHECK (spec Part 5.1)
// ---------------------------------------------------------------------------

fn context_run_insert(context_run_id: i64, scan_run_id: i64, snapshot_hit: i64) -> String {
    format!(
        "INSERT INTO context_runs(
            context_run_id, scan_run_id, context_profile_hash, status,
            final_context, context_sha256, source_file_count, success_count,
            timeout_count, included_file_count, omitted_file_count,
            error_file_count, input_chars, output_chars, total_duration_ms,
            discovery_duration_ms, parse_duration_ms, compression_duration_ms,
            created_at_ms, snapshot_hit
         ) VALUES (
            {context_run_id}, {scan_run_id}, '{}', 'success', 'ctx', '{}',
            1, 1, 0, 1, 0, 0, 1, 1, 1, 0, 0, 0, 1, {snapshot_hit}
         )",
        "0".repeat(64),
        "1".repeat(64)
    )
}

fn seed_scan_runs(connection: &rusqlite::Connection, count: i64) {
    for index in 1..=count {
        connection
            .execute(
                "INSERT INTO scan_runs(
                    request_id, canonical_request_json, request_hash_algorithm, request_hash,
                    owner_id, status, created_at_ms, started_at_ms, updated_at_ms,
                    finished_at_ms, final_envelope_json
                 ) VALUES (?1, '{}', 'sha256-request-v1', ?2, 'owner', 'success', 1, 1, 1, 2, '{}')",
                rusqlite::params![
                    format!("00000000-0000-4000-8000-{index:012}"),
                    "0".repeat(64)
                ],
            )
            .expect("scan_runs row");
    }
}

#[test]
fn fresh_v2_schema_enforces_snapshot_hit_reused_check() {
    use ai_daily_scanner_core::store::schema::{configure_connection, migrate};

    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("snapshot-check.sqlite3");
    let mut connection = rusqlite::Connection::open(path).expect("database opens");
    configure_connection(&connection).expect("pragmas");
    migrate(&mut connection).expect("migration");
    seed_scan_runs(&connection, 3);

    // snapshot_hit=0 with a NULL reused_from row is valid.
    connection
        .execute(&context_run_insert(1, 1, 0), [])
        .expect("snapshot_hit=0 without reused_from must insert");

    // snapshot_hit=1 requires a non-null reused_from_context_run_id.
    let orphan_hit = connection.execute(&context_run_insert(2, 2, 1), []);
    assert!(
        orphan_hit.is_err(),
        "snapshot_hit=1 without reused_from_context_run_id must be rejected"
    );

    // A valid hit row references the source context_runs row.
    connection
        .execute(
            "INSERT INTO context_runs(
                context_run_id, scan_run_id, context_profile_hash, status,
                final_context, context_sha256, source_file_count, success_count,
                timeout_count, included_file_count, omitted_file_count,
                error_file_count, input_chars, output_chars, total_duration_ms,
                discovery_duration_ms, parse_duration_ms, compression_duration_ms,
                created_at_ms, snapshot_hit, reused_from_context_run_id
             ) VALUES (
                3, 3, ?1, 'success', 'ctx', ?2,
                1, 1, 0, 1, 0, 0, 1, 1, 1, 0, 0, 0, 1, 1, 1
             )",
            rusqlite::params!["0".repeat(64), "1".repeat(64)],
        )
        .expect("snapshot_hit=1 with reused_from_context_run_id must insert");
}
