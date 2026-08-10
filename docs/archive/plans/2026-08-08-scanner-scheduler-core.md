# Scanner BudgetedContextScheduler 核心 实施计划（Plan 2）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `run.rs` 里的分散状态机收敛进 `BudgetedContextScheduler` 深 module，落地 SourceGuardV2、确定性两阶段准入计划、唯一 ContextBudgetModel、PDF 分类器与分类缓存、缓存硬上限/GC/maintenance，并让语义 quota 与安全 deadline 分离。

**Architecture:** 新增 `BudgetedContextScheduler::execute(ScheduledRunInput) -> Result<BudgetedScanOutcome, SchedulerFailure>` 单一入口；内部拥有 nominal ranking、ClassificationPlan、ContentAdmissionPlan、分类/cache/parser 合并、Context 渲染与 deadline 终态。ParserScheduler 只执行选中任务，Compressor 保持纯确定性渲染（二者都是 Scheduler 内部实现），ScannerStore 事务化应用 outcome。**依赖 Plan 1 的 profile v2 / schema v2 / ErrorCode。**

**Tech Stack:** Rust（scanner_core）、pypdfium2（Python worker 分类器）、rusqlite。

**Spec:** `docs/superpowers/specs/2026-08-08-scanner-budget-aware-cache-and-pdf-performance-design.md`（v4）
**前置计划:** `docs/superpowers/plans/2026-08-08-scanner-foundation.md`（Plan 1）

## Global Constraints

- 同 Plan 1 全部约束。
- `BudgetedContextScheduler::execute` 是唯一外部执行入口；`ScheduledRunInput` 只含当前 run ID/started_at、不可变 discovery snapshot、`NormalizedScannerProfileV2`、已校验 worker identities、由 `total_deadline_ms` 唯一推导的 monotonic `AbsoluteDeadline`/`WorkDeadline`（`WorkDeadline = AbsoluteDeadline - 2,000ms`，构造器校验同一 origin）。`BudgetedScanOutcome` 一次返回 inventory/file results/decisions、已提交 cache write receipts、artifact draft、diagnostics、metrics、terminal intent；调用方不得在返回后重新决定 action/计数/准入集合。
- 已定义业务终态返回 `Ok(BudgetedScanOutcome)`（terminal intent 表达 Success/Partial/Error）；`Err(SchedulerFailure)` 只表示形成可 validate outcome 之前的 adapter/contract/internal failure，必须携带唯一 scanner-side Diagnostic 与 retryable。
- 语义 quota（nominal charge）：cache hit 与 miss 收相同名额；实际 inspected pages/是否启动进程/真实耗时只记 execution metrics，不返还/追加名额。
- SourceGuardV2：`windows_file_id_change_time_v1`（volume serial+file ID+size+mtime+change time）/ `unix_inode_ctime_v1`（device+inode+size+mtime_ns+ctime_ns）/ 回退 `content_sha256_v1`（完整流式 SHA-256）；全不可用 → `SOURCE_GUARD_UNAVAILABLE` retryable Error，不启动 cache/classifier/parser。guard 进入全部 cache/snapshot key。
- PDF 分类五态：`text_in_parse_window | no_text_in_parse_window | not_classified_by_budget | unknown | error`；只判 `min(page_count, pdf_max_pages)` 窗口。`pdf_text_presence_v1`：任一字符非 whitespace、`unicodedata.category ∉ Cc/Cf/Cs/Co`、非 U+FFFD 即 text。无负缓存。分类成功缓存只存 text/no-text。
- `cache_retention_v1` 硬上限：parse 1GiB、classification 128MiB、artifacts 512MiB、terminal audit 2GiB/500 runs/90 天；opportunistic GC 10ms budget。eviction tuple 按 spec Part 4 表。
- 计数等式与状态矩阵（spec Part 2）为 contract。
- nominal priority 表（spec Part 1.1）。

## File Structure

- `rust/scanner_core/src/source_guard.rs`（新建）：SourceGuardV2 生成与校验。
- `rust/scanner_core/src/nominal.rs`（新建）：nominal rank 唯一实现。
- `rust/scanner_core/src/budget_model.rs`（新建）：ContextBudgetModel + OmittedSummaryPlan。
- `rust/scanner_core/src/admission.rs`（新建）：ClassificationPlan + ContentAdmissionPlan。
- `rust/scanner_core/src/scheduler.rs`（新建）：`BudgetedContextScheduler` 深 module。
- `rust/scanner_core/src/parsers/mod.rs`（修改）：暴露 parser/classifier executor 作本地 adapter；`ParserScheduler` 收缩为只执行。
- `rust/scanner_core/src/compressor.rs`（修改）：共用 `budget_model` 的字符计数与 section 上限，删除「成功后因预算 Omit」分支。
- `rust/scanner_core/src/run.rs`（修改）：只保留 preflight、discovery、调用 Scheduler、terminal finalization。
- `rust/scanner_core/src/store/{mod,new_types}.rs`（修改/新建）：`CachePort`（inventory 前置 upsert + receipt 型 cache transaction）、cache/classification 硬上限、GC、maintenance。
- `src/workers/document_parser_worker.py` + `src/workers/pdf_classifier.py`（新建）：classifier-version / classify-pdf one-shot。
- 测试：`rust/scanner_core/tests/scheduler_core.rs`、`tests/test_pdf_classifier.py`、`tests/test_maintenance.py`、`rust/scanner_core/tests/source_guard.rs`。

