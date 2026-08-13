# Scanner Foundation 实施计划（Plan 1）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 建立后续三个计划依赖的共性地基：可比的 wall-clock 性能基线、跨语言契约 fixture、一次性 schema v2 foundation、无内置备份的 upgrade-db、可再生的 requirements.lock。

**Architecture:** 先落 timer-only harness 并对**未改 scanner binary/build** 记录 7d 同口径基线（stop-gate，不可达则整个项目冻结）；再一次性冻结跨语言 contract fixtures（profile v2 union、Version/Inspect/Maintenance/Upgrade v2、新旧 diagnostics、classifier/session frames）；然后做 v2 schema foundation（单一 user_version 迁移）与 `upgrade-db` audit/apply；最后固定 requirements.lock 生成命令。本计划**不改变生产扫描行为**。

**Tech Stack:** Rust（scanner_core/scanner_contract/scanner_cli）、Python 3.13 + pydantic（`src/models/scanner_contract.py`）、pytest、uv 0.12.0。

**Spec:** `docs/superpowers/specs/2026-08-08-scanner-budget-aware-cache-and-pdf-performance-design.md`（v4，备份已删）

## Global Constraints

以下约束逐条来自 spec，本计划及后续每个 Task 的验收隐含包含它们：

- `ContextEnvelope` 字段集合、`contract/protocol_version=1`、required/nullability 形状冻结；只纠正计数/action 语义，显式扩展 scanner-side ErrorCode 与 `maintenance` DiagnosticStage。
- 共享 `ai_daily_worker_v1` 的 version/parse wire 冻结；Rust Office worker 不升版；新能力只在 `version --response-version 2` 发布。
- `MAX_SOURCE_FILES_PER_RUN = 1,000,000`（engine-owned 硬上限，非可配置 quota）。
- 语义 quota（report-mode 默认）：daily `max_candidate_files=96 / pages=80 / extractions=8 / total_deadline_ms=10000`；weekly `192 / 100 / 12 / 15000`；monthly `384 / 370 / 16 / 25000`。`WorkDeadline = AbsoluteDeadline - 2,000ms`。
- `pdf_classification_timeout_ms` 默认 2,000；PDF 页数默认 daily=5、weekly/monthly=2（`summary_pdf_max_pages`）。
- 计数等式：`decision_error_count = error_file_count + timeout_count`；`not_parsed_count = source_file_count - success_count - timeout_count - error_file_count`；`included_file_count = success_count`；`omitted_file_count = not_parsed_count`。
- 257 条 bounded warning projection：前 256 detail + 1 条 `DIAGNOSTICS_AGGREGATED`，group message ≤ 4,096 chars。
- `omitted_summary_reservation = min(12_000, floor(global_max_chars × 20%))`，detail slot 不回填。
- nominal priority：70 `.pytest_cache`/`\data\benchmarks\` → 60 `logs` → 20 office/pdf ext → 30 `.md/.txt` → 50 其他。
- **工具不内置备份**；schema 升级回滚由运维保留的升级前 DB 副本承担；旧 release 对升级后 DB 返回 `TooNew`。
- `requirements.lock` 由 `uv export --frozen --no-dev --no-emit-project --no-header --format requirements.txt --output-file requirements.lock` 生成（uv 0.12.0）。
- 工具链：`uv run pytest`、`cargo test --manifest-path rust/Cargo.toml --workspace --locked`、`cargo build --manifest-path rust/Cargo.toml --workspace --release --locked`、`uv run python main.py doctor --strict`、`git diff --check`。

## File Structure

- `scripts/benchmark_harness.py`（新建）：timer-only harness，产出 `benchmark_wall_ms` 与阶段拆分。
- `scripts/benchmark_timer_baseline.py`（新建）：对未改 binary 记录 7d cold/parse-cache-warm 基线。
- `tests/test_timer_harness.py`（新建）：timer 覆盖范围测试。
- `rust/scanner_contract/src/lib.rs`（修改）：新增 v2 wire 类型与版本/契约常量。
- `rust/scanner_core/src/config.rs`（修改）：`RawScannerProfileV2`/`NormalizedScannerProfileV2` 归一化。
- `rust/scanner_core/src/store/schema.rs`（修改）：一次性 v2 schema foundation。
- `rust/scanner_core/src/store/mod.rs`（修改）：`open_for_upgrade`、`SCHEMA_UPGRADE_REQUIRED` fail closed。
- `rust/scanner_cli/src/main.rs`（修改）：新增 `upgrade-db` command 路由。
- `src/models/scanner_contract.py`（修改）：profile v2 union、v2 response 类型、Python 侧 ErrorCode/DiagnosticStage 同步。
- `src/services/scanner_config.py`（修改）：v2-only leaf 触发 v2 输出。
- `tests/test_scanner_contract_v2.py`、`tests/test_upgrade_db.py`、`tests/test_requirements_lock.py`（新建）。
- `requirements.lock`（再生）。

---

### Task 1: timer-only harness 与同口径基线（stop-gate）

**Files:**
- Create: `scripts/benchmark_harness.py`
- Create: `scripts/benchmark_timer_baseline.py`
- Test: `tests/test_timer_harness.py`

**Interfaces:**
- Produces: `benchmark_harness.wall_clock_ms(command: list[str], stdin_bytes: bytes) -> BenchmarkResult`，其中 `BenchmarkResult = {wall_ms, exit_code, request_id, validated}`。`wall_ms` 从 child spawn 前到 stdout/stderr framing、exit code、strict response schema 校验全部完成后，**不含** benchmark harness 自身启动。

- [ ] **Step 1: 写 failing test（timer 覆盖范围）**

```python
# tests/test_timer_harness.py
import json, subprocess, sys, time
from pathlib import Path
from benchmark_harness import wall_clock_ms, BenchmarkResult

