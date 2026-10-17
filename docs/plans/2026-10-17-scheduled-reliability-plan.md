# 定时链路可靠性实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 subagent-driven-development（推荐）或 executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 让每日 09:00 日报在模型响应偶发无效或成稿容量不足时自动恢复，失败后当天自动续跑，并通过健康巡检与本地通知暴露异常。

**架构：** 三层自愈：应用层对 `MODEL_RESPONSE_INVALID` 做有界重试、对成稿容量做拆分或降级待确认；宿主层为 daily 任务增加 09:30/10:30 两次兜底触发（已发布时快速返回、零模型调用）；观测层新增只读 `health` 命令与独立健康任务（11:00/23:00），异常时写健康文件并弹 Windows 本地通知。调度 JSONL 补齐 kind/run_id/target_date/phase/error_code/duration。

**技术栈：** Rust 2021（clap、rusqlite、serde_json、windows-sys 0.61）、Windows PowerShell 5.1、MiniJinja 无需改动、现有本地 HTTP 测试夹具。

**规格：** [docs/plans/2026-10-17-scheduled-reliability-design.md](2026-10-17-scheduled-reliability-design.md)

## 全局约束

- 平台 Windows x64；不新增第三方 crate；workspace 依赖版本不变。
- 一律使用隔离的临时 config/data 目录做测试；测试中 0 次真实模型调用（用本地 HTTP 夹具或 `Script` transport）。
- 不修改正式计划任务、正式配置、`.releases` 发布包；不 push、不部署、不删除旧版材料。
- 新增命令沿用 `--config-dir` 全局参数与现有 JSON/文本输出约定；错误码为 `SCREAMING_SNAKE_CASE`。
- 文档与注释使用中文；提交为本地 commit，提交信息用短祈使句。
- 阶段门禁：`cargo fmt --manifest-path rust/Cargo.toml --all -- --check`、`cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings`、`cargo test --manifest-path rust/Cargo.toml --workspace --locked`。

## 审查重点（Review Focus）

1. 日报/归档数据库缺失或损坏时，`health` 必须以 unhealthy（退出码 2）返回，不能崩溃、也不能返回 0。→ 任务 5 `health_reports_database_failures_as_unhealthy`。
2. `MODEL_RESPONSE_INVALID` 重试不得突破单步骤总尝试上限（`max_retries`）或在运行截止时间后继续发送。→ 任务 2 `invalid_response_retry_is_bounded_by_attempt_limit`。
3. 成稿容量拆分/降级后，每个 fact 仍必须出现在正文或待确认，`source_ids` 与引用不丢。→ 任务 3 `compose_capacity_keeps_every_fact_covered`。
4. 已发布目标日被兜底触发重复执行时，不得重复发布或新增模型调用。→ 既有测试 `tests/archive.rs::archive_dry_run_keeps_frozen_identity_and_existing_report_does_not_create_a_run`（任务 7 步骤 1 复跑确认）。
5. 采集连续性检查必须对基线日豁免，且历史缺口不影响当日判定。→ 任务 5 `health_baseline_window_is_not_a_collection_gap`。

## 文件结构

创建：

- `rust/report_app/src/health.rs`：只读健康检查、JSON/文本渲染、退出码；含采集连续性、日报发布、运行状态、解析队列、磁盘、数据库完整性 6 个检查，以及任务名集合。
- `rust/report_app/tests/health.rs`：health 检查与 CLI 退出码的集成测试。
- `rust/report_app/tests/scheduled_log.rs`：调度日志字段的集成测试。
- `scripts/health_notify.ps1`：调用 `health --scheduled`、合并计划任务回读、写健康文件、托盘通知、去重。
- `docs/plans/2026-10-17-scheduled-reliability-plan.md`：本计划。

修改：

- `rust/report_app/src/config.rs`、`src/config.defaults.json`、`config/settings.example.yaml`：新增 `llm.invalid_response_retries`（默认 1，0–2）。
- `rust/report_app/src/model/mod.rs`：响应无效有界重试；`split_error` 增加成稿容量码。
- `rust/report_app/src/model/plan.rs`：compose 分组拆分/降级；compose 步骤不再携带 archive 元数据。
- `rust/report_app/src/model/contracts.rs`：`ComposedReport::pending_from` 本地待确认构造。
- `rust/report_app/src/lib.rs`：注册 `pub mod health;`。
- `rust/report_app/src/cli.rs`：新增 `Health(HealthArgs)`，`TaskCommand` 的 kind 增加 `health`。
- `rust/report_app/src/main.rs`：分发 health、按结论返回 0/1/2；调度日志调用带 kind/run 上下文。
- `rust/report_app/src/operations.rs`：调度日志字段、task plan 触发器与 health 任务类型。
- `scripts/daily_task_common.ps1`、`install_daily_task.ps1`、`check_daily_task.ps1`、`remove_daily_task.ps1`：多触发器解析与 health kind。
- `scripts/build_windows_release.ps1`：打包 `health_notify.ps1`。
- `tests/windows_operations.ps1`：daily 三触发器、health 任务、通知脚本测试。
- `docs/daily-report-operations.md`、`docs/windows-deployment.md`、`AGENTS.md`、`README.md`：命令与运维说明。

---

### 任务 1：`llm.invalid_response_retries` 配置项

**文件：**
- 修改：`rust/report_app/src/config.rs`（`Llm` 结构体与 `validate`）
- 修改：`rust/report_app/src/config.defaults.json`
- 修改：`config/settings.example.yaml`
- 测试：`rust/report_app/tests/cli_config.rs`

- [ ] **步骤 1：编写失败的测试**

在 `tests/cli_config.rs` 末尾添加：

```rust
#[test]
fn invalid_response_retries_defaults_and_is_bounded() {
    let (_dir, config) = test_config("paths:\n  work_dir: work\nllm:\n  context_window_tokens: 100000\n  verified_max_output_tokens: 8192\n");
    assert_eq!(config.llm.invalid_response_retries, 1);
    let (_dir, too_many) = test_config("llm:\n  invalid_response_retries: 5\n");
    assert!(too_many.validate().is_err());
    let (_dir, zero) = test_config("llm:\n  invalid_response_retries: 0\n");
    assert_eq!(zero.llm.invalid_response_retries, 0);
}
```

