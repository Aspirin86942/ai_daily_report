# 架构基线地图、依赖图与规范偏离清单（Baseline: `b69a04a4`）

本文档记录项目在架构重构工作树（分支 `refactor/arch-deentropy-20261129`，基线提交 `b69a04a4`）下的完整架构测绘。

## 1. 工作区结构与模块规模基线

工作区包含三个 Rust Crate（[Cargo.toml](../../rust/Cargo.toml)）：

| Crate | 职责边界 | 顶层模块数（含 `_root_`） | 非测试行 | 内联测试行 | 模块环（SCC） |
| --- | --- | ---: | ---: | ---: | ---: |
| `daily_report` | CLI、每日流程、扫描调度与缓存、报告存储与发布、检索、健康检查与运维 | 18 | 19,810 | 2,463 | 1（12 个模块互相成环） |
| `daily_parsers` | 文件发现、文本与 Office 进程内解析、共享证据契约、PDF 通信帧 | 7 | 9,415 | 1,205 | 1（`contract <-> settings`） |
| `pdf_worker` | 隔离 PDFium 解析与可选 OCR 子进程 | 5 | 1,506 | 124 | 0（已无环） |
| **合计** | — | **30** | **30,731** | **3,792** | **2** |

## 2. 基线模块依赖图与强连通分量（SCC）

### 2.1 `daily_parsers`（1 个环：`contract <-> settings`）
- `settings -> contract`：`settings/mod.rs:7` 仅重导出 `crate::contract::SourceGuardKind`。
- `contract -> settings`：`contract/mod.rs:2-8` 使用 `settings::Validate`、`require_range`、`require_source_version`、`require_absolute_path`、`OfficeParseProfile` 与 `PdfPageAudit` 等设置校验原语。
- 其余模块（`discovery`、`text`、`office`、`pdf_wire`）均为有向无环依赖。

### 2.2 `daily_report`（1 个 12 模块巨环）
基线非测试代码的 12 模块强连通分量：
`{config, evidence, maintenance, model, processing_cache, publish, reports, run, scan, snapshots, store, types}`。

关键反向/跨层依赖边（实体测绘自 `src/**/*.rs`）：
1. `types -> snapshots`：`new_run_id` / `RunContext::validate` 直接调用 `crate::snapshots::{digest, valid_run_id}`。
2. `snapshots <-> processing_cache`：`snapshots.rs` 依赖 `processing_cache::{write_immutable, write_reservation_bytes, CacheQuota, CacheReservation}`；`processing_cache.rs` 反向依赖 `snapshots::reject_redirects` 与 `config::AppConfig`、`store::locking::ReportLock`、`maintenance::write_synced`。
3. `store -> {processing_cache, reports, snapshots}`：`store/locking.rs` 调用 `processing_cache::normalized` 与 `snapshots::{digest, reject_redirects}`；`store/reports.rs` / `store/periods.rs` 反向解析 `reports::personal::PersonalReport` 与 `reports::period`。
4. `config -> {reports, scan, snapshots}`：`config.rs` 反向导入 `reports::personal::MAX_FILE_CHARS` 与 `scan::cache::CACHE_FILENAME`。
5. `reports <-> evidence`：`reports/personal.rs` 导入 `evidence::EvidenceRef`，而 `evidence.rs` 导入 `reports::data::ReportSource` 与 `config::AppConfig`、`scan::config::normalize_scanner_settings`。
6. `reports <-> model`：`reports/period.rs:1` 仅为了 `invalid()` 错误构造而导入 `model::contracts::invalid`；同时 `model/personal.rs` 消费 `reports::personal::*`。
7. `scan -> {snapshots, store, types}`：`scan/cache.rs` 引用 `store::locking::ReportLock` 与 `snapshots::valid_hash`；`scan/cache/records.rs` 引用 `snapshots::digest`；`scan/cache/storage.rs` 引用 `snapshots::reject_redirects`。
8. `maintenance -> {run, snapshots, store, types}`：`maintenance/retention.rs` 反向依赖上层编排 `run::App`。

### 2.3 目标 6 层无环有向图（DAG）

```
Layer 5 (Entry):      cli, main
Layer 4 (Use-Cases):  run, operations, health, maintenance, search, personal_recovery
Layer 3 (Services):   store, model, publish
Layer 2 (Domain):     config, reports, evidence
Layer 1 (Core/IO):    snapshots, scan, processing_cache
Layer 0 (Leaf/SSOT):  types, infra (fs, json, locking, sqlite)
```

- `daily_parsers` 目标：`office`, `text`, `pdf_wire` -> `contract` -> `settings`, `discovery`（strict DAG，0 环）。
- `pdf_worker` 保持无环：`main` -> `reader` -> `excerpt`, `ocr`, `source`。

## 3. 状态与持久化所有权地图