---

### Task 1: SourceGuardV2

**Files:**
- Create: `rust/scanner_core/src/source_guard.rs`
- Modify: `rust/scanner_core/src/lib.rs`（注册 module）
- Modify: `rust/discovery/src/lib.rs`（discovery 时产 guard，或由 scanner_core 在 discovery 后补）
- Test: `rust/scanner_core/tests/source_guard.rs`

**Interfaces:**
- Produces: `SourceGuardV2 { kind: SourceGuardKind, guard_sha256: Option<String> }`；`SourceGuardKind = WindowsFileIdChangeTimeV1 | UnixInodeCtimeV1 | ContentSha256V1 | Unavailable`；`fn compute_source_guard(path: &Path) -> io::Result<SourceGuardV2>`；`fn verify_guard(path, expected: &SourceGuardV2) -> bool`。guard 进入 cache key 与 snapshot key。

- [ ] **Step 1: 写 failing 测试**

```rust
// rust/scanner_core/tests/source_guard.rs
use ai_daily_scanner_core::source_guard::{compute_source_guard, SourceGuardKind};

#[test]
fn same_size_and_mtime_replacement_must_change_guard() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.txt");
    std::fs::write(&p, "AAAA").unwrap();
    // 记录并伪造同 size+mtime 的替换内容
    let before = compute_source_guard(&p).unwrap();
    std::fs::write(&p, "BBBB").unwrap();
    let after = compute_source_guard(&p).unwrap();
    // guard 要么不可用（Unavailable，此时上层 fail closed），要么与内容绑定
    match (&before.kind, &after.kind) {
        (SourceGuardKind::Unavailable, _) => {}
        (_, SourceGuardKind::Unavailable) => {}
        _ => assert_ne!(before.guard_sha256, after.guard_sha256),
    }
}
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked source_guard`
Expected: FAIL（module 不存在）

- [ ] **Step 3: 实现 source_guard.rs**

```rust
//! SourceGuardV2：cache/snapshot 内容身份，绑定文件系统身份或完整内容哈希，不依赖 mtime+size 猜测。
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceGuardKind { WindowsFileIdChangeTimeV1, UnixInodeCtimeV1, ContentSha256V1, Unavailable }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGuardV2 { pub kind: SourceGuardKind, pub guard_sha256: Option<String> }

pub fn compute_source_guard(path: &Path) -> io::Result<SourceGuardV2> {
    #[cfg(windows)]
    {
        if let Some(meta) = windows_identity(path)? {
            return Ok(SourceGuardV2 { kind: SourceGuardKind::WindowsFileIdChangeTimeV1, guard_sha256: Some(meta) });
        }
    }
    #[cfg(unix)]
    {
        if let Some(meta) = unix_identity(path)? {
            return Ok(SourceGuardV2 { kind: SourceGuardKind::UnixInodeCtimeV1, guard_sha256: Some(meta) });
        }
    }
    // metadata guard 无法形成 → 完整流式 SHA-256（不以首尾采样冒充）
    match full_content_sha256(path) {
        Some(h) => Ok(SourceGuardV2 { kind: SourceGuardKind::ContentSha256V1, guard_sha256: Some(h) }),
        None => Ok(SourceGuardV2 { kind: SourceGuardKind::Unavailable, guard_sha256: None }),
    }
}

fn full_content_sha256(path: &Path) -> Option<String> {
    let mut hasher = Sha256::new();
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    io::copy(&mut reader, &mut hasher).ok()?;
    Some(hex(&hasher.finalize()))
}
// windows_identity/unix_identity：从同一 opened handle 取 canonical 字段，
// 任一字段缺失/返回平台 sentinel/无法无损规范化 → 返回 None（触发 content 回退或 Unavailable）。
// domain-separated：b"source-guard-v2\0" + 固定字段顺序。
fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }
```

- [ ] **Step 4: 接入 discovery 与消费点**

