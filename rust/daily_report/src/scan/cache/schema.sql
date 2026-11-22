CREATE TABLE parse_cache (
    file_id TEXT NOT NULL,
    source_guard TEXT NOT NULL,
    profile_hash TEXT NOT NULL,
    parser_backend TEXT NOT NULL,
    content_text TEXT NOT NULL,
    content_sha256 TEXT NOT NULL CHECK (length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
    source_sha256 TEXT NOT NULL CHECK (length(source_sha256) = 64 AND source_sha256 NOT GLOB '*[^0-9a-f]*'),
    payload BLOB NOT NULL,
    record_sha256 TEXT NOT NULL,
    last_access_day INTEGER NOT NULL,
    written_at_ms INTEGER NOT NULL,
    PRIMARY KEY (file_id, source_guard, profile_hash)
) STRICT, WITHOUT ROWID;

CREATE TABLE pdf_negative_cache (
    file_id TEXT NOT NULL,
    source_guard TEXT NOT NULL,
    profile_hash TEXT NOT NULL,
    payload BLOB NOT NULL,
    record_sha256 TEXT NOT NULL,
    last_access_day INTEGER NOT NULL,
    written_at_ms INTEGER NOT NULL,
    PRIMARY KEY (file_id, source_guard, profile_hash)
) STRICT, WITHOUT ROWID;

CREATE TABLE scan_snapshots (
    snapshot_key TEXT PRIMARY KEY,
    manifest BLOB NOT NULL,
    refs TEXT NOT NULL,
    record_sha256 TEXT NOT NULL,
    last_access_day INTEGER NOT NULL,
    written_at_ms INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE INDEX parse_cache_access ON parse_cache(last_access_day, written_at_ms);
CREATE INDEX parse_cache_written ON parse_cache(written_at_ms);
CREATE INDEX pdf_negative_cache_access ON pdf_negative_cache(last_access_day, written_at_ms);
CREATE INDEX pdf_negative_cache_written ON pdf_negative_cache(written_at_ms);
CREATE INDEX scan_snapshots_access ON scan_snapshots(last_access_day, written_at_ms);
CREATE INDEX scan_snapshots_written ON scan_snapshots(written_at_ms);
PRAGMA application_id = 1094996803;
PRAGMA user_version = 4;
