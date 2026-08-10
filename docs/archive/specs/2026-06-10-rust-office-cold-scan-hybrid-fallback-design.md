# Rust Office Cold-Scan Hybrid Fallback Design

Status: REVIEW_READY
Mode: Brainstorming + grill-with-docs
Date: 2026-06-10

## 目标

优化 Office 文件在 **Cold scanner run** 中的解析性能，同时保留报告可用性和 scanner 审计解释力。

本设计不把项目改写为全 Rust。当前阶段只把 Rust Office parser 的失败分类、Python fallback 决策、parser profile、benchmark 输出和测试验收收敛成一个可执行的阶段 1 方案。Python 继续负责 CLI 编排、配置、SQLite、LLM、模板渲染、scan cache、metrics 和 benchmark 汇总；Rust 继续作为 Office 解析主路径。

本设计采纳 ADR:

- `docs/adr/0001-performance-first-hybrid-office-fallback.md`

相关 glossary:

- `Cold scanner run`
- `Hybrid Office fallback policy`
- `Deterministic Office parse failure`
- `Environment-unavailable Office parse failure`
- `Office parser contract failure`

## 输入

- `FileScanner` 发现到的 Office 文件:
  - `.docx`
  - `.xlsx`
  - `.pptx`
  - `.doc`
  - `.xls`
  - `.ppt`
- scanner parser profile:
  - `office_parser_backend`
  - `rust_office_parser_bin`
  - `office_parser_fallback_enabled`
  - `office_parser_fallback_order`
  - `office_fallback_after_timeout`
  - `office_external_fallback`
  - `office_legacy_extensions_enabled`
  - Office 预算字段，例如 `document_excerpt_max_chars`、`excel_max_sheets`、`excel_max_rows`、`excel_max_columns`、`docx_max_paragraphs`、`pptx_max_slides`
- Rust Office parser stdout contract:
  - `FileContext.file_path`
  - `FileContext.file_type`
  - `FileContext.content`
  - `FileContext.error`
  - `FileContext.parser_backend`
  - `FileContext.truncated`
- Python fallback 能力:
  - `python_office_v1`
  - `python_sharepoint_text_v1`
- benchmark evidence:
  - `parser_backend`
  - `attempted_backend`
  - `fallback_backend`
  - `fallback_reason`
  - `rust_duration_ms`
  - `fallback_duration_ms`
  - `failure_class`

## 输出

成功输出:

- Rust 成功时返回 Rust `FileContext`:
  - `.xlsx` 正常应为 `rust_xlsx_bounded_v1`
  - `.docx` / `.pptx` 正常应为 `rust_office_oxide_v1`
- Rust 失败但 Python fallback 成功时返回 Python `FileContext`，并保留 Rust attempt audit。
- Rust 失败且 fallback 不允许或 fallback 失败时返回可审计 error `FileContext`，不抛出到 scanner 主流程。

失败分类输出:

- `deterministic`: 可重复失败或会制造 cold-run 长尾，不 fallback。
- `environment_unavailable`: Rust binary 或运行环境不可用，允许 fallback，但不能用于评价 Rust parser 性能。
- `contract_failure`: Rust CLI 完成但 stdout / payload 违反 Python scanner contract，允许 fallback，并视为 Rust-Python 边界缺陷。
- `recoverable_parser_failure`: Rust 解析失败但尚未证明为 deterministic，可按配置 fallback。

benchmark 输出必须能区分:

- Rust 成功解析。
- Python fallback 成功解析。
- deterministic failure 没有 fallback。
- environment-unavailable failure fallback。
- contract failure fallback。
- recoverable parser failure fallback 或 no-fallback。

本设计新增显式 audit / benchmark 字段:

- `failure_class`: 取值为 `deterministic`、`environment_unavailable`、`contract_failure`、`recoverable_parser_failure` 或空字符串。

空字符串只允许出现在 Rust 或 Python 成功路径没有失败分类时。

## 非目标

