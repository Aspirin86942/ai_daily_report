# 架构降熵决策记录（ADR）与待确认事项

本文档记录分支 `refactor/arch-deentropy-20261129` 中的关键架构收敛决策及 Deletion Test 依据。

## ADR-001：消除 `daily_parsers` 中的 `contract <-> settings` 双向环

- **背景**：`rust/daily_parsers/src/settings/mod.rs` 重导出 `crate::contract::SourceGuardKind`，而 `contract/mod.rs` 依赖 `settings` 中的校验原语与解析配置结构，构成 2 模块 SCC。
- **决策**：将 `SourceGuardKind` 定义迁移至 `settings/mod.rs`（作为扫描源校验基础枚举），在 `contract/mod.rs` 中保留 `pub use crate::settings::SourceGuardKind;`。
- **Deletion Test**：移除 `settings -> contract` 这条仅服务重导出的反向边后，`contract`、`office`、`text`、`pdf_wire` 对 `settings` 形成单向 DAG，消除循环依赖且零外部签名破坏。
- **兼容影响**：`daily_parsers::contract::SourceGuardKind` 与 `daily_parsers::settings::SourceGuardKind` 均保持可用；同步通过 `tests/test_parser_content_identity.py` 验证构建身份。

## ADR-002：提取 Layer-0 `infra` 叶模块并收口 `types` 系统常量

- **背景**：`snapshots`、`store::locking`、`maintenance::files`、`operations::display_path`、`processing_cache::normalized`、`reports::data::sha256`、`health::quick_check_yielding` 等通用基础设施分散在各业务模块中，引发了 `daily_report` 12 模块互锁成环，且同一语义存在 2–5 份平行实现。
- **决策**：建立严格零内部依赖（仅允许 `crate::ReportError` 与外部 crate）的 `daily_report::infra`（`fs`、`json`、`locking`、`sqlite`）；将通用系统标识/时区/数据库文件名/配额常量收敛到 `types.rs`，在 `AppConfig` 提供 `reports_db_path()` 等规范路径访问器，并删除 `maintenance/files.rs`。
- **兼容边界**：`snapshots.rs` 与 `store/locking.rs` 仅为外部集成测试保留轻量 `pub use`（内部源码禁止回指）；`scripts/health_notify.ps1` 依赖 `logs/.health-publish.lock` 的 OS `File::try_lock()` 共享模型，因此 `.health-publish.lock` 不迁入 `ReportLock`。

## ADR-003：打破 `daily_report` 剩余模块环（12 模块 SCC -> 0 环）

- **背景**：提取 `infra` 后，仍有三条环边阻碍严格分层：`processing_cache -> config`、`reports <-> evidence`、`reports <-> model`（其中 `model/contracts.rs` 仅 5 行）。
- **决策**：
  1. `ProcessingSession::acquire` 直接接收 `data_dir: &Path`（迁移唯一调用方 `run.rs` 与测试 `personal_pipeline.rs`），彻底切断 `processing_cache` 对 `config` 的依赖。
  2. 将 `EvidenceRef` 数据结构从 `evidence.rs` 移入 `reports/data.rs`（`evidence.rs` 保留公开 `pub use` 给外部测试），消除 `reports -> evidence`。
  3. 删除 5 行的浅层文件 `model/contracts.rs`，将其常量/错误辅助收口入 `model/mod.rs`，在 `reports/period.rs` 使用局部辅助消除 `reports -> model`。
- **Deletion Test**：删除 `model/contracts.rs` 与 `processing_cache -> config` 转发依赖后，复杂性并未扩散到调用方，反而使每一层的构建测试均可独立于上层存在。

## ADR-004：在 `store::queries` 收口 `reports.sqlite3` 元数据键与只读查询

