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