注意 `test_config` 返回的是 `(TempDir, AppConfig)`，且测试文件顶部已导入 `AppConfig`。

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test cli_config invalid_response_retries_defaults_and_is_bounded`
预期：编译失败，`Llm` 没有 `invalid_response_retries` 字段。

- [ ] **步骤 3：实现配置字段**

在 `config.rs` 的 `Llm` 结构体末尾（`max_strategy_changes` 之后）加：

```rust
#[serde(default = "default_invalid_response_retries")]
pub invalid_response_retries: u32,
```

文件内加：

```rust
fn default_invalid_response_retries() -> u32 { 1 }
```

在 `validate()` 的 llm 容量分支中追加条件：

```rust
|| llm.invalid_response_retries > 2
```

并在 `config.defaults.json` 的 `llm` 对象中加入 `"invalid_response_retries":1`。

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test cli_config invalid_response_retries_defaults_and_is_bounded`
预期：PASS。

- [ ] **步骤 5：更新示例配置**

在 `config/settings.example.yaml` 的 `llm` 节 `max_batches` 之后加两行注释与字段：

```yaml
  # 模型响应结构/引用无效时的额外重试次数；与 max_retries 共享单步骤总尝试上限。
  invalid_response_retries: 1
```

- [ ] **步骤 6：Commit**

```bash
git add rust/report_app/src/config.rs rust/report_app/src/config.defaults.json rust/report_app/tests/cli_config.rs config/settings.example.yaml
git commit -m "Add invalid response retry config"
```

---

### 任务 2：`MODEL_RESPONSE_INVALID` 有界重试

**文件：**
- 修改：`rust/report_app/src/model/mod.rs`（`execute` 重试循环）
- 测试：`rust/report_app/tests/model_execution.rs`

- [ ] **步骤 1：编写失败的测试**

在 `tests/model_execution.rs` 末尾添加（复用现有 `Harness`、`Script`、`response`、`facts`、`source`）：

```rust
fn invalid_body() -> HttpResponse {
    HttpResponse { status: 200, retry_after: None, body: serde_json::to_vec(&json!({"choices":[{"finish_reason":"stop","message":{"content":"{\"schema_version\":3,\"facts\":\"oops\"}"}}]})).unwrap() }
}

#[test]
fn invalid_response_is_retried_then_succeeds() {
    let h = Harness::new();
    let plan = h.plan(&[source("s1", "采购审批拟调整，超过五万元增加审批。")]);
    let network = Script::new(vec![Ok(invalid_body()), Ok(response(facts("s1")))]);
    let result = h.runner(&network).execute(&plan.batches[0]).unwrap();
    assert!(matches!(result.output, ai_daily_report::model::StepOutput::Facts(_)));
    let calls = h.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["status"], "failed");
    assert_eq!(calls[0]["error_code"], "MODEL_RESPONSE_INVALID");
    assert_eq!(calls[1]["status"], "succeeded");
}

#[test]
fn invalid_response_retry_is_bounded_by_attempt_limit() {
    let mut h = Harness::new();
    h.config.llm.invalid_response_retries = 2;
    let plan = h.plan(&[source("s1", "采购审批拟调整，超过五万元增加审批。")]);
    let network = Script::new(vec![Ok(invalid_body()), Ok(invalid_body()), Ok(invalid_body())]);
    let error = h.runner(&network).execute(&plan.batches[0]).unwrap_err();
    assert_eq!(error.code, "MODEL_RESPONSE_INVALID");
    assert_eq!(network.count(), 3, "每次发送都必须有账本记录");
}

#[test]
fn invalid_response_with_zero_retries_fails_after_one_call() {
    let mut h = Harness::new();
    h.config.llm.invalid_response_retries = 0;
    let plan = h.plan(&[source("s1", "采购审批拟调整，超过五万元增加审批。")]);
    let network = Script::new(vec![Ok(invalid_body())]);
    let error = h.runner(&network).execute(&plan.batches[0]).unwrap_err();
    assert_eq!(error.code, "MODEL_RESPONSE_INVALID");
    assert_eq!(network.count(), 1);
}
```

`StepOutput` 需在 `use ai_daily_report::model::{...}` 导入列表中加入。

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test model_execution invalid_response`
预期：FAIL，第一次无效响应后只调用 1 次或返回错误。

- [ ] **步骤 3：实现重试**

在 `model/mod.rs` 的 `execute` 中，`let mut step = self.effective(...)` 之前加：

```rust
let mut invalid_retries = 0u32;
```

把无效响应分支：

```rust
(error, false, None)
```

改为：

```rust
if error.code == "MODEL_RESPONSE_INVALID"
    && invalid_retries < self.llm.invalid_response_retries
{
    invalid_retries += 1;
    (error, true, None)
} else {
    (error, false, None)
}
```

保留其后的 `attempts >= self.llm.max_retries` 检查作为总尝试上限；`reserve_call` 的 `max_attempts` 不变。

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test model_execution invalid_response`
预期：3 个测试全部 PASS。

- [ ] **步骤 5：回归测试**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test model_execution`
预期：全部 PASS（现有截断/重放/并行用例不受影响）。

- [ ] **步骤 6：Commit**

```bash
git add rust/report_app/src/model/mod.rs rust/report_app/tests/model_execution.rs
git commit -m "Retry invalid model responses within the attempt limit"
```

---

### 任务 3：成稿容量拆分与单条降级

**文件：**
- 修改：`rust/report_app/src/model/plan.rs`（`compose_groups`、`retain_sources`、`ComposeGroup`）
- 修改：`rust/report_app/src/model/mod.rs`（`split_error`、`generate_with_workers`）
- 修改：`rust/report_app/src/model/contracts.rs`（`ComposedReport::pending_from`）
- 测试：`rust/report_app/tests/model_execution.rs`

- [ ] **步骤 1：编写失败的测试**

在 `tests/model_execution.rs` 末尾添加：

```rust
#[test]
fn compose_capacity_keeps_every_fact_covered() {
    use ai_daily_report::model::plan::ComposeGroup;
    let mut h = Harness::new();
    h.config.llm.max_request_utf8_bytes = 10000;
    let plan = h.plan(&[source("s1", "采购审批拟调整，超过五万元增加审批。")]);
    let facts: Vec<SelectedFact> = (0..6).map(|i| {
        let mut fact: SelectedFact = SelectedFact {
            fact_id: format!("batch_000:{i:03}"),
            pending: false,
            fact: serde_json::from_value(fact("s1")).unwrap(),
            matter_id: None,
            evidence: vec![],
        };
        fact.fact.detail = "采购审批调整需要保留金额档位与适用条件。".repeat(60);
        fact
    }).collect();
    let groups = plan.compose_groups(&h.config.llm, facts.clone()).unwrap();
    let mut covered = std::collections::BTreeSet::new();
    for group in &groups {
        if let ComposeGroup::Step(step) = group {
            covered.extend(step.facts.iter().map(|f| f.fact_id.clone()));
        }
    }
    for fact in &facts {
        assert!(covered.contains(&fact.fact_id), "事实未进入任何成稿分组: {}", fact.fact_id);
    }
    assert!(groups.len() > 1, "容量不足时应拆分");
}