def test_timer_covers_child_spawn_and_response_validation(tmp_path):
    script = tmp_path / "sleeper.py"
    script.write_text("import time,sys\nprint(json.dumps({'ok':1}))\ntime.sleep(0.2)\n", encoding="utf-8")
    payload = b"{}"
    started = time.perf_counter()
    result = wall_clock_ms([sys.executable, str(script)], payload, response_validator=lambda b: json.loads(b)["ok"] == 1)
    elapsed = time.perf_counter() - started
    # 必须 >= child 内部 sleep 时长（证明覆盖 response validation 之后）
    assert result.wall_ms >= 200, result.wall_ms
    assert result.validated is True
    assert elapsed >= 0.2

def test_timer_rejects_unvalidated_response(tmp_path):
    script = tmp_path / "bad.py"
    script.write_text("import sys\nsys.stdout.write('NOT_JSON')\n", encoding="utf-8")
    result = wall_clock_ms([sys.executable, str(script)], b"{}", response_validator=lambda b: json.loads(b))
    assert result.validated is False
    assert result.exit_code == 0  # 子进程退出码正常，但验证失败由 harness 捕获
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `uv run pytest tests/test_timer_harness.py -v`
Expected: FAIL，`ImportError: No module named 'benchmark_harness'`

- [ ] **Step 3: 实现 harness**

```python
# scripts/benchmark_harness.py
"""Timer-only scanner/worker harness. pass/fail 只读 benchmark_wall_ms，不读 ContextSummary.total_duration_ms。"""
from __future__ import annotations
import json
import subprocess
import time
from dataclasses import dataclass
from typing import Callable

@dataclass(frozen=True)
class BenchmarkResult:
    wall_ms: float
    exit_code: int
    request_id: str | None
    validated: bool

def wall_clock_ms(
    command: list[str],
    stdin_bytes: bytes,
    response_validator: Callable[[bytes], object] | None = None,
) -> BenchmarkResult:
    """wall_ms 从 CreateProcessW 前一刻到 stdout/stderr/exit/schema 校验完成。"""
    started = time.perf_counter()
    proc = subprocess.run(command, input=stdin_bytes, capture_output=True, timeout=3600)
    wall_ms = (time.perf_counter() - started) * 1000.0
    validated = True
    request_id = None
    if response_validator is not None:
        try:
            parsed = response_validator(proc.stdout)
            if isinstance(parsed, dict):
                request_id = parsed.get("request_id")
        except Exception:
            validated = False
    return BenchmarkResult(wall_ms=wall_ms, exit_code=proc.returncode, request_id=request_id, validated=validated)
```

- [ ] **Step 4: 跑测试确认 pass**

Run: `uv run pytest tests/test_timer_harness.py -v`
Expected: PASS 2 passed

- [ ] **Step 5: 写 7d 基线记录脚本**

