# 架构降熵优化进度与度量台账

- **隔离工作树**：`.worktrees/arch-deentropy-20261129`
- **分支**：`refactor/arch-deentropy-20261129`（基于 `main` 提交 `b69a04a4`）

## 1. 度量基线 vs 收敛目标

| 指标 | 基线（`b69a04a4`） | Batch 0 | Batch 1 | Batch 2 | Batch 3 | Batch 4 | Batch 5 | Batch 6（最终） |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `daily_parsers` 模块 SCC 环数 | 1 | 1 | 0 | — | — | — | — | — |
| `daily_report` 模块 SCC 环数 | 1（12 模块） | 1 | 1 | — | — | — | — | — |
| `pdf_worker` 模块 SCC 环数 | 0 | 0 | 0 | — | — | — | — | — |
| 工作区非测试源码行数（`src/**/*.rs`） | 30,731 | 30,731 | 30,731 | — | — | — | — | — |
| 工作区 `cargo test` 通过数 | 367 | 370 | 370 | — | — | — | — | — |

## 2. 批次执行日志与任务池

- [done] **Batch 0**：建立隔离工作树 `.worktrees/arch-deentropy-20261129`、架构地图与 ADR 文档、基线特征测试（`architecture_baseline.rs`）及全量门禁基线验证。
- [done] **Batch 1**：打破 `daily_parsers` `contract <-> settings` 环，验证 `test_parser_content_identity.py`。
- [in_progress] **Batch 2**：提取 Layer-0 `infra`（`fs`、`json`、`locking`、`sqlite`）与 `types` 系统常量，删除 `maintenance/files.rs` 与重复工具实现。
- [pending] **Batch 2**：提取 Layer-0 `infra`（`fs`、`json`、`locking`、`sqlite`）与 `types` 系统常量，删除 `maintenance/files.rs` 与重复工具实现。
- [pending] **Batch 3**：切断 `processing_cache -> config`、`reports <-> evidence`、`reports <-> model` 环，实现三大 Crate 全无环（0 SCCs）。
- [pending] **Batch 4**：收口 `store::queries` 与元数据键前缀单一真相来源，消除跨层 SQL 与重复谓词。
- [pending] **Batch 5**：拆分 `operations` 与 `config`，分解 `execute_frozen`、`cleanup`、`health::check` 等超长流程函数并复核 `retryable()`。
- [pending] **Batch 6**：固化 `architecture_contracts.rs` 防回归契约测试、`.github/workflows/rust-gates.yml`、全量 Rust/Python 验证与最终复审。
