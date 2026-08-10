# 最大化保留文件原文的上下文压缩改进 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将上下文压缩改为「大预算 + 结构感知双端兜底」：全局 500k / 单文件 100k 字符默认值使绝大多数文件全文逐字进入 LLM 上下文；仅超预算巨文件用头+尾（`.log` 尾优先）兜底切割，契约与预算模型不变量零改动。

**Architecture:** 改动集中在 Rust core：`compressor.rs` 的 `render_file_section` 把「砍头」替换为「头+尾逐字保留」（行边界切割 + 省略标记，body ≤ limit 结构性成立）；`config.rs` 两个归一化分支的默认值全面提额；`scanner_contract` 压缩策略版本常量 v2→v3 使缓存失效。Python 侧零代码改动，只更新两份 YAML 配置与文档。

**Tech Stack:** Rust (ai-daily-scanner-core / ai-daily-scanner-contract, cargo workspace `rust/`)，Python (uv，仅改配置/文档)。

## Global Constraints

- 逐字保留，无 LLM 参与压缩；所有保留内容必须与原文逐字一致。
- 默认值：全局 `global_max_chars=500_000`、单文件 `per_file_max_chars=100_000`，日/周/月三模式统一；summary 档与 daily 档解析上限统一。
- 字符计数一律使用 `count_chars`（Unicode scalar，== `str::chars().count()`）；禁止字节/按 UTF-8 码元切割。
- 契约 wire 形状、`ContextDecision` 字段、`BudgetError`/`BUDGET_MODEL_MISMATCH` 语义零改动。
- 不变量：每个文件的 `rendered ≤ reserved`（rendered = `count_chars(body)`，body 含标记）必须成立；违反即 `BUDGET_MODEL_MISMATCH` 非重试错误。
- `COMPRESSION_POLICY_VERSION` 常量 v2→v3（`rust/scanner_contract/src/lib.rs:704`）；v1 分支的 `require_const` 固定 `"markdown_context_v1"`（lib.rs:1458）与 config.rs:171 硬编码**保持不动**——v1 是 legacy 路径，其预算字段默认值变化已足以使缓存身份失效。
- 省略标记计入 `output_chars` 与 body 预算：`OMITTED_MARKER_RESERVE = 64` 字符预留（实际标记最长 34 字符）。
- 周/月 summary 档与日报档所有解析上限统一（同一数值），保留 summary_mode 代码路径供配置覆盖。

---

### Task 1: compressor 双端兜底切割（TDD）

**Files:**
- Modify: `rust/scanner_core/src/compressor.rs` — `render_file_section` 的 body 分支（`:298-310`）、删除 `take_prefix_chars`/`take_suffix_chars`（`:455-462`）、测试模块（`:479-613`）
- Modify: `rust/scanner_core/tests/context_pipeline.rs` — `golden_large_log_uses_the_recent_tail_once`（`:106-119`）

**Interfaces:**
- Produces（本任务内定义，Task 2-5 不依赖这些符号）:
  - `fn newline_boundary_at_or_before(content: &str, position: usize) -> usize` — 最后一个 index < position 的 `'\n'` 之后的位置；无则返回 `position`（返回 ≤ position）
  - `fn newline_boundary_at_or_after(content: &str, position: usize) -> usize` — 第一个 index ≥ position 的 `'\n'` 之后的位置；无则返回 `position`（返回 ≥ position）
  - `fn omitted_marker(prefix: &str, omitted: u64) -> String` — `…（已省略{prefix}约 {omitted} 字符）…`（prefix ∈ {"头部", "中部"}）
  - `fn take_head_and_tail(content: &str, limit: usize) -> String` — 头 40% + 尾 60%（`HEAD_RATIO_PER_MILLE = 400`），行边界切割，中缝 marker，`count_chars(body) ≤ limit` 恒成立
  - `fn take_log_tail(content: &str, limit: usize) -> String` — 尾部预算 = limit − 64，前缀 marker，body ≤ limit
  - `const OMITTED_MARKER_RESERVE: usize = 64`（模块级）
- Consumes: `count_chars` / `MAX_U64_DIGITS`（已在 `use crate::budget_model::...`，compressor.rs:7-10）

