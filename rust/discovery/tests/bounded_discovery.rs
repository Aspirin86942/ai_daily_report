use ai_daily_discovery::{
    discover_files_bounded, discover_files_bounded_from, DiscoveryIssueKind, DiscoveryRequest,
};
use chrono::NaiveDate;
use std::{
    collections::BTreeSet,
    fs,
    time::{Duration, Instant},
};

fn request(path: &std::path::Path) -> DiscoveryRequest {
    DiscoveryRequest {
        work_dir: path.into(),
        start_date: NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
        end_date: NaiveDate::from_ymd_opt(9999, 12, 31).unwrap(),
        window_start_utc: None,
        window_end_utc: None,
        follow_symlinks: false,
        allowed_extensions: vec![".txt".into()],
        ignored_patterns: vec![],
        excluded_dirs: vec![],
    }
}

#[test]
fn pages_eventually_visit_all_entries_and_do_not_claim_complete_resumed_inventory() {
    let dir = tempfile::tempdir().unwrap();
    for folder in ["a", "b", "c"] {
        fs::create_dir(dir.path().join(folder)).unwrap();
        for i in 0..23 {
            fs::write(
                dir.path().join(folder).join(format!("{i:03}.txt")),
                "content",
            )
            .unwrap();
        }
    }
    let request = request(dir.path());
    let mut cursor = None;
    let mut seen = BTreeSet::new();
    for _ in 0..100 {
        let (page, next, visited) = discover_files_bounded_from(
            &request,
            Instant::now() + Duration::from_secs(5),
            7,
            cursor,
        )
        .unwrap();
        assert!(visited <= 7);
        assert!(page
            .issues
            .iter()
            .any(|i| i.kind == DiscoveryIssueKind::Budget));
        seen.extend(page.files.iter().map(|f| f.path.clone()));
        cursor = next.map(|c| serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap());
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none());
    assert_eq!(seen.len(), 69);
}

#[test]
fn expired_deadline_visits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file.txt"), "x").unwrap();
    let result = discover_files_bounded(&request(dir.path()), Instant::now(), 10).unwrap();
    assert!(result.files.is_empty());
    assert!(result
        .issues
        .iter()
        .any(|i| i.kind == DiscoveryIssueKind::Budget));
}
