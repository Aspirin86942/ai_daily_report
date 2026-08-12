use ai_daily_scanner_contract::{NormalizedScannerSettings, ReportMode, ScannerSettings};
use ai_daily_scanner_core::classifier::{classify_candidate, ClassificationError, ParserRoute};
use ai_daily_scanner_core::config::normalize_scanner_settings;
use ai_daily_scanner_core::discovery::{
    bootstrap_file_identity, discover_files_with_diagnostics, normalize_contract_path_text,
    DiscoveredFileOut, DiscoveryIssueKind, DiscoveryRequest,
};
use ai_daily_scanner_core::planner::{plan_candidates, PlanAction};
use chrono::Local;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

#[test]
fn rust_defaults_match_all_frozen_normalized_profiles() {
    let raw: ScannerSettings =
        serde_json::from_str(r#"{}"#).expect("minimal raw profile should parse");
    let cases = [
        (
            ReportMode::Daily,
            include_str!(
                "../../../tests/fixtures/scanner_contract/v1/normalized-settings-daily.json"
            ),
        ),
        (
            ReportMode::Weekly,
            include_str!(
                "../../../tests/fixtures/scanner_contract/v1/normalized-settings-weekly.json"
            ),
        ),
        (
            ReportMode::Monthly,
            include_str!(
                "../../../tests/fixtures/scanner_contract/v1/normalized-settings-monthly.json"
            ),
        ),
    ];

    for (mode, fixture) in cases {
        let expected: NormalizedScannerSettings =
            serde_json::from_str(fixture).expect("fixture should parse");
        let actual =
            normalize_scanner_settings(&raw, mode).expect("frozen defaults should normalize");
        assert_eq!(actual, expected);
    }
}

#[test]
fn raw_profile_sets_and_timeout_keys_become_canonical() {
    let raw: ScannerSettings = serde_json::from_value(serde_json::json!({

        "allowed_extensions": [".txt", ".md", ".txt"],
        "ignored_patterns": [" ~$* ", "*.tmp", "*.tmp"],
        "excluded_dirs": [" C:\\Synthetic Excluded ", "C:\\Synthetic Excluded"],
        "file_timeout_by_extension": {".xlsx": 12, ".pdf": 7},
        "direct_text_max_bytes": 1234,
        "text_excerpt_max_chars": 321
    }))
    .expect("raw profile should parse");

    let normalized =
        normalize_scanner_settings(&raw, ReportMode::Daily).expect("profile should normalize");

    assert_eq!(normalized.discovery.allowed_extensions, [".md", ".txt"]);
    assert_eq!(normalized.discovery.ignored_patterns, ["*.tmp", "~$*"]);
    assert_eq!(
        normalized.discovery.excluded_dirs,
        ["C:\\Synthetic Excluded"]
    );
    assert_eq!(
        normalized.execution.file_timeout_by_extension_ms,
        [(".pdf".to_string(), 7_000), (".xlsx".to_string(), 12_000)]
            .into_iter()
            .collect()
    );
    assert_eq!(normalized.parse.text.read_head_bytes, 1234);
    assert_eq!(normalized.parse.text.excerpt_max_chars, 321);
}

#[test]
fn discovery_filters_and_preserves_bootstrap_identity_and_source_version() {
    let directory = tempdir().expect("temporary root should exist");
    let work_dir = directory.path().join("工作 目录");
    let included = work_dir.join("Included");
    let excluded = work_dir.join("Excluded");
    fs::create_dir_all(&included).expect("included directory should exist");
    fs::create_dir_all(&excluded).expect("excluded directory should exist");
    let alpha = included.join("甲 报告.TXT");
    let beta = included.join("beta.md");
    fs::write(&alpha, "alpha").expect("alpha should be written");
    fs::write(&beta, "beta").expect("beta should be written");
    fs::write(included.join("~$draft.md"), "ignored").expect("ignored fixture should be written");
    fs::write(excluded.join("blocked.md"), "blocked").expect("excluded fixture should be written");
    let today = Local::now().date_naive();
    let request = DiscoveryRequest {
        work_dir: work_dir.clone(),
        start_date: today,
        end_date: today,
        allowed_extensions: vec![".md".to_string(), ".txt".to_string()],
        ignored_patterns: vec!["~$*".to_string()],
        excluded_dirs: vec![PathBuf::from("Excluded")],
    };

    let first = discover_files_with_diagnostics(&request).expect("discovery should succeed");
    let second =
        discover_files_with_diagnostics(&request).expect("repeated discovery should succeed");

    assert!(first.issues.is_empty());
    assert_eq!(first, second);
    assert_eq!(first.files.len(), 2);
    assert!(first
        .files
        .windows(2)
        .all(|pair| pair[0].path <= pair[1].path));
    let alpha_file = first
        .files
        .iter()
        .find(|file| file.path.ends_with("甲 报告.TXT"))
        .expect("display path casing should be preserved");
    assert_eq!(alpha_file.extension, ".txt");
    for file in &first.files {
        let display_path = PathBuf::from(&file.path);
        let resolved =
            fs::canonicalize(&display_path).expect("discovered file should canonicalize");
        let metadata = fs::metadata(&display_path).expect("discovered file should have metadata");
        assert_eq!(file.file_identity, bootstrap_file_identity(&resolved));
        assert_eq!(file.size_bytes, metadata.len());
        assert!(file.source_version.starts_with("mtime_ns="));
        assert!(file
            .source_version
            .ends_with(&format!(":size={}", metadata.len())));
    }
}

#[test]
fn relative_exclusions_are_resolved_from_the_work_dir() {
    let directory = tempdir().expect("temporary root should exist");
    let work_dir = directory.path().join("relative exclusion work");
    let unique_name = format!(
        "excluded-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let excluded = work_dir.join(&unique_name);
    fs::create_dir_all(&excluded).expect("excluded directory should exist");
    fs::write(excluded.join("blocked.md"), "blocked").expect("excluded fixture should be written");
    let today = Local::now().date_naive();
    let request = DiscoveryRequest {
        work_dir,
        start_date: today,
        end_date: today,
        allowed_extensions: vec![".md".to_string()],
        ignored_patterns: Vec::new(),
        excluded_dirs: vec![PathBuf::from(unique_name)],
    };

    let discovery = discover_files_with_diagnostics(&request).expect("discovery should succeed");

    assert!(discovery.files.is_empty());
    assert!(discovery.issues.is_empty());
}

#[test]
fn discovery_deduplicates_resolved_aliases() {
    let directory = tempdir().expect("temporary root should exist");
    let work_dir = directory.path().join("alias work");
    fs::create_dir_all(&work_dir).expect("work directory should exist");
    let target = work_dir.join("a-target.md");
    let alias = work_dir.join("z-alias.md");
    fs::write(&target, "same file").expect("target should be written");
    if create_file_symlink(&target, &alias).is_err() {
        return;
    }
    let today = Local::now().date_naive();
    let request = DiscoveryRequest {
        work_dir,
        start_date: today,
        end_date: today,
        allowed_extensions: vec![".md".to_string()],
        ignored_patterns: Vec::new(),
        excluded_dirs: Vec::new(),
    };

    let discovery = discover_files_with_diagnostics(&request).expect("discovery should succeed");

    assert_eq!(discovery.files.len(), 1);
    assert!(discovery.files[0].path.ends_with("a-target.md"));
    assert_eq!(discovery.issues.len(), 1);
    assert_eq!(discovery.issues[0].kind, DiscoveryIssueKind::Alias);
    assert!(discovery.issues[0]
        .path
        .as_deref()
        .is_some_and(|path| path.ends_with("z-alias.md")));
}

#[test]
fn planner_is_deterministic_and_rejects_large_files_before_parsing() {
    let raw: ScannerSettings =
        serde_json::from_str(r#"{}"#).expect("minimal raw profile should parse");
    let profile =
        normalize_scanner_settings(&raw, ReportMode::Daily).expect("defaults should normalize");
    let files = vec![
        synthetic_file("C:\\scan\\z.xlsx", ".xlsx", 100),
        synthetic_file("C:\\scan\\a.txt", ".txt", 100),
        synthetic_file("C:\\scan\\b.xls", ".xls", 100),
        synthetic_file("C:\\scan\\c.pdf", ".pdf", 100),
        synthetic_file(
            "C:\\scan\\too-large.md",
            ".md",
            profile.execution.max_file_size_bytes + 1,
        ),
    ];

    let plans = plan_candidates(files, &profile);

    assert!(plans
        .windows(2)
        .all(|pair| pair[0].file.path <= pair[1].file.path));
    assert_eq!(plans[0].action, PlanAction::Parse(ParserRoute::LightText));
    assert_eq!(
        plans[1].action,
        PlanAction::Parse(ParserRoute::PythonOffice)
    );
    assert_eq!(plans[2].action, PlanAction::Parse(ParserRoute::Pdf));
    assert_eq!(
        plans[3].action,
        PlanAction::Reject(ClassificationError::FileTooLarge)
    );
    assert_eq!(plans[4].action, PlanAction::Parse(ParserRoute::RustXlsx));
    assert_eq!(plans[1].timeout_ms, 60_000);
}

#[test]
fn legacy_doc_and_ppt_routes_require_the_explicit_profile_switch() {
    let disabled_raw: ScannerSettings = serde_json::from_str(r#"{"allowed_extensions":[".doc"]}"#)
        .expect("disabled profile should parse");
    let enabled_raw: ScannerSettings =
        serde_json::from_str(r#"{"allowed_extensions":[".doc"],"legacy_office_enabled":true}"#)
            .expect("enabled profile should parse");
    let disabled = normalize_scanner_settings(&disabled_raw, ReportMode::Daily)
        .expect("disabled profile should normalize");
    let enabled = normalize_scanner_settings(&enabled_raw, ReportMode::Daily)
        .expect("enabled profile should normalize");
    let file = synthetic_file("C:\\scan\\legacy.doc", ".doc", 100);

    assert_eq!(
        classify_candidate(&file, &disabled),
        Err(ClassificationError::LegacyExtensionDisabled)
    );
    assert_eq!(
        classify_candidate(&file, &enabled),
        Ok(ParserRoute::PythonSharepointText)
    );
}

#[test]
fn windows_verbatim_drive_unc_and_identity_fixtures_are_frozen() {
    assert_eq!(
        normalize_contract_path_text(r"\\?\C:\Synthetic Root\报告.txt"),
        r"C:\Synthetic Root\报告.txt"
    );
    assert_eq!(
        normalize_contract_path_text(r"\\?\UNC\server\share\报告.txt"),
        r"\\server\share\报告.txt"
    );
    assert_eq!(
        bootstrap_file_identity(Path::new(r"C:\Synthetic Root\报告.TXT")),
        bootstrap_file_identity(Path::new(r"c:\synthetic root\报告.txt"))
    );
}

#[cfg(windows)]
#[test]
fn windows_long_path_discovery_returns_paths_without_verbatim_prefix() {
    let directory = tempdir().expect("temporary root should exist");
    let normal_root = directory.path().join("long path 工作区");
    let verbatim_root = PathBuf::from(format!(r"\\?\{}", normal_root.display()));
    let mut nested = verbatim_root.clone();
    for index in 0..8 {
        nested.push(format!("segment-{index:02}-abcdefghijklmnopqrstuvwxyz"));
    }
    fs::create_dir_all(&nested).expect("long directory should be created");
    let sample = nested.join("报告.txt");
    fs::write(&sample, "long path fixture").expect("long-path file should be written");
    let today = Local::now().date_naive();
    let request = DiscoveryRequest {
        work_dir: verbatim_root.clone(),
        start_date: today,
        end_date: today,
        allowed_extensions: vec![".txt".to_string()],
        ignored_patterns: Vec::new(),
        excluded_dirs: Vec::new(),
    };

    let report =
        discover_files_with_diagnostics(&request).expect("long-path discovery should succeed");

    assert!(report.issues.is_empty());
    assert_eq!(report.files.len(), 1);
    assert!(!report.files[0].path.starts_with(r"\\?\"));
    assert!(!report.files[0].file_identity.contains(r"\\?\"));
    fs::remove_dir_all(&verbatim_root).expect("verified long-path fixture should be removed");
}

fn synthetic_file(path: &str, extension: &str, size_bytes: u64) -> DiscoveredFileOut {
    DiscoveredFileOut {
        file_identity: format!("bootstrap:{}", path.to_lowercase()),
        path: path.to_string(),
        extension: extension.to_string(),
        modified_at: "2026-07-15T12:00:00.000000".to_string(),
        size_bytes,
        source_version: format!("mtime_ns=1:size={size_bytes}"),
        source_guard_kind: None,
        source_guard_sha256: None,
    }
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, alias: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, alias)
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, alias: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, alias)
}