- [ ] **Step 1: 写失败测试（compressor.rs 测试模块）**

在测试模块顶部追加 evidence/profile helper（替换/补充现有 helper；`use` 需补 `AuditWorkerLane` 与 `ParseStatus`，参照 context_pipeline.rs 的 `evidence()`）：

```rust
fn head_tail_evidence(path: &str, extension: &str, content: &str) -> ContextFileEvidence {
    ContextFileEvidence {
        file_identity: format!("fixture:{path}"),
        absolute_path: format!("C:\\fixture\\{}", path.replace('/', "\\")),
        relative_path: path.replace('/', "\\"),
        extension: extension.to_string(),
        size_bytes: Some(content.len() as u64),
        content: content.to_string(),
        parser_backend: "light_text_v1".to_string(),
        worker_lane: AuditWorkerLane::RustCore,
        cache_status: CacheStatus::Miss,
        parse_status: ParseStatus::Success,
        truncated: false,
        error: None,
        reason: None,
    }
}

fn head_tail_profile(global_max_chars: u64, per_file_max_chars: u64) -> ContextProfile {
    ContextProfile {
        profile_name: "daily_balanced_v1".to_string(),
        global_max_chars,
        per_file_max_chars,
        small_file_max_bytes: 65_536,
        medium_file_max_bytes: 1_048_576,
        large_file_max_bytes: 10_485_760,
        priority_policy_version: "default_v1".to_string(),
        compression_policy_version: "markdown_context_v1".to_string(),
    }
}
```

替换原 `character_helpers_preserve_unicode_boundaries` 为边界 helper 测试（**先让编译失败**——新函数尚不存在）：

```rust
#[test]
fn boundary_helpers_respect_unicode_char_positions() {
    // 位置: 0甲 1\n 2乙 3丙 4\n 5丁
    let content = "甲\n乙丙\n丁";
    assert_eq!(newline_boundary_at_or_before(content, 6), 5);
    assert_eq!(newline_boundary_at_or_before(content, 2), 2);
    assert_eq!(newline_boundary_at_or_before(content, 0), 0);
    assert_eq!(newline_boundary_at_or_after(content, 0), 2);
    assert_eq!(newline_boundary_at_or_after(content, 3), 5);
    assert_eq!(newline_boundary_at_or_after(content, 5), 5);
}
```

新增 head+tail 测试：

```rust
#[test]
fn head_tail_cuts_at_line_boundaries_and_counts_marker_chars() {
    // 8 行 × 50 字符/行 + 7 个换行 = 407 字符
    let content = (0..8)
        .map(|i| format!("第{i}行") + &"字".repeat(47))
        .collect::<Vec<_>>()
        .join("\n");
    let limit = 300_usize;
    let body = take_head_and_tail(&content, limit);

    assert!(count_chars(&body) <= limit as u64);
    // head = 第0行(50) + '\n' = 51 字符；tail = 第6行(50)+'\n'+第7行(50) = 101 字符
    assert!(body.starts_with(&format!("第0行{}\n", "字".repeat(47))));
    assert!(body.ends_with(&format!("第7行{}", "字".repeat(47))));
    // 省略 407 - 51 - 101 = 255
    assert!(body.contains("省略中部约 255 字符"));
    // 头部必须结束于行边界（标记紧随换行）
    let marker_index = body.find("…（已省略中部").expect("marker must exist");
    assert!(body[..marker_index].ends_with('\n'));
}

#[test]
fn head_tail_keeps_partial_first_line_when_no_boundary_in_head_budget() {
    let content = format!("{}尾行", "字".repeat(300)); // 303 字符，第一行无换行
    let limit = 200_usize;
    let body = take_head_and_tail(&content, limit);
    assert!(count_chars(&body) <= limit as u64);
    assert!(body.starts_with(&"字".repeat(54))); // head_budget = (200-64)*40% = 54
    assert!(body.ends_with("尾行"));
}

#[test]
fn log_tail_keeps_recent_content_with_head_marker() {
    let content = format!("{}RECENT_TAIL", "old-".repeat(200)); // 810 字符
    let limit = 300_usize;
    let body = take_log_tail(&content, limit);
    assert!(count_chars(&body) <= limit as u64);
    assert!(body.ends_with("RECENT_TAIL"));
    assert!(body.contains("省略头部约 574 字符")); // 810 - 236 = 574
    assert!(!body.contains(&"old-".repeat(60))); // 240 字符 > 236 尾部预算
}

#[test]
fn build_context_renders_head_and_tail_for_long_file() {
    let content = (0..8)
        .map(|i| format!("第{i}行") + &"字".repeat(47))
        .collect::<Vec<_>>()
        .join("\n");
    let result = build_context(
        vec![head_tail_evidence("notes/long.md", ".md", &content)],
        &head_tail_profile(100_000, 300),
        ReportMode::Daily,
    )
    .expect("long file context");
    assert_eq!(result.decisions[0].decision.action, ContextAction::Compress);
    assert!(result.decisions[0].decision.truncated);
    assert!(result.decisions[0].decision.output_chars <= 300);
    assert!(result.content.contains("第0行"));
    assert!(result.content.contains("第7行"));
    assert!(result.content.contains("省略中部"));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd rust && cargo test -p ai-daily-scanner-core --lib compressor`