#[test]
fn single_oversized_fact_degrades_to_pending() {
    use ai_daily_report::model::plan::ComposeGroup;
    let mut h = Harness::new();
    h.config.llm.max_request_utf8_bytes = 2048;
    let plan = h.plan(&[source("s1", "采购审批拟调整，超过五万元增加审批。")]);
    let mut one = SelectedFact {
        fact_id: "batch_000:000".into(),
        pending: false,
        fact: serde_json::from_value(fact("s1")).unwrap(),
        matter_id: None,
        evidence: vec![],
    };
    one.fact.detail = "采购审批调整。".repeat(200);
    let groups = plan.compose_groups(&h.config.llm, vec![one.clone()]).unwrap();
    let ComposeGroup::Degraded(degraded) = &groups[0] else { panic!("超大单条事实应降级") };
    assert!(degraded.pending);
    assert!(degraded.fact.uncertainty.as_deref().unwrap_or("").contains("容量"));
    let part = ai_daily_report::model::contracts::ComposedReport::pending_from(degraded, 3);
    let merged = ai_daily_report::model::contracts::ComposedReport::merge(vec![part], &[degraded.clone()]).unwrap();
    assert_eq!(merged.pending.len(), 1);
    assert_eq!(merged.pending[0].source_ids, vec!["s1".to_string()]);
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test model_execution compose_capacity`
预期：编译失败（`ComposeGroup`、`pending_from` 不存在）。

- [ ] **步骤 3：实现 `ComposeGroup` 与降级**

`model/plan.rs` 增加：

```rust
pub enum ComposeGroup {
    Step(PlannedStep),
    Degraded(SelectedFact),
}
```

把 `compose_groups` 返回类型改为 `Result<Vec<ComposeGroup>, ReportError>`：

- 首行单组快速路径改为 `return Ok(vec![ComposeGroup::Step(step)]);`
- 两处 `groups.push(self.compose(...)?)` 替换为 `self.push_group(&mut groups, std::mem::take(&mut pending), llm)?;`
- 删除末尾按 index 重编号的循环（`push_group` 已负责编号）。

新增方法：

```rust
fn push_group(&self, groups: &mut Vec<ComposeGroup>, mut facts: Vec<SelectedFact>, llm: &Llm) -> Result<(), ReportError> {
    match self.compose(llm, facts.clone()) {
        Ok(mut step) => { step.logical_step = format!("compose_{:03}", groups.len()); groups.push(ComposeGroup::Step(step)); Ok(()) }
        Err(error) if error.code == "MODEL_COMPOSE_CAPACITY_INSUFFICIENT" && facts.len() > 1 => {
            let middle = facts.len() / 2;
            let right = facts.split_off(middle);
            self.push_group(groups, facts, llm)?;
            self.push_group(groups, right, llm)
        }
        Err(error) if error.code == "MODEL_COMPOSE_CAPACITY_INSUFFICIENT" => {
            let fact = &mut facts[0];
            fact.pending = true;
            fact.fact.uncertainty = Some("成稿请求容量不足，本事实未纳入成稿依据。".into());
            groups.push(ComposeGroup::Degraded(facts.pop().unwrap()));
            Ok(())
        }
        Err(error) => Err(error),
    }
}
```

`retain_sources` 对 `StepKind::Compose` 额外移除 `archive` 键，减少请求体积：

```rust
for value in sources.values_mut() {
    if let Some(object) = value.as_object_mut() { object.remove("archive"); }
}
```

在 `model/contracts.rs` 的 `impl ComposedReport` 中加（`schema_version` 由调用方按计划版本传入：`step.version == 5` 时为 4，否则为 3）：

```rust
pub fn pending_from(fact: &SelectedFact, schema_version: u32) -> Self {
    Self {
        schema_version,
        overview: vec![],
        matters: vec![],
        pending: vec![CitedText {
            text: format!("{}：{}", fact.fact.topic, fact.fact.detail),
            fact_ids: vec![fact.fact_id.clone()],
            source_ids: fact.fact.source_ids.clone(),
        }],
    }
}
```

在 `model/mod.rs` 的 `split_error` 中加：

```rust
"MODEL_COMPOSE_CAPACITY_INSUFFICIENT" => Some("MODEL_COMPOSE_CAPACITY_INSUFFICIENT"),
```

并改写 `generate_with_workers` 的成稿循环：

```rust
let schema_version = if plan.batches.first().map(|batch| batch.version) == Some(5) { 4 } else { 3 };
for group in plan.compose_groups(self.llm, facts.clone())? {
    match group {
        ComposeGroup::Step(step) => {
            for result in self.execute_complete(&step)? {
                if let StepOutput::Compose(report) = &result.output { parts.push(report.clone()); }
                steps.push(result);
            }
        }
        ComposeGroup::Degraded(fact) => parts.push(ComposedReport::pending_from(&fact, schema_version)),
    }
}
```

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test model_execution compose_capacity`
预期：2 个测试 PASS。

- [ ] **步骤 5：回归成稿链路**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test model_execution`
预期：全部 PASS。

- [ ] **步骤 6：Commit**

```bash
git add rust/report_app/src/model/plan.rs rust/report_app/src/model/mod.rs rust/report_app/src/model/contracts.rs rust/report_app/tests/model_execution.rs
git commit -m "Split or degrade oversized composition groups"
```

---

### 任务 4：调度日志字段补齐

**文件：**
- 修改：`rust/report_app/src/operations.rs`（`append_scheduled_log`）
- 修改：`rust/report_app/src/main.rs`（调用点）
- 测试：`rust/report_app/tests/scheduled_log.rs`（新建）

- [ ] **步骤 1：编写失败的测试**

新建 `tests/scheduled_log.rs`：

```rust
use ai_daily_report::{config::AppConfig, operations::append_scheduled_log, store::Store, ReportError};
use chrono::{DateTime, Utc};
use serde_json::json;
use std::{collections::BTreeMap, fs, time::Instant};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-17T01:00:00Z").unwrap().with_timezone(&Utc)
}

