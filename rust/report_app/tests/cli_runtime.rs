use serde_json::Value;
use std::{fs, process::Command};

#[test]
fn cli_dry_run_list_and_runs_use_isolated_config_without_model_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    fs::create_dir(&config).unwrap();
    let work = dir.path().join("work");
    fs::create_dir(&work).unwrap();
    let settings = serde_json::json!({"paths":{
        "work_dir":work,"data_dir":dir.path().join("data"),"db_dir":dir.path().join("db"),
        "reports_dir":dir.path().join("reports"),"log_dir":dir.path().join("logs")}});
    fs::write(config.join("settings.yaml"), settings.to_string()).unwrap();
    let call = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_ai-daily-report"))
            .arg("--config-dir")
            .arg(&config)
            .args(args)
            .env_remove("DEEPSEEK_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    assert_eq!(
        call(&["daily", "--target-date", "2026-10-11", "--dry-run"])["action"],
        "dry_run"
    );
    call(&["runs", "--target-date", "2026-10-11"]);
    call(&["list"]);
    assert!(!dir.path().join("reports/daily/2026-10-11.md").exists());
}