- 不做 batch Office parser。
- 不做 long-running Rust worker。
- 不做 Rust scanner core。
- 不做全 Rust rewrite。
- 不做 Python wheel / bundled Rust binaries 发布路线。
- 不重写 LLM、模板、日报、周报、月报业务逻辑。
- 不把 `.doc` / `.ppt` 自动加入默认扫描范围。

## 当前事实

当前代码已经具备 Rust Office parser 主路径:

- `src/services/office_parser.py::RustOfficeParserRunner` 通过 subprocess 调用 `rust/office_parser`。
- `parse_office_with_fallback()` 负责 Rust primary + Python fallback。
- `RUST_OFFICE_TIMEOUT` 默认不 fallback，除非 `office_fallback_after_timeout=true`。
- `.xlsx` 的确定性坏 ZIP 错误已经通过 `_should_skip_python_fallback()` 跳过 Python fallback。
- `_validate_rust_payload_context()` 已经检查 `file_path`、`file_type` 和 `parser_backend`。
- `OfficeParseAudit` 已经有 `attempted_backend`、`fallback_backend`、`fallback_reason`、`rust_duration_ms`、`fallback_duration_ms`。

本设计不是推翻这些行为，而是把它们升级为明确的 **Hybrid Office fallback policy**，并新增 `failure_class` 让 benchmark 不需要从 `fallback_reason` 猜测失败类别。

## 失败分类

### Deterministic failure

这类失败不走 Python fallback，直接返回可审计错误。

包含:

- `RUST_OFFICE_TIMEOUT`
- `RUST_XLSX_BOUNDED_PARSE_FAILED: ZIP error`
- legacy extension 被配置禁用时的 unsupported / disabled failure

原因:

- timeout 是 cold scanner run 长尾的主要来源，再 fallback 会扩大长尾。
- 确定性坏 `.xlsx` 反复 fallback 会让 cold/warm benchmark 都失真。
- legacy extension 未启用时，不应通过 Python fallback 悄悄扩大扫描范围。

### Environment-unavailable failure

这类失败允许 Python fallback，但 benchmark 必须标明不能评价 Rust parser 性能。

包含:

- Rust binary 缺失。
- Rust binary 路径错误。
- Rust binary 无执行权限。
- 平台后缀错误，例如 Windows 上配置到无 `.exe` 的 release binary。

对应错误:

- `RUST_OFFICE_START_FAILED`

原因:

- 环境不可用不代表文件内容不可解析。
- fallback 能保住报告可用性。
- 但这轮 benchmark 不能说明 Rust parser 快慢。

### Contract failure

这类失败允许 Python fallback，但应视为 Rust-Python 边界缺陷。

包含:

- Rust stdout 不是 JSON。
- Rust stdout 可以解析但不是合法 `FileContext`。
- Rust payload 的 `file_path` 与请求不一致。
- Rust payload 的 `file_type` 与请求不一致。
- Rust payload 的 `parser_backend` 与扩展名不匹配。

对应错误:

- `RUST_OFFICE_INVALID_JSON`
- `RUST_OFFICE_INVALID_PAYLOAD`

原因:

- contract failure 不一定是文件内容问题。
- fallback 可以保住报告内容。
- 但工程优先级应是修 contract 或补测试，而不是扩大 fallback。

### Recoverable parser failure

这类失败可以按配置 fallback。

包含:

- 非 deterministic 的 `RUST_OFFICE_PARSE_FAILED`。
- Rust parser 对某个文件失败，但尚无证据证明 Python fallback 也会慢失败或重复失败。

原因:

- 第一阶段不应过早把所有 Rust parse failure 都归入 deterministic。
- benchmark 可以帮助识别高频慢失败，后续再提升为 deterministic 子类。

## Fallback 决策表

| Failure class | 默认 fallback | 说明 |
|---|---:|---|
| deterministic | 否 | 控制 cold-run 长尾，保留审计错误 |
| environment_unavailable | 是 | 保住报告内容，但不评价 Rust parser 性能 |
| contract_failure | 是 | 保住报告内容，同时暴露边界缺陷 |
| recoverable_parser_failure | 按配置 | 由 `office_parser_fallback_enabled` 和 fallback order 决定 |

