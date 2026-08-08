# Scanner Python session + 性能门禁 实施计划（Plan 4）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 `ai_daily_python_session_v1` 长驻流式 PDF worker（分类+提取同会话、逐请求独立 deadline、v1 one-shot fallback），再跑 fixed-corpus 九态门禁、真实目录手工 acceptance 与 release/requirements 投影全验证。

**Architecture:** Python worker 新增独立 `session-version`/`session` 命令与独立契约 `ai_daily_python_session_v1`（NDJSON 逐请求、单 in-flight、Job Object 超时杀会话重启、v1 one-shot 保留为 capability-absent fallback，Office worker 不升版）。随后用隔离 seed DB 跑 3×3 缓存组合一致性门禁，并对 `D:\01- 工作` 跑 30d/90d 手工 acceptance（反作弊条件），最后验证 release 投影。

**Tech Stack:** Rust（scanner_core 进程/Job Object）、Python（worker 会话）、pypdfium2 + pdfplumber、pytest、uv 0.12.0。
**Spec:** `docs/superpowers/specs/2026-08-08-scanner-budget-aware-cache-and-pdf-performance-design.md`（v4）
**前置:** Plan 1（契约/依赖）、Plan 2（scheduler/分类器/缓存）、Plan 3（Inspect v2/快照/seed）。

## Global Constraints

- 同 Plan 1/2/3 约束。
- 版本决策（不留给实施）：现有单文件 `version`/`parse` 与 `ai_daily_worker_v1` 逐字段不变；Python worker 新增独立命令 `classifier-version`/`classify-pdf`（Plan 2 已建）与 `session-version`/`session`，独立契约 `ai_daily_python_session_v1`；Rust Office worker 完全不升版。旧 worker 对 `session-version` 返回 exit 2 + 严格 `ai_daily_transport` 单 response frame（`INVALID_REQUEST`）→ capability absent，整轮 v1 one-shot；其他非零/坏 JSON/build 不一致 → handshake failure，不静默降级。
- session 不变量：一条 NDJSON request ↔ 一条 response 按 request ID 配对；每 session 同时最多一个 in-flight；hello/request/classification 每 frame 1MiB，parse response 沿用 `worker_response_capture_limit(request)`，stderr 每 in-flight 累计 ≤1MiB；每文件 source-version 前后校验；超时杀整个 Job Object 并重启 session，**不得对同一超时文件无限 fallback**（classify ≤3 attempt、parse ≤3 attempt，两操作分开计数）。
- 参数：`session_concurrency = min(max_workers,4)`（1..8）、`max_requests_per_session=128`、idle TTL=30s、RSS recycle threshold=512MiB；`batch_size` 从配置/profile/文档/测试删除。
- session 只允许 `parse_v1` 执行 `pdf_text_v1`；Python Office/SharePoint 继续 v1 one-shot（不改 office transport）。
- `classifier_build`/`worker_build` 独立；build 改变分别使 classification/parse cache + snapshot miss。
- fixed-corpus 九态门禁：parse cache × classification cache 各 `empty/randomized-partial/full`，每态独立新 DB；无 deadline 时九态 semantic output 完全一致；`text_pdf_coverage=100%`；只有 manifest 指定 NotParsed；无安全 deadline。
- 真实目录手工 acceptance（非 CI）：`D:\01- 工作`、monthly、`summary_pdf_max_pages=5`；30d `(384,370,16,25000)` 目标 median≤20s/max≤25s；90d `(600,800,32,45000)` 目标 median≤40s/max≤50s；每样本 `stage_deadline_exhausted_count==0`、无 runtime NotParsed/Error/Timeout/unknown、`pdfplumber_invocations` 与 no-text=0、`source_guard_unavailable_count=0`、session capability present、fallback_count=0；证据只存聚合值/匿名 hash/硬件，不存真实路径/正文。
- `requirements.lock` 逐字节可再生成（uv 0.12.0）。

## File Structure