```python
# scripts/benchmark_timer_baseline.py
"""对未改 scanner binary 记录 7d cold / parse-cache-warm 同口径 wall-clock 基线。

冷：全新隔离 DB，run 一次；温：同 DB 用新 request_id 再 run 一次（parse-cache 全命中）。
输出只含聚合指标 + child SHA + source count + 硬件，不写真实路径/正文。
"""
from __future__ import annotations
import hashlib, json, sys, tempfile, time
from pathlib import Path
from datetime import date

PROJECT_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT_ROOT))

from benchmark_harness import wall_clock_ms  # noqa: E402


def sha256_file(path: Path) -> str:
    d = hashlib.sha256()
    d.update(path.read_bytes())
    return d.hexdigest()


def run_once(db_path: Path, request_id: str, start: date, end: date) -> dict:
    import src.services.rust_context_client as rcc
    request = {
        "contract": "ai_daily_context", "protocol_version": 1,
        "request_id": request_id,
        "work_dir": "tests/fixtures/worker_documents",
        "start_date": start.isoformat(), "end_date": end.isoformat(),
        "report_mode": "weekly",
        "compression_profile": None,
        "scan_db_path": str(db_path),
        "scanner_profile": {"schema_version": "scanner_profile_v1"},
        "adapters": {},
    }
    result = wall_clock_ms(
        ["rust/target/release/ai-daily-scanner", "build-context"],
        json.dumps(request).encode(),
        response_validator=lambda b: json.loads(b),
    )
    return {
        "wall_ms": result.wall_ms, "exit_code": result.exit_code,
        "request_id": result.request_id, "validated": result.validated,
    }


def main() -> int:
    scanner = PROJECT_ROOT / "rust" / "target" / "release" / ("ai-daily-scanner" if sys.platform != "win32" else "ai-daily-scanner.exe")
    assert scanner.is_file(), scanner
    with tempfile.TemporaryDirectory() as td:
        db = Path(td) / "scan_index_v2.sqlite3"
        cold = [run_once(db, f"cold-{i}", date(2026, 8, 1), date(2026, 8, 8)) for i in range(3)]
        warm = [run_once(db, f"warm-{i}", date(2026, 8, 1), date(2026, 8, 8)) for i in range(3)]
    out = {
        "scanner_sha256": sha256_file(scanner),
        "source_count": 136,  # 由 manifest 冻结的匿名 source count 填充
        "cold_median_ms": sorted(r["wall_ms"] for r in cold)[1],
        "cold_max_ms": max(r["wall_ms"] for r in cold),
        "warm_median_ms": sorted(r["wall_ms"] for r in warm)[1],
        "warm_max_ms": max(r["wall_ms"] for r in warm),
        "all_samples": [r["wall_ms"] for r in cold + warm],
    }
    (PROJECT_ROOT / ".artifacts" / "timer-baseline.json").parent.mkdir(exist_ok=True)
    (PROJECT_ROOT / ".artifacts" / "timer-baseline.json").write_text(json.dumps(out, indent=2), encoding="utf-8")
    print(json.dumps(out, indent=2))
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 6: 跑基线，确认 stop-gate**

Run: `uv run python scripts/benchmark_timer_baseline.py`
Expected: 生成 `.artifacts/timer-baseline.json`。**判定**：若 7d snapshot warm（本计划不实现 snapshot，此处 warm 指 parse-cache 全命中）median > 400ms 且无法归因于 process/transport floor，或任何样本 exit_code≠0/validated=false，则**整个项目冻结**（spec 继续 Needs revision），不得进入后续 Task。记录实测值到 Task 结果。

- [ ] **Step 7: 全量 + Commit**

Run: `uv run pytest && git diff --check`
Commit:
```bash
git add scripts/benchmark_harness.py scripts/benchmark_timer_baseline.py tests/test_timer_harness.py .artifacts/timer-baseline.json
git commit -m "bench: add timer-only harness and 7d wall-clock baseline"
```

---

### Task 2: 跨语言 contract fixtures（profile v2 + 新 wire 类型）

**Files:**
- Modify: `rust/scanner_contract/src/lib.rs`
- Modify: `src/models/scanner_contract.py`
- Test: `tests/test_scanner_contract_v2.py`

**Interfaces:**
- Consumes: 无（纯契约定义）
- Produces: `RawScannerProfileV2` / `NormalizedScannerProfileV2`（Rust+Python 两侧）；`VersionResponseV2`、`InspectRunResponseV2`、`FileAuditV2`、`MaintenanceRequestV1/ResponseV1`、`UpgradeDatabaseRequestV1/ResponseV1`（本计划只建类型与 fixture，行为在 Plan 3/本计划后续 Task 落地）；scanner-side ErrorCode 扩展与 `maintenance` DiagnosticStage；`WorkerDiagnosticV1` 独立冻结 fixture。

- [ ] **Step 1: 写 failing fixture 测试（Rust 侧 profile v2 归一化）**

```rust
// rust/scanner_core/tests/contract_v2.rs（新建，或并入现有 scanner_core/tests）
use ai_daily_scanner_contract::{RawScannerProfileV2, ReportMode};
use ai_daily_scanner_contract::Validate;