#[test]
fn scheduled_log_records_kind_run_date_phase_and_duration() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("config")).unwrap();
    fs::create_dir_all(dir.path().join("data/db")).unwrap();
    fs::write(dir.path().join("config/settings.yaml"), "paths:\n  work_dir: work\n  data_dir: data\n  reports_dir: data/reports\n  db_dir: data/db\n  log_dir: logs\n").unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    let store = Store::open(&dir.path().join("data/db/reports.sqlite3"), now()).unwrap();
    let context: ai_daily_report::types::RunContext =
        serde_json::from_str(include_str!("fixtures/context-v2.json")).unwrap();
    let generation = "a".repeat(32);
    store.create(&context, "report", &generation, None, now()).unwrap();
    let started = Instant::now();
    let ok = Ok(json!({"run_id":context.run_id,"target_date":"2026-10-16","action":"generated","quality":"partial"}));
    append_scheduled_log(&config, "daily", started.elapsed().as_millis() as u64, Some(&context.run_id), Some("2026-10-16"), &ok).unwrap();
    let failed = Err(ReportError::new("MODEL_RESPONSE_INVALID", "无效响应"));
    append_scheduled_log(&config, "daily", 5, Some(&context.run_id), Some("2026-10-16"), &failed).unwrap();
    let text = fs::read_to_string(dir.path().join("logs/scheduled-2026-10-17.jsonl")).unwrap();
    let lines: Vec<serde_json::Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines[0]["kind"], "daily");
    assert_eq!(lines[0]["run_id"], context.run_id);
    assert_eq!(lines[0]["target_date"], "2026-10-16");
    assert_eq!(lines[0]["status"], "succeeded");
    assert!(lines[0]["duration_ms"].as_u64().is_some());
    assert_eq!(lines[1]["status"], "failed");
    assert_eq!(lines[1]["error_code"], "MODEL_RESPONSE_INVALID");
    assert_eq!(lines[1]["phase"], "created", "失败记录应带运行阶段");
}
```

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test scheduled_log`
预期：编译失败（`append_scheduled_log` 参数不匹配）。

- [ ] **步骤 3：实现日志字段**

`operations.rs` 中把 `append_scheduled_log` 改为：

```rust
pub fn append_scheduled_log(
    config: &AppConfig,
    kind: &str,
    duration_ms: u64,
    run_id: Option<&str>,
    target_date: Option<&str>,
    result: &Result<Value, ReportError>,
) -> Result<(), ReportError>
```

逻辑：

- `Ok(value)`：写 `{"at","kind","status":"succeeded","run_id":value["run_id"],"target_date":value["target_date"],"action":value["action"],"quality":value["quality"],"duration_ms"}`。
- `Err(error)`：`run_id`/`target_date` 取参数；用只读连接查 `report_runs` 的 `phase`（`rev` 传入 run_id 时），查询失败写 `"unknown"`；字段为 `{"at","kind","status":"failed","run_id","target_date","phase","error_code","duration_ms"}`。

新增私有函数：

```rust
fn run_phase(config: &AppConfig, run_id: &str) -> String
```

用 `Connection::open_with_flags(..., SQLITE_OPEN_READ_ONLY)` 查询 `SELECT phase FROM report_runs WHERE run_id=?`，任何错误返回 `"unknown"`。

`main.rs` 两处调用改为：

```rust
let started = std::time::Instant::now();
// ... 执行 result
if scheduled { ai_daily_report::operations::append_scheduled_log(&config, "daily", started.elapsed().as_millis() as u64, None, None, &result)?; }
```

失败时 run_id 可在 reports 分支通过 `request.context.run_id` 传入；collect 分支传 `None`/`None`，kind 为 `"collect"`。

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test scheduled_log`
预期：PASS。

- [ ] **步骤 5：确认现有 CLI 测试不受影响**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test cli_runtime`
预期：PASS。

- [ ] **步骤 6：Commit**

```bash
git add rust/report_app/src/operations.rs rust/report_app/src/main.rs rust/report_app/tests/scheduled_log.rs
git commit -m "Record scheduled run context in JSONL logs"
```

---

### 任务 5：`health` 检查核心

**文件：**
- 创建：`rust/report_app/src/health.rs`
- 修改：`rust/report_app/src/lib.rs`（`pub mod health;`）
- 测试：`rust/report_app/tests/health.rs`（新建）

- [ ] **步骤 1：编写失败的测试**

新建 `tests/health.rs`，包含以下 4 个测试（测试内直接建库、写文件，不调用模型）：

```rust
use ai_daily_report::{config::AppConfig, health, store::Store};
use chrono::{DateTime, Utc};
use std::{collections::BTreeMap, fs};

fn now() -> DateTime<Utc> { DateTime::parse_from_rfc3339("2026-10-18T03:00:00Z").unwrap().with_timezone(&Utc) }

fn setup() -> (tempfile::TempDir, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("config")).unwrap();
    fs::create_dir_all(dir.path().join("work")).unwrap();
    let work = dir.path().join("work").display().to_string().replace('\\', "/");
    let data = dir.path().join("data").display().to_string().replace('\\', "/");
    fs::write(dir.path().join("config/settings.yaml"), format!("paths:\n  work_dir: '{work}'\n  data_dir: '{data}'\n  reports_dir: '{data}/reports'\n  db_dir: '{data}/db'\n  log_dir: '{data}/logs'\n")).unwrap();
    let config = AppConfig::load_with_env(dir.path(), None, &BTreeMap::new()).unwrap();
    fs::create_dir_all(&config.paths.db_dir).unwrap();
    fs::create_dir_all(&config.paths.log_dir).unwrap();
    Store::open(&config.paths.db_dir.join("reports.sqlite3"), now()).unwrap();
    (dir, config)
}

#[test]
fn health_reports_error_when_daily_report_is_missing() {
    let (_dir, config) = setup();
    let report = health::check(&config, chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(), now()).unwrap();
    assert_eq!(report.status, "unhealthy");
    assert!(report.checks.iter().any(|c| c.code == "REPORT_NOT_PUBLISHED" && c.level == "error"));
    assert_eq!(health::exit_code(&report), 2);
}

#[test]
fn health_reports_database_failures_as_unhealthy() {
    let (dir, config) = setup();
    fs::write(config.paths.db_dir.join("reports.sqlite3"), b"not a database").unwrap();
    let report = health::check(&config, chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(), now()).unwrap();
    assert_eq!(report.status, "unhealthy");
    assert!(report.checks.iter().any(|c| c.code == "DATABASE_INTEGRITY" && c.level == "error"));
    let _ = dir;
}

#[test]
fn health_baseline_window_is_not_a_collection_gap() {
    let (_dir, config) = setup();
    use ai_daily_report::archive::store::ArchiveStore;
    let store = ArchiveStore::open(&config, true).unwrap();
    store.conn.execute("INSERT INTO metadata(key,value) VALUES('baseline','2026-10-17T05:06:34+00:00')", []).unwrap();
    let start = DateTime::parse_from_rfc3339("2026-10-16T16:00:00+00:00").unwrap().with_timezone(&Utc);
    let end = DateTime::parse_from_rfc3339("2026-10-17T16:00:00+00:00").unwrap().with_timezone(&Utc);
    let baseline = DateTime::parse_from_rfc3339("2026-10-17T05:06:34+00:00").unwrap().with_timezone(&Utc);
    let mut at = start;
    while at <= end {
        if at >= baseline - chrono::Duration::minutes(6) {
            store.conn.execute("INSERT INTO collections(id,started_at,finished_at,status,baseline) VALUES(?,?,?,'complete',0)", rusqlite::params![format!("c{}", at.timestamp()), at.to_rfc3339(), at.to_rfc3339()]).unwrap();
        }
        at = at + chrono::Duration::minutes(30);
    }
    let report = health::check(&config, chrono::NaiveDate::from_ymd_opt(2026, 10, 17).unwrap(), now()).unwrap();
    assert!(!report.checks.iter().any(|c| c.code == "COLLECTION_GAP" && c.level == "error"), "{:?}", report.checks);
}
```