- `src/workers/document_parser_worker.py`（修改）：`session-version`/`session` dispatch + NDJSON 主循环。
- `rust/scanner_core/src/process.rs`（修改）：Windows Job Object 生命周期/超时杀/重建。
- `rust/scanner_core/src/parsers/mod.rs`（修改）：session 客户端 adapter（hello → 逐请求 → 重建）；capability 握手。
- `rust/scanner_core/src/session.rs`（新建）：session 参数、attempt 计数、fallback 规则。
- `tests/test_worker_session.py`（新建）。
- `scripts/corpus_gate.py`（新建）：fixed-corpus 九态门禁 runner。
- `scripts/acceptance_real_dir.py`（新建）：真实目录手工 acceptance runner。
- `scripts/corpus_manifest.json`（新建）：sanitized corpus manifest（匿名 hash/count）。
- `tests/test_corpus_gate.py`、`tests/test_requirements_lock.py`（已有）扩展。

---

### Task 1: Python session v1（NDJSON 流式会话）

**Files:**
- Modify: `src/workers/document_parser_worker.py`
- Create: `rust/scanner_core/src/session.rs`
- Modify: `rust/scanner_core/src/process.rs`、`rust/scanner_core/src/parsers/mod.rs`
- Modify: `rust/scanner_core/src/parsers/mod.rs`（握手 capability）
- Test: `tests/test_worker_session.py`

**Interfaces:**
- Produces: `session-version` → `PythonSessionVersionResponseV1 {contract=ai_daily_python_session, protocol_version=1, session_contract_version=ai_daily_python_session_v1, worker_build, classifier_build, supported_operations=[classify_pdf_v1, parse_v1]}`；`session` 首帧 `PythonSessionHelloV1`（build 与 preflight 完全相同，scanner 校验后才发请求）；帧 envelope `{contract, protocol_version, request_id, operation, payload}`；`operation=classify_pdf_v1|parse_v1`。`SessionParams { session_concurrency, max_requests_per_session, idle_ttl_ms, rss_limit_bytes }`。

- [ ] **Step 1: 写 failing 测试（会话往返 + 逐请求配对 + 错配杀会话）**

```python
# tests/test_worker_session.py
import json, subprocess, threading, queue

def _spawn_session():
    # 用当前 venv 解释器以模块方式启动 worker session（等价于 materialized worker 的入口）
    import sys
    p = subprocess.Popen(
        [sys.executable, "-m", "src.workers.document_parser_worker", "session"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    return p

def test_session_hello_then_request_response_pairing(tmp_path):
    p = _spawn_session()
    hello = json.loads(p.stdout.readline())
    assert hello["frame"] == "hello" and hello["session_contract_version"] == "ai_daily_python_session_v1"
    req = {"contract": "ai_daily_python_session", "protocol_version": 1, "request_id": "r1",
           "operation": "classify_pdf_v1",
           "payload": {"file_path": str(tmp_path / "x.pdf"), "source_version": "mtime_ns=1:size=1",
                       "max_pages": 5, "policy_version": "pdf_text_presence_v1"}}
    p.stdin.write((json.dumps(req) + "\n").encode()); p.stdin.flush()
    resp = json.loads(p.stdout.readline())
    assert resp["request_id"] == "r1" and resp["operation"] == "classify_pdf_v1"
    p.stdin.write(b'{"contract":"ai_daily_python_session","protocol_version":1,"request_id":"r2",'
                  b'"operation":"parse_v1","payload":{}}\n'); p.stdin.flush()
    # 错配/坏 payload → 会话应被杀或返回可识别 error；不得静默复用会话
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `uv run pytest tests/test_worker_session.py -v`
Expected: FAIL（`session` 命令不存在）

- [ ] **Step 3: 实现 worker 侧 session 主循环**

在 `document_parser_worker.py` dispatch 增加 `session-version` 与 `session`。`session` 主循环：`sys.stdout` 逐行（`\n`）写严格 JSON，禁止 BOM/日志/多余 stdout；读一行请求 → 校验 contract/protocol/request_id/operation/payload → 执行 `classify_pdf_v1`（复用 Plan 2 `classify_pdf`）或 `parse_v1`（仅 `pdf_text_v1`，复用现有 parse 逻辑 + source-version 前后校验）→ 写一行 typed result。outer `status=ok` 表示 transport/operation 完整执行并携带 typed result（typed `unknown/error` 或 `WorkerParseResponseV1.status=error` 也放 outer ok）；outer `status=error` 仅 transport/session 失败，带 `PythonOperationDiagnosticV1`。错误输出**只写一行 JSON error frame**，随后退出非 0。

- [ ] **Step 4: 实现 Rust 侧 session 客户端**

- `session.rs`：`SessionParams` 默认（concurrency=min(max_workers,4)、128、30_000ms、512MiB）；attempt 上限 classify=3、parse=3（分开计数，禁止把两者混为「单文件共 3 次」）。
- `process.rs`：每个 session/one-shot child 独立 Windows Job Object；杀一个超时请求不连带杀 pool 中其他 session；recycle 条件只在当前 response 完整接收后优雅重建。
- `parsers/mod.rs`：并行 preflight batch（office v1 version、python v1 version、classifier-version、session-version 一次性启动，逻辑校验顺序固定：仅 python v1 version 成功后接受 classifier/session-version 结果）；capability absent → 整轮 v1 one-shot（classify-pdf + parse），不计 degradation；capability 已宣告却失败必须审计，不得无声切回。
- 操作 timeout = min(自身, remaining_to_work_deadline)；operation timeout → 杀该 child Job Object、当前文件 Timeout、重建 session、**该 logical operation 不再 one-shot 重试**。session start/EOF/协议损坏/crash → 重建并重试当前 operation 最多 1 次；第二次失败仅对 retryable 且非 timeout 的 transport failure 允许 one-shot 1 次。绝无递归 fallback。

- [ ] **Step 5: 全量 + Commit**

Run: `uv run pytest tests/test_worker_session.py -v && cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add src/workers/document_parser_worker.py rust/scanner_core/src/session.rs rust/scanner_core/src/process.rs rust/scanner_core/src/parsers/mod.rs tests/test_worker_session.py
git commit -m "feat: add python streaming session contract with per-request deadline"
```

---

### Task 2: fixed-corpus 九态缓存一致性门禁

**Files:**
- Create: `scripts/corpus_gate.py`、`scripts/corpus_manifest.json`
- Test: `tests/test_corpus_gate.py`

**Interfaces:**
- Consumes: Plan 2/3 的 scheduler + seed preparer（复用 cache 预种）。
- Produces: 九态（parse × classification = empty/randomized-partial/full）各自独立新 DB 的语义一致性、`text_pdf_coverage`、NotParsed 集合、no-text `pdfplumber_invocations=0`、分类数值门禁。

- [ ] **Step 1: 写 failing 门禁 runner 测试**

```python
# tests/test_corpus_gate.py
import json, subprocess
from pathlib import Path