- discovery 后、消费 cache/snapshot 或启动 worker 前复核 guard；cache value/worker result 进入 Scheduler 前后验复核；snapshot 命中对全部 artifact rows 复核。任一次变化丢弃 value/result 并按 `SOURCE_VERSION_CHANGED` retryable 处理。
- `file_inventory` 的 `source_guard_kind/source_guard_sha256` 随 full-v2 upsert 填写；v2 cache key 含 guard。

- [ ] **Step 5: 跑通 + Commit**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add rust/scanner_core/src/source_guard.rs rust/scanner_core/src/lib.rs rust/discovery/src/lib.rs rust/scanner_core/tests/source_guard.rs
git commit -m "feat: add SourceGuardV2 content identity for cache and snapshot"
```

---

### Task 2: nominal rank + ContextBudgetModel + 两阶段准入计划

**Files:**
- Create: `rust/scanner_core/src/nominal.rs`
- Create: `rust/scanner_core/src/budget_model.rs`
- Create: `rust/scanner_core/src/admission.rs`
- Modify: `rust/scanner_core/src/compressor.rs`
- Test: `rust/scanner_core/tests/scheduler_core.rs`（本任务先落预算模型部分）

**Interfaces:**
- Produces:
  - `fn nominal_rank(relative_path: &str, extension: &str) -> (u64, String, String, String)`（= `(priority, lower_path, path, file_identity)`，优先级按 spec Part 1.1 表）。
  - `struct OmittedSummaryPlan { reservation: u64, detail_slots: Vec<SlotKey> }`；`ContextBudgetModel::new(profile, fixed_sections) -> Result<Self, String>`；`fn reserved_delta(&self, route: &RouteHint, size_bytes: Option<u64>) -> u64`；`fn admits(&self, running: u64, delta: u64) -> bool`；不变量 `reserved_chars(file) >= rendered_chars(file)`，违反返回 `BUDGET_MODEL_MISMATCH`。
  - `enum PlanAction`（admission）：`Admit{route} | NotParsed{reason}`；`ClassificationPlan::build(files, profile, page_budget) -> Vec<ClassifiedPlan>`；`ContentAdmissionPlan::build(classified, profile) -> Vec<AdmissionDecision>`。
- 消费：Compressor 用同一 `ContextBudgetModel` 与计数函数渲染。

- [ ] **Step 1: 写 failing 测试（nominal rank + 预算不变量）**

```rust
// rust/scanner_core/tests/scheduler_core.rs
use ai_daily_scanner_core::nominal::nominal_rank;
use ai_daily_scanner_core::budget_model::ContextBudgetModel;

