use ai_daily_report::{
    config::AppConfig,
    snapshots::{canonical_json, digest, valid_hash},
    store::locking::ReportLock,
};
use serde_json::json;
use std::{collections::BTreeMap, fs, time::Duration};

#[test]
fn personal_config_fingerprints_are_deterministic_across_refactors() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        "paths:\n  work_dir: C:/fixed_baseline_work\nreport:\n  profile: personal\n  notes_dir: C:/fixed_baseline_notes\nscanner:\n  max_workers: 2\nllm:\n  provider: openai\n  model_id: gpt-4o-mini\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    let normal = config.fingerprint(false).unwrap();
    let recovery = config.fingerprint(true).unwrap();
    assert!(valid_hash(&normal));
    assert!(valid_hash(&recovery));
    assert_eq!(
        normal,
        "2fad9c27b7259d17296c475e4218a0a7cad175abd9ecfd872ea24551b36348bd"
    );
    assert_eq!(
        recovery,
        "16f2ce28a7f1f69bc9b4f21065b1c5c1edf779c7a660d3033589aecbce1de6d9"
    );
    assert_ne!(normal, recovery);
    assert!(config.matches_personal_fingerprint(&normal, false).unwrap());
    assert!(config
        .matches_personal_fingerprint(&recovery, true)
        .unwrap());
}

#[test]
fn canonical_json_and_digest_match_baseline_contract() {
    let value = json!({
        "z": 1_000_000_000_000_000_000.0_f64,
        "a": [true, null, "text"],
        "m": {"b": 2, "a": 1}
    });
    let canonical = canonical_json(&value).unwrap();
    let text = std::str::from_utf8(&canonical).unwrap();
    let hash = digest(&canonical);
    assert_eq!(
        text,
        r#"{"a":[true,null,"text"],"m":{"a":1,"b":2},"z":1e+18}"#
    );
    assert_eq!(
        hash,
        "fd02ab6a79fb50debbf9c8d4cc0b19c9caddfe9f5cb0c8e6fb2d35ccaff98cc4"
    );
}

#[test]
fn report_lock_enforces_scope_and_is_held_determinism() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("reports.sqlite3");
    assert!(!ReportLock::is_held(&db_path, "personal_work"));

    let lock = ReportLock::acquire(&db_path, "personal_work", Duration::from_secs(1)).unwrap();
    assert!(ReportLock::is_held(&db_path, "personal_work"));
    assert!(!ReportLock::is_held(&db_path, "other_scope"));

    let conflict = ReportLock::acquire(&db_path, "personal_work", Duration::from_millis(50));
    assert_eq!(conflict.err().unwrap().code, "REPORT_ALREADY_RUNNING");

    let other_lock =
        ReportLock::acquire(&db_path, "other_scope", Duration::from_millis(200)).unwrap();
    drop(other_lock);
    drop(lock);
    assert!(!ReportLock::is_held(&db_path, "personal_work"));
}