| 持久化载体 / 锁 | 文件/路径标识 | 规范所有者（目标） | 基线越权访问点 |
| --- | --- | --- | --- |
| 报告主库（Schema v1） | `data/db/reports.sqlite3` | `store`（含 `store::queries` 只读查询层） | `health.rs`、`operations.rs`、`personal_recovery.rs`、`maintenance/retention.rs`、`operations/rollback.rs` 直接拼接 SQL 与元数据键 |
| 个人扫描缓存 | `data/db/scan_cache_v4.sqlite3` | `scan::cache` | `config.rs`、`health.rs` 硬编码文件名或直接建连 |
| 全文检索索引 | `data/db/report_search.sqlite3` | `search` | 归属基本清晰（`search.rs` 从 `reports.sqlite3` 只读同步） |
| 个人处理配额与保留 | `data/daily_cache/.cache-ledger.json` | `processing_cache` | `health.rs`、`operations.rs`、`operations/rollback.rs` 直接拼路径或自行遍历目录统计字节 |
| 运行不可变快照 | `data/run_snapshots/` + `daily_cache/snapshots/` | `snapshots` + `processing_cache` | `run.rs` 硬编码 `"run_snapshots"`，`maintenance/retention.rs` 反向依赖 `run::App` 并直接删除快照 |
| 报告/处理 OS 字节锁 | `.report-<hash>.lock` | `infra::locking::ReportLock` | 原错置于 `store::locking`，导致 `processing_cache` 与 `scan::cache` 反向依赖 `store` |
| 健康发布文件锁 | `logs/.health-publish.lock` | `health`（与 `scripts/health_notify.ps1` `FileShare.None` 配对） | 无（必须保持 `File::try_lock()` 语义，禁止改为字节-0 `LockFileEx`） |

## 4. 重复实现与规范偏离清单

| 类别 | 重复/偏离位置 | 规范收口目标 |
| --- | --- | --- |
| 路径 `\\?\` 前缀剥离 | `operations.rs::display_path`、`processing_cache.rs::normalized`、`evidence.rs::comparable` | 收口至 `infra::fs::{strip_verbatim_prefix, display_path, normalized_path}` |
| 原子落盘 / 有界读取 / 文件哈希 | `maintenance/files.rs`（30 行浅模块）、`health.rs::write_report`、`store/maintenance.rs::file_sha`、`publish.rs::output_hash` | 收口至 `infra::fs::{write_synced, read_bounded_bytes, write_atomic, file_sha256, optional_file_sha256}` 并删除 `maintenance/files.rs` |
| 64 位小写十六进制校验 | `snapshots.rs::valid_hash` vs `reports/data.rs::sha256`（重复实现） | 统一使用 `infra::json::valid_hash` |
| 上海时区偏移量 `UTC+8` | `types.rs::shanghai()` vs `reports.rs:75` 直接 `FixedOffset::east_opt(8 * 3600)` | 全仓统一调用 `types::shanghai()`，以架构测试守卫 |
| 领域常量与文件名魔法字符串 | `"personal_work"`、`"Asia/Shanghai"`、`180_000`、`2_000`、`"reports.sqlite3"` 等散布 5–9 个文件；`personal_run:`/`personal_calls:`/`personal_summary:` 散布 5 个文件 | 系统级常量与文件名收口至 `types.rs` + `AppConfig` 路径访问器；元数据键前缀收口至 `store` |
| 只读 SQLite 打开与一致性检查 | `health.rs`、`operations.rs`（缺少 `busy_timeout`）、`operations/rollback.rs`、`search.rs` | 收口至 `infra::sqlite::{open_read_only, open_read_only_nolock, quick_check, quick_check_yielding}` |
| 浅层转发 / 单文件畸轻模块 | `model/contracts.rs`（5 行，引入双向环）、`maintenance/files.rs`（30 行） | 分别内联至 `model/mod.rs`（局部 `reports/period.rs` 错误辅助）与 `infra/fs.rs` 并删除 |
| 职责过载与超长函数 | `operations.rs`（477 行混合 4 类职责）、`config.rs`（753 行）、`model/mod.rs::execute_frozen`（166 行）、`maintenance/retention.rs::cleanup`（241 行）、`health.rs::check`（175 行）、`health.rs:350/366` 重复有效摘要 SQL 谓词 | 按职责内聚拆分模块与私有阶段函数；统一 `store::queries` 单一 SQL 谓词 |
| `PARSER_CONTENT_IDENTITY` 覆盖 | `rust/parser-inputs.json` 列举了 `config.rs`、`run.rs`、`run/personal_scan.rs`、`scan`；若提取 `infra` 或拆分 `config/` 须同步核对并保持 `test_parser_content_identity.py` 全绿 | 拆分 `config/` 时同步更新 `rust/parser-inputs.json`（保留 `publish.rs` 独立于解析构建身份之外） |
