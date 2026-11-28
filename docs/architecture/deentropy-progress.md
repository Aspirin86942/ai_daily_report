# 架构降熵优化进度与度量台账

- **隔离工作树**：`.worktrees/arch-deentropy-20261129`
- **分支**：`refactor/arch-deentropy-20261129`（基于 `main` 提交 `b69a04a4`）

## 1. 度量基线 vs 收敛目标

| 指标 | 基线（`b69a04a4`） | Batch 0 | Batch 1 | Batch 2 | Batch 3 | Batch 4 | Batch 5 | Batch 6（最终） |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `daily_parsers` 模块 SCC 环数 | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| `daily_report` 模块 SCC 环数 | 1（12 模块） | 1 | 1 | 1（4 模块） | 0 | 0 | 0 | 0 |
| `pdf_worker` 模块 SCC 环数 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| 工作区非测试源码行数（不含 `tests/` 与 `tests.rs`） | 29,731 | 29,731 | 29,730 | 29,696 | 29,697 | 29,939 | 30,073 | 30,073 |
| 工作区 `cargo test` 通过数 | 367 | 370 | 370 | 372 | 372 | 372 | 373 | 377 |

## 2. 批次执行日志与任务池

- [done] **Batch 0**：建立隔离工作树 `.worktrees/arch-deentropy-20261129`、架构地图与 ADR 文档、基线特征测试（`architecture_baseline.rs`）及全量门禁基线验证。
- [done] **Batch 1**：打破 `daily_parsers` `contract <-> settings` 环，验证 `test_parser_content_identity.py`。
- [done] **Batch 2**：提取 Layer-0 `infra`（`fs`、`json`、`locking`、`sqlite`）与 `types` 系统常量，删除 `maintenance/files.rs` 与重复工具实现，`daily_report` SCC 从 12 模块收敛至 4 模块。
- [done] **Batch 3**：切断 `processing_cache -> config`、`reports <-> evidence`、`reports <-> model` 环（并删除 `model/contracts.rs`），三大 Crate 实现 **0 模块环（0 SCCs）**。
- [done] **Batch 4**：建立 `store::queries` 与元数据键前缀常量单一真相来源，迁移 `health`、`operations`、`personal_recovery`、`maintenance/retention`、`operations/rollback` 消除跨层原始 SQL 与重复有效摘要谓词。
- [done] **Batch 5**：将 `operations.rs` 拆分为 `operations/{mod,task_plan,doctor,scheduled_log,rollback,personal_log}`，将 `config.rs` 加载与校验/指纹拆分为 `config/{load,validate}` 并纳入 `rust/parser-inputs.json`，分解 `model::execute_frozen`、`maintenance::retention::cleanup`、`health::check`（提取 `processing_cache::measure_directory_bounded`），清理 `ReportError::retryable()` 4 个已废弃错误码。
- [done] **Batch 6**：新增可执行架构契约测试 [`rust/daily_report/tests/architecture_contracts.rs`](../../rust/daily_report/tests/architecture_contracts.rs)（强制 0 SCC、6 层单向边界、Single Source of Truth 与 31,000 行上限）与 CI 工作流 [`.github/workflows/rust-gates.yml`](../../.github/workflows/rust-gates.yml)，完成全量 Rust + Python 回归验证。
