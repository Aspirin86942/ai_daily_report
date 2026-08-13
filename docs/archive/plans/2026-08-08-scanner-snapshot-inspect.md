# Scanner 快照 + Inspect v2 实施计划（Plan 3）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 context artifact 关系模型（正文只存一次、当前 run 重算耗时与 provenance）、`ContextEnvelope v1` 从 metadata 重建、`InspectRunResponseV2`/`FileAuditV2`/`VersionResponseV2` 观测接口、隔离 cache-only seed 的 snapshot warm 基准。

**Architecture:** 新增 `context_artifacts`（final_context 去重存储 + 可选 snapshot source）与 `context_runs`（当前 run 自己的 ID、`artifact_id` 共享、`reused_from_context_run_id` 溯源）。快照命中：保留 live worker handshake（验证 worker 可用性），跳过 classification/parse lookup 与执行、Context 重算与新 payload 写入，仍写当前 run audit/decision/context metadata 并完成 terminal finalization。`InspectRunResponseV2` 是独立观测 interface，不向 `ContextEnvelope v1` 塞字段。

**Tech Stack:** Rust（scanner_core/scanner_contract）、Python pydantic、rusqlite。
**Spec:** `docs/superpowers/specs/2026-08-08-scanner-budget-aware-cache-and-pdf-performance-design.md`（v4）
**前置:** Plan 1（schema v2 / 契约 fixture）、Plan 2（scheduler / guard / 缓存）。

## Global Constraints

- 同 Plan 1/2 约束。
- `context_artifacts` 同时承担非空 context 的去重 payload storage 与可选 snapshot source；所有 Success/Partial run 引用一个 artifact，仅 `snapshot_eligible=true` 的行有 snapshot key。eligible 必须为每个 source file 各有一条 artifact file + decision row（双向约束，insert 与 replay 两处校验）；`false` 的带-warning Success、Partial、migrated payload artifact 完全无这两组 rows。
- `final_envelope_metadata_json` 是内部 storage schema，不冒充 wire；重建 `ContextEnvelope v1` 并重新 validate。
- 快照键 = canonical logical request（去 request_id）+ 有序 discovery rows（含 legacy source_version + SourceGuardV2 kind/hash）+ 归一化 discovery issues + 完整 normalized profile v2 + report_mode + engine build + route-stack worker builds + session capability/contract（或 one-shot marker）+ classifier contract/build/profile。以 domain-separated SHA-256 索引，命中后逐字节比较 `snapshot_key_json`。
- 快照资格：仅 `EngineStatus::Ok`、warnings 空、无 runtime NotParsed/Error/Timeout/unknown/error 分类、无安全 deadline、worker/provenance 完整、ContextBudgetModel 不变量通过。语义/policy NotParsed 允许存在。
- 快照命中当前 run 的 rows：`parse_cache_status=snapshot`、`cache_miss_reason=''`、`parse_duration_ms=0`、`parse_attempt_count=0`；backend/lane/source/content hash 与 classifier provenance 来自 artifact。不得复制源 run 的 miss/hit、旧耗时、旧读页数。
- `InspectRunResponseV2` 字段/顺序冻结（spec Part 5.3）；`FileAuditV2` 字段/顺序冻结；`execution_metrics` 字段/类型/nullability 冻结；migrated v1 run 对 v2 inspect fail closed（`INSPECT_V2_PROVENANCE_UNAVAILABLE`）。
- 温扫目标：7d snapshot warm median ≤330ms/max ≤400ms（Part 6 三段实测后）；30d/90d snapshot warm 比 cache-only warm median 改善 ≥20%。

## File Structure