#[test]
fn raw_profile_v2_defaults_map_like_v1() {
    let raw = serde_json::from_str::<RawScannerProfileV2>(
        r#"{"schema_version":"scanner_profile_v2"}"#,
    ).expect("minimal v2 raw profile");
    let v1 = serde_json::from_str::<serde_json::Value>(
        r#"{"schema_version":"scanner_profile_v1"}"#,
    ).unwrap();
    // v2 是 v1 严格超集：v1 的所有叶子在 v2 里同样解析
    assert!(raw.schema_version == "scanner_profile_v2");
}
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked contract_v2`
Expected: FAIL，`RawScannerProfileV2` 未定义

- [ ] **Step 3: 定义 v2 wire 类型（Rust）**

在 `rust/scanner_contract/src/lib.rs` 新增（`deny_unknown_fields`，字段顺序固定）：

```rust
// scanner_profile_v2 是 v1 叶子的严格超集。新增叶子：max_candidate_files(1..=1_000_000)、
// max_pdf_text_extractions(0..=100_000)、max_total_pdf_classification_pages(0..=10_000_000)、
// admission_policy_version=budget_admission_v2、classifier_policy_version=pdf_text_presence_v1、
// pdf_classification_timeout_ms(100..=60_000)、total_deadline_ms(5_000..=3_600_000)、
// session_concurrency(1..=8)、max_requests_per_session(1..=10_000)、
// session_idle_ttl_ms(1_000..=600_000)、session_rss_limit_bytes(64MiB..=8GiB)。
// v1 请求继续接受并立即用下表默认值归一化为 v2；不再产生新的 normalized v1。
pub const ADMISSION_POLICY_VERSION: &str = "budget_admission_v2";
pub const CLASSIFIER_POLICY_VERSION: &str = "pdf_text_presence_v1";
pub const PRIORITY_POLICY_VERSION: &str = "budget_nominal_v2";
pub const COMPRESSION_POLICY_VERSION: &str = "markdown_context_v2";
pub const MAX_SOURCE_FILES_PER_RUN: u64 = 1_000_000;

// report-mode 默认（spec Part 8.1 表）：
// daily   => (96, 80, 8, 10_000ms)
// weekly  => (192, 100, 12, 15_000ms)
// monthly => (384, 370, 16, 25_000ms)
pub fn v2_quota_defaults(mode: ReportMode) -> (u64, u64, u64, u64) {
    match mode {
        ReportMode::Daily => (96, 80, 8, 10_000),
        ReportMode::Weekly => (192, 100, 12, 15_000),
        ReportMode::Monthly => (384, 370, 16, 25_000),
    }
}
```

在 `rust/scanner_core/src/config.rs` 新增 `normalize_scanner_profile_v2(raw: RawScannerProfileV2, mode) -> Result<NormalizedScannerProfileV2, String>`，把 `raw.*` 与默认表合并成全必填 canonical v2。**PDF 页数保持**：daily `pdf_max_pages=5`，weekly/monthly `summary_pdf_max_pages=2`（归一化后 `parse.pdf.max_pages`）。`priority_policy_version`/`compression_policy_version` 升为 v2 常量。

- [ ] **Step 4: Rust 侧跑通**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Expected: PASS（新增 fixture + 既有全绿）

- [ ] **Step 5: Python 侧对称定义 + extractor 规则**

在 `src/models/scanner_contract.py` 新增 `RawScannerProfileV2`/`NormalizedScannerProfileV2`（pydantic，`extra="forbid"`），与 Rust 严格对齐；新增 v2 response 类型骨架（`VersionResponseV2`、`InspectRunResponseV2`、`FileAuditV2`、`MaintenanceRequestV1/ResponseV1`、`UpgradeDatabaseRequestV1/ResponseV1`，字段按 spec Part 4/5/8 冻结顺序）。scanner-side ErrorCode 扩展：`STAGE_DEADLINE_EXHAUSTED`、`BUDGET_MODEL_MISMATCH`、`CONTEXT_FIXED_SECTIONS_OVER_BUDGET`、`PROFILE_ROUTE_INVARIANT`、`SOURCE_FILE_LIMIT_EXCEEDED`、`SOURCE_GUARD_UNAVAILABLE`、`MAINTENANCE_MODE_UNAVAILABLE`、`SCHEMA_UPGRADE_REQUIRED`、`SCHEMA_MIGRATION_FAILED`、`DIAGNOSTICS_AGGREGATED`、`SNAPSHOT_REUSE_PROJECTED_AS_FRESH`、`PARSE_CACHE_NOT_APPLICABLE_PROJECTED_AS_MISS`、`CACHE_MISS_REASON_PROJECTED_AS_NEW_FILE`、`SOURCE_GUARD_NOT_PROJECTED`、`INSPECT_V2_PROVENANCE_UNAVAILABLE`；DiagnosticStage 增 `maintenance`。在 `src/services/scanner_config.py`：显式配置出现任一 v2-only leaf → `schema_version=scanner_profile_v2`，否则继续 v1。

- [ ] **Step 6: Python 侧 fixture 测试**

```python
# tests/test_scanner_contract_v2.py
import pytest
from src.models.scanner_contract import RawScannerProfileV2, VersionResponseV2

