CREATE TABLE daily_reports (
                    date TEXT PRIMARY KEY,
                    completed_work TEXT NOT NULL,
                    work_summary TEXT NOT NULL,
                    next_plan TEXT NOT NULL,
                    raw_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                , schema_version INTEGER NOT NULL DEFAULT 1, report_kind TEXT NOT NULL DEFAULT 'legacy_work_diary', current_run_id TEXT REFERENCES report_runs(run_id) ON DELETE RESTRICT);

CREATE TABLE monthly_reports (
                    year_month TEXT PRIMARY KEY,
                    overview TEXT NOT NULL,
                    completed_work TEXT NOT NULL,
                    work_summary TEXT NOT NULL,
                    next_plan TEXT NOT NULL,
                    raw_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

CREATE TABLE report_metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

CREATE TABLE report_model_calls (
            call_id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id TEXT NOT NULL REFERENCES report_runs(run_id) ON DELETE CASCADE,
            generation_id TEXT NOT NULL,
            request_fingerprint TEXT NOT NULL,
            logical_step TEXT NOT NULL,
            attempt INTEGER NOT NULL,
            call_date TEXT NOT NULL,
            status TEXT NOT NULL,
            request_path TEXT,
            request_sha256 TEXT,
            response_path TEXT,
            response_sha256 TEXT,
            usage_json TEXT,
            error_code TEXT,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            duration_ms INTEGER
        );

CREATE TABLE report_run_sources (
            run_id TEXT NOT NULL REFERENCES report_runs(run_id) ON DELETE CASCADE,
            source_id TEXT NOT NULL,
            metadata_json TEXT NOT NULL,
            PRIMARY KEY(run_id, source_id)
        );

CREATE TABLE report_runs (
            run_id TEXT PRIMARY KEY,
            scope_id TEXT NOT NULL CHECK(scope_id='personal_work'),
            target_date TEXT NOT NULL,
            timezone TEXT NOT NULL CHECK(timezone='Asia/Shanghai'),
            run_kind TEXT NOT NULL DEFAULT 'report',
            context_json TEXT NOT NULL,
            config_fingerprint TEXT NOT NULL,
            input_fingerprint TEXT,
            publication_fingerprint TEXT,
            generation_id TEXT NOT NULL,
            regeneration_reason TEXT,
            phase TEXT NOT NULL DEFAULT 'created',
            status TEXT NOT NULL DEFAULT 'running',
            quality TEXT,
            snapshot_path TEXT,
            snapshot_sha256 TEXT,
            report_json TEXT,
            markdown TEXT,
            output_path TEXT,
            expected_sha256 TEXT,
            previous_sha256 TEXT,
            previous_run_id TEXT,
            error_code TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            finished_at TEXT
        );

CREATE TABLE report_schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL
        );

CREATE TABLE weekly_reports (
                    week_label TEXT PRIMARY KEY,
                    date_range TEXT NOT NULL,
                    completed_work TEXT NOT NULL,
                    self_growth TEXT NOT NULL,
                    improvement_actions TEXT NOT NULL,
                    work_summary TEXT NOT NULL,
                    next_plan TEXT NOT NULL,
                    support_needed TEXT NOT NULL,
                    other_notes TEXT NOT NULL,
                    raw_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

CREATE INDEX idx_daily_reports_date ON daily_reports(date);

CREATE INDEX idx_report_model_calls_day ON report_model_calls(call_date);

CREATE INDEX idx_report_model_calls_reuse
            ON report_model_calls(generation_id, request_fingerprint, status);

CREATE INDEX idx_report_runs_business_key
            ON report_runs(scope_id, target_date, created_at);

CREATE INDEX idx_report_runs_fingerprint
            ON report_runs(input_fingerprint, status);