注意：`health_baseline_window_is_not_a_collection_gap` 断言基线前时段（10-17 00:00–05:06 上海时间）不计为采集缺口。

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test health`
预期：编译失败（`ai_daily_report::health` 不存在）。

- [ ] **步骤 3：实现 `health.rs`**

公开接口：

```rust
pub struct HealthCheck { pub code: &'static str, pub level: &'static str, pub message: String, pub evidence: serde_json::Value }
pub struct TaskNames { pub daily: String, pub collect: String, pub health: String }
pub struct HealthReport { pub status: &'static str, pub target_date: String, pub checks: Vec<HealthCheck>, pub task_names: TaskNames, pub log_dir: String, pub generated_at: String }
pub fn check(config: &AppConfig, target: NaiveDate, now: DateTime<Utc>) -> Result<HealthReport, ReportError>
pub fn render(report: &HealthReport, format: crate::archive::cli::Format) -> String
pub fn exit_code(report: &HealthReport) -> i32
```

实现要点：

- 报告库：`rusqlite::Connection::open_with_flags(path, SQLITE_OPEN_READ_ONLY)`；文件不存在或打开失败 → `DATABASE_INTEGRITY` error。执行 `PRAGMA quick_check`，结果非 `ok` → error。
- 归档库：文件不存在 → `COLLECTION_GAP` error（“归档库不存在，无法核对采集”）；用只读连接执行 `PRAGMA quick_check`；读 `metadata.baseline`。
- 采集连续性：从 `collections` 取目标日（started_at 上海日期）内 `status IN ('complete','partial')` 且 `finished_at` 非空的记录，以 baseline 目标日起始较大者为起点、目标日结束与 `now` 较小者为终点；30 分钟间隔 ±60 秒判定；缺口 → `COLLECTION_GAP` error，detail 列前 5 段。基线日晚于窗口起点时不得把基线前时段计为缺口。
- 日报发布：`report_runs` 中 `target_date=? AND run_kind='report' AND status='succeeded' AND phase='published'` 最近一条；无 → `REPORT_NOT_PUBLISHED` error。有则校验 `output_path` 文件存在且 `sha256(文件字节)==expected_sha256`，不符 → `REPORT_HASH_MISMATCH` error。
- 运行状态：待续运行 `store.pending` 等价 SQL 计数 >0，或近 24 小时 `status='failed'` 计数 >0 → `RUN_ATTENTION` warning。
- 解析队列：归档库 `extractions` 中 `status IN ('queued','running')` 计数 >0 → `PARSE_QUEUE` warning。
- 磁盘：`#[cfg(windows)]` 用 `windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW` 取 `data_dir` 所在卷；<20 GiB warning、<5 GiB error（`DISK_SPACE`）；非 Windows 跳过。
- `status`：任一 error → `unhealthy`；否则任一 warning → `warning`；否则 `healthy`。
- `task_names`：`daily=config.schedule.task_name`、`collect=task_name + " Collect"`、`health=task_name + " Health"`；`log_dir` 为 `display_path(&config.paths.log_dir)`。
- `render`：json 用 `serde_json::to_string_pretty`；text 每行 `[level] code: message`，首行 `status target_date`。
- `exit_code`：healthy 0、warning 1、unhealthy 2。
- `HealthCheck`、`TaskNames`、`HealthReport` 均派生 `serde::Serialize`，字段名保持 snake_case（脚本依赖 `task_names.daily/collect/health`、`log_dir`、`checks[].code/level/message`）。

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test health`
预期：3 个测试 PASS（第 1 个无归档库时 `COLLECTION_GAP` 与 `REPORT_NOT_PUBLISHED` 同时出现，断言仍成立）。

- [ ] **步骤 5：Commit**

```bash
git add rust/report_app/src/health.rs rust/report_app/src/lib.rs rust/report_app/tests/health.rs
git commit -m "Add read-only health checks"
```

---

### 任务 6：`health` CLI 与退出码

**文件：**
- 修改：`rust/report_app/src/cli.rs`（`Health` 命令与参数）
- 修改：`rust/report_app/src/main.rs`（分发与退出码）
- 测试：`rust/report_app/tests/cli_runtime.rs`

- [ ] **步骤 1：编写失败的测试**

在 `tests/cli_runtime.rs` 末尾加：

```rust
#[test]
fn health_command_reports_unhealthy_config_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    fs::create_dir(&config).unwrap();
    let work = dir.path().join("work");
    fs::create_dir(&work).unwrap();
    let settings = serde_json::json!({"paths":{
        "work_dir":work,"data_dir":dir.path().join("data"),"db_dir":dir.path().join("db"),
        "reports_dir":dir.path().join("reports"),"log_dir":dir.path().join("logs")}});
    fs::write(config.join("settings.yaml"), settings.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ai-daily-report"))
        .arg("--config-dir").arg(&config)
        .args(["health", "--format", "text"])
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "缺少报告库时应返回 unhealthy");
    assert!(String::from_utf8_lossy(&output.stdout).contains("unhealthy"));
}
```

`cli_runtime.rs` 顶部已导入 `std::process::Command`、`fs` 与 `serde_json`，无需新增依赖。

- [ ] **步骤 2：运行测试验证失败**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test cli_runtime health_command`
预期：FAIL，clap 报未知子命令 `health`。

