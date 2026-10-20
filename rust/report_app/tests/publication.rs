use ai_daily_report::{
    publish::Publisher,
    snapshots::{digest, SnapshotStore},
    store::{StagedReport, Store},
    types::{RunContext, RunPhase},
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::fs;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-12T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    snapshots: SnapshotStore,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("reports.sqlite3"), now()).unwrap();
        let snapshots = SnapshotStore::new(dir.path().join("snapshots"), 1024 * 1024).unwrap();
        Self {
            _dir: dir,
            store,
            snapshots,
        }
    }
    fn publisher(&self) -> Publisher<'_> {
        Publisher::new(&self._dir.path().join("reports"), &self.store).unwrap()
    }
    fn stage(&self, id: &str, text: &str) {
        let mut context: RunContext =
            serde_json::from_str(include_str!("fixtures/context-v2.json")).unwrap();
        context.run_id = id.into();
        self.store
            .create(&context, "report", id, None, now())
            .unwrap();
        let snapshot = self
            .snapshots
            .put(id, "input", &json!({"version":3}))
            .unwrap();
        self.store
            .prepare(id, &snapshot, &snapshot.sha256, &[], now())
            .unwrap();
        let report: Value = json!({"date":"2026-10-11","window_start_utc":"2026-10-10T16:00:00Z","schema_version":3,"quality":"complete","sources":[]});
        let path = self.publisher().output_path("2026-10-11").unwrap();
        self.store
            .stage_report(
                id,
                &StagedReport {
                    report: &report,
                    markdown: text,
                    output_path: path.to_str().unwrap(),
                    publication_fingerprint: &digest(text.as_bytes()),
                },
                now(),
            )
            .unwrap();
    }
}

#[test]
fn publishes_committed_bytes_and_repeated_repair_makes_no_model_calls() {
    let f = Fixture::new();
    let id = "a".repeat(32);
    let body = "# 日报\n\n方案待确认。\n";
    f.stage(&id, body);
    assert!(f.store.current("2026-10-11").unwrap().is_none());
    let path = f.publisher().publish(&id, now()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), body.as_bytes());
    assert_eq!(f.store.get(&id).unwrap().phase, RunPhase::Published);
    assert_eq!(f.publisher().publish(&id, now()).unwrap(), path);
    assert!(f.store.calls(&id).unwrap().is_empty());
    fs::remove_file(&path).unwrap();
    f.publisher().publish(&id, now()).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), body);
}

#[test]
fn output_written_before_database_pointer_update_is_recoverable() {
    let f = Fixture::new();
    let id = "b".repeat(32);
    let body = "# 已提交正文\n";
    f.stage(&id, body);
    let path = f.publisher().output_path("2026-10-11").unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, body).unwrap();
    f.publisher().publish(&id, now()).unwrap();
    assert_eq!(f.store.current("2026-10-11").unwrap().unwrap().run_id, id);
    assert!(f.store.calls(&id).unwrap().is_empty());
}

#[test]
fn large_output_publishes_and_recovers_without_ignoring_external_tail_edits() {
    let f = Fixture::new();
    let first = "6".repeat(32);
    let second = "7".repeat(32);
    let body = format!("# 日报\n{}\n结束。\n", "a".repeat(9 * 1024 * 1024));
    f.stage(&first, &body);
    let path = f.publisher().publish(&first, now()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), body.as_bytes());
    assert_eq!(f.publisher().publish(&first, now()).unwrap(), path);

    let next = format!("{body}新增证据。\n");
    f.stage(&second, &next);
    // Reproduce an interrupted publication after the file was written.
    fs::write(&path, &next).unwrap();
    f.publisher().publish(&second, now()).unwrap();
    assert_eq!(
        f.store.current("2026-10-11").unwrap().unwrap().run_id,
        second
    );
    assert!(f.store.calls(&second).unwrap().is_empty());

    let external = format!("{body}人工修改。\n");
    assert_eq!(external.len(), next.len());
    fs::write(&path, &external).unwrap();
    assert_eq!(
        f.publisher().publish(&second, now()).unwrap_err().code,
        "OUTPUT_CONFLICT"
    );
    assert_eq!(fs::read(&path).unwrap(), external.as_bytes());
    let candidate = f
        .publisher()
        .root
        .join("conflicts")
        .join(format!("2026-10-11.{second}.md"));
    assert_eq!(fs::read(candidate).unwrap(), next.as_bytes());
}