#[test]
fn nominal_rank_puts_error_before_text_but_order_is_parse_independent() {
    // office/pdf 优先级 20 < 文本 30：位置不依赖解析结果
    let office = nominal_rank(r"\A\b.xlsx", ".xlsx");
    let text = nominal_rank(r"\A\b.md", ".md");
    assert!(office.0 < text.0);
    // 同 priority 用 lower path -> path -> identity 稳定 tie-break
    let a = nominal_rank(r"\B\a.md", ".md");
    let b = nominal_rank(r"\B\b.md", ".md");
    assert!(a.1 < b.1);
}
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked scheduler_core`
Expected: FAIL

- [ ] **Step 3: 实现 nominal.rs**

按 spec Part 1.1 表实现 `nominal_rank`；`relative_path` 的 `/` 转 `\`、trim 首尾、Unicode lowercase 生成 `path_key = "\\" + lower + "\\"`；priority 按第一条命中：`.pytest_cache` segment 或 `\data\benchmarks\` → 70；`logs` segment → 60；office/pdf ext → 20；`.md/.txt` → 30；否则 50。返回四元组 `(priority, relative_path.to_lowercase(), relative_path, file_identity)`。

- [ ] **Step 4: 实现 budget_model.rs**

- 渲染计数用 Unicode scalar（`chars().count()`），不按 UTF-8 bytes/token。
- `OmittedSummaryPlan`：先冻结 `omitted_summary_reservation = min(12_000, floor(global_max_chars × 20%))`；预留内先放 mandatory header + 1 个 catch-all aggregate row；再按 nominal rank 用 `max_omitted_row_chars` 预选 detail slots，下一行放不下即停止；detail 后按 `(reason, extension)` canonical order 放 aggregate rows，放不下合并进唯一 catch-all。整段 ≤ reservation。
- `reserved_delta(file)` = `max(success_section_max, metadata_section_max, bounded_error_section_max)`，全部从 normalized route/parser limits 推导，不读 cache/真实结果。成本覆盖：路径/Markdown 标题、action/reason/backend/lane、最大正文、input/output chars、围栏/换行、metadata 多行、固定提示。
- `base_chars = exact(header + fixed_sections + preexisting_bounded_error_sections) + omitted_summary_reservation`；`base_chars + sum(admitted.reserved_delta) <= global_max_chars`。`base_chars > global_max_chars` → 非重试 `CONTEXT_FIXED_SECTIONS_OVER_BUDGET`。
- `file_context` 禁止嵌入 request_id/run ID/cache status/duration/wall-clock timestamp。
- admitted 真实渲染必须 `rendered_chars <= reserved_chars`，违反返回 `BUDGET_MODEL_MISMATCH`（不 panic、不静默 Omit、不截断其他文件）。

- [ ] **Step 5: 实现 admission.rs（两阶段不可变计划）**

- 阶段 A `ClassificationPlan`（任何 PDF 分类 I/O 前冻结）：先按 Part 2 无 I/O disposition 处理 policy/invariant reject；按 nominal rank 分配前 `max_candidate_files` 个 slots（其余 `NotParsed/semantic_file_quota_exhausted`）；每个候选 PDF 按顺序预留完整 `pdf_max_pages`，不足则 `not_classified_by_budget`；cache hit/miss 同 charge。
- 阶段 B `ContentAdmissionPlan`（分类完成、正文 parse I/O 前冻结）：沿 nominal rank 单趟；普通文件/no-text metadata 的 `reserved_delta` 放得下即 admitted，放不下则 `NotParsed/global_context_budget_exceeded` 并**继续考虑后续更小文件（不做回填）**；text PDF 需 `reserved_delta` 放得下**且**有 extraction slot；no-text admitted → 成功 metadata-only draft（不占 extraction slot）。
- 两个计划冻结后，cache 只决定「复用还是执行」，不得改变 ParseStatus/action/reason/排序/名额。

- [ ] **Step 6: Compressor 共用预算模型**

改 `compressor.rs`：渲染计数函数抽到 `budget_model`（二者共用）；删除「成功后因全局预算 Omit」分支——出现即 `BUDGET_MODEL_MISMATCH` 内部错误路径。

- [ ] **Step 7: 预算不变量 property test**

```rust
#[test]
fn every_route_reserved_covers_rendered() {
    // 对 spec 定义的每种 section（keep/compress/metadata/error）构造最大输入，
    // 断言 reserved_delta >= 实际渲染 chars().count()。
}
```
补齐后用 `rust/scanner_core/tests/scheduler_core.rs` 的 property 循环覆盖全部 route/limits 组合。

- [ ] **Step 8: 跑通 + Commit**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add rust/scanner_core/src/nominal.rs rust/scanner_core/src/budget_model.rs rust/scanner_core/src/admission.rs rust/scanner_core/src/compressor.rs rust/scanner_core/tests/scheduler_core.rs
git commit -m "feat: add nominal ranking and deterministic two-phase admission plan"
```

---

### Task 3: PDF 分类器（pypdfium2 one-shot）+ 分类缓存 + 门禁

**Files:**
- Create: `src/workers/pdf_classifier.py`
- Modify: `src/workers/document_parser_worker.py`（dispatch `classifier-version` / `classify-pdf`）
- Modify: `rust/scanner_core/src/parsers/mod.rs`（classifier 作为本地 adapter 注入）
- Modify: `rust/scanner_core/src/store/schema.rs`（`classification_cache` 表）
- Test: `tests/test_pdf_classifier.py`

**Interfaces:**
- Produces: `classifier-version` → `ClassifierVersionResponseV1 {contract=ai_daily_pdf_classifier, protocol_version=1, classifier_contract_version=ai_daily_pdf_classifier_v1, classifier_build, policy_version=pdf_text_presence_v1, python_implementation, python_version, unicode_data_version, pypdfium2_version, pdfium_version, target_triple}`；one-shot `classify-pdf` → `PdfClassifierResponseV1`；`classifier_build` 为 domain-separated SHA-256（输入：classifier source allowlist、policy、`sys.implementation.name`、`platform.python_version()`、`unicodedata.unidata_version`、exact pypdfium2/PDFium native versions、target triple）。分类缓存表 `classification_cache`（key：file_identity+source_version+SourceGuardV2+classifier_profile_hash+classifier_build；成功值只存 text/no-text + page_count + result_examined_pages + identity）。

- [ ] **Step 1: 写 failing 分类器测试**

