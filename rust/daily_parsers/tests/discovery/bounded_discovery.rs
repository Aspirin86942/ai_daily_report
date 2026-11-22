use chrono::NaiveDate;
use daily_parsers::discovery::{discover_files_until, DiscoveryRequest};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    path::Path,
    time::{Duration, Instant},
};

fn recorded() -> Value {
    serde_json::from_str(include_str!("fixtures/collection-pages.json")).unwrap()
}

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
    // Recovery evidence from the retired collector. Paths in the saved cursors
    // are relative to the fixture root so the same pages can be checked anywhere.
    let recorded = recorded();
    let mut cursor = Value::Null;
    let mut seen = BTreeSet::new();
    let pages = recorded["pages"].as_array().unwrap();
    for (index, page) in pages.iter().enumerate() {
        assert_eq!(page["next"].is_null(), index + 1 == pages.len());
        assert_eq!(page["from"], cursor);
        assert!(page["visited"].as_u64().unwrap() <= 7);
        assert_eq!(page["issues"], json!(["budget"]));
        for file in page["files"].as_array().unwrap() {
            assert!(seen.insert(file.as_str().unwrap().to_owned()));
        }
        cursor = serde_json::from_slice(&serde_json::to_vec(&page["next"]).unwrap()).unwrap();
    }
    assert!(cursor.is_null());
    assert_eq!(seen.len(), 69);
    let active = discover_files_until(
        &request(dir.path()),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert!(active.issues.is_empty());
    let paths: BTreeSet<_> = active
        .files
        .iter()
        .map(|file| {
            Path::new(&file.path)
                .strip_prefix(dir.path())
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    assert_eq!(paths, seen);
}

#[test]
fn expired_deadline_visits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file.txt"), "x").unwrap();
    assert_eq!(
        recorded()["expired"],
        json!({"files": [], "issues": ["budget"]})
    );
    let error = discover_files_until(&request(dir.path()), Instant::now()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}