Expected: FAIL — `newline_boundary_at_or_before` / `take_head_and_tail` / `take_log_tail` 未定义；`character_helpers_preserve_unicode_boundaries` 引用的 `take_prefix_chars` 仍存在但旧测试已被替换。

- [ ] **Step 3: 实现双端兜底**

在 `render_file_section` 的 body 分支（compressor.rs:300-310）替换为：

```rust
    let input_count = count_chars(&evidence.content);
    let limit = profile.per_file_max_chars();
    let body = if input_count > limit {
        decision.action = ContextAction::Compress;
        decision.truncated = true;
        if evidence.extension == ".log" {
            take_log_tail(&evidence.content, limit as usize)
        } else {
            take_head_and_tail(&evidence.content, limit as usize)
        }
    } else {
        evidence.content.clone()
    };
```

替换 `take_prefix_chars`/`take_suffix_chars`（`:455-462`）为新实现（模块级常量与函数）：

```rust
const OMITTED_MARKER_RESERVE: usize = 64;
const HEAD_RATIO_PER_MILLE: usize = 400;

fn omitted_marker(prefix: &str, omitted: u64) -> String {
    format!("…（已省略{prefix}约 {omitted} 字符）…")
}

fn newline_boundary_at_or_before(content: &str, position: usize) -> usize {
    content
        .chars()
        .take(position)
        .enumerate()
        .filter(|(_, character)| *character == '\n')
        .map(|(index, _)| index + 1)
        .last()
        .unwrap_or(position)
}

fn newline_boundary_at_or_after(content: &str, position: usize) -> usize {
    match content.chars().enumerate().skip(position).find(|(_, character)| *character == '\n') {
        Some((index, _)) => index + 1,
        None => position,
    }
}

/// 头+尾逐字保留：头 40% + 尾 60%，切点回退/前进到行边界（区域内无换行时
/// 按字符截断），中缝插入省略标记。边界移动只会缩短头/尾，因此
/// `count_chars(body) <= limit` 结构性成立（marker ≤ 64 预留）。
fn take_head_and_tail(content: &str, limit: usize) -> String {
    let total = count_chars(content);
    let available = limit.saturating_sub(OMITTED_MARKER_RESERVE).max(1);
    let head_budget = available * HEAD_RATIO_PER_MILLE / 1000;
    let tail_budget = available - head_budget;
    let head_end = newline_boundary_at_or_before(content, head_budget);
    let tail_start = newline_boundary_at_or_after(content, total as usize - tail_budget);
    let head = content.chars().take(head_end).collect::<String>();
    let tail = content.chars().skip(tail_start).collect::<String>();
    let omitted = total - count_chars(&head) - count_chars(&tail);
    format!("{head}{}{tail}", omitted_marker("中部", omitted))
}

/// `.log` 尾部优先：保留最后 `limit - 64` 字符（逐字后缀），前缀头部省略标记。
fn take_log_tail(content: &str, limit: usize) -> String {
    let total = count_chars(content);
    let available = limit.saturating_sub(OMITTED_MARKER_RESERVE).max(1);
    let tail_start = (total as usize).saturating_sub(available);
    let tail = content.chars().skip(tail_start).collect::<String>();
    let omitted = total - count_chars(&tail);
    format!("{}{tail}", omitted_marker("头部", omitted))
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd rust && cargo test -p ai-daily-scanner-core --lib compressor`
Expected: PASS（含新旧全部用例）。