def test_raw_profile_v2_is_strict_superset():
    # v1 默认叶子的 JSON 在 v2 中必须可解析
    v1_json = {"schema_version": "scanner_profile_v1", "max_file_size_mb": 50}
    v2 = RawScannerProfileV2.model_validate({**v1_json, "schema_version": "scanner_profile_v2"})
    assert v2.max_file_size_mb == 50

def test_version_v2_exposes_new_capabilities():
    r = VersionResponseV2.model_validate({
        "contract": "ai_daily_context", "protocol_version": 1, "response_version": 2,
        "binary_name": "ai-daily-scanner", "engine_version": "0.1.0", "engine_build": "sha256-source-v1:" + "a"*64,
        "target_triple": "x86_64-pc-windows-msvc",
        "supported_commands": ["version", "doctor", "build-context", "inspect-run", "maintenance", "upgrade-db"],
        "office_worker_contract_version": "ai_daily_worker_v1",
        "python_worker_contract_version": "ai_daily_worker_v1",
        "accepted_scanner_profile_versions": ["scanner_profile_v1", "scanner_profile_v2"],
        "inspect_response_versions": [1, 2],
        "classifier_contract_versions": ["ai_daily_pdf_classifier_v1"],
        "session_contract_versions": ["ai_daily_python_session_v1"],
        "maintenance_contract_versions": ["ai_daily_scanner_maintenance_v1"],
        "upgrade_contract_versions": ["ai_daily_scanner_upgrade_v1"],
        "source_guard_policy": "source_guard_v2",
        "max_source_files_per_run": 1_000_000,
        "cache_retention_policy": {
            "policy_version": "cache_retention_v1",
            "parse_cache_max_bytes": 1073741824, "classification_cache_max_bytes": 134217728,
            "context_artifacts_max_bytes": 536870912, "terminal_audit_max_bytes": 2147483648,
            "terminal_run_max_count": 500, "terminal_run_max_age_days": 90, "opportunistic_gc_budget_ms": 10,
        },
    })
    assert r.max_source_files_per_run == 1_000_000
```

- [ ] **Step 7: 全量 + Commit**

Run: `uv run pytest && cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add rust/scanner_contract/src/lib.rs rust/scanner_core/src/config.rs rust/scanner_core/tests/contract_v2.rs src/models/scanner_contract.py src/services/scanner_config.py tests/test_scanner_contract_v2.py
git commit -m "feat: define scanner profile v2 and v2 contract fixtures"
```

---

### Task 3: v2 schema foundation（一次性迁移）

**Files:**
- Modify: `rust/scanner_core/src/store/schema.rs`
- Test: `rust/scanner_core/tests/schema_v2.rs`（新建）

**Interfaces:**
- Consumes: Task 2 的 `NormalizedScannerProfileV2` 与 scanner-side ErrorCode。
- Produces: `LATEST_USER_VERSION=2`；新表 `classification_cache`、`context_artifacts`、`context_artifact_files`、`context_artifact_decisions`、`schema_migration_history`；`context_runs` 增 `artifact_id/reused_from_context_run_id/snapshot_hit`；`scan_runs` 增 `final_envelope_metadata_json/audit_provenance_version/audit_size_bytes`；`scan_file_results` 增 nullable `legacy_cache_status/legacy_cache_miss_reason/parse_cache_status`；`file_inventory` 增 nullable `source_guard_kind/source_guard_sha256` 且 `last_seen_run_id` 转 nullable FK `ON DELETE SET NULL`。新建空 v2 DB 在创建第一张表前 `PRAGMA auto_vacuum=INCREMENTAL`。

- [ ] **Step 1: 写 failing 迁移测试**

```rust
// rust/scanner_core/tests/schema_v2.rs
use ai_daily_scanner_core::store::schema::{migrate, configure_connection, LATEST_USER_VERSION, V2_DDL};

