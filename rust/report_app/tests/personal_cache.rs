use ai_daily_report::{processing_cache::ProcessingSession, snapshots::SnapshotStore};
use chrono::Utc;
use std::{
    fs,
    sync::Arc,
    time::{Duration, Instant},
};

fn open(root: &std::path::Path) -> ProcessingSession {
    ProcessingSession::at(
        root,
        Utc::now(),
        Instant::now() + Duration::from_secs(30),
        32_768,
        8_192,
        Duration::ZERO,
    )
    .unwrap()
}

#[test]
fn reservations_share_capacity_and_survive_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let session = open(dir.path());
    let quota = &session.quota;
    let mut first = quota.reserve(16_000).unwrap();
    assert!(quota.reserve(10_000).is_err());
    let path = quota.root.join("material.txt");
    first.track(&path).unwrap();
    fs::write(&path, vec![b'a'; 12_000]).unwrap();
    first.commit(std::slice::from_ref(&path)).unwrap();
    assert_eq!(quota.used_bytes().unwrap(), 20_192);
    drop(session);
    let session = open(dir.path());
    assert_eq!(session.quota.used_bytes().unwrap(), 20_192);
    session.quota.remove(&path).unwrap();
    assert_eq!(session.quota.used_bytes().unwrap(), 8_192);
}

#[test]
fn unknown_files_and_redirects_fail_closed_without_deleting_data() {
    let dir = tempfile::tempdir().unwrap();
    let session = open(dir.path());
    assert!(session.quota.path("../escape").is_err());
    assert!(ProcessingSession::at(
        dir.path(),
        Utc::now(),
        Instant::now() + Duration::from_secs(1),
        32_768,
        8_192,
        Duration::ZERO
    )
    .is_err());
    fs::write(dir.path().join("unknown.txt"), b"preserve").unwrap();
    drop(session);
    assert!(ProcessingSession::at(
        dir.path(),
        Utc::now(),
        Instant::now() + Duration::from_secs(1),
        32_768,
        8_192,
        Duration::ZERO
    )
    .is_err());
    assert_eq!(
        fs::read(dir.path().join("unknown.txt")).unwrap(),
        b"preserve"
    );
}

#[test]
fn snapshots_reserve_both_paths_and_release_temporary_space() {
    let dir = tempfile::tempdir().unwrap();
    let session = open(dir.path());
    let store = SnapshotStore::with_quota(
        session.quota.root.join("snapshots"),
        24_576,
        Arc::clone(&session.quota),
    )
    .unwrap();
    let id = "a".repeat(32);
    let first = store
        .put_bytes(&id, "request", &vec![b'a'; 10_000])
        .unwrap();
    assert_eq!(store.read_bytes(&first).unwrap().len(), 10_000);
    assert!(store
        .put_bytes(&id, "response", &vec![b'b'; 10_000])
        .is_err());
    drop(store);
    drop(session);
    assert_eq!(open(dir.path()).quota.used_bytes().unwrap(), 18_192);
}

#[test]
fn abandoned_writer_is_counted_and_over_limit_commit_cleans_only_its_paths() {
    let dir = tempfile::tempdir().unwrap();
    let session = open(dir.path());
    let mut reservation = session.quota.reserve(10).unwrap();
    let path = session.quota.root.join("interrupted.txt");
    reservation.track(&path).unwrap();
    fs::write(&path, b"partial").unwrap();
    std::mem::forget(reservation); // Emulate an exited writer with a durable pending entry.
    drop(session);
    // The forgotten Arc deliberately retains no OS lock; a new process reconciles actual bytes.
    let session = open(dir.path());
    assert_eq!(session.quota.used_bytes().unwrap(), 8199);
    let mut over = session.quota.reserve(4).unwrap();
    let rejected = session.quota.root.join("too-large.txt");
    over.track(&rejected).unwrap();
    fs::write(&rejected, b"12345").unwrap();
    assert!(over.commit(std::slice::from_ref(&rejected)).is_err());
    assert!(!rejected.exists());
    assert_eq!(fs::read(path).unwrap(), b"partial");
}