- [ ] **Step 5: 更新 .log golden 集成测试并跑全 crate 测试**

`tests/context_pipeline.rs` 的 `golden_large_log_uses_the_recent_tail_once`（`:106-119`）整体替换为：

```rust
#[test]
fn golden_large_log_uses_the_recent_tail_once() {
    let content = format!("{}RECENT_TAIL", "old-".repeat(200));
    let result = build_context(
        vec![evidence("logs/app.log", ".log", &content)],
        &profile(2_000, 300),
        ReportMode::Daily,
    )
    .expect("golden context");

    assert!(result.content.contains("RECENT_TAIL"));
    assert!(result.content.contains("省略头部"));
    assert!(!result.content.contains(&"old-".repeat(60)));
    assert_eq!(result.decisions[0].decision.action, ContextAction::Compress);
    assert_eq!(result.decisions[0].decision.reason, "large_log_tail");
    assert!(result.decisions[0].decision.output_chars <= 300);
}
```

（原断言 `output_chars == 48` 与 48 字符预算不兼容 marker 预留，改为 `<= 300` 并断言 marker 存在。）

Run: `cd rust && cargo test -p ai-daily-scanner-core`
Expected: PASS 全绿。

- [ ] **Step 6: Commit**

```bash
git add rust/scanner_core/src/compressor.rs rust/scanner_core/tests/context_pipeline.rs
git commit -m "feat: head+tail verbatim truncation fallback in context compressor

超预算文件改为头40%+尾60%行边界逐字保留, .log 尾部优先;
省略标记计入预算, rendered <= reserved 结构性成立。"
```

---

### Task 2: 默认值提额 + 压缩策略版本 bump（TDD）

**Files:**
- Modify: `rust/scanner_core/src/config.rs` — v1 分支（`:24,26,28,47-55,59-67,71-73,117,121`）与 v2 分支（`:254,256,258,277-285,289-297,301-303,355,359`）
- Modify: `rust/scanner_contract/src/lib.rs:704` — 版本常量
- Test: `rust/scanner_core/tests/contract_v2.rs` — 新增默认值断言测试

**Interfaces:**
- Produces: 归一化后 `NormalizedScannerProfileV2.context.global_max_chars == 500_000`、`per_file_max_chars == 100_000`、`compression_policy_version == "markdown_context_v3"`（三模式统一）；`parse.text.max_chars == 100_000`、`read_head_bytes/read_tail_bytes == 2*1024*1024`、`parse.pdf.max_pages == 100`、`parse.office.excel_max_rows == 20_000`、`document_excerpt_max_chars == 100_000`、`parse.aggregate_max_chars == 500_000`
- Consumes: 无（不依赖 Task 1 的符号；任务可并行）

- [ ] **Step 1: 写失败测试（contract_v2.rs）**

在 `normalize_v2_merges_frozen_report_mode_defaults` 测试后新增（该文件已 import `normalize_scanner_profile_v2`、`RawScannerProfileV2`、`ScannerProfile`、`ReportMode`）：

