//! 健康报告原子 UTF-8 落盘：显式路径与 log_dir 日期文件名。
use ai_daily_report::{config::AppConfig, health, health::HealthCheck, health::HealthReport};
use serde_json::json;
use std::{collections::BTreeMap, fs};

fn report(generated_at: &str) -> HealthReport {
    HealthReport {
        status: "warning",
        target_date: "2026-10-18".into(),
        checks: vec![HealthCheck {
            code: "COLLECTION_GAP",
            level: "warning",
            message: "目标范围缺少已完成采集记录".into(),
            evidence: json!({"gaps": 2, "nested": {"中文": "值"}}),
        }],
        task_names: health::TaskNames {
            daily: "AI Daily Report".into(),
            health: "AI Daily Report Health".into(),
        },
        log_dir: "unused".into(),
        generated_at: generated_at.into(),
    }
}

fn setup() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("config")).unwrap();
    fs::create_dir_all(dir.path().join("work")).unwrap();
    let work = dir
        .path()
        .join("work")
        .display()
        .to_string()
        .replace('\\', "/");
    let data = dir
        .path()
        .join("data")
        .display()
        .to_string()
        .replace('\\', "/");
    fs::write(
        dir.path().join("config/settings.yaml"),
        format!("paths:\n  work_dir: '{work}'\n  data_dir: '{data}'\n  reports_dir: '{data}/reports'\n  db_dir: '{data}/db'\n  log_dir: '{data}/logs'\n"),
    )
    .unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    fs::create_dir_all(&config.paths.log_dir).unwrap();
    (dir, config)
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut values: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    values.sort();
    values
}

#[test]
fn write_report_writes_utf8_json_without_leaving_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("nested/health.json");
    let value = report("2026-10-18T03:00:00+00:00");
    health::write_report(&value, &target).unwrap();

    let bytes = fs::read(&target).unwrap();
    let text = String::from_utf8(bytes.clone()).expect("健康文件必须是 UTF-8");
    assert!(text.contains("目标范围缺少已完成采集记录"), "{text}");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed, serde_json::to_value(&value).unwrap());
    assert_eq!(parsed["checks"][0]["evidence"]["nested"]["中文"], "值");
    assert_eq!(parsed["task_names"]["health"], "AI Daily Report Health");
    assert_eq!(parsed["task_names"].as_object().unwrap().len(), 2);
    assert_eq!(names(&dir.path().join("nested")), vec!["health.json"]);
}

#[test]
fn write_report_atomically_replaces_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("health.json");
    fs::write(&target, "旧内容旧内容旧内容").unwrap();
    fs::write(dir.path().join("keep.txt"), "保留").unwrap();

    let value = report("2026-10-18T03:00:00+00:00");
    health::write_report(&value, &target).unwrap();

    let parsed: serde_json::Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    assert_eq!(parsed["status"], "warning");
    assert_eq!(
        names(dir.path()),
        vec!["health.json".to_string(), "keep.txt".to_string()]
    );
}

#[test]
fn persist_uses_shanghai_date_of_generated_at() {
    let (_dir, config) = setup();
    let mut value = report("2026-10-18T16:30:00+00:00");
    value.log_dir = config
        .paths
        .log_dir
        .display()
        .to_string()
        .replace('\\', "/");
    // UTC 16:30 已是上海次日 00:30，文件名必须按上海日期。
    let written = health::persist(&value).unwrap();
    assert_eq!(
        written.file_name().unwrap().to_string_lossy(),
        "health-2026-10-19.json"
    );
    assert_eq!(
        fs::canonicalize(written.parent().unwrap()).unwrap(),
        fs::canonicalize(&config.paths.log_dir).unwrap(),
        "persist 必须写入配置的 log_dir"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&fs::read(&written).unwrap()).unwrap();
    assert_eq!(parsed["target_date"], "2026-10-18");
}

#[test]
fn persist_creates_log_dir_and_rejects_invalid_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let mut value = report("2026-10-18T03:00:00+00:00");
    value.log_dir = dir
        .path()
        .join("logs/new")
        .display()
        .to_string()
        .replace('\\', "/");
    let written = health::persist(&value).unwrap();
    assert!(written.is_file());
    assert_eq!(
        written.file_name().unwrap().to_string_lossy(),
        "health-2026-10-18.json"
    );

    let broken = report("not-a-timestamp");
    assert!(health::persist(&broken).is_err(), "无法解析的时间必须报错");
}

#[test]
fn persist_failure_is_reported_and_keeps_previous_file() {
    let (_dir, config) = setup();
    let previous = config.paths.log_dir.join("health-2026-10-18.json");
    fs::write(&previous, "上一份健康文件").unwrap();
    let mut value = report("2026-10-18T03:00:00+00:00");
    value.log_dir = previous.display().to_string().replace('\\', "/");
    // 目标路径实际是一个已存在的文件，无法当作目录写入：必须报错且不破坏已有内容。
    assert!(health::persist(&value).is_err());
    assert_eq!(fs::read_to_string(&previous).unwrap(), "上一份健康文件");
}

#[cfg(windows)]
#[test]
fn write_report_rejects_redirected_directory() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    fs::create_dir_all(&real).unwrap();
    let link = dir.path().join("link");
    let created = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&real)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !created {
        // 无法创建联接时跳过：该断言依赖 Windows 重解析点，不改变其余持久化合同。
        return;
    }
    let target = link.join("new-directory/health.json");
    let error = health::write_report(&report("2026-10-18T03:00:00+00:00"), &target).unwrap_err();
    assert_eq!(error.code, "HEALTH_PATH_REDIRECTED", "{error}");
    assert!(!real.join("health.json").exists(), "重定向路径不得写入文件");
    assert!(
        fs::read_dir(&real).unwrap().next().is_none(),
        "重定向目录必须保持为空"
    );
}