- [ ] **步骤 3：实现 CLI**

`cli.rs` 的 `Command` 枚举加：

```rust
/// 只读检查任务、采集、日报发布、磁盘与数据库状态
Health(HealthArgs),
```

并加：

```rust
#[derive(Debug, Args)]
pub struct HealthArgs {
    #[arg(long, value_parser = types::parse_date)]
    pub target_date: Option<NaiveDate>,
    #[arg(long, value_enum, default_value = "json")]
    pub format: crate::archive::cli::Format,
    #[arg(long, help = "写入调度日志；供健康任务使用")]
    pub scheduled: bool,
}
```

`main.rs` 的 `execute` 中在 `Command::Task { .. }` 之前加：

```rust
Command::Health(args) => {
    let target = args.target_date.unwrap_or_else(|| {
        (now.with_timezone(&ai_daily_report::types::shanghai()) - chrono::Days::new(1)).date_naive()
    });
    let started = std::time::Instant::now();
    let report = ai_daily_report::health::check(&config, target, now)?;
    if args.scheduled {
        let value = serde_json::to_value(&report)?;
        ai_daily_report::operations::append_scheduled_log(&config, "health", started.elapsed().as_millis() as u64, None, Some(&target.to_string()), &Ok(value))?;
    }
    println!("{}", ai_daily_report::health::render(&report, args.format));
    let code = ai_daily_report::health::exit_code(&report);
    if code != 0 { std::process::exit(code); }
    Ok(())
}
```

`HealthReport` 需实现 `Serialize`（加 `#[derive(serde::Serialize)]` 及字段 `snake_case`）。

- [ ] **步骤 4：运行测试验证通过**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test cli_runtime health_command`
预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/report_app/src/cli.rs rust/report_app/src/main.rs rust/report_app/tests/cli_runtime.rs
git commit -m "Expose health checks through the CLI"
```

---

### 任务 7：daily 任务兜底触发（09:30 / 10:30）

**文件：**
- 修改：`rust/report_app/src/operations.rs`（`task_plan` 触发器渲染、`task_plan_kind`）
- 修改：`scripts/daily_task_common.ps1`（多触发器回读）
- 修改：`tests/windows_operations.ps1`

- [ ] **步骤 1：编写失败的 PowerShell 契约测试**

`tests/windows_operations.ps1` 在 `$plan = Get-ReportTaskPlan ...` 之后加：

```powershell
$dailyFields = Get-ReportTaskXmlFields -Xml $plan.xml
Assert-Test ($dailyFields['calendar_count'] -eq 3) 'Daily plan must have three calendar triggers'
Assert-Test ($dailyFields['calendar_times'] -eq '09:00;09:30;10:30') ('Daily trigger times: ' + $dailyFields['calendar_times'])
```

- [ ] **步骤 2：运行 PowerShell 测试验证失败**

先构建：`cargo build --manifest-path rust/Cargo.toml -p ai-daily-report`
再运行：`powershell -NoProfile -ExecutionPolicy Bypass -File tests/windows_operations.ps1 -Program rust/target/debug/ai-daily-report.exe -OutputDirectory .artifacts/windows-operations-dev`
预期：FAIL，daily 只有 1 个 calendar trigger。

- [ ] **步骤 3：实现多触发器**

`operations.rs`：

- 新增 `fn trigger_times(local_time: &str) -> Result<[String; 3], ReportError>`：解析 `HH:MM`，返回 `[local_time, +30min, +90min]`（用 `chrono::NaiveTime`）。
- `task_plan` 的 XML 模板把 `<Triggers>` 改为由 `{triggers}` 占位填充；daily 渲染三个 `<CalendarTrigger id="DailyShanghai|DailyCatchup30|DailyCatchup90">`。
- `task_plan_kind`：`daily` 与 `collect` 走新渲染；`collect` 的触发器由字符串替换改为同一渲染函数传入单个 00:00 + Repetition PT30M。

`daily_task_common.ps1` 在 `Get-ReportTaskXmlFields` 内加：

```powershell
$calendar = @($document.SelectNodes('/t:Task/t:Triggers/t:CalendarTrigger', $ns))
$fields['calendar_count'] = $calendar.Count
$fields['calendar_times'] = (($calendar | ForEach-Object {
    ([DateTimeOffset]::Parse($_.SelectSingleNode('t:StartBoundary', $ns).InnerText)).ToOffset([TimeSpan]::FromHours(8)).ToString('HH:mm')
}) | Sort-Object) -join ';'
```

注意：`SelectSingleNode` 需要命名空间管理器；`calendar_times` 排序后比较，避免 Windows 回读时调整触发器顺序；保留现有单节点字段以兼容 collect 测试。

- [ ] **步骤 4：运行 PowerShell 测试验证通过**

运行：`powershell -NoProfile -ExecutionPolicy Bypass -File tests/windows_operations.ps1 -Program rust/target/debug/ai-daily-report.exe -OutputDirectory .artifacts/windows-operations-dev-02`
预期：PASS，输出 `checks` 数增加且 `real_tasks_changed` 为 0。

- [ ] **步骤 5：复跑既有幂等回归**

运行：`cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --test archive archive_dry_run_keeps_frozen_identity_and_existing_report_does_not_create_a_run`
预期：PASS；该测试已断言重复执行返回 `action=existing`、`model_calls=0`，作为兜底触发不重复发布的锁定证据。

- [ ] **步骤 6：Commit**

```bash
git add rust/report_app/src/operations.rs scripts/daily_task_common.ps1 tests/windows_operations.ps1
git commit -m "Add catch-up triggers to the daily task"
```

---

### 任务 8：`health` 任务类型

**文件：**
- 修改：`rust/report_app/src/operations.rs`（`task_plan_kind`、health XML）
- 修改：`rust/report_app/src/cli.rs`（kind 可选值）
- 修改：`scripts/daily_task_common.ps1`、`install_daily_task.ps1`、`check_daily_task.ps1`、`remove_daily_task.ps1`
- 修改：`tests/windows_operations.ps1`

- [ ] **步骤 1：编写失败的 PowerShell 契约测试**

在 `tests/windows_operations.ps1` 中加：