```rust
#[test]
fn normalize_defaults_preserve_full_file_content_budget() {
    use ai_daily_scanner_core::config::{normalize_scanner_profile, normalize_scanner_profile_v2};

    for mode in [
        ReportMode::Daily,
        ReportMode::Weekly,
        ReportMode::Monthly,
    ] {
        let raw_v2: RawScannerProfileV2 = serde_json::from_value(serde_json::json!({
            "schema_version": "scanner_profile_v2"
        }))
        .expect("minimal v2 raw profile should decode");
        let normalized = normalize_scanner_profile_v2(&ScannerProfile::V2(raw_v2), mode)
            .expect("minimal v2 profile should normalize");
        assert_eq!(normalized.context.global_max_chars, 500_000, "{mode:?}");
        assert_eq!(normalized.context.per_file_max_chars, 100_000, "{mode:?}");
        assert_eq!(normalized.context.compression_policy_version, "markdown_context_v3");
        assert_eq!(normalized.parse.text.max_chars, 100_000, "{mode:?}");
        assert_eq!(normalized.parse.text.read_head_bytes, 2 * 1024 * 1024, "{mode:?}");
        assert_eq!(normalized.parse.text.read_tail_bytes, 2 * 1024 * 1024, "{mode:?}");
        assert_eq!(normalized.parse.pdf.max_pages, 100, "{mode:?}");
        assert_eq!(normalized.parse.office.excel_max_rows, 20_000, "{mode:?}");
        assert_eq!(normalized.parse.office.excel_max_sheets, 100, "{mode:?}");
        assert_eq!(normalized.parse.office.docx_max_paragraphs, 50_000, "{mode:?}");
        assert_eq!(normalized.parse.office.pptx_max_slides, 500, "{mode:?}");
        assert_eq!(normalized.parse.office.document_excerpt_max_chars, 100_000, "{mode:?}");
        assert_eq!(normalized.parse.aggregate_max_chars, 500_000, "{mode:?}");

        let raw_v1: RawScannerProfileV1 = serde_json::from_value(serde_json::json!({
            "schema_version": "scanner_profile_v1"
        }))
        .expect("minimal v1 raw profile should decode");
        let v1 = normalize_scanner_profile(&raw_v1, mode).expect("minimal v1 profile should normalize");
        assert_eq!(v1.context.global_max_chars, 500_000, "{mode:?}");
        assert_eq!(v1.context.per_file_max_chars, 100_000, "{mode:?}");
        assert_eq!(v1.parse.text.max_chars, 100_000, "{mode:?}");
        assert_eq!(v1.parse.office.document_excerpt_max_chars, 100_000, "{mode:?}");
        assert_eq!(v1.parse.pdf.max_pages, 100, "{mode:?}");
        assert_eq!(v1.parse.aggregate_max_chars, 500_000, "{mode:?}");
    }
}
```

（`RawScannerProfileV1` 全部字段为 `Option` + serde default，最小 JSON 可解码——已验证 `rust/scanner_contract/src/lib.rs:282` 起。）

- [ ] **Step 2: 运行测试确认失败**

Run: `cd rust && cargo test -p ai-daily-scanner-core --test contract_v2 normalize_defaults_preserve_full_file_content_budget`
Expected: FAIL（断言旧默认值 50_000/8_000/6_000/5/50 等）。

- [ ] **Step 3: 实现提额**

`rust/scanner_core/src/config.rs`，v1 分支（`normalize_scanner_profile`）逐处替换：

| 行 | 现值 | 新值 |
|---|---|---|
| 24 | `raw.summary_text_max_chars.unwrap_or(2_000)` | `unwrap_or(100_000)` |
| 26 | `raw.text_max_chars.unwrap_or(6_000)` | `unwrap_or(100_000)` |
| 28 | `raw.direct_text_max_bytes.unwrap_or(262_144)` | `unwrap_or(2 * 1024 * 1024)` |
| 47-55（summary 档） | 2 / 10 / 12 / 80 / 8 / 20 / 8 / 15 / 2 | `100 / 20_000 / 50 / 50_000 / 100 / 10_000 / 50 / 500 / 100` |
| 59-67（daily 档） | 5 / 50 / 20 / 200 / 20 / 50 / 12 / 50 / 5 | `100 / 20_000 / 50 / 50_000 / 100 / 10_000 / 50 / 500 / 100` |
| 71-73 | `(daily_balanced_v1, 50_000, 8_000)` / `(weekly_balanced_v1, 50_000, 5_000)` / `(monthly_balanced_v1, 60_000, 4_000)` | 均为 `("…_balanced_v1", 500_000, 100_000)`（profile_name 不变） |
| 117 | `raw.total_max_chars.unwrap_or(50_000)` | `unwrap_or(500_000)` |
| 121 | `raw.log_tail_read_bytes.unwrap_or(262_144)` | `unwrap_or(2 * 1024 * 1024)` |