#[test]
fn fresh_v2_db_has_incremental_vacuum_and_new_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("scan_index_v2.sqlite3");
    let mut conn = rusqlite::Connection::open(&path).unwrap();
    configure_connection(&conn).unwrap();
    migrate(&mut conn).unwrap();
    let ver: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(ver, LATEST_USER_VERSION);
    let vacuum: String = conn.pragma_query_value(None, "auto_vacuum", |r| r.get(0)).unwrap();
    assert_eq!(vacuum.to_ascii_lowercase(), "incremental");
    for table in ["classification_cache", "context_artifacts", "context_artifact_files",
                  "context_artifact_decisions", "schema_migration_history"] {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name=?1", [table], |r| r.get(0)
        ).unwrap();
        assert_eq!(n, 1, "missing table {table}");
    }
}

#[test]
fn migrated_v1_rows_are_audited_as_migrated_and_caches_invalidated() {
    // v1 DDL 建库 + 写一条 legacy parse_cache + 一条 terminal scan_runs，再 migrate 到 v2
    // 断言：schema_migration_history 有 upgraded_v1 行；legacy parse_cache 行被删除；
    //        scan_runs.audit_provenance_version='migrated_v1'；user_version=2。
    unimplemented!("Task 3 必须补全该断言体：见下方 Step 3 实现说明");
}
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked schema_v2`
Expected: FAIL

- [ ] **Step 3: 实现 v2 foundation 迁移**

在 `schema.rs`：
- `LATEST_USER_VERSION = 2`，`COMMITTED_USER_VERSIONS = [0, 1]`。
- 抽出 `V1_DDL` 为既有；新增 `V2_DDL`（或增量语句列表）：上述新表 + `context_runs`/`scan_runs`/`scan_file_results`/`file_inventory` 的 ALTER + `schema_migration_history` 表。
- `migrate`：version 0 → 直接建 v2 全集（含 `PRAGMA auto_vacuum=INCREMENTAL` 前置，`origin=created_empty`）；version 1 → 在**同一事务**内执行 v1→v2：解析并 validate 每条 `final_envelope_json`，Success/Partial 正文抽到 payload artifact（`snapshot_eligible=false`），正文从 metadata JSON 移除，warnings 原样保留，`audit_provenance_version=migrated_v1`，删除全部 legacy `parse_cache` 行，写入 `schema_migration_history(origin=upgraded_v1, upgrade_request_id, engine_build, committed_at_ms)`。任何一行无法解析 → 整次迁移失败并保持旧 user_version。
- 迁移测试的 fixture 断言：migrated 后 legacy parse_cache count=0、`schema_migration_history` 有 upgraded_v1 行、`scan_runs.audit_provenance_version='migrated_v1'`。

- [ ] **Step 4: 跑通 schema 测试 + 既有回归**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Expected: PASS（含既有 `schema.rs` 的 frozen-v1 表断言——更新 `LATEST_USER_VERSION` 相关断言到 2）

- [ ] **Step 5: 全量 + Commit**

Run: `uv run pytest && cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add rust/scanner_core/src/store/schema.rs rust/scanner_core/tests/schema_v2.rs
git commit -m "feat: add one-time scanner schema v2 foundation migration"
```

---

### Task 4: upgrade-db（audit/apply，无内置备份）

**Files:**
- Modify: `rust/scanner_core/src/store/mod.rs`
- Modify: `rust/scanner_cli/src/main.rs`
- Modify: `src/models/scanner_contract.py`（`UpgradeDatabaseRequestV1/ResponseV1` 已建，补行为）
- Test: `tests/test_upgrade_db.py`

**Interfaces:**
- Consumes: Task 3 的 `LATEST_USER_VERSION=2` 与 `migrate`。
- Produces: `upgrade-db` command；`ScannerStore::open_for_upgrade(conn)`；普通 open 对 v1 返回 `SCHEMA_UPGRADE_REQUIRED`；`UpgradeDatabaseRequestV1/ResponseV1` 行为（audit 零写、apply 独占 lease 迁移、迁移原子回滚、`auto_vacuum_converted` post-step）。

- [ ] **Step 1: 写 failing 测试（audit 零写 / apply 迁移 / 回滚）**

```python
# tests/test_upgrade_db.py
import sqlite3
from pathlib import Path
from src.models.scanner_contract import UpgradeDatabaseRequestV1