MANIFEST = Path(__file__).resolve().parents[1] / "scripts" / "corpus_manifest.json"

def _build_context(db_path, request_id):
    req = {"contract": "ai_daily_context", "protocol_version": 1, "request_id": request_id,
           "work_dir": str(MANIFEST.parent / "corpus"), "start_date": "2026-08-01",
           "end_date": "2026-08-08", "report_mode": "weekly", "compression_profile": None,
           "scan_db_path": str(db_path),
           "scanner_profile": {"schema_version": "scanner_profile_v2"},
           "adapters": {}}
    out = subprocess.run(["rust/target/release/ai-daily-scanner", "build-context"],
                         input=json.dumps(req).encode(), capture_output=True)
    return json.loads(out.stdout)

def test_nine_cache_combo_semantic_output_identical(tmp_path):
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    context_hashes = set()
    for combo in manifest["combos"]:  # parse x classification = 9 种，各独立新 DB
        db = tmp_path / f"{combo}.sqlite3"
        # 预种该组合 cache（empty/randomized-partial/full），随后跑 build-context
        envelope = _build_context(db, f"gate-{combo}")
        assert envelope["status"] != "error"
        context_hashes.add(envelope["summary"]["context_sha256"] if "context_sha256" in envelope["summary"] else envelope["file_context"])
        assert envelope["summary"]["error_file_count"] == 0
    assert len(context_hashes) == 1, f"9 种缓存组合的 context 不一致: {context_hashes}"
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `uv run pytest tests/test_corpus_gate.py -v`
Expected: FAIL

- [ ] **Step 3: 实现 corpus_gate.py**

