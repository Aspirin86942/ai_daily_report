use ai_daily_report::{
    cli::{Cli, Command},
    config::AppConfig,
    types::{self, Period, ReportSource, RunContext},
};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use clap::{CommandFactory, Parser};
use std::{collections::BTreeMap, fs};

fn now(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}
fn test_config(text: &str) -> (tempfile::TempDir, AppConfig) {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("config")).unwrap();
    fs::write(
        directory.path().join("config/settings.yaml"),
        format!("paths:\n  work_dir: work\n{text}"),
    )
    .unwrap();
    let config = AppConfig::load_with_env(directory.path(), None, &BTreeMap::new()).unwrap();
    (directory, config)
}

#[test]
fn command_tree_and_existing_daily_constraints() {
    Cli::command().debug_assert();
    for args in [
        vec!["app", "daily", "--date", "2026-10-11"],
        vec!["app", "weekly", "2026-W41", "--source", "db"],
        vec!["app", "monthly", "2026-10", "--source", "scan"],
        vec!["app", "check-config"],
        vec!["app", "task", "plan", "--sid", "S-1-5-21-1"],
    ] {
        assert!(Cli::try_parse_from(args).is_ok());
    }
    for args in [
        vec!["app", "daily", "--scheduled", "--date", "2026-10-11"],
        vec!["app", "daily", "--no-save", "--dry-run"],
        vec!["app", "daily", "--regenerate"],
        vec!["app", "daily", "--reason", "why"],
        vec!["app", "weekly"],
        vec!["app", "daily", "--timezone", "UTC"],
        vec!["app", "backfill", "--workers", "5"],
        vec!["app", "daily", "--date", "2026-1-1"],
    ] {
        assert!(Cli::try_parse_from(args.clone()).is_err(), "{args:?}");
    }
}

#[test]
fn retired_collection_settings_are_rejected_in_effective_configuration() {
    for (key, value) in [
        ("collection_interval_minutes", "30"),
        (
            "collection_workday_window",
            "{start: '09:00', end: '18:00'}",
        ),
        ("daily_source", "scan"),
        ("read_workers", "2"),
        ("parse_workers", "5"),
        ("memory_mib", "3072"),
        ("system_reserve_mib", "2048"),
        ("parent_memory_mib", "512"),
        ("run_seconds", "1200"),
        ("copy_seconds", "120"),
        ("warn_free_mib", "20480"),
        ("reserve_free_mib", "5120"),
        ("model_group_max_units", "16"),
        ("model_group_max_utf8_bytes", "8192"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("config")).unwrap();
        fs::write(
            directory.path().join("config/settings.yaml"),
            format!("paths:\n  work_dir: work\narchive:\n  {key}: {value}\n"),
        )
        .unwrap();
        assert!(
            AppConfig::load_with_env(directory.path(), None, &BTreeMap::new()).is_err(),
            "accepted retired setting {key}"
        );
    }
}

#[test]
fn shanghai_date_and_scheduled_boundary_match_python() {
    let due = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    let day = |text| types::parse_date(text).unwrap();
    assert_eq!(
        types::target_date(now("2026-10-12T00:59:59Z"), None, true, due).unwrap(),
        day("2026-10-10")
    );
    assert_eq!(
        types::target_date(now("2026-10-12T01:00:00Z"), None, true, due).unwrap(),
        day("2026-10-11")
    );
    assert_eq!(
        types::target_date(now("2026-10-11T16:00:00Z"), None, false, due).unwrap(),
        day("2026-10-11")
    );
    assert!(types::target_date(
        now("2026-10-12T01:00:00Z"),
        Some(day("2026-10-12")),
        false,
        due
    )
    .is_err());
    let context: RunContext =
        serde_json::from_str(include_str!("fixtures/context-v2.json")).unwrap();
    context.validate().unwrap();
    assert_eq!(context.window_start_utc, now("2026-10-10T16:00:00Z"));
    assert!(types::parse_date("9999-12-31").is_err());
}