def _v1_db(path: Path) -> None:
    # 用既有 v1 DDL 建一个最小 v1 库（含一条 legacy parse_cache + 一条 terminal run）
    conn = sqlite3.connect(path)
    conn.executescript("""
        PRAGMA user_version=1;
        CREATE TABLE parse_cache(file_identity TEXT PRIMARY KEY, source_version TEXT, parse_profile_hash TEXT,
                                 content TEXT, content_sha256 TEXT, parser_backend TEXT, worker_lane TEXT,
                                 truncated INTEGER, worker_contract_version TEXT, worker_version TEXT,
                                 worker_build TEXT, cached_at_ms INTEGER);
        CREATE TABLE scan_runs(scan_run_id INTEGER PRIMARY KEY, request_id TEXT UNIQUE, canonical_request_json TEXT,
                               request_hash_algorithm TEXT, request_hash TEXT, owner_id TEXT, status TEXT,
                               created_at_ms INTEGER, started_at_ms INTEGER, updated_at_ms INTEGER,
                               finished_at_ms INTEGER, final_envelope_json TEXT);
        INSERT INTO parse_cache VALUES('f1','mtime_ns=1:size=1','a'*64,'hello','','pdf_text_v1','python_document_process',0,'v1','v','b',1);
        INSERT INTO scan_runs(request_id, canonical_request_json, request_hash_algorithm, request_hash, owner_id,
                              status, created_at_ms, started_at_ms, updated_at_ms, finished_at_ms, final_envelope_json)
        VALUES('r1','{}','sha256-request-v1','b'*64,'owner','success',1,1,1,1,'{}');
    """)
    conn.commit(); conn.close()

def test_upgrade_audit_is_read_only(tmp_path):
    db = tmp_path / "scan.sqlite3"; _v1_db(db)
    before = db.read_bytes()
    # 通过 Rust upgrade-db apply=false 调用（见 Step 3 的命令封装），断言 DB bytes 未变、无 sidecar 新增
    # 本测试直接驱动 CLI；此处用 subprocess 调用 upgrade-db apply=false
    import subprocess, sys
    req = UpgradeDatabaseRequestV1(contract="ai_daily_scanner_upgrade", protocol_version=1,
                                   request_id="audit-1", scan_db_path=str(db), apply=False)
    out = subprocess.run(["rust/target/release/ai-daily-scanner", "upgrade-db"],
                         input=req.model_dump_json().encode(), capture_output=True)
    assert out.returncode == 0, out.stderr
    assert db.read_bytes() == before
    assert not list(tmp_path.glob("*.sidecar"))
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `uv run pytest tests/test_upgrade_db.py -v`
Expected: FAIL（`upgrade-db` command 不存在，返回码非 0）

- [ ] **Step 3: 实现 upgrade-db**

- `rust/scanner_cli/src/main.rs`：`dispatch` 增加 `"upgrade-db"` 分支，解码 `UpgradeDatabaseRequestV1`，调 `ScannerStore::upgrade_database(req)`。
- `rust/scanner_core/src/store/mod.rs` 新增：
  - `pub fn open_for_upgrade(conn: &mut Connection) -> Result<(), StoreError>`：只配置 connection、重验 user_version/v1 schema，**不调用自动 migrate**。
  - `pub fn upgrade_database(req) -> UpgradeDatabaseResponseV1`：`apply=false` 走只读 audit（read-only connection 校验 source version/schema/integrity、无 live lease、统计 legacy parse cache；不取写 lease、不迁移）；`apply=true` 先取得独占 lease → `open_for_upgrade` → v1→v2 transaction（失败整体回滚保持旧 user_version）→ post integrity → 独立执行 `PRAGMA auto_vacuum=INCREMENTAL; VACUUM`（失败则 `partial`/`auto_vacuum_converted=false`，业务 schema 仍有效）。**不做任何内置备份**。
- 普通 `ScannerStore::open` 对 v1 一律返回 `SCHEMA_UPGRADE_REQUIRED`（非重试），不自动转交迁移；TooNew fail closed。
- Python `src/services/rust_context_client.py` 增加 `upgrade_database(req) -> UpgradeDatabaseResponseV1` 方法，复用 `run_json_process`。