- `scripts/corpus_manifest.json` 冻结：discovery rows、classification truth、nominal rank、两阶段计划、included/omitted/reason 集合、final_context SHA-256、partial subset/seed。
- 对 9 种组合各建独立新 DB，只按 manifest 预种该组合，运行前断言 artifact/run 表为空（正常 snapshot lookup 必须 miss，不使用 bypass 开关）。
- 判定（spec Part 9.1）：无 deadline 时九态 semantic output 完全一致；`text_pdf_coverage = 成功提取或 parse-cache 命中的 admitted text PDF / admitted text PDF = 100%`（分母 0 时按 100% 并单列 count=0）；只有 manifest 指定的 semantic/policy NotParsed；safety guard 未触发、`pdfplumber_invocations` 等于获得 extraction slot 的 PDF cache misses、no-text 必须 0；Part 3 分类数值门禁独立全绿。
- 输出只写聚合值 + manifest hash。

- [ ] **Step 4: 全量 + Commit**

Run: `uv run pytest tests/test_corpus_gate.py -v && uv run python scripts/corpus_gate.py`
Commit:
```bash
git add scripts/corpus_gate.py scripts/corpus_manifest.json tests/test_corpus_gate.py .artifacts/corpus-gate.json
git commit -m "test: add nine-state cache consistency gate"
```

---

### Task 3: 真实目录手工 acceptance（30d/90d）

**Files:**
- Create: `scripts/acceptance_real_dir.py`
- 证据: `.artifacts/acceptance-real-dir.json`（只存聚合值/匿名 hash/硬件/build）

**Interfaces:**
- Consumes: Plan 3 seed preparer、Plan 4 session。
- Produces: 30d/90d 三次 cold（隔离 DB）的 wall-clock、`stage_deadline_exhausted_count`、golden counts、text_pdf_coverage、session/guard/fallback 指标；30d/90d snapshot vs cache-only warm 对比。

- [ ] **Step 1: 写 acceptance runner（脚本即测试，含反作弊断言）**

```python
# scripts/acceptance_real_dir.py
# 固定：release build、D:\01- 工作、report_mode=monthly、RawScannerProfileV2 summary_pdf_max_pages=5。
# 场景：30d(2026-07-10..2026-08-08, quota 384/370/16/25000)、90d(2026-05-11..2026-08-08, 600/800/32/45000)。
# 每样本：新建隔离 DB（parse/classification/artifact/run 表全空），重启 scanner/Python worker，不清 OS page cache 但证据声明。
# 每个场景 3 个独立 cold DB；另按 Plan 3 做 snapshot vs cache-only warm 对比。
def _assert_sample(sample):
    assert sample["stage_deadline_exhausted_count"] == 0
    assert sample["runtime_not_parsed_count"] == 0 and sample["unknown_count"] == 0
    assert sample["error_count"] == 0 and sample["timeout_count"] == 0
    assert sample["text_pdf_coverage"] == 1.0
    assert sample["no_text_pdfplumber_invocations"] == 0
    assert sample["source_guard_unavailable_count"] == 0
    assert sample["session_capability_present"] is True and sample["session_fallback_count"] == 0
    assert sample["validated"] is True
```

- [ ] **Step 2: 实现并运行（本 Task 是本机手工 acceptance，非 CI）**

- 先运行 Plan 1 Task 1 的 timer-only baseline 确认 wall-clock 口径。
- 跑 `uv run python scripts/acceptance_real_dir.py`，每个场景 3 个 cold DB，报告中位数/max。
- 判定：30d median≤20s/max≤25s；90d median≤40s/max≤50s；每样本反作弊断言通过。真实目录内容若在复评前变化，先生成只含匿名 hash/count 的新 manifest 并人工批准，不放宽门槛。
- 证据只提交聚合值、匿名 corpus hash、硬件/build 信息；**禁止真实路径/文件名/正文/可逆映射**。

- [ ] **Step 3: 记录结论到 `.artifacts/acceptance-real-dir.json` 并 Commit**

Commit:
```bash
git add scripts/acceptance_real_dir.py .artifacts/acceptance-real-dir.json
git commit -m "bench: record real-directory 30d/90d manual acceptance"
```
> 若任一门槛未达标，如实记录原因与占比；不得通过跳过 discovery/validation 或更换 timer 达标。spec 状态保持 Needs revision 直到独立复评。

---

### Task 4: release/requirements 投影 + 全量验证