- `rust/scanner_core/src/artifact.rs`（新建）：artifact/snapshot 关系模型、envelope 重建、快照键。
- `rust/scanner_core/src/store/schema.rs`（修改）：`context_artifacts/context_artifact_files/context_artifact_decisions` 已由 Plan 1 建；补 `context_runs`/`scan_runs` 新列约束。
- `rust/scanner_core/src/inspect.rs`（新建）：Inspect v2 / FileAuditV2 / execution_metrics 装配与投影。
- `rust/scanner_core/src/store/mod.rs`（修改）：artifact 引用保护、snapshot hit finalization、reused_from 选择、orphan GC。
- `rust/scanner_core/src/run.rs`（修改）：快照 lookup 入口。
- `rust/scanner_cli/src/main.rs`（修改）：`inspect-run --response-version 2`、`version --response-version 2`。
- `scripts/benchmark_seed_preparer.py`（新建）：隔离 cache-only seed DB 克隆。
- `src/models/scanner_contract.py`（修改）：InspectRunResponseV2/FileAuditV2 行为侧（类型已在 Plan 1 建）。
- 测试：`rust/scanner_core/tests/artifact_snapshot.rs`、`tests/test_inspect_v2.py`、`scripts/benchmark_seed_preparer.py` 的 fixture 测试。

---

### Task 1: artifact 关系模型 + envelope 重建 + 快照键

**Files:**
- Create: `rust/scanner_core/src/artifact.rs`
- Modify: `rust/scanner_core/src/store/schema.rs`
- Modify: `rust/scanner_core/src/lib.rs`
- Test: `rust/scanner_core/tests/artifact_snapshot.rs`

**Interfaces:**
- Produces:
  - `struct ArtifactDraft { final_context, context_sha256, semantic_summary, file_rows: Vec<ArtifactFileRow>, decision_rows: Vec<ArtifactDecisionRow> }`。
  - `fn snapshot_key(logical_request, discovery, issues, profile, worker_ids, classifier_ids) -> String`（canonical JSON + domain-separated SHA-256）。
  - `fn rebuild_envelope(metadata, current_summary, artifact) -> Result<ContextEnvelope, String>`（重建并 validate `ContextEnvelope v1`）。
  - `struct ArtifactFileRow { file_identity, legacy_source_version, source_guard, parse_/classifier_ provenance（去运行态） }`。

- [ ] **Step 1: 写 failing 测试（正文只存一次 + 重建 validate）**

```rust
// rust/scanner_core/tests/artifact_snapshot.rs
use ai_daily_scanner_core::artifact::{rebuild_envelope, snapshot_key, ArtifactDraft};
use ai_daily_scanner_contract::{ContextEnvelope, Validate};

#[test]
fn snapshot_key_changes_when_report_mode_changes() {
    let k1 = snapshot_key(daily_request(), vec![], vec![], &profile, &workers, &classifier);
    let k2 = snapshot_key(weekly_request(), vec![], vec![], &profile, &workers, &classifier);
    assert_ne!(k1, k2);
}

#[test]
fn rebuilt_envelope_validates_and_omits_file_context_from_scan_runs() {
    let draft = ArtifactDraft { final_context: "# 文件证据上下文\n".into(), /* ... */ };
    let envelope = rebuild_envelope(&metadata, &current_summary, &draft).unwrap();
    envelope.validate().unwrap();
    assert_eq!(envelope.file_context, draft.final_context);
}
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked artifact_snapshot`
Expected: FAIL

- [ ] **Step 3: 实现 artifact.rs**

- `snapshot_key`：canonical JSON 精确包含 spec 列出的全部字段，domain-separated SHA-256 建索引；命中后逐字节比较 `snapshot_key_json`，不只信 hash。
- `rebuild_envelope`：用 `final_envelope_metadata_json`（request/engine/status/warnings/error 小字段）+ 当前 `context_runs` summary + `context_artifacts.final_context` 重建并重新 validate `ContextEnvelope v1`。Success/Partial run 不再在 `scan_runs` JSON 重复正文；Error run 无 artifact，重建空 context。
- `ArtifactDraft` 构造时校验 eligible 双向约束（file/decision row count == semantic summary source_file_count）。

- [ ] **Step 4: schema 约束补全**

在 `store/schema.rs` 补 `context_runs` 与 `context_artifacts` 的 CHECK（eligible ⇔ 两 snapshot key 字段非空；ineligible ⇔ 两者为空；`context_sha256 == SHA-256(final_context)` 在 insert/replay 两处校验）。

