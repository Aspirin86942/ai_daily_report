use chrono::{DateTime, NaiveDate, Utc};
use daily_parsers::discovery::{
    discover_files_until, discover_files_with_diagnostics, DiscoveryIssueKind, DiscoveryRequest,
};
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

#[cfg(windows)]
#[test]
fn an_update_through_another_hard_link_is_in_the_current_window() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    fs::create_dir(&work).unwrap();
    write_at(dir.path(), "writer.md", "2026-10-09T01:00:00Z");
    let link = work.join("report.md");
    fs::hard_link(dir.path().join("writer.md"), &link).unwrap();
    let input = request(&work);
    assert!(discover_files_with_diagnostics(&input)
        .unwrap()
        .files
        .is_empty());
    // NTFS may keep the old timestamp in the other name's directory entry.
    write_at(dir.path(), "writer.md", "2026-10-11T01:00:00Z");
    let enumerated = fs::read_dir(&work)
        .unwrap()
        .map(Result::unwrap)
        .find(|item| item.path() == link)
        .unwrap()
        .metadata()
        .unwrap()
        .modified()
        .unwrap();
    let report = discover_files_until(
        &input,
        std::time::Instant::now() + std::time::Duration::from_secs(5),
    )
    .unwrap();
    let mtime = fs::File::open(&link)
        .unwrap()
        .metadata()
        .unwrap()
        .modified()
        .unwrap();
    println!(
        "NTFS directory timestamp stale after alias write: {}",
        mtime != enumerated
    );
    assert!(report.issues.is_empty());
    assert_eq!(report.files.len(), 1);
    assert!(std::path::Path::new(&report.files[0].path).ends_with("report.md"));
    let ns = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    assert_eq!(
        report.files[0].source_version,
        format!("mtime_ns={ns}:size=0")
    );
    write_at(dir.path(), "writer.md", "2026-10-11T16:00:00Z");
    let report = discover_files_with_diagnostics(&input).unwrap();
    assert!(report.issues.is_empty());
    assert!(report.files.is_empty());
}

#[cfg(windows)]
#[test]
fn a_large_discovery_batch_preserves_hard_links_and_exclusions_in_path_order() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let old = "2026-10-09T01:00:00Z";
    let current = "2026-10-11T01:00:00Z";
    for index in 0..520 {
        write_at(
            &work,
            &format!("project{}/old{index:03}.md", index / 130),
            old,
        );
    }
    let expected = ["project0/current.md", "project3/current.md"];
    for (index, path) in expected.iter().enumerate() {
        let writer = format!("writer{index}.md");
        write_at(dir.path(), &writer, old);
        fs::hard_link(dir.path().join(&writer), work.join(path)).unwrap();
        write_at(dir.path(), &writer, current);
    }
    write_at(&work, "project0/.git/ignored.md", current);
    write_at(&work, "project3/private/ignored.md", current);
    write_at(&work, "project3/~$ignored.md", current);
    let mut input = request(&work);
    input.excluded_dirs.push("project3/private".into());
    let report = discover_files_until(
        &input,
        std::time::Instant::now() + std::time::Duration::from_secs(10),
    )
    .unwrap();
    let paths: Vec<_> = report
        .files
        .iter()
        .map(|item| {
            std::path::Path::new(&item.path)
                .strip_prefix(&work)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert_eq!(paths, expected);
    assert!(report.issues.is_empty());
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
fn expired_active_walk_does_not_even_stat_a_missing_root() {
    let temp = tempfile::tempdir().unwrap();
    let input = request(&temp.path().join("missing"));
    let deadline = std::time::Instant::now();
    assert_eq!(
        discover_files_until(&input, deadline).unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert_eq!(
        discover_files_with_diagnostics(&input).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
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

#[cfg(windows)]
#[test]
#[ignore = "release timing on a synthetic Windows directory tree"]
fn synthetic_directory_discovery() {
    let dir = tempfile::tempdir().unwrap();
    for project in 0..16 {
        for item in 0..128 {
            write_at(
                dir.path(),
                &format!("project{project:02}/old{item:03}.md"),
                "2026-10-09T01:00:00Z",
            );
        }
        write_at(
            dir.path(),
            &format!("project{project:02}/today.md"),
            "2026-10-11T10:00:00Z",
        );
    }
    let request = request(dir.path());
    let mut samples = Vec::new();
    for _ in 0..12 {
        let started = std::time::Instant::now();
        let result = discover_files_with_diagnostics(&request).unwrap();
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(result.files.len(), 16);
        assert!(result.files.iter().all(|f| f.path.ends_with("today.md")));
        assert!(result.issues.is_empty());
    }
    println!("synthetic discovery ms: {samples:.2?}");
    samples.sort_by(f64::total_cmp);
    let median = (samples[5] + samples[6]) / 2.0;
    println!("median {median:.2} ms");
}