- **背景**：`health.rs`、`personal_recovery.rs`、`operations.rs` 与 `maintenance/retention.rs` 各自直接对 `reports.sqlite3` 书写原始 SQL 与 `personal_run:` / `personal_calls:` / `personal_summary:` 字符串键；其中“已发布有效个人日报”的 JSON 判定 SQL 谓词在 `health.rs` 内重复出现两次。
- **决策**：在 `store/queries.rs` 与 `store/personal.rs` 建立唯一的元数据键常量与只读查询函数（参数接收调用方的 `&Connection` 或只读事务），保持连接与事务边界清晰的同时实现 Single Source of Truth 与 Locality of Change。

## ADR-005：按业务职责重构 `operations`、`config` 与超长流程函数

- **背景**：`operations.rs`（477 行）混杂 Windows 任务 XML 计划、环境/模型自检 `doctor`、定时 JSONL 日志、`display_path` 四类职责；`config.rs`（753 行）混杂配置结构、多源 YAML/ENV 合并加载与校验指纹；`model/mod.rs::execute_frozen`（166 行）、`maintenance/retention.rs::cleanup`（241 行）与 `health.rs::check`（175 行）存在过深嵌套与长流程耦合。
- **决策**：
  - 将 `operations.rs` 拆为 `operations/{mod.rs, task_plan.rs, doctor.rs, scheduled_log.rs, rollback.rs, personal_log.rs}`，仅在明确边界拆分，不引入浅转发层；
  - 将 `config.rs` 的加载与校验/指纹拆入 `config/{load.rs, validate.rs}` 并更新 `rust/parser-inputs.json` 保证 `PARSER_CONTENT_IDENTITY` 完整覆盖；
  - 将 `execute_frozen`、`cleanup`、`health::check` 抽分为清晰的私有阶段方法，并将处理缓存字节遍历收敛为 `processing_cache::measure_directory_bounded` 单一实现；
  - 核查 `ReportError::retryable()` 列表并明确标注各状态码来源（`MODEL_TIMEOUT` 等由 `TransportFailure::code()` 动态产生而非字面量 `ReportError::new`）。

## ADR-006：可执行的架构契约测试与 CI 门禁

- **决策**：在 `rust/daily_report/tests/architecture_contracts.rs` 中将模块无环检查（Tarjan SCC）、分层方向约束、Single Source of Truth（`FixedOffset::east_opt`、verbatim 前缀、DB 文件名、`personal_*:` 键、裸 `open_with_flags` 禁用）与非测试代码行数上限固化为 `cargo test --workspace --locked` 的自动门禁，并补充 `.github/workflows/rust-gates.yml`。

---

## 待确认事项（Deferred / Product Decision Required）

以下事项涉及部署决策或历史基线清理，在本隔离治理分支中仅登记、不擅自执行：

1. **合并与新发布包部署**：[done] `refactor/arch-deentropy-20261129` 已快进合并回 `main`（`c90302d0`），并完成 `.releases/rust-20261129-arch-deentropy-01` 构建、严格自检、计划任务切换、实跑健康核验与远端推送准备。
2. **历史离线脚本与基准产物退场**：[done] 经确认授权，`scripts/pdf_benchmark/`（9 个文件）已按原相对路径打包并校验 SHA-256 归档至 `.artifacts/pdf-benchmark-retirement-20261129/pdf-benchmark-scripts.zip`（索引 `archived.json`）后从工作树移除；`tests/fixtures/pdf_benchmark/` 与 `rust/pdf_extract_bench/` 按基线规则保留。
3. **本周周报草稿（`2026-11-23_2026-11-29`）**：CLI `weekly` 命令已按 `AGENTS.md` 永久退役，`data/weekly_drafts/` 无代码引用；目前 Phase 1 已补齐 `2026-11-25..27` 日报，如需生成本周 `2026-11-23_2026-11-29` 周报审核稿，可结合本周已发布的 `23`–`27` 日报按周会工作流单独触发。
4. **`config` 中的历史兼容字段**：`ReportProfile::Evidence`、`ArchiveSettings` 与 `matches_personal_fingerprint` 仍为历史 `report_runs` 冻结回放所必需，保持只读保留直至明确放弃历史库回放支持。