- [ ] **Step 5: 全量 + Commit**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add rust/scanner_core/src/artifact.rs rust/scanner_core/src/store/schema.rs rust/scanner_core/src/lib.rs rust/scanner_core/tests/artifact_snapshot.rs
git commit -m "feat: add context artifact model and envelope rebuild with snapshot key"
```

---

### Task 2: 快照命中 finalization + 当前 run 审计语义

**Files:**
- Modify: `rust/scanner_core/src/artifact.rs`
- Modify: `rust/scanner_core/src/store/mod.rs`
- Modify: `rust/scanner_core/src/run.rs`
- Test: `rust/scanner_core/tests/artifact_snapshot.rs`

**Interfaces:**
- Consumes: Task 1 的 `snapshot_key`/`rebuild_envelope`/`ArtifactDraft`。
- Produces: `snapshot_lookup(store, key) -> Option<(artifact_id, source_context_run_id)>`；snapshot hit finalization：先建当前 `context_runs.artifact_id` 引用并临时 protected set，再 retention/orphan sweep（防止误回收刚命中的 artifact）；当前 run rows 以 `parse_cache_status=snapshot`/0ms/0 attempts 生成；`reused_from_context_run_id` 从 transaction 开始前已提交、Success、引用该 artifact 的 context_runs 按 `(finished_at_ms DESC, context_run_id DESC)` 取第一条。

- [ ] **Step 1: 写 failing 测试（命中后源 run 删除仍可用）**

```rust
#[test]
fn snapshot_hit_reuses_artifact_and_current_run_recomputes_timings() {
    // 1) cold run 得 artifact A + source run R1
    // 2) 同 key 新 run R2 → snapshot_hit=true，reused_from=R1，R2 rows 全 snapshot/0ms，
    //    当前 summary 的 durations 为 R2 实测（非 R1 旧值）
    // 3) 删除 R1 → R2 仍可引用 artifact A（artifact-owned rows），reused_from SET NULL 可选
}
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked artifact_snapshot`
Expected: FAIL

- [ ] **Step 3: 实现 snapshot hit finalization**

- `snapshot_lookup` 的 SQL 必须同时选中 artifact 与至少一个已提交、Success、引用该 artifact 的 source `context_runs`；无 source run 的 orphan artifact 不算 hit（按 miss 重算；重算逐字段相同后可 dedup 引用且 `snapshot_hit=false/reused_from=null`）。
- finalization 顺序：先建立当前 `context_runs.artifact_id` 引用 + 把所选 artifact/source run 加入 protected set，再执行 retention/orphan sweep；禁止「旧引用已删、当前引用未建」窗口误回收。
- 当前 run rows 从 artifact 复制并置 snapshot 语义；classification text/no-text 行重建为 cache=snapshot/run pages=0/attempt=0/transport=snapshot；not_classified_by_budget 仍 not_eligible/0/0/not_applicable；pre-classification reject 仍 null。不得复制源 run 的 miss/hit、旧耗时、旧读页数。
- 当前 decisions 从 artifact 复制；当前 timings/worker handshake/snapshot metric 重新计算；`finalize` 校验当前 rows、当前 summary、artifact semantic summary、重建 envelope 四者一致（`store/mod.rs` 既有校验保留）。

- [ ] **Step 4: 全量 + Commit**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add rust/scanner_core/src/artifact.rs rust/scanner_core/src/store/mod.rs rust/scanner_core/src/run.rs rust/scanner_core/tests/artifact_snapshot.rs
git commit -m "feat: implement snapshot hit finalization with current-run audit semantics"
```

---

### Task 3: Inspect v2 + Version v2 + v1 lossy projection

**Files:**
- Create: `rust/scanner_core/src/inspect.rs`
- Modify: `rust/scanner_cli/src/main.rs`
- Modify: `rust/scanner_core/src/context_audit.rs`（execution_metrics 装配）
- Modify: `src/models/scanner_contract.py`、`src/services/rust_context_client.py`
- Test: `tests/test_inspect_v2.py`