```python
# tests/test_pdf_classifier.py
import json
from pathlib import Path
from src.workers.pdf_classifier import classify_pdf

FIXTURES = Path(__file__).parent / "fixtures" / "pdf_benchmark"

def test_text_pdf_is_text_in_parse_window():
    # case_01 是含中文的文本 PDF（fixture 已存在）
    result = classify_pdf(str(FIXTURES / "case_01.pdf"), max_pages=5)
    assert result["status"] == "text_in_parse_window", result

def test_image_pdf_is_no_text_in_parse_window(tmp_path):
    # 用 reportlab 造一张纯图片 PDF（无文字层）
    from reportlab.pdfgen import canvas
    pdf = tmp_path / "img.pdf"
    c = canvas.Canvas(str(pdf)); c.drawImage(FIXTURES_IMAGE_PLACEHOLDER_IF_ANY, 10, 10); c.showPage(); c.save()
    result = classify_pdf(str(pdf), max_pages=5)
    assert result["status"] == "no_text_in_parse_window"
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `uv run pytest tests/test_pdf_classifier.py -v`
Expected: FAIL（module 不存在）

- [ ] **Step 3: 实现 pdf_classifier.py**

```python
"""pypdfium2 文本层检测。pdf_text_presence_v1：窗口内任一有效字符即 text。"""
import pypdfium2 as pdfium
import platform, sys, unicodedata

POLICY_VERSION = "pdf_text_presence_v1"

def _is_valid_text_char(ch: str) -> bool:
    if ch.isspace() or ch == "�":
        return False
    return unicodedata.category(ch) not in ("Cc", "Cf", "Cs", "Co")

def classify_pdf(path: str, max_pages: int, timeout_ms: int = 2000) -> dict:
    pdf = pdfium.PdfDocument(path)
    try:
        page_count = len(pdf)
        window = min(page_count, max_pages)
        for i in range(window):
            page = pdf[i]
            try:
                tp = page.get_textpage()
                text = tp.get_text_range() or ""
            finally:
                tp.close(); page.close()
            if any(_is_valid_text_char(ch) for ch in text):
                return {"status": "text_in_parse_window", "page_count": page_count,
                        "result_examined_pages": i + 1}
        return {"status": "no_text_in_parse_window", "page_count": page_count,
                "result_examined_pages": window}
    finally:
        pdf.close()
```

在 `document_parser_worker.py` dispatch 增加：
- `classifier-version` → 返回 `ClassifierVersionResponseV1`（含 classifier_build）。
- `classify-pdf` → 读取 `PdfClassifierRequestV1`，调 `classify_pdf`，返回 `PdfClassifierResponseV1`（`unknown`/`error` 带 `PythonOperationDiagnosticV1`，不抛裸异常）。

- [ ] **Step 4: Rust 侧 classifier adapter + 分类缓存**

- `parsers/mod.rs`：新增 `PdfClassifierPort`（本地可替换 adapter），生产实现调 `classify-pdf` one-shot 子进程（复用 `run_process`）；测试 adapter 返回内存结果。
- `store/schema.rs`：`classification_cache` 表（含 classifier_profile_hash、classifier_build、status、page_count、result_examined_pages、source guard）。
- 分类缓存 key 固定 `file_identity + source_version + SourceGuardV2 + classifier_profile_hash + classifier_build`；`classifier_profile_hash` canonical payload = policy version + `pdf_max_pages` + `pdf_classification_timeout_ms`（不含全局 page quota/session）。
- **无负缓存**：`unknown/error` 都不写分类缓存。

- [ ] **Step 5: 分类数值门禁**

用 spec Part 3.3 的 manifest（text 30 / no-text 100 / error 5 + 稀疏/CJK/旋转/mixed/OCR-layer/blank/beyond-max_pages/加密/损坏各 ≥3）断言：text false-negative=0；no-text false-positive ≤0.1%；valid fixture unknown/error=0；确定性 error fixture 状态匹配率 100%。

- [ ] **Step 6: 全量 + Commit**

Run: `uv run pytest tests/test_pdf_classifier.py -v && cargo test --manifest-path rust/Cargo.toml --workspace --locked`
Commit:
```bash
git add src/workers/pdf_classifier.py src/workers/document_parser_worker.py rust/scanner_core/src/parsers/mod.rs rust/scanner_core/src/store/schema.rs tests/test_pdf_classifier.py
git commit -m "feat: add pypdfium2 pdf text classifier with classification cache"
```

---

### Task 4: BudgetedContextScheduler 组装 + 状态矩阵 + deadline

**Files:**
- Create: `rust/scanner_core/src/scheduler.rs`
- Modify: `rust/scanner_core/src/run.rs`
- Modify: `rust/scanner_core/src/decision.rs`
- Modify: `rust/scanner_core/src/context_audit.rs`
- Modify: `rust/scanner_core/src/store/mod.rs`（计数校验同步）
- Test: `rust/scanner_core/tests/scheduler_core.rs`（状态矩阵 + deadline + 缓存一致性）

**Interfaces:**
- Consumes: Task 1 guard、Task 2 计划、Task 3 classifier。
- Produces: `BudgetedContextScheduler::execute(ScheduledRunInput) -> Result<BudgetedScanOutcome, SchedulerFailure>`；`TerminalFailure { phase: PreOutcome|PostOutcome, diagnostic, execution_metrics }`；`BudgetedScanOutcome`（inventory/file results/decisions、committed cache receipts、artifact draft、diagnostics、metrics、terminal intent）。`run.rs` 顺序固定：静态校验 → begin_run/idempotent replay → 创建 monotonic deadline → bounded parallel handshakes → discovery → Scheduler → terminal finalization。

- [ ] **Step 0: 改码前表征当前 golden（契约变更前提）**

在**任何** decision.rs/compressor.rs/run.rs 语义改动之前，先对当前 unmodified binary 在 fixed corpus 上记录证据：
- 用现有 `scripts/benchmark_scanner.py` 在 frozen corpus 跑一次，记录 `context_sha256`、included/omitted/reason 集合、各文件 parse 顺序。
- **专门验证当前排序 bug**：构造一个含 Error 文件的 corpus，断言当前实现把 Error 文件排到 priority 80（证明「顺序依赖解析结果」现状），存为 `pre_change_order_evidence`。
- 保存到 `.artifacts/golden-pre-change.json`。这一步是 spec「接受新确定性准入策略前先表征旧行为」的要求——新顺序会改变含 Error run 的最终 context，这是**行为变更**，不是无感重构。

- [ ] **Step 1: 写 failing 状态矩阵测试（缓存无关确定性 + NotParsed 计数）**

```rust
// 在 scheduler_core.rs 追加
use ai_daily_scanner_core::scheduler::{BudgetedContextScheduler, ScheduledRunInput};