v2 分支（`normalize_scanner_profile_v2_raw`）同样替换：`:254` 2_000→100_000、`:256` 6_000→100_000、`:258` 262_144→2MB、`:277-285` 与 `:289-297` 同上表、`:301-303` 同上、`:355` 50_000→500_000、`:359` 262_144→2MB。

> 注意：v1 分支 `:171` 的 `compression_policy_version: "markdown_context_v1"` **保持不动**（`ContextProfile::validate` 的 `require_const` 在 scanner_contract lib.rs:1458 固定该值）。

`rust/scanner_contract/src/lib.rs:704`：

```rust
pub const COMPRESSION_POLICY_VERSION: &str = "markdown_context_v3";
```

（v2 的 `ContextProfileV2::validate` 引用该常量，自动一致；无需改校验代码。）

- [ ] **Step 4: 运行测试确认通过**

Run: `cd rust && cargo test -p ai-daily-scanner-core --test contract_v2`
Expected: PASS（新旧用例全绿；`v2_quota_defaults` 等既有断言不受影响）。

- [ ] **Step 5: Commit**

```bash
git add rust/scanner_core/src/config.rs rust/scanner_contract/src/lib.rs rust/scanner_core/tests/contract_v2.rs
git commit -m "feat: raise context budget defaults to 500k global / 100k per-file

三模式统一: summary 档与 daily 档解析上限对齐, 读头/读尾放宽到 2MB,
压缩策略版本 v2→v3 使缓存 profile 失效重建。"
```

---

### Task 3: 配置文件更新（settings.example.yaml + settings.windows.yaml）

**Files:**
- Modify: `config/settings.example.yaml`（scanner 段 `:56-70`）
- Modify: `config/settings.windows.yaml`（scanner 段 `:55-69`）

**Interfaces:**
- Produces: 两份 YAML 的 wire 叶子显式钉住新默认值（覆盖 Rust 默认；键名不变）
- Consumes: 无

- [ ] **Step 1: 更新两份 YAML 的 scanner 段**

`settings.example.yaml`（`:56-70`）替换为：

```yaml
  max_workers: 4
  # 大预算: 全局 500k / 单文件 100k 字符, 预算内正文逐字保留。
  excel_max_rows: 20000
  pdf_max_pages: 100
  text_max_chars: 100000
  summary_excel_max_rows: 20000
  summary_pdf_max_pages: 100
  summary_text_max_chars: 100000
  total_max_chars: 500000
  max_file_size_mb: 50
  file_timeout_seconds: 30
  # PDF 页数放宽后单文件解析可能超过默认 45s, 同步放宽超时。
  file_timeout_by_extension:
    ".pdf": 120
    ".xlsx": 60
    ".xls": 60
  parser_profile_version: "v1"
```

`settings.windows.yaml` 做同样替换（`:56-64` 起，保持其后的 `pdf_classification_timeout_ms: 10000` / `total_deadline_ms: 60000` 等本地覆盖不动；`.pdf` 45 → 120）。

- [ ] **Step 2: 校验配置可加载**

Run: `uv run python main.py doctor`
Expected: doctor 通过，无 scanner 配置校验错误。

- [ ] **Step 3: Commit**

```bash
git add config/settings.example.yaml config/settings.windows.yaml
git commit -m "config: pin generous context budgets in settings (500k/100k chars)"
```

---

### Task 4: 文档更新（spec 校正 + scanner-backends.md + CLAUDE.md）

**Files:**
- Modify: `docs/superpowers/specs/2026-08-10-max-preserve-context-design.md`（§3 版本行、§4 不变量规则、§6 测试名）
- Modify: `docs/scanner-backends.md`（`:84-94` Cache identity 节后追加）
- Modify: `CLAUDE.md`（Key Patterns 扫描策略行）

**Interfaces:**
- Consumes: 无（纯文档，可与 Task 1-3 并行）

- [ ] **Step 1: 校正 spec**

`docs/superpowers/specs/2026-08-10-max-preserve-context-design.md` 三处：