#[test]
fn periods_cover_iso_year_and_leap_month_without_scan_side_effects() {
    let today = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
    let week = Period::weekly(today, Some("2026-W01"), ReportSource::Db).unwrap();
    assert_eq!(week.start.to_string(), "2025-12-29");
    assert_eq!(week.end.to_string(), "2026-01-04");
    assert!(Period::weekly(today, Some("中中ab"), ReportSource::Db).is_err());
    assert!(Period::weekly(today, Some("2025-W53"), ReportSource::Db).is_err());
    assert_eq!(
        Period::monthly(today, Some("2024-02"), ReportSource::Db)
            .unwrap()
            .end
            .to_string(),
        "2024-02-29"
    );
}

#[test]
fn merge_precedence_and_scanner_replacement_are_explicit() {
    let (directory, _) = test_config(
        "llm:\n  model_id: generic\nscanner:\n  allowed_extensions: ['.md']\n  max_workers: 9\n",
    );
    let config_dir = directory.path().join("config");
    fs::write(config_dir.join("settings.windows.yaml"), "llm:\n  model_id: platform\nscanner:\n  dynaconf_merge: false\n  allowed_extensions: ['.txt']\n").unwrap();
    fs::write(
        config_dir.join(".secrets.yaml"),
        "llm:\n  model_id: secret-setting\napi:\n  deepseek_api_key: synthetic-file-token\n",
    )
    .unwrap();
    let env = BTreeMap::from([
        (
            "DAILY_REPORT_LLM__MODEL_ID".into(),
            "environment-model".into(),
        ),
        (
            "DAILY_REPORT_RECOVERY__ENFORCE_MODEL_BUDGETS".into(),
            "false".into(),
        ),
        ("DEEPSEEK_API_KEY".into(), "synthetic-env-token".into()),
    ]);
    let config = AppConfig::load_with_env(directory.path(), Some(&config_dir), &env).unwrap();
    assert_eq!(config.llm.model_id, "environment-model");
    assert_eq!(config.api_key(), "synthetic-env-token");
    assert!(config.config_check().get("enforce_model_budgets").is_none());
    assert_eq!(config.scanner.allowed_extensions.unwrap(), [".txt"]);
    assert_eq!(config.scanner.max_workers, None);
}

#[test]
fn malformed_or_retired_configuration_fails_without_exposing_credentials() {
    let (directory, _) = test_config("");
    let path = directory.path().join("config/settings.yaml");
    for (extra, expected) in [
        (
            "scanner:\n  fallback_after_timeout: false\n",
            "CONFIG_RETIRED_OPTION",
        ),
        ("llm: []\n", "CONFIG_INVALID"),
        ("llm:\n  max_retries: 4\n", "CONFIG_INVALID"),
        ("schedule:\n  local_time: '9:00'\n", "CONFIG_INVALID"),
    ] {
        fs::write(&path, format!("paths:\n  work_dir: work\n{extra}")).unwrap();
        assert_eq!(
            AppConfig::load_with_env(directory.path(), None, &BTreeMap::new())
                .unwrap_err()
                .code,
            expected
        );
    }
    fs::write(path, "api: [synthetic-secret: \"unterminated").unwrap();
    let error = AppConfig::load_with_env(directory.path(), None, &BTreeMap::new()).unwrap_err();
    assert!(!error.to_string().contains("synthetic-secret"));
}

#[test]
fn installed_paths_must_be_existing_directories_under_shared() {
    let (directory, _) = test_config("");
    let shared = directory.path().join("shared");
    fs::create_dir_all(shared.join("config")).unwrap();
    fs::write(
        shared.join("config/settings.yaml"),
        "paths:\n  work_dir: work\n",
    )
    .unwrap();
    let mut env = BTreeMap::from([
        (
            "DAILY_REPORT_INSTALL_ROOT".into(),
            directory.path().to_string_lossy().into_owned(),
        ),
        (
            "DAILY_REPORT_CONFIG_DIR".into(),
            shared.join("config").to_string_lossy().into_owned(),
        ),
    ]);
    for name in ["DATA_DIR", "REPORTS_DIR", "DB_DIR", "LOG_DIR"] {
        let path = shared.join(name);
        fs::create_dir(&path).unwrap();
        env.insert(
            format!("DAILY_REPORT_{name}"),
            path.to_string_lossy().into_owned(),
        );
    }
    assert!(AppConfig::load_with_env(directory.path(), None, &env).is_ok());
    env.insert(
        "DAILY_REPORT_DB_DIR".into(),
        directory.path().to_string_lossy().into_owned(),
    );
    assert!(AppConfig::load_with_env(directory.path(), None, &env).is_err());
}