```powershell
$healthPlan = Get-ReportTaskPlan -ProjectRoot (Split-Path -Parent $Program) -ConfigDirectory (Join-Path $root 'config') -Kind health
Assert-Test ($healthPlan.kind -eq 'health') 'Health plan kind missing'
Assert-Test ($healthPlan.task_name -like '* Health') 'Health task name missing'
$healthFields = Get-ReportTaskXmlFields -Xml $healthPlan.xml
Assert-Test ($healthFields['calendar_times'] -eq '11:00;23:00') ('Health trigger times: ' + $healthFields['calendar_times'])
Assert-Test ($healthFields['/t:Task/t:Actions/t:Exec/t:Command'] -match 'powershell') 'Health action must use PowerShell'
Assert-Test ($healthFields['/t:Task/t:Actions/t:Exec/t:Arguments'] -match 'health_notify.ps1') 'Health action must call health_notify.ps1'
$script:task = $null; $script:taskXml = ''
$script:registered = 0
Install-ReportDailyTask -Plan $healthPlan -Apply | Out-Null
Assert-Test ($script:registered -eq 1) 'Health task registration failed without credential check'
```

同时把 `Get-ReportTaskPlan` 的 `ValidateSet` 更新为 `"daily","collect","health"`。

- [ ] **步骤 2：运行验证失败**

运行：`powershell -NoProfile -ExecutionPolicy Bypass -File tests/windows_operations.ps1 -Program rust/target/debug/ai-daily-report.exe -OutputDirectory .artifacts/windows-operations-dev-03`
预期：FAIL，`task plan --kind health` 被 clap 拒绝。

- [ ] **步骤 3：实现 health 任务计划**

`cli.rs`：`value_parser=["daily","collect","health"]`。
`operations.rs`：

- 新增 `fn task_plan_health(config, sid, now) -> Result<Value, ReportError>`：
  - `name = format!("{} Health", config.schedule.task_name)`；
  - 触发器：`11:00`、`23:00` 两个 `<CalendarTrigger>`；
  - action：`<Command>powershell.exe</Command>`，参数 `-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "<program_dir>/scripts/health_notify.ps1" -Program "<program_dir>/ai-daily-report.exe" -ConfigDirectory "<config_dir>"`（用现有 `xml()` 转义）；
  - Settings：`MultipleInstancesPolicy IgnoreNew`、`ExecutionTimeLimit PT10M`、`StartWhenAvailable false`、无 `RestartOnFailure`；
  - 返回与 `task_plan` 相同结构的 JSON（`kind:"health"`，另加 `triggers` 数组）。
- `task_plan_kind` 增加 `"health"` 分支。

`install_daily_task.ps1`：health 跳过 `doctor` 与模型检查；Apply 前仅校验目录存在。
`daily_task_common.ps1`：`Install-ReportDailyTask` 中 `$health = $Plan.kind -eq 'health'`，与 collect 一样跳过凭据/模型检查。

- [ ] **步骤 4：运行验证通过**

运行：`powershell -NoProfile -ExecutionPolicy Bypass -File tests/windows_operations.ps1 -Program rust/target/debug/ai-daily-report.exe -OutputDirectory .artifacts/windows-operations-dev-04`
预期：PASS。

- [ ] **步骤 5：Commit**

```bash
git add rust/report_app/src/operations.rs rust/report_app/src/cli.rs scripts/daily_task_common.ps1 scripts/install_daily_task.ps1 scripts/check_daily_task.ps1 scripts/remove_daily_task.ps1 tests/windows_operations.ps1
git commit -m "Add the health scheduled task kind"
```

---

### 任务 9：`health_notify.ps1` 与发布打包

**文件：**
- 创建：`scripts/health_notify.ps1`
- 修改：`scripts/build_windows_release.ps1`
- 修改：`tests/windows_operations.ps1`

- [ ] **步骤 1：编写失败的脚本测试**

在 `tests/windows_operations.ps1` 末尾加：

```powershell
$healthConfig = Join-Path $root 'health-config'
[IO.Directory]::CreateDirectory($healthConfig) | Out-Null
$healthData = (Join-Path $root 'health-data').Replace('\','/')
$healthWork = (Join-Path $root 'work').Replace('\','/')
[IO.File]::WriteAllText((Join-Path $healthConfig 'settings.yaml'), "paths:`n  work_dir: '$healthWork'`n  data_dir: '$healthData'`n  reports_dir: '$healthData/reports'`n  db_dir: '$healthData/db'`n  log_dir: '$healthData/logs'`n", [Text.UTF8Encoding]::new($false))
& (Join-Path $repo 'scripts/health_notify.ps1') -Program $Program -ConfigDirectory $healthConfig -NoNotify | Out-Null
$healthFile = Join-Path $healthData 'logs/health-' + (Get-Date -Format 'yyyy-MM-dd') + '.json'
Assert-Test (Test-Path -LiteralPath $healthFile) 'Health file was not written'
$healthJson = Get-Content -LiteralPath $healthFile -Raw -Encoding UTF8 | ConvertFrom-Json
Assert-Test ($healthJson.status -ne 'healthy') 'Empty data directory should not be healthy'
Assert-Test (@($healthJson.tasks).Count -eq 3) 'Health report must include all three task readbacks'
```

- [ ] **步骤 2：运行验证失败**

运行：`powershell -NoProfile -ExecutionPolicy Bypass -File tests/windows_operations.ps1 -Program rust/target/debug/ai-daily-report.exe -OutputDirectory .artifacts/windows-operations-dev-05`
预期：FAIL，脚本不存在。

- [ ] **步骤 3：实现脚本**

`scripts/health_notify.ps1`（PowerShell 5.1 兼容，不用 `??`/三元）：