1. §3 表格版本行（`:53`）：`markdown_context_v1` → **`markdown_context_v2`** 改为 `markdown_context_v2` → **`markdown_context_v3`**（现状常量已是 v2，实测 `rust/scanner_contract/src/lib.rs:704`；v1 分支保持 `markdown_context_v1` 不动）。
2. §5 风险 4（`:88`）同校。
3. §4 第 4 条（不变量规则）与 §6 测试行改写为：

```markdown
4. **不变量保证**：边界回退/前进只会缩短头/尾（`head_end ≤ head_budget`、
   `tail_start ≥ total − tail_budget`），marker 计入 `OMITTED_MARKER_RESERVE=64`
   预留——`count_chars(body) ≤ limit` 结构性成立，无需补偿逻辑；测试显式断言
   边界放置与预算不变量。
```

§6 中 `- 不变量规则：尾部前进超限时头部收缩（构造「尾部行特别长」的用例，断言 body ≤ limit 且无半行）；` 一行改为：

```markdown
- 边界规则：头部结束于换行边界（`'\n'` 之后）、尾部起始于行首；无换行区域退化为字符截断；body ≤ limit 与 marker 字符数断言；
```

- [ ] **Step 2: scanner-backends.md 追加压缩策略节**

在 `docs/scanner-backends.md` 的 `## Cache identity` 节（`:84-94`）之后追加：

```markdown
## Context compression

The compressor preserves file content verbatim within the per-file budget
(default 100_000 chars); files within budget pass through unchanged. Files
exceeding the budget keep the first 40% and last 60% cut at line boundaries,
joined by an explicit omission marker, so no mid-file content is dropped
silently. `.log` files keep the recent tail with a head-omission marker.
Global context budget defaults to 500_000 chars for every report mode; all
values are overridable through the scanner profile leaves.
```

- [ ] **Step 3: CLAUDE.md 更新扫描策略行**

`CLAUDE.md` Key Patterns 中 `- 扫描策略：summary_mode + total_max_chars 控制上下文长度` 替换为：

```markdown
- 扫描策略：单文件预算内正文逐字保留；超预算文件头+尾行边界兜底（`.log` 尾优先）；默认全局 500k / 单文件 100k 字符，均可用 scanner profile 覆盖
```

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-08-10-max-preserve-context-design.md docs/scanner-backends.md CLAUDE.md
git commit -m "docs: correct compression version target and document verbatim-preserving budgets"
```

---

### Task 5: 全量验证

**Files:**
- 无代码改动；发现回归时修复并另开 commit

**Interfaces:**
- Consumes: Task 1-4 的产出

- [ ] **Step 1: Rust 全 workspace 测试**

Run: `cd rust && cargo test`
Expected: 全绿（scanner_core / scanner_contract / discovery / office_parser）。

- [ ] **Step 2: Python 全量测试**

Run: `uv run pytest`
Expected: 全绿。若个别用例断言旧默认值失败（预期无——Python 测试均显式构造 profile），把该用例的显式值更新为 500k/100k 并单独 commit。

- [ ] **Step 3: corpus gate 回归**

Run: `uv run python scripts/corpus_gate.py`
Expected: 通过（冻结 profile 显式设值，不受新默认影响；顺带确认新默认下全链路不回归）。

- [ ] **Step 4: 基准冒烟**

Run: `uv run python scripts/benchmark_scanner.py --smoke`（如无该 flag，运行 `uv run python scripts/benchmark_scanner.py --help` 按现有参数跑一次最短路程）
Expected: 正常出包络不报错；耗时仅作参考。

- [ ] **Step 5: 端到端冒烟（可选，需 LLM 可用）**

Run: `uv run python main.py daily --no-save -i "验证大预算上下文"`（或 `--source scan` 指向真实工作目录）
Expected: 生成的上下文摘要显示 `全局上下文预算: 500000`、`单文件正文预算: 100000`、`压缩策略: markdown_context_v3`；无 `BUDGET_MODEL_MISMATCH` 报错。

- [ ] **Step 6: 收尾 commit（如 Step 2-3 有修复）**

```bash
git add -A
git commit -m "test: align assertions with generous default budgets"
```
（无修复则跳过。）
