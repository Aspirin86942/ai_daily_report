use super::*;

#[test]
fn timeout_inside_walk_stops_before_the_next_file_operation() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("report.md");
    fs::write(&file, "source").unwrap();
    let date = Local::now().date_naive();
    let request = DiscoveryRequest {
        work_dir: temp.path().into(),
        start_date: date,
        end_date: date,
        window_start_utc: None,
        window_end_utc: None,
        follow_symlinks: false,
        allowed_extensions: vec![".md".into()],
        ignored_patterns: vec![],
        excluded_dirs: vec![],
    };
    let checks = std::cell::Cell::new(0);
    let expired = std::cell::Cell::new(false);
    let result = discover_files_report(&request, |path| {
        assert!(
            !expired.get(),
            "a file operation was attempted after timeout"
        );
        if path == file {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                expired.set(true);
                return Err(io::ErrorKind::TimedOut.into());
            }
        }
        Ok(())
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(expired.get());
}

#[test]
fn allowed_extension_is_case_insensitive() {
    assert!(has_allowed_extension("REPORT.MD", &[".md".to_string()]));
    assert!(!has_allowed_extension("REPORT.tmp", &[".md".to_string()]));
}

#[test]
fn ignored_patterns_match_file_name_only() {
    let patterns = compile_patterns(&["~$*".to_string(), "*.tmp".to_string()]).unwrap();

    assert!(matches_ignored_pattern("~$draft.md", &patterns));
    assert!(matches_ignored_pattern("scratch.tmp", &patterns));
    assert!(!matches_ignored_pattern("report.md", &patterns));
}

#[test]
fn excluded_dir_matches_directory_and_children() {
    let root = PathBuf::from("/work/skip");

    assert!(is_excluded_dir(
        Path::new("/work/skip"),
        std::slice::from_ref(&root)
    ));
    assert!(is_excluded_dir(Path::new("/work/skip/nested"), &[root]));
    assert!(!is_excluded_dir(
        Path::new("/work/keep"),
        &[PathBuf::from("/work/skip")]
    ));
}

#[test]
fn discovery_request_defaults_missing_or_null_excluded_dirs() {
    let base = serde_json::json!({
        "work_dir": "/work",
        "start_date": "2026-05-25",
        "end_date": "2026-05-25",
        "allowed_extensions": [".md"],
        "ignored_patterns": []
    });

    let missing: DiscoveryRequest = serde_json::from_value(base.clone()).unwrap();
    let mut with_null = base;
    with_null["excluded_dirs"] = serde_json::Value::Null;
    let null_value: DiscoveryRequest = serde_json::from_value(with_null).unwrap();

    assert!(missing.excluded_dirs.is_empty());
    assert!(null_value.excluded_dirs.is_empty());
}

#[test]
fn discover_files_rejects_missing_work_dir() {
    let today = Local::now().date_naive();
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let request = DiscoveryRequest {
        work_dir: missing,
        start_date: today,
        end_date: today,
        window_start_utc: None,
        window_end_utc: None,
        follow_symlinks: false,
        allowed_extensions: vec![".md".to_string()],
        ignored_patterns: vec![],
        excluded_dirs: vec![],
    };

    let error = match discover_files_with_diagnostics(&request) {
        Ok(_) => panic!("missing work_dir must not be treated as an empty scan"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        error.to_string().contains("work_dir does not exist"),
        "unexpected error: {error}"
    );
}

#[test]
fn candidate_metadata_failure_is_a_structured_issue() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.md");
    let issue = read_candidate_metadata(&missing, &missing)
        .expect_err("missing candidate should return an issue");

    assert_eq!(issue.kind, DiscoveryIssueKind::Metadata);
    assert_eq!(issue.path, Some(contract_path_string(&missing)));
    assert!(issue.message.contains("cannot stat"));
}

#[test]
fn source_version_uses_mtime_ns_and_size() {
    assert_eq!(build_source_version(123, 456), "mtime_ns=123:size=456");
}

#[test]
fn date_range_includes_final_day_nanoseconds_and_excludes_next_day() {
    let start_date = NaiveDate::from_ymd_opt(2026, 5, 24).unwrap();
    let end_date = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
    let final_day_last_ns = end_date.and_hms_nano_opt(23, 59, 59, 999_999_999).unwrap();
    let next_day_start = NaiveDate::from_ymd_opt(2026, 5, 26)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    assert!(is_within_date_range(final_day_last_ns, start_date, end_date).unwrap());
    assert!(!is_within_date_range(next_day_start, start_date, end_date).unwrap());
}

