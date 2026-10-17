PRAGMA user_version=1;
CREATE TABLE metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE collections(
 id TEXT PRIMARY KEY,started_at TEXT NOT NULL,finished_at TEXT,status TEXT NOT NULL,
 baseline INTEGER NOT NULL,metrics_json TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE documents(
 id TEXT PRIMARY KEY,root TEXT NOT NULL,path TEXT NOT NULL UNIQUE,extension TEXT NOT NULL,
 fs_id TEXT,guard TEXT,current_version TEXT,seen_run TEXT,missing_count INTEGER NOT NULL DEFAULT 0,
 baseline INTEGER NOT NULL, audit_day TEXT
);
CREATE INDEX documents_fs_id ON documents(fs_id);
CREATE TABLE paths(document_id TEXT NOT NULL REFERENCES documents(id),path TEXT NOT NULL,observed_at TEXT NOT NULL,PRIMARY KEY(document_id,path,observed_at));
CREATE TABLE objects(sha256 TEXT PRIMARY KEY,size_bytes INTEGER NOT NULL,created_at TEXT NOT NULL);
CREATE TABLE versions(
 id TEXT PRIMARY KEY,document_id TEXT NOT NULL REFERENCES documents(id),parent_id TEXT REFERENCES versions(id),
 sha256 TEXT NOT NULL REFERENCES objects(sha256),path TEXT NOT NULL,extension TEXT NOT NULL,
 size_bytes INTEGER NOT NULL,modified_at TEXT NOT NULL,observed_at TEXT NOT NULL,
 collection_id TEXT NOT NULL REFERENCES collections(id),baseline INTEGER NOT NULL,pruned_at TEXT
);
CREATE INDEX versions_document ON versions(document_id,observed_at,id);
CREATE INDEX versions_sha ON versions(sha256);
CREATE TABLE extractions(
 id TEXT PRIMARY KEY,sha256 TEXT NOT NULL REFERENCES objects(sha256),profile TEXT NOT NULL,
 extension TEXT NOT NULL,status TEXT NOT NULL,attempts INTEGER NOT NULL DEFAULT 0,
 started_at TEXT,ready_at TEXT,manifest_json TEXT,error_code TEXT,
 UNIQUE(sha256,profile,extension)
);
CREATE INDEX extractions_queue ON extractions(status,attempts);
CREATE TABLE version_extractions(version_id TEXT NOT NULL REFERENCES versions(id),extraction_id TEXT NOT NULL REFERENCES extractions(id),PRIMARY KEY(version_id,extraction_id));
CREATE TABLE changes(
 id TEXT PRIMARY KEY,version_id TEXT NOT NULL REFERENCES versions(id),before_version TEXT REFERENCES versions(id),
 extraction_id TEXT NOT NULL REFERENCES extractions(id),before_extraction TEXT REFERENCES extractions(id),
 kind TEXT NOT NULL,observed_at TEXT NOT NULL,ready_at TEXT NOT NULL,manifest_json TEXT NOT NULL
);
CREATE INDEX changes_ready ON changes(ready_at,observed_at);
CREATE TABLE report_evidence(run_id TEXT PRIMARY KEY,target_date TEXT NOT NULL,status TEXT NOT NULL,path TEXT NOT NULL,sha256 TEXT NOT NULL,created_at TEXT NOT NULL,markdown_sha256 TEXT);
CREATE TABLE report_changes(run_id TEXT NOT NULL REFERENCES report_evidence(run_id),change_id TEXT NOT NULL REFERENCES changes(id),PRIMARY KEY(run_id,change_id));
CREATE TABLE report_versions(run_id TEXT NOT NULL REFERENCES report_evidence(run_id),version_id TEXT NOT NULL REFERENCES versions(id),PRIMARY KEY(run_id,version_id));
CREATE TABLE gaps(id INTEGER PRIMARY KEY,collection_id TEXT NOT NULL REFERENCES collections(id),path TEXT,code TEXT NOT NULL,detail TEXT NOT NULL,resolved_at TEXT);
CREATE TABLE file_metrics(collection_id TEXT NOT NULL REFERENCES collections(id),path TEXT NOT NULL,version_id TEXT,capture_ms INTEGER,bytes_read INTEGER NOT NULL DEFAULT 0,parse_ms INTEGER,diff_ms INTEGER,queue_ms INTEGER,backend TEXT,lane TEXT,status TEXT NOT NULL,PRIMARY KEY(collection_id,path));
CREATE TABLE prune_audit(plan_id TEXT PRIMARY KEY,executed_at TEXT NOT NULL,plan_json TEXT NOT NULL);