```powershell
[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$Program, [Parameter(Mandatory = $true)][string]$ConfigDirectory, [switch]$NoNotify)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$text = & $Program --config-dir $ConfigDirectory health --scheduled | Out-String
$exit = $LASTEXITCODE
$report = $text | ConvertFrom-Json
$zone = [TimeZoneInfo]::FindSystemTimeZoneById('China Standard Time')
$tasks = @()
foreach ($name in @($report.task_names.daily, $report.task_names.collect, $report.task_names.health)) {
    $task = Get-ScheduledTask -TaskName $name -ErrorAction SilentlyContinue
    if ($null -eq $task) { $tasks += [pscustomobject]@{ name = $name; exists = $false; state = ''; last_result = $null; next_run = '' }; continue }
    $info = Get-ScheduledTaskInfo -TaskName $name
    $tasks += [pscustomobject]@{ name = $name; exists = $true; state = [string]$task.State;
        last_result = $info.LastTaskResult;
        next_run = [TimeZoneInfo]::ConvertTime($info.NextRunTime, $zone).ToString('yyyy-MM-dd HH:mm:ss') }
}
$report | Add-Member -NotePropertyName tasks -NotePropertyValue $tasks
$date = Get-Date -Format 'yyyy-MM-dd'
$logDir = [string]$report.log_dir
[IO.Directory]::CreateDirectory($logDir) | Out-Null
$healthFile = Join-Path $logDir ('health-' + $date + '.json')
$report | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $healthFile -Encoding UTF8
$unhealthy = $report.status -ne 'healthy'
$changed = $exit -ne 0 -or @($tasks | Where-Object { -not $_.exists -or $_.last_result -ne 0 }).Count -gt 0
$stateFile = Join-Path $logDir 'health-notify-state.json'
$state = $null
if (Test-Path -LiteralPath $stateFile) { $state = Get-Content -LiteralPath $stateFile -Raw -Encoding UTF8 | ConvertFrom-Json }
$codes = @($report.checks | Where-Object { $_.level -ne 'ok' } | ForEach-Object { $_.code }) -join ','
$already = ($null -ne $state -and $state.date -eq $date -and $state.codes -eq $codes)
if (($unhealthy -or $changed) -and -not $already) {
    $message = '目标日 ' + $report.target_date + '：' + (@($report.checks | Where-Object { $_.level -ne 'ok' } | ForEach-Object { $_.message }) -join '；')
    if (-not $NoNotify) {
        Add-Type -AssemblyName System.Windows.Forms
        Add-Type -AssemblyName System.Drawing
        $icon = New-Object System.Windows.Forms.NotifyIcon
        $icon.Icon = [System.Drawing.SystemIcons]::Warning
        $icon.Visible = $true
        $icon.ShowBalloonTip(10000, 'AI Daily Report 健康异常', $message, [System.Windows.Forms.ToolTipIcon]::Warning)
        Start-Sleep -Seconds 6
        $icon.Dispose()
    }
    [ordered]@{ date = $date; codes = $codes; status = $report.status } | ConvertTo-Json | Set-Content -LiteralPath $stateFile -Encoding UTF8
    Add-Content -LiteralPath (Join-Path $logDir 'health-notify.log') -Value ($date + ' ' + $codes)
}
if ($unhealthy) { exit 1 }
exit 0
```

- [ ] **步骤 4：加入发布包**

`scripts/build_windows_release.ps1` 的脚本复制列表加入 `'health_notify.ps1'`。

- [ ] **步骤 5：运行验证通过**

运行：`powershell -NoProfile -ExecutionPolicy Bypass -File tests/windows_operations.ps1 -Program rust/target/debug/ai-daily-report.exe -OutputDirectory .artifacts/windows-operations-dev-06`
预期：PASS。

- [ ] **步骤 6：Commit**

```bash
git add scripts/health_notify.ps1 scripts/build_windows_release.ps1 tests/windows_operations.ps1
git commit -m "Add health notification script to the release"
```

---

### 任务 10：文档与示例更新

**文件：**
- 修改：`docs/daily-report-operations.md`、`docs/windows-deployment.md`、`AGENTS.md`、`README.md`

- [ ] **步骤 1：更新日报操作文档**

在 `docs/daily-report-operations.md` 增加“健康巡检”一节：`health` 用法、6 项检查、0/1/2 退出码、健康任务 11:00/23:00、健康文件与通知去重、`--scheduled` 只写调度日志。

- [ ] **步骤 2：更新 Windows 部署文档**

在 `docs/windows-deployment.md` 增加：daily 任务 09:00/09:30/10:30 三次触发；health 任务动作、安装/检查/卸载命令；`health_notify.ps1` 随包发布；健康文件位置。

- [ ] **步骤 3：更新 AGENTS.md 与 README**

`AGENTS.md` 的命令段落加一行 `health`；README 的“使用”加一条健康检查示例。

- [ ] **步骤 4：校验**

运行：`git diff --stat` 与人工复读；确认没有把历史计划/归档文档的“当前”叙述改成新状态（`docs/current-state.md` 留到授权部署后再改）。

- [ ] **步骤 5：Commit**

```bash
git add docs/daily-report-operations.md docs/windows-deployment.md AGENTS.md README.md
git commit -m "Document health checks and catch-up triggers"
```

---

### 任务 11：发布与上线（需单独授权，不自动执行）

**文件：**
- 不修改代码；执行既有发布与任务脚本。

- [ ] **步骤 1：门禁全量验证**

```bash
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked
powershell -NoProfile -ExecutionPolicy Bypass -File tests/windows_operations.ps1 -Program rust/target/debug/ai-daily-report.exe -OutputDirectory .artifacts/windows-operations-final
```

预期：全部通过；PS 输出 `status=passed`、`real_tasks_changed=0`。

- [ ] **步骤 2：构建新版本包**

```bash
scripts/build_windows_release.ps1 -OutputDirectory .releases/rust-20261018-reliability-01 -ReleaseVersion rust-20261018-reliability-01
scripts/verify_windows_release.ps1 -BundleDirectory .releases/rust-20261018-reliability-01
```

- [ ] **步骤 3：授权后执行模型检查与任务更新**

- 用户授权后运行 `doctor --check-model`（1 次真实模型请求），因为 `llm` 配置指纹已变化。
- 先 `install_daily_task.ps1`/`install_daily_task.ps1 -Kind health` 生成计划并保存原任务 XML，用户确认后 `-Apply`。
- 注册后回读比对；不一致则禁用任务并回退。

- [ ] **步骤 4：受控失败演练与观察**

- 用隔离任务名演练一次“首次失败 → 09:30 续跑”，确认无重复发布。
- 连续 2 个自然日核对 09:00 触发、健康任务输出、健康文件与通知去重、采集连续性。
- 验收证据与最终状态写入 `docs/current-state.md`；未获得授权前不应执行本任务任何步骤。

---

## 自检记录

- 规格覆盖：A1→任务 2；A2→任务 3；A3→任务 7；A4→任务 5、6；A5→任务 8、9；A6→任务 4；配置与文档→任务 1、10；部署→任务 11。
- 类型一致性：`ComposeGroup`（任务 3）在 `plan.rs` 定义、`model/mod.rs` 使用；`HealthReport/HealthCheck/TaskNames`（任务 5）在任务 6 的 CLI 与任务 9 的脚本中字段名一致（`task_names.daily/collect/health`、`log_dir`、`checks[].code/level/message`）。
- 无占位符；未决事项仅任务 11 的授权门禁（用户明确要求“全部做”，部署仍按 AGENTS 单项授权）。
