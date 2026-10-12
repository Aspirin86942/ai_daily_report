use ai_daily_discovery::{discover_files_with_diagnostics, DiscoveryIssueKind, DiscoveryRequest};
use chrono::{DateTime, NaiveDate, Utc};
use std::{fs, time::SystemTime};

fn request(root: &std::path::Path) -> DiscoveryRequest {
    DiscoveryRequest {
        work_dir: root.to_path_buf(),
        // Deliberately outside the UTC window: tests prove that absolute bounds
        // control discovery independently of the system's local calendar.
        start_date: NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
        end_date: NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
        window_start_utc: Some("2026-10-10T16:00:00Z".parse().unwrap()),
        window_end_utc: Some("2026-10-11T16:00:00Z".parse().unwrap()),
        follow_symlinks: false,
        allowed_extensions: vec![".md".to_string()],
        ignored_patterns: vec!["~$*".to_string()],
        excluded_dirs: vec![".git".into()],
    }
}

fn write_at(root: &std::path::Path, name: &str, timestamp: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = fs::File::create(&path).unwrap();
    let instant: DateTime<Utc> = timestamp.parse().unwrap();
    file.set_times(fs::FileTimes::new().set_modified(SystemTime::from(instant)))
        .unwrap();
}

#[test]
fn excluded_directory_link_does_not_report_a_coverage_gap() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write_at(outside.path(), "ignored.md", "2026-10-11T01:00:00Z");
    let link = root.path().join("node_modules");
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(outside.path(), &link);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(outside.path(), &link);
    if linked.is_err() {
        return;
    }
    let mut input = request(root.path());
    input.excluded_dirs.push("node_modules".into());
    let result = discover_files_with_diagnostics(&input).unwrap();
    assert!(result.files.is_empty());
    assert!(result.issues.is_empty());
}

#[test]
fn utc_start_is_inclusive_end_is_exclusive_even_when_local_dates_differ() {
    let dir = tempfile::tempdir().unwrap();
    write_at(dir.path(), "before.md", "2026-10-10T15:59:59.999999Z");
    write_at(dir.path(), "start.md", "2026-10-10T16:00:00Z");
    write_at(dir.path(), "last.md", "2026-10-11T15:59:59.999999Z");
    write_at(dir.path(), "end.md", "2026-10-11T16:00:00Z");
    let report = discover_files_with_diagnostics(&request(dir.path())).unwrap();
    assert!(report.issues.is_empty());
    let names: Vec<_> = report
        .files
        .iter()
        .map(|f| {
            std::path::Path::new(&f.path)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        })
        .collect();
    assert_eq!(names, ["last.md", "start.md"]);
}

#[test]
fn ignored_directories_are_excluded_at_every_depth() {
    let dir = tempfile::tempdir().unwrap();
    write_at(dir.path(), "project/.git/cache.md", "2026-10-11T10:00:00Z");
    write_at(dir.path(), "project/report.md", "2026-10-11T10:00:00Z");
    write_at(dir.path(), "project/~$temp.md", "2026-10-11T10:00:00Z");
    let report = discover_files_with_diagnostics(&request(dir.path())).unwrap();
    assert!(report.issues.is_empty());
    assert_eq!(report.files.len(), 1);
    assert!(report.files[0].path.ends_with("report.md"));
}

#[test]
fn incomplete_window_is_rejected_before_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let mut input = request(dir.path());
    input.window_end_utc = None;
    assert_eq!(
        discover_files_with_diagnostics(&input).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
fn links_are_diagnosed_and_cannot_escape_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write_at(outside.path(), "outside.md", "2026-10-11T10:00:00Z");
    let target = outside.path().join("outside.md");
    let link = dir.path().join("link.md");
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(target, link);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(target, link);
    // Windows accounts without developer mode may not create symlinks.
    if linked.is_err() {
        return;
    }
    let mut input = request(dir.path());
    let report = discover_files_with_diagnostics(&input).unwrap();
    assert!(report.files.is_empty());
    assert_eq!(report.issues[0].kind, DiscoveryIssueKind::LinkExcluded);
    input.follow_symlinks = true;
    let report = discover_files_with_diagnostics(&input).unwrap();
    assert!(report.files.is_empty());
    assert_eq!(report.issues[0].kind, DiscoveryIssueKind::OutsideRoot);
}
