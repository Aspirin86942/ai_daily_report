//! Public-boundary integration gate for the production native session path.

#[cfg(windows)]
mod windows {
    use std::path::{Path, PathBuf};

    use ai_daily_scanner_contract::BuildContextRequest;
    use ai_daily_scanner_core::{ScanRequest, Scanner, ScannerConfig};
    use serde_json::json;

    fn repository_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
    }

    fn local_runtime(root: &Path) -> Option<(PathBuf, PathBuf, PathBuf)> {
        let directory = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("rust/target/debug"));
        let python = directory.join("ai-daily-pdf-parser.exe");
        let office = directory.join("ai-daily-office-parser.exe");
        let fixture = root
            .join("tests")
            .join("fixtures")
            .join("pdf_classifier")
            .join("text_plain_01.pdf");
        assert!(
            python.is_file() && office.is_file() && fixture.is_file(),
            "native worker test dependencies are required"
        );
        Some((python, office, fixture))
    }

    /// Run once per fresh database. The external sampler compares the same
    /// synthetic work directory across processes and records peak working sets.
    #[test]
    #[ignore = "requires an isolated synthetic worker benchmark directory"]
    fn cold_worker_scan() {
        let root = PathBuf::from(std::env::var_os("AI_DAILY_BENCH_ROOT").unwrap())
            .canonicalize()
            .unwrap();
        let run = PathBuf::from(std::env::var_os("AI_DAILY_BENCH_RUN").unwrap())
            .canonicalize()
            .unwrap();
        assert!(run.starts_with(&root) && run != root);
        let db = run.join("scan_index_rust_v3.sqlite3");
        assert!(!db.exists());
        let workers: u64 = std::env::var("AI_DAILY_BENCH_WORKERS")
            .unwrap()
            .parse()
            .unwrap();
        assert!(matches!(workers, 1 | 2));
        let (pdf, office, _) = local_runtime(&repository_root()).unwrap();
        let mut config = json!({
            "contract": "ai_daily_context", "protocol_version": 1,
            "request_id": "71616161-7161-4161-8161-716161616161",
            "work_dir": root.join("corpus"), "scan_db_path": db,
            "start_date": "2026-11-20", "end_date": "2026-11-20", "report_mode": "daily",
            "compression_profile": null,
            "scanner_settings": {"max_workers": workers, "total_deadline_ms": 120000,
                "text_max_chars": 60000, "document_excerpt_max_chars": 60000,
                "total_max_chars": 180000, "context_max_chars": 5000000,
                "excel_max_rows": 500, "pdf_max_pages": 50,
                "max_candidate_files": 96, "max_total_pdf_classification_pages": 600,
                "max_pdf_text_extractions": 12},
            "adapters": {"office_worker_path": office, "pdf_worker_path": pdf}
        });
        if std::env::var_os("AI_DAILY_BENCH_LIGHT").is_some() {
            config["scanner_settings"]["direct_text_max_bytes"] = json!(8 * 1024 * 1024);
            config["scanner_settings"]["text_max_chars"] = json!(2000);
        }
        let request: BuildContextRequest = serde_json::from_value(config).unwrap();
        let started = std::time::Instant::now();
        let scanner = Scanner::open(ScannerConfig::from_build_request(&request)).unwrap();
        let scan = scanner
            .build_context(&ScanRequest::from_build_request(&request))
            .unwrap();
        let elapsed = started.elapsed().as_millis();
        assert_eq!(
            scan.exit_code,
            0,
            "scan diagnostic {:?}",
            scan.value.envelope.error.0.as_ref().map(|d| d.error_code)
        );
        let evidence = scan.value.evidence.unwrap();
        assert!(!evidence.execution_metrics.snapshot_hit);
        assert_eq!(evidence.execution_metrics.stage_deadline_exhausted_count, 0);
        let mut chars = 0;
        let mut sources = Vec::new();
        let mut budget_limited_files = 0;
        for source in &scan.value.sources {
            chars += source.excerpt.chars().count();
            budget_limited_files += usize::from(source.excerpt_truncated);
            assert!(!source.excerpt.is_empty() && source.content_sha256.is_some());
            sources.push((
                &source.relative_path,
                &source.content_sha256,
                &source.excerpt_sha256,
                source.parse_status,
                source.worker_lane,
                &source.parser_backend,
                &source.coverage_limits,
            ));
        }
        assert!(!sources.is_empty() && chars <= 180_000);
        let semantics = serde_json::to_vec(&(
            sources,
            evidence.decisions,
            &scan.value.envelope.file_context,
        ))
        .unwrap();
        println!(
            "WORKER_BENCH {}",
            json!({"elapsed_ms": elapsed, "source_count": scan.value.sources.len(), "budget_limited_files": budget_limited_files,
            "excerpt_chars": chars, "semantics_sha256": ai_daily_scanner_core::store::sha256_hex(&semantics)})
        );
    }

    #[test]
    fn build_context_uses_recyclable_native_session_for_pdf_work() {
        let root = repository_root();
        let Some((python, office, fixture)) = local_runtime(&root) else {
            return;
        };
        let temp = tempfile::tempdir().expect("temporary scanner root");
        let work_dir = temp.path().join("work");
        std::fs::create_dir(&work_dir).expect("work directory");
        std::fs::copy(fixture, work_dir.join("text.pdf")).expect("copy PDF fixture");
        let db_path = temp.path().join("scan_index_rust_v3.sqlite3");

        let request = json!({
            "contract": "ai_daily_context",
            "protocol_version": 1,
            "request_id": "72111111-7211-4211-8211-721111111111",
            "work_dir": work_dir,
            "start_date": "2000-01-01",
            "end_date": "2099-12-31",
            "report_mode": "weekly",
            "compression_profile": null,
            "scan_db_path": db_path,
            "scanner_settings": {

                "max_candidate_files": 8,
                "max_total_pdf_classification_pages": 20,
                "max_pdf_text_extractions": 4,
                "total_deadline_ms": 60000,
                "max_workers": 1,
                "worker_max_requests": 1
            },
            "adapters": {
                "office_worker_path": office,
                "pdf_worker_path": python,
            }
        });
        let request_typed: BuildContextRequest =
            serde_json::from_value(request.clone()).expect("typed build request");
        let scanner =
            Scanner::open(ScannerConfig::from_build_request(&request_typed)).expect("open scanner");
        let build = scanner
            .build_context_with_request_id(
                &ScanRequest::from_build_request(&request_typed),
                request_typed.request_id.clone(),
            )
            .expect("in-process build-context");
        let envelope = serde_json::to_value(&build.value.envelope).expect("build response value");
        assert_eq!(build.exit_code, 0, "{envelope}");
        assert_eq!(envelope["status"], "ok", "{envelope}");
        let scan_run_id = envelope["scan_run_id"].as_u64().expect("successful run id");

        let response = serde_json::to_value(
            build
                .value
                .evidence
                .expect("build must return complete evidence"),
        )
        .expect("evidence value");
        assert_eq!(response["scan_run_id"], scan_run_id);
        assert_eq!(response["run_status"], "success", "{response}");
        let metrics = &response["execution_metrics"];

        assert_eq!(metrics["classify_attempt_count"], 1);
        assert_eq!(metrics["parse_attempt_count"], 1);
        assert_eq!(metrics["session_fallback_count"], 0);
        assert!(
            metrics["session_restart_count"].as_u64().unwrap_or(0) >= 1,
            "worker_max_requests=1 must recycle the session between classify and parse: {response}"
        );
        assert!(
            metrics["peak_worker_rss_bytes"].as_u64().unwrap_or(0) > 0,
            "the production session must report observable worker RSS: {response}"
        );
        let file = &response["files"][0];
        assert_eq!(file["parse_transport"], "session", "{response}");
        assert_eq!(file["parse_attempt_count"], 1, "{response}");
        let classification = &file["pdf_classification"];
        assert_eq!(classification["transport"], "session", "{response}");
        assert_eq!(classification["attempt_count"], 1, "{response}");

        let mut snapshot_request = request.clone();
        snapshot_request["request_id"] = json!("72333333-7233-4233-8233-723333333333");
        let snapshot_request_typed: BuildContextRequest =
            serde_json::from_value(snapshot_request).expect("typed snapshot request");
        let snapshot = scanner
            .build_context_with_request_id(
                &ScanRequest::from_build_request(&snapshot_request_typed),
                snapshot_request_typed.request_id.clone(),
            )
            .expect("in-process snapshot build-context");
        let snapshot_envelope =
            serde_json::to_value(&snapshot.value.envelope).expect("snapshot response value");
        assert_eq!(snapshot.exit_code, 0, "{snapshot_envelope}");
        assert_eq!(snapshot_envelope["status"], "ok", "{snapshot_envelope}");
        let snapshot_run_id = snapshot_envelope["scan_run_id"]
            .as_u64()
            .expect("snapshot run id");
        let snapshot_response = serde_json::to_value(
            snapshot
                .value
                .evidence
                .expect("snapshot build must return evidence"),
        )
        .expect("snapshot evidence value");
        assert_eq!(snapshot_response["scan_run_id"], snapshot_run_id);
        let snapshot_metrics = &snapshot_response["execution_metrics"];
        assert_eq!(snapshot_metrics["worker_handshake_ms"], 0);
        assert_eq!(
            snapshot_metrics["snapshot_hit"], true,
            "{}",
            snapshot_response
        );
        assert_eq!(snapshot_metrics["classify_attempt_count"], 0);
        assert_eq!(snapshot_metrics["parse_attempt_count"], 0);
        assert_eq!(snapshot_metrics["peak_worker_rss_bytes"], 0);
    }

    #[test]
    fn daily_pdf_selection_invalidates_cached_cover_and_reuses_five_page_evidence() {
        let root = repository_root();
        let (pdf, office, _) = local_runtime(&root).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        std::fs::copy(
            root.join("rust/pdf_parser/tests/fixtures/settlement-pages.pdf"),
            work.join("pages.pdf"),
        )
        .unwrap();
        let scan = |max_pages| {
            let request: BuildContextRequest = serde_json::from_value(json!({
                "contract": "ai_daily_context", "protocol_version": 1,
                "request_id": "72555555-7255-4255-8255-725555555555",
                "work_dir": work, "scan_db_path": temp.path().join("scan_index_rust_v3.sqlite3"),
                "start_date": "2000-01-01", "end_date": "2099-12-31", "report_mode": "daily",
                "compression_profile": null,
                "scanner_settings": {"allowed_extensions": [".pdf"], "max_workers": 1,
                    "total_deadline_ms": 60000, "pdf_max_pages": max_pages,
                    "document_excerpt_max_chars": 3200},
                "adapters": {"office_worker_path": office, "pdf_worker_path": pdf}
            }))
            .unwrap();
            let scanner = Scanner::open(ScannerConfig::from_build_request(&request)).unwrap();
            let result = scanner
                .build_context(&ScanRequest::from_build_request(&request))
                .unwrap();
            assert_eq!(result.exit_code, 0, "{:?}", result.value.envelope);
            result.value
        };
        let cover = scan(1);
        assert_eq!(cover.sources.len(), 1);
        assert!(!cover.sources[0].excerpt.contains("Final settlement"));
        let selected = scan(5);
        let evidence = selected.evidence.unwrap();
        assert_eq!(evidence.execution_metrics.parse_attempt_count, 1);
        assert_eq!(
            evidence.files[0].cache_miss_reason,
            "parser_identity_changed"
        );
        let source = &selected.sources[0];
        assert_eq!(source.parser_backend, "rust_pdfium_v3");
        assert!(source.excerpt.contains("Final settlement USD 4817.25"));
        assert!(source.excerpt.contains("Effective only after review"));
        assert!(source.parse_truncated);
        let warm = scan(5);
        assert_eq!(
            warm.evidence.unwrap().execution_metrics.parse_attempt_count,
            0
        );
        assert_eq!(warm.sources[0].excerpt, source.excerpt);
        assert_eq!(
            warm.sources[0].coverage_limits["evidence_audit"]["read_ranges"],
            source.coverage_limits["evidence_audit"]["read_ranges"]
        );
    }

    #[test]
    fn native_scan_covers_all_office_formats_and_continues_after_corrupt_input() {
        let root = repository_root();
        let (pdf, office, pdf_fixture) = local_runtime(&root).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let fixtures = root.join("tests/fixtures/worker_documents");
        for name in [
            "modern_sample.docx",
            "modern_sample.pptx",
            "legacy_sample.doc",
            "legacy_sample.ppt",
            "legacy_sample.xls",
        ] {
            std::fs::copy(fixtures.join(name), work.join(name)).unwrap();
        }
        std::fs::copy(pdf_fixture, work.join("text.pdf")).unwrap();
        std::fs::write(
            work.join("note.md"),
            "采购审批方案待确认，金额 1234.56 元。\n",
        )
        .unwrap();
        std::fs::write(work.join("broken.docx"), b"invalid Office container").unwrap();
        let request: BuildContextRequest = serde_json::from_value(json!({
            "contract": "ai_daily_context", "protocol_version": 1,
            "request_id": "71111111-7111-4111-8111-711111111111", "work_dir": work,
            "start_date": "2000-01-01", "end_date": "2099-12-31", "report_mode": "daily", "compression_profile": null,
            "scan_db_path": temp.path().join("scan_index_rust_v3.sqlite3"),
            "scanner_settings": { "allowed_extensions": [".md", ".docx", ".pptx", ".xls", ".doc", ".ppt", ".pdf"],
                "legacy_office_enabled": true, "max_workers": 2, "total_deadline_ms": 60000 },
            "adapters": { "office_worker_path": office, "pdf_worker_path": pdf }
        })).unwrap();
        let scanner = Scanner::open(ScannerConfig::from_build_request(&request)).unwrap();
        let output = scanner
            .build_context_with_request_id(
                &ScanRequest::from_build_request(&request),
                request.request_id.clone(),
            )
            .unwrap();
        assert_eq!(output.exit_code, 0, "{:?}", output.value.envelope);
        let evidence = serde_json::to_value(output.value.evidence.unwrap()).unwrap();
        let files = evidence["files"].as_array().unwrap();
        assert_eq!(files.len(), 8, "{evidence}");
        for file in files {
            let path = file["relative_path"].as_str().unwrap();
            assert_eq!(file["parse_attempt_count"], 1, "{file}");
            assert_eq!(file["fallback_backend"], "", "{file}");
            if path == "broken.docx" {
                assert_eq!(file["parse_status"], "error", "{file}");
                continue;
            }
            assert_eq!(file["parse_status"], "success", "{file}");
            let expected_lane = if path.ends_with(".pdf") {
                "rust_pdf_process_v3"
            } else if path.ends_with(".md") {
                "rust_core"
            } else {
                "rust_office_process_v3"
            };
            assert_eq!(file["worker_lane"], expected_lane, "{file}");
            assert!(!file["parser_backend"].as_str().unwrap().contains("python"));
        }
        assert_eq!(evidence["execution_metrics"]["session_fallback_count"], 0);
        assert!(output
            .value
            .envelope
            .file_context
            .contains("Legacy DOC worker content"));
        assert!(output
            .value
            .envelope
            .file_context
            .contains("Legacy PPT worker content"));
    }
}