`office_fallback_after_timeout` 默认保持 `false`。只有用户显式选择内容覆盖优先时，timeout 才允许进入 Python fallback。

## Cache Profile

以下字段必须保持在 `ScanPlanner` parser profile 中，避免 fallback 策略或 Rust binary 变化后复用旧 parse cache:

- `office_parser_backend`
- `rust_office_parser_bin`
- `office_parser_fallback_enabled`
- `office_parser_fallback_order`
- `office_fallback_after_timeout`
- `office_external_fallback`
- `office_legacy_extensions_enabled`
- `office_fallback_policy_version`
- `parser_profile_version`
- Office 预算字段

`office_fallback_policy_version` 第一版取值为 `hybrid_v1`。如果 failure classification 或 fallback policy 后续改变，必须 bump 这个字段，避免同一个文件在策略变化后继续命中旧 cache，导致 benchmark 和报告内容都不可解释。

## Benchmark 设计

Cold scanner run 的 benchmark 必须使用隔离的 scan index DB 或清空 parse cache。它不要求清空 OS 文件系统缓存，也不包含 LLM 调用和报告渲染。

推荐 benchmark 读法:

- `extension_metrics`: 看 Office 扩展名的 parse duration 和 error 数。
- `parser_backend_summary.by_extension`: 看 `.xlsx` 是否走 `rust_xlsx_bounded_v1`。
- `reparse_details`: 看每个 Office 文件的 attempt、fallback 和耗时。
- `rust_duration_ms`: 只解释 Rust attempt 成本。
- `fallback_duration_ms`: 只解释 Python fallback 成本。
- `failure_class`: 直接解释 Rust failure 的分类。
- `fallback_reason`: 保留原始 Rust error 摘要，辅助排查。

本阶段不承诺固定百分比提升。成功标准是:

- deterministic failure 不再产生 fallback_duration_ms 长尾。
- timeout 默认不触发 Python fallback。
- environment-unavailable failure 能明确说明这轮不能评价 Rust parser 性能。
- contract failure 能明确指向 Rust-Python contract，而不是文件内容。
- Rust 成功路径和 Python fallback 路径的耗时分开统计。

期望结果:

- 在包含多个 Office cache miss 的 cold scanner run 中，坏文件和 timeout 不再把整轮扫描拖入 Python fallback 长尾。
- 如果样本主要是 Rust 成功解析，整体 Office parse duration 不劣于当前基线。

## 测试策略

Python unit tests:

- timeout 默认不 fallback:
  - Rust runner 返回 `RUST_OFFICE_TIMEOUT`
  - `office_fallback_after_timeout=false`
  - 断言没有调用 Python fallback
  - 断言 audit 包含 `fallback_reason`
- timeout 显式 fallback:
  - `office_fallback_after_timeout=true`
  - 断言可以调用 Python fallback
- deterministic bad `.xlsx` 不 fallback:
  - Rust context 为 `.xlsx`
  - backend 为 `rust_xlsx_bounded_v1`
  - error 为 `RUST_XLSX_BOUNDED_PARSE_FAILED: ZIP error: ...`
  - 断言没有调用 Python fallback
- start failure fallback:
  - Rust runner 返回 `RUST_OFFICE_START_FAILED`
  - 断言调用 Python fallback
  - 断言 benchmark classification 为 `environment_unavailable`
- invalid JSON / payload fallback:
  - Rust runner 返回 `RUST_OFFICE_INVALID_JSON` 或 `RUST_OFFICE_INVALID_PAYLOAD`
  - 断言调用 Python fallback
  - 断言 benchmark classification 为 `contract_failure`
- fallback disabled:
  - `office_parser_fallback_enabled=false`
  - recoverable parser failure 不 fallback

Rust tests:

- 继续覆盖 `.xlsx` bounded preview。
- 继续覆盖 `.docx` / `.pptx` 基本 contract。
- 不要求 Rust 自己决定 Python fallback；fallback policy 仍属于 Python scanner orchestration。

Benchmark tests:

- benchmark JSON / Markdown 保留 `attempted_backend`、`fallback_backend`、`fallback_reason`、`rust_duration_ms`、`fallback_duration_ms`。
- benchmark JSON / Markdown 稳定输出 `failure_class`。
- Markdown 对 environment-unavailable failure 给出不能评价 Rust parser 性能的解释。

## 伪代码草案

```python
# [伪代码草案]
# 目标：按错误类型优先的 hybrid policy 决定 Office parser 是否进入 Python fallback。
# 输入：
# - file_path: 待解析的 Office 文件路径
# - file_type: 标准化扩展名，例如 ".xlsx" / ".docx" / ".pptx"
# - limits: 本轮 scanner parser profile 里的 Office 解析预算
# - scanner_cfg: scanner 配置，包含 backend、fallback、timeout、legacy 开关
# - rust_runner: Rust Office parser runner，负责 stdout JSON contract
# - python_fallback: Python fallback，可为空，默认使用配置里的 fallback order
# 输出：
# - OfficeParseOutcome.context: 成功内容或可审计错误 FileContext
# - OfficeParseOutcome.audit: attempted/fallback/reason/duration 审计

class FailureDecision:
    def __init__(self, failure_class, allow_fallback, reason):
        self.failure_class = failure_class
        self.allow_fallback = allow_fallback
        self.reason = reason


def parse_office_with_hybrid_fallback(
    file_path,
    file_type,
    limits,
    scanner_cfg,
    rust_runner,
    python_fallback=None,
):
    normalized_type = normalize_extension(file_type)

    # 1. 非 Rust backend 不进入 hybrid policy；这保留显式配置的 Python-only 路径。
    if scanner_cfg.office_parser_backend != "rust_office_oxide_v1":
        return run_configured_python_backend(file_path, normalized_type, limits)

    rust_started_at = now()
    rust_context = rust_runner.parse(
        file_path=file_path,
        file_type=normalized_type,
        limits=limits,
        timeout_seconds=resolve_timeout(scanner_cfg, normalized_type),
    )
    rust_duration_ms = elapsed_ms(rust_started_at)

    if rust_context.error is None:
        return OfficeParseOutcome(
            context=rust_context,
            audit=OfficeParseAudit(
                attempted_backend=rust_context.parser_backend,
                rust_duration_ms=rust_duration_ms,
                failure_class="",
            ),
        )

    decision = classify_office_failure(
        file_type=normalized_type,
        rust_backend=rust_context.parser_backend,
        rust_error=rust_context.error,
        scanner_cfg=scanner_cfg,
    )

    # 2. 性能优先：deterministic failure 直接审计，不再花 cold-run 预算 fallback。
    if not decision.allow_fallback:
        return OfficeParseOutcome(
            context=rust_context,
            audit=OfficeParseAudit(
                attempted_backend=rust_context.parser_backend,
                fallback_reason=rust_context.error,
                rust_duration_ms=rust_duration_ms,
                failure_class=decision.failure_class,
            ),
        )

    fallback_started_at = now()
    fallback_context = run_python_fallback(
        file_path=file_path,
        file_type=normalized_type,
        limits=limits,
        fallback_order=scanner_cfg.office_parser_fallback_order,
        python_fallback=python_fallback,
    )
    fallback_duration_ms = elapsed_ms(fallback_started_at)

    if fallback_context.error is None:
        return OfficeParseOutcome(
            context=fallback_context,
            audit=OfficeParseAudit(
                attempted_backend="rust_office_oxide_v1",
                fallback_backend=fallback_context.parser_backend,
                fallback_reason=rust_context.error,
                rust_duration_ms=rust_duration_ms,
                fallback_duration_ms=fallback_duration_ms,
                failure_class=decision.failure_class,
            ),
        )

    return OfficeParseOutcome(
        context=merge_rust_and_python_errors(
            file_path=file_path,
            file_type=normalized_type,
            rust_error=rust_context.error,
            python_error=fallback_context.error,
        ),
        audit=OfficeParseAudit(
            attempted_backend="rust_office_oxide_v1",
            fallback_backend=fallback_context.parser_backend,
            fallback_reason=rust_context.error,
            rust_duration_ms=rust_duration_ms,
            fallback_duration_ms=fallback_duration_ms,
            failure_class=decision.failure_class,
        ),
    )


def classify_office_failure(file_type, rust_backend, rust_error, scanner_cfg):
    # timeout 是 cold scanner run 的主要长尾来源；默认不 fallback。
    if rust_error.startswith("RUST_OFFICE_TIMEOUT:"):
        return FailureDecision(
            failure_class="deterministic",
            allow_fallback=bool(scanner_cfg.office_fallback_after_timeout),
            reason="timeout",
        )

    # 坏 xlsx zip 属于重复慢失败；当前策略直接审计，避免 fallback 长尾。
    if (
        file_type == ".xlsx"
        and rust_backend == "rust_xlsx_bounded_v1"
        and rust_error.startswith("RUST_XLSX_BOUNDED_PARSE_FAILED: ZIP error:")
    ):
        return FailureDecision(
            failure_class="deterministic",
            allow_fallback=False,
            reason="deterministic_xlsx_zip_error",
        )

    # binary 或运行环境不可用时，fallback 是为了保住报告可用性；
    # benchmark 不能据此评价 Rust parser 性能。
    if rust_error.startswith("RUST_OFFICE_START_FAILED:"):
        return FailureDecision(
            failure_class="environment_unavailable",
            allow_fallback=bool(scanner_cfg.office_parser_fallback_enabled),
            reason="rust_binary_unavailable",
        )

    # contract failure 是 Rust-Python 边界缺陷，允许 fallback 但必须暴露分类。
    if rust_error.startswith("RUST_OFFICE_INVALID_JSON:") or rust_error.startswith(
        "RUST_OFFICE_INVALID_PAYLOAD:"
    ):
        return FailureDecision(
            failure_class="contract_failure",
            allow_fallback=bool(scanner_cfg.office_parser_fallback_enabled),
            reason="rust_python_contract_failed",
        )

    return FailureDecision(
        failure_class="recoverable_parser_failure",
        allow_fallback=bool(scanner_cfg.office_parser_fallback_enabled),
        reason="rust_parse_failed",
    )
```