**Files:**
- Modify: `requirements.lock`（最终再生）
- Modify: `docs/windows-deployment.md`（升级/回滚声明：旧 release 对新 DB TooNew、回滚需恢复升级前 DB 副本、无内置备份）
- Test: `tests/test_requirements_lock.py`（扩展：逐字节比对 + CI 模拟）

**Interfaces:**
- Produces: `requirements.lock` 与冻结 uv export 逐字节一致；Windows install + worker handshake + doctor + fixed corpus 通过；发布说明确认回滚/备份声明。

- [ ] **Step 1: 扩展 lock 测试（含 CI 模拟）**

```python
# tests/test_requirements_lock.py 追加
def test_lock_hashes_and_no_dev_editable():
    export = _export_lock()
    assert "--hash" in export.decode()      # 不使用 --no-hashes
    assert "ai-daily-report" not in export.decode() or "editable" not in export.decode()
```

- [ ] **Step 2: 最终再生与 Windows 验证**

```powershell
uv sync
uv export --frozen --no-dev --no-emit-project --no-header --format requirements.txt --output-file requirements.lock
uv run pytest tests/test_requirements_lock.py -v
uv run python main.py doctor --strict
```
再在 Windows 用 `python -m pip install --requirement requirements.lock` 验证安装链，随后 worker handshake、doctor、fixed corpus。

- [ ] **Step 3: 更新部署文档**

`docs/windows-deployment.md` 增补：v2 schema 升级为单向；`upgrade-db apply=false` 审计、`apply=true` 需单独授权；工具不内置备份，apply 前运维保留升级前 DB 副本（DB+WAL/shm 或部署快照）；回滚会丢失升级后新增 runs；旧 release 对新 DB 返回 `TooNew`。

- [ ] **Step 4: 全量最终验证 + Commit**

Run: `uv run pytest && cargo test --manifest-path rust/Cargo.toml --workspace --locked && cargo build --manifest-path rust/Cargo.toml --workspace --release --locked && uv run python main.py doctor --strict && git diff --check && uv lock --check`
Commit:
```bash
git add requirements.lock uv.lock pyproject.toml docs/windows-deployment.md tests/test_requirements_lock.py
git commit -m "release: freeze requirements projection and document schema upgrade contract"
```

---

## Self-Review

**Spec 覆盖（Plan 4 范围）**：流式 session 契约与生命周期（Part 7）→ Task 1；fixed-corpus 九态门禁（Part 9.1）→ Task 2；真实目录手工 acceptance 反作弊（Part 9.2）→ Task 3；release/requirements 投影 + 部署声明（Part 10 + 8.3 回滚声明）→ Task 4。

**占位符检查**：无 TBD。Task 1 Step 1 的错配测试是 TDD 首步。

**类型一致性**：`session-version`/`PythonSessionVersionResponseV1`/`PythonSessionHelloV1`/帧 envelope 在 Task 1 定义，与 Plan 1 Task 2 的 `session_contract_versions` 常量、Plan 2 Task 3 的 `classify_pdf_v1` payload 一致；`SessionParams` 与 Plan 2 profile v2 的 `session_concurrency/max_requests_per_session/session_idle_ttl_ms/session_rss_limit_bytes` 一致；`wall_clock_ms`/`benchmark_wall_ms` 沿用 Plan 1/3；`execution_metrics.session_restart_count/session_fallback_count/classify_attempt_count/parse_attempt_count` 与 Plan 3 Inspect v2 字段一致。

## 四个计划的依赖链

- **Plan 1（Foundation）** → 无前置，产出 timer 基线、契约 fixtures、schema v2、upgrade-db、lock 溯源。
- **Plan 2（Scheduler Core）** → 依赖 Plan 1 的 profile v2/schema v2/ErrorCode；产出 SourceGuardV2、两阶段准入、预算模型、分类器、scheduler、缓存 GC/maintenance。
- **Plan 3（Snapshot + Inspect v2）** → 依赖 Plan 1/2；产出 artifact 关系模型、快照命中、Inspect v2、seed 基准。
- **Plan 4（Session + Gates）** → 依赖 Plan 1/2/3；产出流式 session、九态门禁、真实目录 acceptance、release 投影。

每个计划独立产出可测试软件；Plan 1 Task 1 的 timer stop-gate 是全局门禁——若 7d 同口径 warm 不可达，四计划全部冻结并维持 spec Needs revision。
