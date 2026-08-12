//! Public-boundary integration gate for the production Python session path.

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
        let python = root.join(".venv").join("Scripts").join("python.exe");
        let office = root
            .join("rust")
            .join("target")
            .join("release")
            .join("ai-daily-office-parser.exe");
        let fixture = root
            .join("tests")
            .join("fixtures")
            .join("pdf_classifier")
            .join("text_plain_01.pdf");
        (python.is_file() && office.is_file() && fixture.is_file())
            .then_some((python, office, fixture))
    }

    #[test]
    fn build_context_uses_recyclable_python_session_for_pdf_work() {
        let root = repository_root();
        let Some((python, office, fixture)) = local_runtime(&root) else {
            return;
        };
        let temp = tempfile::tempdir().expect("temporary scanner root");
        let work_dir = temp.path().join("work");
        std::fs::create_dir(&work_dir).expect("work directory");
        std::fs::copy(fixture, work_dir.join("text.pdf")).expect("copy PDF fixture");
        let db_path = temp.path().join("scan_index_v2.sqlite3");

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
            "scanner_profile": {
                "schema_version": "scanner_profile_v2",
                "max_candidate_files": 8,
                "max_total_pdf_classification_pages": 20,
                "max_pdf_text_extractions": 4,
                "total_deadline_ms": 60000,
                "session_concurrency": 1,
                "max_requests_per_session": 1
            },
            "adapters": {
                "office_worker_path": office,
                "python_executable": python,
                "python_module_root": root,
                "python_document_worker_module": "src.workers.document_parser_worker"
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
        assert_eq!(response["status"], "ok", "{response}");
        let metrics = &response["execution_metrics"];

        assert_eq!(metrics["classify_attempt_count"], 1);
        assert_eq!(metrics["parse_attempt_count"], 1);
        assert_eq!(metrics["session_fallback_count"], 0);
        assert!(
            metrics["session_restart_count"].as_u64().unwrap_or(0) >= 1,
            "max_requests_per_session=1 must recycle the session between classify and parse: {response}"
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
        assert_eq!(
            snapshot_metrics["snapshot_hit"], true,
            "{}",
            snapshot_response
        );
        assert_eq!(snapshot_metrics["classify_attempt_count"], 0);
        assert_eq!(snapshot_metrics["parse_attempt_count"], 0);
        assert!(
            snapshot_metrics["peak_worker_rss_bytes"]
                .as_u64()
                .is_some_and(|peak| peak > 0),
            "snapshot must report its live preflight child peak: {}",
            snapshot_response
        );
    }
}