## 风险点 / 边界条件

- 如果 failure class 进入 benchmark 但不进入 parse cache profile，策略变化后可能复用旧缓存。
- 如果将 timeout 改成默认 fallback，cold scanner run 的性能目标会被内容覆盖目标反向破坏。
- 如果把 extension 作为 fallback 第一判断条件，`.xlsx` / `.docx` / `.pptx` 的行为会变成经验规则，后续难解释。
- 如果 Rust binary 没构建，本轮 benchmark 只能验证 fallback 可用性，不能验证 Rust Office parser 性能。
- 如果未来做 batch parser，必须重新设计 per-file timeout、partial result、batch crash 和 fallback 所属职责。

## 验收条件

- `CONTEXT.md` 中有 `Cold scanner run`、`Hybrid Office fallback policy`、`Deterministic Office parse failure`、`Environment-unavailable Office parse failure`、`Office parser contract failure`。
- `docs/adr/0001-performance-first-hybrid-office-fallback.md` 记录性能优先 fallback 取舍。
- Python tests 覆盖 deterministic no-fallback、timeout 默认 no-fallback、start failure fallback、contract failure fallback、fallback disabled。
- Rust Office parser tests 继续通过。
- `ScanPlanner` parser profile 覆盖 fallback、timeout、backend、binary path 和 Office budget 字段。
- benchmark JSON / Markdown 显式输出 Office `failure_class`。
- cold scanner run benchmark 使用隔离 index DB，不污染本地 `data/db/scan_index.sqlite3`。
- `git diff --check` 不出现新增 whitespace 错误。

## 后续路线

如果本阶段 benchmark 显示 Office cold-run 仍主要消耗在 per-file Rust subprocess 启动成本，再单独设计 batch Office parser。batch parser 不应作为本 spec 的隐藏任务进入实施。