#[test]
fn cache_state_does_not_change_semantic_output() {
    // 同一 discovery snapshot + profile：空/部分/全 parse+classification cache
    // → ClassificationPlan、ContentAdmissionPlan、decisions、semantic summary、context hash 完全一致。
    // 用测试 CachePort 注入三种 cache 状态，断言 outcome 语义字段一致。
    unimplemented!("Task 4：三态缓存一致性必须为 contract 测试");
}

#[test]
fn not_parsed_counts_are_derived_not_error() {
    // NotParsed(semantic) → omit + no Diagnostic + not_parsed_count 派生；不进 error metric。
}
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked scheduler_core`
Expected: FAIL

- [ ] **Step 3: 重定义 decision.rs 状态矩阵**

按 spec Part 2 表：`has_error` 仅 `ParseStatus::Error`；`Timeout` → error/timeout_count；`NotParsed`（semantic/policy）→ omit + `budget_reason`（无 error Diagnostic，reason 在允许列表）；`NotParsed(runtime)` → omit + run-level deadline Diagnostic（file row 无伪造 error）。`ContextFileEvidence::validate`：NotParsed 允许无 Diagnostic。`error_code`：Success 与所有 NotParsed 固定空；Error/Timeout 必须等于 final Diagnostic code。删除「成功后因预算 Omit」兼容分支。

- [ ] **Step 4: 计数与审计同步**

- `context_audit.rs`：extension metrics `error_count` 只算 Error；`not_parsed_count` 由 `file_count - success - timeout - error` 派生。
- `store/mod.rs` `validate_context_relations`：`error_count = Error only`；`timeout_count = Timeout`；summary 等式全部落地。
- `ContextSummary.input_chars = sum(decision.input_chars)`；`output` 恒等于 `file_context.chars().count()`（不要求等于 decisions.output_chars 之和）。NotParsed/no-text 无正文时 `input_chars` 用 `size_bytes` 近似，渲染 `~`。

- [ ] **Step 5: 实现 scheduler.rs 主循环与 deadline**

- `ScheduledRunInput` 构造时校验 WorkDeadline = AbsoluteDeadline - 2000ms 同一 origin。
- execute 内部顺序：policy/invariant reject → ClassificationPlan（分类 I/O 前冻结）→ 并发执行被选分类（effective timeout = min(自身, remaining_to_work_deadline)）→ ContentAdmissionPlan（parse I/O 前冻结）→ 分块执行 admitted parse（cache hit 复用 / miss 执行）→ 增量渲染记账 → 形成 `BudgetedScanOutcome`。
- **语义 quota vs 安全 deadline**：quota 耗尽产生确定性 NotParsed（可快照）；WorkDeadline 触发 → 停止启动新工作、终止 in-flight Job Object、未启动→runtime NotParsed、in-flight→Timeout、形成 Partial（有非空 context）或 Error（无），`STAGE_DEADLINE_EXHAUSTED`，**永不生成快照**。AbsoluteDeadline 耗尽 → terminal 未提交 → rollback/Abandoned。
- 已完成且 source-version 后验通过的成功 classification/parse 结果按有界 batch 提交独立短事务（receipt 型）；未完成与可复用 snapshot draft 不提交。每个 cache COMMIT 前检查 `remaining_to_work_deadline>0`；COMMIT 成功即 receipt 权威。
- 257 条 bounded warning projection：run-level 优先，前 256 detail + 1 `DIAGNOSTICS_AGGREGATED`（`stage=internal`、retryable=被折叠任一 true、file_path/backend=null）。

- [ ] **Step 6: run.rs 收敛 + terminal finalization**

- `run.rs` 删除分散状态机，只保留：静态校验 → `begin_run`/idempotent replay → 创建 monotonic deadline → bounded parallel worker handshakes → discovery（含 SourceGuardV2 复核）→ `Scheduler::execute` → terminal finalization。
- `TerminalRecord = Outcome | TerminalFailure`；`TerminalFailure(post_outcome)` 只在打开 terminal transaction 前发现 invariant 破坏时使用（空 rows、零 counts、`artifact_id=null`）；transaction 打开后的失败 rollback/Abandoned，不得二次覆盖。pre-begin failure 返回 IDs=null 空 Error；post-begin 尝试提交 IDs=当前 run 的最小空 Error；其 COMMIT 也未发生才 abandon。

- [ ] **Step 7: 补 deadline 与终态测试**

用 fake clock 在 classification/parse/context 各阶段前触发 deadline，断言：queued → runtime NotParsed；in-flight → Timeout；Partial/Error 判定；cache commit 规则；不可复用 payload；snapshot 禁止。以及：post-begin pre-outcome failure 提交最小空 Error；transaction 后失败不二次覆盖；COMMIT 未发生 → abandon/lease 原子 cleanup。

- [ ] **Step 8: 冻结新 golden + 缓存无关一致性基线**

- 在 Task 4 语义改动全部落地后，对 fixed corpus 重新生成 golden：`context_sha256`、ClassificationPlan/ContentAdmissionPlan、included/omitted/reason 集合、semantic counts。保存为 `.artifacts/golden-post-change.json`，与 pre-change 证据对比，**显式记录含 Error run 的排序差异**。
- 冻结 golden fixture：新建 `tests/fixtures/scanner_golden/manifest.json`（匿名 hash/count，不含真实路径），作为后续所有回归的语义基准。Plan 4 Task 2 的九态门禁消费该 golden。
- **缓存无关确定性是本 Task 的第一验收门槛**：三态一致性测试（Step 1）与九态门禁（Plan 4 Task 2）都以此 golden 为断言目标；任何后续改动不得静默漂移 context_sha256。

- [ ] **Step 9: 全量 + Commit**

Run: `cargo test --manifest-path rust/Cargo.toml --workspace --locked && uv run pytest`
Commit:
```bash
git add rust/scanner_core/src/scheduler.rs rust/scanner_core/src/run.rs rust/scanner_core/src/decision.rs rust/scanner_core/src/context_audit.rs rust/scanner_core/src/store/mod.rs rust/scanner_core/tests/scheduler_core.rs tests/fixtures/scanner_golden/manifest.json .artifacts/golden-pre-change.json .artifacts/golden-post-change.json
git commit -m "feat: assemble BudgetedContextScheduler with frozen golden output"
```

---

### Task 5: 缓存硬上限 + GC + maintenance（gc/incremental_vacuum）

**Files:**
- Modify: `rust/scanner_core/src/store/mod.rs`
- Modify: `rust/scanner_core/src/store/cache.rs`（eviction tuple）
- Modify: `rust/scanner_cli/src/main.rs`（`maintenance` command 路由）
- Test: `tests/test_maintenance.py`

**Interfaces:**
- Consumes: Task 4 的 receipt 型 cache transaction。
- Produces: `cache_retention_v1` 硬上限与 eviction tuple（spec Part 4 表）；`maintenance` command（`MaintenanceRequestV1/ResponseV1`，mode=gc|incremental_vacuum，dry_run）；opportunistic GC（10ms budget，独立事务，只形成 freelist）。

- [ ] **Step 1: 写 failing 维护测试**

```python
# tests/test_maintenance.py
import subprocess, json
from src.models.scanner_contract import MaintenanceRequestV1