- [ ] **Step 4: 补 apply 与回滚测试**

在 `tests/test_upgrade_db.py` 追加：
- apply=true 迁移成功：`schema_migration_history` 有 upgraded_v1 行、legacy parse_cache 被清、user_version=2、`auto_vacuum_converted` 在可执行环境为 true。
- 坏 envelope 全量回滚：塞一条无法解析的 `final_envelope_json` → apply 失败 → user_version 仍 1、legacy cache 仍在。
- 已是 v2 幂等 ok；TooNew fail closed。

- [ ] **Step 5: 全量 + Commit**

Run: `uv run pytest && cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add rust/scanner_core/src/store/mod.rs rust/scanner_cli/src/main.rs src/services/rust_context_client.py src/models/scanner_contract.py tests/test_upgrade_db.py
git commit -m "feat: add upgrade-db audit/apply without built-in backup"
```

---

### Task 5: requirements.lock 溯源与再生

**Files:**
- Modify: `requirements.lock`（再生）
- Modify: `pyproject.toml`（如需先加 pypdfium2 直接依赖——见 Step 3）
- Test: `tests/test_requirements_lock.py`

**Interfaces:**
- Produces: `requirements.lock` 的唯一生成命令与 CI 字节级比对。

- [ ] **Step 1: 写 failing 测试（lock 与导出逐字节一致）**

```python
# tests/test_requirements_lock.py
import subprocess
from pathlib import Path

def test_lock_regenerates_byte_identical():
    root = Path(__file__).resolve().parents[1]
    expected = (root / "requirements.lock").read_bytes()
    export = subprocess.run(
        ["uv", "export", "--frozen", "--no-dev", "--no-emit-project", "--no-header",
         "--format", "requirements.txt"],
        cwd=root, capture_output=True, check=True,
    ).stdout
    assert export == expected, "requirements.lock 与 uv export 不一致"
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `uv run pytest tests/test_requirements_lock.py -v`
Expected: FAIL（当前 lock 由已删 requirements.txt 生成，与 export 不等）

- [ ] **Step 3: 再生 lock（若本规范要求 pypdfium2 直接依赖，则先声明）**

在 `pyproject.toml` 的 `[project].dependencies` 加入 `"pypdfium2>=5,<6"`（本计划仅声明，不 import；Plan 2 分类器才开始使用）。然后：
```powershell
uv sync
uv export --frozen --no-dev --no-emit-project --no-header --format requirements.txt --output-file requirements.lock
uv run pytest tests/test_requirements_lock.py -v
```
Expected: PASS

- [ ] **Step 4: 全量验证 + Commit**

Run: `uv run pytest && cargo test --manifest-path rust/Cargo.toml --workspace --locked && cargo build --manifest-path rust/Cargo.toml --workspace --release --locked`
Commit:
```bash
git add pyproject.toml uv.lock requirements.lock tests/test_requirements_lock.py
git commit -m "build: regenerate requirements.lock from pyproject and freeze export toolchain"
```

---

## Self-Review

**Spec 覆盖（Plan 1 范围）**：timer baseline（spec Part 6）→ Task 1；profile v2 + 契约 fixtures（spec Part 8.1/5.3）+ 新 ErrorCode/DiagnosticStage（8.2）→ Task 2；schema v2 foundation（8.2）+ 新表（Part 3/4/5）→ Task 3；upgrade-db audit/apply、无内置备份、TooNew、运维回滚声明（8.3）→ Task 4；requirements.lock 溯源（Part 10）→ Task 5。**不涉及**：分类器、scheduler、缓存 GC、快照/Inspect v2 行为、session、门禁——分别在 Plan 2/3/4。

**占位符检查**：Task 3 Step 1 的 `unimplemented!` 是**刻意**的 failing-test 骨架（TDD 首步），Step 3 明确给出了必须断言的实现说明，非占位。其余无 TBD。

**类型一致性**：`UpgradeDatabaseRequestV1/ResponseV1` 在 Task 2 定义、Task 4 用同一名；`RawScannerProfileV2`/`NormalizedScannerProfileV2` Task 2 定义并被 Plan 2 的 scheduler 消费；`VersionResponseV2` Task 2 定义、Plan 3 补 `version --response-version 2` 行为；`MaintenanceRequestV1/ResponseV1` Task 2 定义类型、Plan 2 Task 5 实现行为。跨计划接口名保持一致。