**Interfaces:**
- Produces: `InspectRunResponseV2`（字段/顺序冻结，spec Part 5.3）、`FileAuditV2`（冻结）、`execution_metrics`（冻结对象）；`version --response-version 2` 返回 `VersionResponseV2`；v1 inspect 的 4 种 lossy projection warning（`SNAPSHOT_REUSE_PROJECTED_AS_FRESH`、`PARSE_CACHE_NOT_APPLICABLE_PROJECTED_AS_MISS`、`CACHE_MISS_REASON_PROJECTED_AS_NEW_FILE`、`SOURCE_GUARD_NOT_PROJECTED`）。

- [ ] **Step 1: 写 failing 测试（v2 provenance 完整 / migrated fail closed）**

```python
# tests/test_inspect_v2.py
import subprocess, json
def test_inspect_v2_reports_snapshot_reuse(tmp_path):
    # 冷 run → inspect --response-version 2 → artifact_id 非空、reuse_kind=context_snapshot、reused_from 非空
    ...
def test_migrated_v1_run_v2_inspect_fails_closed(tmp_path):
    # Plan 1 迁移的 migrated_v1 run → inspect --response-version 2 → status=error、
    # error_code=INSPECT_V2_PROVENANCE_UNAVAILABLE
    ...
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `uv run pytest tests/test_inspect_v2.py -v`
Expected: FAIL

- [ ] **Step 3: 实现 inspect.rs**

- `InspectRunResponseV2` 严格字段（`{contract,protocol_version,response_version,request_id,scan_run_id,context_run_id,status,run_status,summary,stage_metrics,extension_metrics,files,decisions,warnings,error,artifact_id,reused_from_context_run_id,reuse_kind,execution_metrics}`）；`reuse_kind=context_snapshot|parse_cache|none` 判定规则按 spec Part 5.3。
- `FileAuditV2` 冻结字段；`pdf_classification: PdfClassificationAuditV1` 严格服从 Plan 2 的完整矩阵；`parse_cache_status=fresh|miss|snapshot|not_applicable`、`parse_transport=session|one_shot|rust_in_process|snapshot|not_applicable`。
- `execution_metrics` 全部字段与计数口径按 spec Part 5.3 表；两个 `*_all_hit` 在对应 lookup_count=0 时 null。
- Inspect status=error 的 sentinel：numeric=0、3 个 nullable=null、`snapshot_hit=false`；不代表被检查 run。
- migrated v1 run 请求 v2 → strict error `INSPECT_V2_PROVENANCE_UNAVAILABLE`（不伪造 0/null）。
- v1 inspect 对 full_v2 rows 做 lossy projection 并附对应 warning。

- [ ] **Step 4: version --response-version 2**

`scanner version` 默认仍返回 `VersionResponse v1`（4 个 command 不变）；`version --response-version 2` 返回 `VersionResponseV2`（字段顺序按 Plan 1 Task 2 定义）。`execution_metrics`/`worker_handshake_ms`/`discovery_ms`/`snapshot_lookup_ms`/`current_run_audit_write_ms` 等 timing 用同一 monotonic clock，parallel span 按 wall interval 非求和。

- [ ] **Step 5: 全量 + Commit**

Run: `uv run pytest && cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add rust/scanner_core/src/inspect.rs rust/scanner_core/src/context_audit.rs rust/scanner_cli/src/main.rs src/models/scanner_contract.py src/services/rust_context_client.py tests/test_inspect_v2.py
git commit -m "feat: add inspect v2 with full provenance and version v2 projection"
```

---

### Task 4: 隔离 cache-only seed + snapshot warm 基准

**Files:**
- Create: `scripts/benchmark_seed_preparer.py`
- Modify: `scripts/benchmark_timer_baseline.py`（复用 wall_clock_ms）
- Test: `tests/test_benchmark_seed_preparer.py`

**Interfaces:**
- Produces: cache-only seed DB 克隆（带 marker sidecar、只读源、逐 key 校验 inventory/parse cache/classification cache、删 run/artifact/lease rows）；snapshot warm 三样本流程与 7d ≤330/400ms、30d/90d ≥20% 改善判定。

- [ ] **Step 1: 写 failing 测试（seed 克隆不改源、无旁路）**

```python
# tests/test_benchmark_seed_preparer.py
def test_seed_clone_keeps_caches_and_zeroes_runs(tmp_path):
    cold_db, sha = _cold_run_with_caches(tmp_path)
    before = sha256_file(cold_db)
    seed = prepare_cache_only_seed(src=cold_db, out_dir=tmp_path / "seed", marker=tmp_path / "marker")
    assert sha256_file(cold_db) == before          # 源只读
    # clone 中 inventory/parse/classification cache count/hash 与 cold 后一致，run/artifact/lease count=0
    ...