#[test]
fn request_creation_keeps_preview_and_reason_semantics() {
    let (_directory, config) = test_config("recovery:\n  enforce_model_budgets: false\n");
    let command = Cli::parse_from(["app", "daily", "--target-date", "2026-10-11", "--no-save"])
        .command
        .unwrap();
    let request = command
        .report_request(
            &config,
            now("2026-10-12T01:00:00Z"),
            "a".repeat(32),
            "fingerprint".into(),
        )
        .unwrap();
    assert!(!request.save && !request.scheduled);
    assert_eq!(request.period.start.to_string(), "2026-10-11");
    let invalid = Cli::parse_from(["app", "daily", "--regenerate", "--reason", "  "])
        .command
        .unwrap();
    assert!(invalid
        .report_request(
            &config,
            now("2026-10-12T01:00:00Z"),
            "a".repeat(32),
            String::new()
        )
        .is_err());
    assert!(matches!(
        Cli::parse_from(["app", "doctor"]).command,
        Some(Command::Doctor { .. })
    ));
}

#[test]
fn invalid_response_retries_defaults_and_is_bounded() {
    let (_directory, config) = test_config("");
    assert_eq!(config.llm.invalid_response_retries, 1);

    let invalid = tempfile::tempdir().unwrap();
    fs::create_dir(invalid.path().join("config")).unwrap();
    fs::write(
        invalid.path().join("config/settings.yaml"),
        "paths:\n  work_dir: work\nllm:\n  invalid_response_retries: 5\n",
    )
    .unwrap();
    assert_eq!(
        AppConfig::load_with_env(invalid.path(), None, &BTreeMap::new())
            .unwrap_err()
            .code,
        "CONFIG_INVALID"
    );

    let (_directory, zero) = test_config("llm:\n  invalid_response_retries: 0\n");
    assert_eq!(zero.llm.invalid_response_retries, 0);
}

#[test]
fn invalid_response_retries_is_excluded_from_recovery_fingerprint() {
    let (directory, baseline) = test_config("");
    let path = directory.path().join("config/settings.yaml");
    fs::write(
        &path,
        "paths:\n  work_dir: work\nllm:\n  invalid_response_retries: 2\n",
    )
    .unwrap();
    let changed = AppConfig::load_with_env(directory.path(), None, &BTreeMap::new()).unwrap();
    assert_eq!(
        baseline.fingerprint(true).unwrap(),
        changed.fingerprint(true).unwrap(),
        "恢复身份不应因重试次数变化而失效"
    );
    assert_ne!(
        baseline.fingerprint(false).unwrap(),
        changed.fingerprint(false).unwrap()
    );
}

#[test]
fn retired_quota_settings_are_ignored_and_quota_flags_are_removed() {
    let (_dir,config) = test_config("recovery:\n  enforce_model_budgets: true\n  max_calls_per_run: 0\n  max_calls_per_calendar_day: 0\n  max_tokens_per_run: 0\n  max_tokens_per_calendar_day: 0\n  max_cost_per_run_microunits: 0\n  max_cost_per_calendar_day_microunits: 0\n");
    let value = serde_json::to_value(config.recovery).unwrap();
    assert!(value.get("max_calls_per_run").is_none());
    assert!(value.get("enforce_model_budgets").is_none());
    assert!(Cli::try_parse_from([
        "app",
        "daily",
        "--regenerate",
        "--reason",
        "retry",
        "--additional-model-calls",
        "1"
    ])
    .is_err());
    assert!(Cli::try_parse_from(["app", "backfill", "--call-budget", "1"]).is_err());
}
