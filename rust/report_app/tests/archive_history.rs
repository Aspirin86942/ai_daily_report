use ai_daily_report::{
    archive::{diff, parse, stats, store::ArchiveStore},
    config::AppConfig,
    snapshots::digest,
};
use ai_daily_worker_contract::{archive::ArchiveWriter, ContentUnitKind};
use rusqlite::params;
use std::{collections::BTreeMap, fs};

fn config() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::write(
        dir.path().join("config/settings.yaml"),
        "paths:\n  work_dir: work\nreport:\n  profile: personal\n",
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    (dir, config)
}

#[test]
fn historical_pending_extractions_are_read_without_workers_or_processing_cache() {
    let (_dir, config) = config();
    let store = ArchiveStore::open(&config, true).unwrap();
    for (n, status) in ["queued", "running", "failed"].into_iter().enumerate() {
        let hash = digest(n.to_string().as_bytes());
        store
            .conn
            .execute("INSERT INTO objects VALUES(?,0,'2026-10-01')", [&hash])
            .unwrap();
        store.conn.execute(
            "INSERT INTO extractions(id,sha256,profile,extension,status) VALUES(?,?,'retired-parser','.txt',?)",
            params![hash, hash, status],
        ).unwrap();
        if status == "failed" {
            store
                .conn
                .execute("INSERT INTO metadata VALUES('retry:old',?)", [&hash])
                .unwrap();
        }
    }
    drop(store);
    let database = config.paths.data_dir.join("archive/index.sqlite3");
    let before = fs::read(&database).unwrap();
    let result = stats::query(&config, None, None, None).unwrap();
    assert_eq!(result["storage"]["historical_pending_extractions"], 3);
    assert!(result["storage"].get("parse_queue").is_none());
    assert_eq!(result["new_model_calls"], 0);
    assert_eq!(fs::read(database).unwrap(), before);
    assert!(!config.paths.data_dir.join("daily_cache").exists());
}

#[test]
fn retained_extractions_support_diff_quotes_and_corruption_detection() {
    let (_dir, config) = config();
    let store = ArchiveStore::open(&config, true).unwrap();
    let mut ids = Vec::new();
    for text in ["unchanged\nold amount", "unchanged\nnew amount"] {
        let id = digest(text.as_bytes());
        let directory = store.extraction_dir(&id).unwrap();
        let mut writer = ArchiveWriter::new(&directory, 100_000).unwrap();
        for line in text.lines() {
            assert!(writer.push(line, ContentUnitKind::Paragraph, vec![]));
        }
        let mut artifact = writer.finish();
        artifact.raw_sha256 = id.clone();
        parse::verify_artifact(&directory, &artifact, &id).unwrap();
        store
            .conn
            .execute("INSERT INTO objects VALUES(?,0,'2026-10-01')", [&id])
            .unwrap();
        store.conn.execute(
            "INSERT INTO extractions(id,sha256,profile,extension,status,manifest_json) VALUES(?,?,'retired-parser','.txt','complete',?)",
            params![id, id, serde_json::to_string(&artifact).unwrap()],
        ).unwrap();
        ids.push(id);
    }
    let changes = diff::compare(&store, Some(&ids[0]), &ids[1], &BTreeMap::new()).unwrap();
    assert_eq!(changes.hunks.len(), 1);
    assert_eq!(changes.hunks[0].before.start, 2);
    assert_eq!(changes.hunks[0].after.start, 2);
    let directory = store.extraction_dir(&ids[1]).unwrap();
    let unit = diff::unit(&directory, 2).unwrap();
    assert_eq!(diff::text(&directory, &unit).unwrap(), "new amount");
    let artifact = serde_json::from_value(store.read_manifest(&ids[1]).unwrap()).unwrap();
    fs::write(directory.join("body.md"), "tampered").unwrap();
    assert_eq!(
        parse::verify_artifact(&directory, &artifact, &ids[1])
            .unwrap_err()
            .code,
        "ARCHIVE_EXTRACTION_CORRUPT"
    );
}