#[test]
fn replacing_large_output_preserves_the_complete_previous_body() {
    let f = Fixture::new();
    let first = "8".repeat(32);
    let second = "9".repeat(32);
    let body = format!("# 原报告\n{}\n原始结尾。\n", "a".repeat(9 * 1024 * 1024));
    f.stage(&first, &body);
    f.publisher().publish(&first, now()).unwrap();
    f.stage(&second, "# 更新报告\n");
    let path = f.publisher().publish(&second, now()).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "# 更新报告\n");
    let backups: Vec<_> = fs::read_dir(f.publisher().root.join("conflicts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".replaced.md"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read(&backups[0]).unwrap(), body.as_bytes());
}

#[test]
fn external_edits_are_preserved_and_candidate_is_saved_separately() {
    let f = Fixture::new();
    let id = "c".repeat(32);
    let body = "# 候选正文\n";
    f.stage(&id, body);
    let publisher = f.publisher();
    let path = publisher.output_path("2026-10-11").unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "用户修改的内容\n").unwrap();
    for _ in 0..2 {
        assert_eq!(
            publisher.publish(&id, now()).unwrap_err().code,
            "OUTPUT_CONFLICT"
        );
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), "用户修改的内容\n");
    assert!(f.store.current("2026-10-11").unwrap().is_none());
    let candidate = publisher
        .root
        .join("conflicts")
        .join(format!("2026-10-11.{id}.md"));
    assert_eq!(fs::read_to_string(&candidate).unwrap(), body);
    fs::write(candidate, "候选也已人工修改").unwrap();
    assert_eq!(
        publisher.publish(&id, now()).unwrap_err().code,
        "OUTPUT_CONFLICT_COPY_MODIFIED"
    );
}

#[test]
fn windows_atomic_replacement_keeps_verified_previous_body() {
    let f = Fixture::new();
    let first = "d".repeat(32);
    let second = "e".repeat(32);
    f.stage(&first, "# 原正式正文\n");
    f.publisher().publish(&first, now()).unwrap();
    f.stage(&second, "# 更新后的正文\n");
    let path = f.publisher().publish(&second, now()).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "# 更新后的正文\n");
    let backups: Vec<_> = fs::read_dir(f.publisher().root.join("conflicts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with(".replaced.md"))
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(fs::read_to_string(&backups[0]).unwrap(), "# 原正式正文\n");
    assert_eq!(
        f.store.current("2026-10-11").unwrap().unwrap().run_id,
        second
    );
}

#[test]
fn stale_candidate_cannot_overwrite_a_newer_published_version() {
    let f = Fixture::new();
    let first = "1".repeat(32);
    let second = "2".repeat(32);
    let stale = "3".repeat(32);
    f.stage(&first, "# 原正文\n");
    f.publisher().publish(&first, now()).unwrap();
    f.stage(&second, "# 新正文\n");
    f.stage(&stale, "# 过期候选\n");
    let path = f.publisher().publish(&second, now()).unwrap();
    assert_eq!(
        f.publisher().publish(&stale, now()).unwrap_err().code,
        "PUBLICATION_VERSION_CHANGED"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "# 新正文\n");
}

#[test]
fn corrupted_staged_body_and_changed_output_location_are_rejected() {
    let f = Fixture::new();
    let id = "4".repeat(32);
    f.stage(&id, "# 原正文\n");
    f.store
        .connection()
        .unwrap()
        .execute(
            "UPDATE report_runs SET markdown='tampered' WHERE run_id=?",
            [&id],
        )
        .unwrap();
    assert_eq!(
        f.publisher().publish(&id, now()).unwrap_err().code,
        "STAGED_REPORT_HASH_MISMATCH"
    );
    f.store
        .connection()
        .unwrap()
        .execute(
            "UPDATE report_runs SET output_path='C:\\outside\\unrelated.md' WHERE run_id=?",
            [&id],
        )
        .unwrap();
    assert_eq!(
        f.publisher().publish(&id, now()).unwrap_err().code,
        "OUTPUT_LOCATION_CHANGED"
    );
}

#[test]
fn captured_race_requires_conflict_resolution_before_history_changes() {
    let f = Fixture::new();
    let id = "5".repeat(32);
    let body = "# 候选正文\n";
    f.stage(&id, body);
    let path = f.publisher().output_path("2026-10-11").unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    f.store
        .fail(&id, "OUTPUT_CONFLICT_CAPTURED", false, now())
        .unwrap();
    assert_eq!(
        f.publisher().publish(&id, now()).unwrap_err().code,
        "OUTPUT_CONFLICT_CAPTURED"
    );
    assert!(f.store.current("2026-10-11").unwrap().is_none());
}