def test_preparer_fails_closed_on_path_escape(tmp_path):
    # clone 不是 marker 记录的普通文件后代 / reparse point / nonce 错配 → fail closed
    ...
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `uv run pytest tests/test_benchmark_seed_preparer.py -v`
Expected: FAIL

- [ ] **Step 3: 实现 seed preparer**

- preparer **只读**源 DB 并复制到本次 harness 新建的临时目录；绝不原地清理 cold/用户 DB。marker sidecar 固定记录 canonical harness root、canonical clone path、随机 nonce、复制前源 SHA-256；preparer 重新 resolve，要求 clone 是 root 的普通文件后代、非 reparse point、与当前配置/default DB 不同、nonce 匹配，任一不符 fail closed。
- 在 clone 中删除 run/attempt/diagnostic/current-audit/context-run/artifact/lease rows，保留并逐 key 校验 inventory、parse cache、classification cache；要求 `integrity_check=ok`、run/artifact/lease count=0、两类 cache count/hash 与 cold 后一致；关闭连接，保存 seed SHA-256。
- **不向 production binary/profile 暴露 snapshot bypass**；preparer/marker/seed hash 只进 benchmark 证据。

- [ ] **Step 4: snapshot warm 基准**

- 7d：成功 cold run 后，同一隔离 DB 用 3 个全新 request_id 连续跑 3 次 snapshot warm（运行前 DB 查询证明 request_id 不存在、每次新 scan_run_id、`idempotent_replay=false`）；判定 median ≤330ms/max ≤400ms、3 次 `snapshot_hit=true`、context hash 与 cold 完全一致。
- 30d/90d：cache-only warm 的 3 样本各从同一只读 seed 克隆新 DB、各只跑一次同 logical request + 新 request_id、断言 `snapshot_hit=false` + 两类 lookup all_hit；snapshot warm 在另一隔离 DB cold 后用 3 个新 request_id 连续跑。判定 snapshot warm median 比 cache-only warm median 改善 ≥20%，且 final_context/decisions/semantic counts 完全一致。

- [ ] **Step 5: 全量 + Commit**

Run: `uv run pytest && cargo test --manifest-path rust/Cargo.toml --workspace --locked && cargo build --manifest-path rust/Cargo.toml --workspace --release --locked`
Commit:
```bash
git add scripts/benchmark_seed_preparer.py scripts/benchmark_timer_baseline.py tests/test_benchmark_seed_preparer.py
git commit -m "bench: add isolated cache-only seed preparer and snapshot warm baseline"
```

---

## Self-Review

**Spec 覆盖（Plan 3 范围）**：artifact/context_runs 关系模型 + envelope 重建（Part 5.1）→ Task 1；快照命中 finalization + 当前 run 审计 + reused_from + orphan GC（Part 5.2/5.4 + Part 4 引用保护）→ Task 2；Inspect v2 / FileAuditV2 / execution_metrics / v1 lossy projection / Version v2（Part 5.3）→ Task 3；隔离 cache-only seed + snapshot warm 基准（Part 6）→ Task 4。**不涉及**：session、fixed-corpus/真实目录门禁、release 投影——Plan 4。

**占位符检查**：无 TBD；`unimplemented!` 均为 TDD 首步骨架且 Step 给出实现要点。

**类型一致性**：`ArtifactDraft`/`snapshot_key`/`rebuild_envelope` Task 1 定义、Task 2 消费；`InspectRunResponseV2`/`FileAuditV2`/`execution_metrics` 类型来自 Plan 1 Task 2，Task 3 落地行为；`PdfClassificationAuditV1` 来自 Plan 2 Task 3，Task 3 引用；`wall_clock_ms` 来自 Plan 1 Task 1，Task 4 复用。`VersionResponseV2` 字段与 Plan 1 Task 2 定义一致。