def test_maintenance_dry_run_has_zero_mutation(tmp_path):
    # 需要 v2 DB；用 Plan 1 的 v2 空库 fixture
    db = _fresh_v2_db(tmp_path)
    req = MaintenanceRequestV1(contract="ai_daily_scanner_maintenance", protocol_version=1,
                               request_id="m-1", scan_db_path=str(db), mode="gc", dry_run=True)
    out = subprocess.run(["rust/target/release/ai-daily-scanner", "maintenance"],
                         input=req.model_dump_json().encode(), capture_output=True)
    body = json.loads(out.stdout)
    assert body["status"] == "ok"
    assert body["deleted"]["parse_cache_rows"] == 0
    assert body["before"] == body["after"]
    assert body["vacuum"]["status"] == "skipped_dry_run"

def test_incremental_vacuum_on_auto_vacuum_none_fails_cleanly(tmp_path):
    # 构造 auto_vacuum=none 的 v2 库 → incremental_vacuum 必须返回 MAINTENANCE_MODE_UNAVAILABLE/error
    ...
```

- [ ] **Step 2: 跑测试确认 fail**

Run: `uv run pytest tests/test_maintenance.py -v`
Expected: FAIL

- [ ] **Step 3: 实现硬上限与 eviction**

- parse/classification/artifact/terminal-audit 各按 `entry_size_bytes`/`artifact_size_bytes`/`audit_size_bytes` 求和，超过对应 cap 时在同一 work-phase cache transaction 内按唯一 tuple 淘汰（spec Part 4 表）。单 entry 超 cap → skipped receipt warning，不改变 context。
- `generation_rank`：0 = contract/schema/policy 不被当前 release 接受或其 build 非 live registered；其余 1。parse `recompute_rank`：light_text=1、xlsx=2、office=2、sharepoint=3、python_office=4、pdf=10。新 backend 未升级 retention → fail closed profile route invariant。
- 被 `context_runs.artifact_id` 引用的 artifact 不得淘汰；删除 run 后引用计数为 0 才进 orphan GC。
- terminal run GC：先删超 90 天且不在 protected set 的 rows，再按 `(finished_at_ms ASC, scan_run_id ASC)` 删到 ≤500 runs 且 ≤2GiB；为当前 record 腾挪用「现存+当前 prepared」比较，删尽仍超 → fail closed。
- `last_accessed_bucket` 用 UTC 日期，同一行同一天最多更新一次，批量 hit 一个事务更新。

- [ ] **Step 4: 实现 opportunistic GC 与 maintenance**

- opportunistic GC：terminal COMMIT 成功且 `remaining_to_absolute_deadline ≥ 10ms` 时，独立事务、`busy_timeout=0`、带索引的单个有界 delete batch，每 statement 前检查 10ms budget。只形成 freelist，不 vacuum。busy/超时/overshoot 不改写已提交 terminal result。
- `maintenance` command：独占 lease → before sizes → pre integrity（`PRAGMA integrity_check` + `foreign_key_check` + entry_size 全量重算 + artifact 不变量）→ mode preflight →（dry-run 结束）→ 深度 row GC transaction → 所选 vacuum → post integrity → after sizes。mode 只 `gc|incremental_vacuum`，**无 full_vacuum**。v1 DB → `SCHEMA_UPGRADE_REQUIRED`。`auto_vacuum=none` 时 `incremental_vacuum` → `MAINTENANCE_MODE_UNAVAILABLE`/error，不回报 ok。失败路径 deleted/before/after 报告真实部分进展。

- [ ] **Step 5: 全量 + Commit**

Run: `uv run pytest && cargo test --manifest-path rust/Cargo.toml --workspace --locked && cargo build --manifest-path rust/Cargo.toml --workspace --release --locked`
Commit:
```bash
git add rust/scanner_core/src/store/mod.rs rust/scanner_core/src/store/cache.rs rust/scanner_cli/src/main.rs tests/test_maintenance.py
git commit -m "feat: enforce cache hard caps with GC and maintenance command"
```

---

## Self-Review

**Spec 覆盖（Plan 2 范围）**：SourceGuardV2（spec 全局）→ Task 1；nominal rank + 两阶段计划 + ContextBudgetModel（Part 1）+ Compressor 共用（Part 1.3）→ Task 2；PDF 分类器 + 分类缓存 + 数值门禁（Part 3）→ Task 3；BudgetedContextScheduler 组装 + 状态矩阵 + 计数 + deadline + run.rs 收敛（Solution + Part 2）→ Task 4；缓存硬上限 + GC + maintenance（Part 4）→ Task 5。**不涉及**：artifact/快照关系模型、Inspect v2、session、fixed-corpus/真实目录门禁、release 投影——分别 Plan 3/4。

**占位符检查**：Task 4 Step 1 的 `unimplemented!` 是 TDD failing-test 骨架，Step 3-7 给出实现要点；Task 1 测试的 Unavailable 分支是合法语义（guard 不可用即 fail closed）。无 TBD。

**类型一致性**：`ScheduledRunInput`/`BudgetedScanOutcome`/`SchedulerFailure` 在 Task 4 定义并被 run.rs 消费；`SourceGuardV2`/`SourceGuardKind` Task 1 定义、Task 3 进分类缓存 key；`NormalizedScannerProfileV2` 来自 Plan 1 Task 2；`MaintenanceRequestV1/ResponseV1` 类型来自 Plan 1 Task 2、本 Plan Task 5 落地行为；`PdfClassifierResponseV1`/`ClassifierVersionResponseV1` Task 3 定义、Plan 4 session 复用同一 typed payload。跨计划/任务接口名一致。