#[test]
fn date_range_accepts_the_maximum_valid_end_date() {
    let final_instant = NaiveDate::MAX
        .and_hms_nano_opt(23, 59, 59, 999_999_999)
        .unwrap();

    assert!(is_within_date_range(final_instant, NaiveDate::MIN, NaiveDate::MAX).unwrap());
}

#[cfg(unix)]
#[test]
fn discover_files_includes_file_symlink_and_emits_stable_contract() {
    let fixture = tempfile::tempdir().unwrap();
    let work_dir = fixture.path().join("work");
    let target_dir = work_dir.join("targets");
    std::fs::create_dir(&work_dir).unwrap();
    std::fs::create_dir(&target_dir).unwrap();

    let target = target_dir.join("target.md");
    let regular = work_dir.join("regular.TXT");
    let link = work_dir.join("LINK.MD");
    std::fs::write(&target, "linked").unwrap();
    std::fs::write(&regular, "regular").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let today = Local::now().date_naive();
    let request = DiscoveryRequest {
        work_dir: work_dir.clone(),
        start_date: today,
        end_date: today,
        window_start_utc: None,
        window_end_utc: None,
        follow_symlinks: true,
        allowed_extensions: vec![".md".to_string(), ".txt".to_string()],
        ignored_patterns: vec![],
        excluded_dirs: vec![],
    };

    let files = discover_files_with_diagnostics(&request).unwrap().files;
    let paths: Vec<&str> = files.iter().map(|item| item.path.as_str()).collect();
    let mut sorted_paths = paths.clone();
    sorted_paths.sort();
    let target_path = target.canonicalize().unwrap().to_string_lossy().to_string();
    let link_path = link.to_string_lossy().to_string();

    assert_eq!(paths, sorted_paths);
    let linked_item = files
        .iter()
        .find(|item| item.path == link_path)
        .expect("file symlink should be discovered through target metadata");
    assert_eq!(linked_item.extension, ".md");
    assert_eq!(
        linked_item.file_identity,
        format!("bootstrap:{}", target_path.to_lowercase())
    );
    assert!(linked_item.source_version.starts_with("mtime_ns="));
    assert!(linked_item.source_version.contains(":size="));
    assert!(chrono::NaiveDateTime::parse_from_str(
        &linked_item.modified_at,
        "%Y-%m-%dT%H:%M:%S%.f"
    )
    .is_ok());
}

#[cfg(unix)]
#[test]
fn discover_files_keeps_symlink_path_when_target_suffix_differs() {
    let fixture = tempfile::tempdir().unwrap();
    let work_dir = fixture.path().join("work");
    let target_dir = work_dir.join("targets");
    std::fs::create_dir(&work_dir).unwrap();
    std::fs::create_dir(&target_dir).unwrap();

    let target = target_dir.join("target.txt");
    let link = work_dir.join("report.MD");
    std::fs::write(&target, "linked text").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let today = Local::now().date_naive();
    let request = DiscoveryRequest {
        work_dir,
        start_date: today,
        end_date: today,
        window_start_utc: None,
        window_end_utc: None,
        follow_symlinks: true,
        allowed_extensions: vec![".md".to_string()],
        ignored_patterns: vec![],
        excluded_dirs: vec![],
    };

    let files = discover_files_with_diagnostics(&request).unwrap().files;
    let target_metadata = std::fs::metadata(&target).unwrap();
    let target_path = target.canonicalize().unwrap().to_string_lossy().to_string();
    let expected_identity = format!("bootstrap:{}", target_path.to_lowercase());

    assert_eq!(files.len(), 1);
    let item = &files[0];
    assert_eq!(item.path, link.to_string_lossy());
    assert_eq!(item.extension, ".md");
    assert_eq!(item.file_identity, expected_identity);
    assert_eq!(item.size_bytes, target_metadata.len());
    assert_eq!(
        item.source_version,
        build_source_version(
            metadata_mtime_ns(&target_metadata).unwrap(),
            target_metadata.len()
        )
    );
}
