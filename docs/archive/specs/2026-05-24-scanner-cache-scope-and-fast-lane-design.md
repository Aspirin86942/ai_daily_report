# Scanner Cache Scope And Fast Lane Design

Status: APPROVED
Mode: Builder

## 目标

在现有 scanner 性能证据基础上继续收敛 warm-cache 扫描耗时。优先修正扫描范围与可观测性，再决定是否优化解析执行路径。

本次设计解决三个具体问题：

1. `config/settings.toml` 中的 `excluded_dirs` 需要真正传入 scanner discovery 层，避免运行产物进入扫描样本。
2. benchmark 需要输出每个重解析文件的明细和 cache miss 原因，避免只看到 `reparsed_count` 后继续靠猜。
3. `.txt` / `.md` / `.csv` / `.json` / `.log` 这类 text-like 文件应支持直接解析路径，减少 Windows `spawn` 子进程固定开销。

## 输入

- `config/settings.toml` 的 scanner 配置：
  - `allowed_extensions`
  - `ignored_patterns`
  - `excluded_dirs`
  - `worker_lane_mode`
  - parser profile 相关限制
- `FileDiscoveryService.bootstrap_full_scan()` 的候选文件元数据
- `ScanPlanner` 生成的 parser profile key
- `ScanIndexStore` 中已有的 `file_inventory`、`parse_cache`、`scan_runs` 和扩展名指标表
- `scripts/benchmark_scanner.py` 的 JSON / Markdown 输出路径

## 输出

- scanner 运行时实际生效的 `excluded_dirs`，由配置层传递到 discovery 层。
- benchmark JSON / Markdown 中新增重解析明细：
  - `path`
  - `extension`
  - `file_identity`
  - `source_version`
  - `cache_status`
  - `cache_miss_reason`
  - `previous_source_version`
  - `parse_duration_ms`
  - `parse_status`
  - `parse_error`
- text-like 文件直接解析后的 scan run 指标，继续保持现有 `ScanResult` 业务契约不变。
- 回归测试覆盖配置透传、cache miss reason、text-like direct parse、重格式 subprocess fallback。

## 事实基础

最近 warm benchmark 显示：

- 总耗时稳定约 1.8 秒。
- discovery 约 450-470ms。
- inventory/cache 约 13-45ms。
- parse 约 1.3 秒。
- 连续两轮 `reparsed_count = 3`。

但当前证据不能直接证明 cache key 不稳定。现有 SQLite inventory 显示扫描样本中包含 `data/benchmarks`、`data/reports`、`logs/2026-05-24.log` 等项目自身运行产物。当天日志会随扫描追加，benchmark 结果文件也会在下一轮被发现。它们的 `source_version = mtime_ns + size` 变化时，按现有缓存契约重解析是正确行为。

同时，当前 `config/settings.toml` 虽然有 `excluded_dirs`，但 `src/core/config.py` 的 `scanner_config` 没有把该字段放入返回字典，导致 discovery 层无法使用这项配置。这是下一轮优化的第一优先级。

## 设计原则

1. Correctness 优先。不能为了提升 cache hit 而放松 `source_version` 判定。
2. 先净化样本，再优化耗时。运行产物不应进入业务扫描范围。
3. cache miss 必须可审计。`reparsed_count` 只能作为摘要，不能替代文件级原因。
4. text-like fast lane 只处理有明确读取上限的纯文本类文件，重格式文件继续保留子进程隔离。
5. 现有模块边界不打散：discovery、planner、index store、worker pool、aggregator 继续各司其职。

## 方案

### 1. 配置透传

在 `Config.scanner_config` 中加入 `excluded_dirs`，并沿用 `_to_builtin_value()` 转成普通 list，确保 Windows `spawn` 路径仍可 pickle。

不在代码里硬编码 `D:\bochu_work\ai_daily_report`。是否排除整个项目目录、仅排除 `data/benchmarks`，或排除 `data` / `logs` / `.codegraph` 等目录，由 `settings.toml` 明确表达。代码只负责让配置真实生效。

### 2. 文件级 cache probe

在 index store 或 planner 附近增加一个只读 probe 能力，用于解释 cache 状态：

- `fresh`: 当前 `file_identity + parser_profile + source_version` 命中 success cache。
- `new_file`: 当前 `file_identity` 从未出现过 cache。
- `source_version_changed`: 同一 `file_identity + parser_profile` 存在历史 success cache，但 `source_version` 不同。
- `parser_profile_changed`: 同一 `file_identity` 存在历史 cache，但 parser profile 不同。
- `error_cache`: 同版本只有 error cache，按现有契约必须重解析。
- `missing_context`: planner 判断命中，但加载 cache 失败，用于保留异常审计。

planner 仍只负责分流 cached / uncached；新增 reason 主要服务 benchmark 和日志审计，不能反向改变缓存正确性。

### 3. 重解析明细指标

新增轻量的 per-file reparse detail 数据结构。它可以先作为 benchmark 输出对象，不必第一步就持久化到 SQLite 表。

最小实现路径：

- `FileScanner.scan_files()` 在 planning 阶段收集 `cache_miss_reason`。
- parse 阶段记录每个 uncached 文件的解析耗时和结果。
- scanner 暴露最近一次 `reparse_details`，或通过 `ScanMetricsCollector` 临时持有。
- benchmark 脚本读取该明细并写入 JSON / Markdown。

如果后续需要跨进程或历史对比，再扩展 SQLite 表；本轮不提前增加表结构复杂度。

### 4. Text-Like Direct Parse

新增解析路径选择函数，例如 `ParserSupervisor.should_use_direct_parse(file_type, scanner_cfg)` 或 `FileScanner._should_parse_direct(file_type)`。

规则：

- `file_type in {".txt", ".md", ".csv", ".json", ".log"}` 且 `worker_lane_mode == "direct"` 时，调用 `_extract_content()` 直接解析。
- 其他格式继续调用 `_run_extract_subprocess()`。
- direct parse 仍执行 `max_file_size_mb`、`text_max_chars` 和 UTF-8 显式读取。
- direct parse 出错时仍返回 `FileContext(error=...)` 并写入 error cache，不静默失败。

这样可以减少 Windows `spawn` 对小文本文件的固定成本，同时保留 PDF / Excel / PPT / Word 这类解析风险较高文件的超时隔离。

## 非目标

- 不改变 `ScanResult` / `FileContext` 对上层 daily、weekly、monthly 的业务契约。
- 不改 cache freshness 契约；`source_version` 变化仍必须重解析。
- 不引入常驻 worker pool。
- 不接入 NTFS Journal。
- 不引入 Rust / Go。
- 不把 benchmark 输出目录硬编码为系统临时目录；脚本参数继续控制输出位置。

## 风险点 / 边界条件

- 如果配置排除了整个项目目录，而 `paths.work_dir` 仍是 `D:\bochu_work`，scanner 会跳过 `D:\bochu_work\ai_daily_report` 下所有项目文件。这通常是期望行为，但需要通过 benchmark 输出确认业务样本仍存在。
- `.log` 文件如果不被排除，会因为运行时追加日志持续改变 `source_version`。这不是 cache bug。
- `error_cache` 不应当被视为 fresh cache。错误缓存只用于审计，不能阻止同版本重试。
- direct parse 没有子进程 timeout，但 text-like 文件有读取字符数上限和文件大小上限。若未来支持超大文本流式解析，需要重新评估 direct lane。
- `parse_duration_ms` 是墙钟阶段耗时，per-file duration 是单文件执行耗时。多线程场景下两者不要求相等。

## 测试策略

- 配置测试：
  - `scanner_config` 返回 `excluded_dirs`。
  - 返回值是普通 list，可 pickle。
- discovery 测试：
  - `excluded_dirs` 通过 `Config.scanner_config` 进入 `FileDiscoveryService` 后真实过滤目录。
- cache probe 测试：
  - fresh cache 返回 `fresh`。
  - 同身份不同 `source_version` 返回 `source_version_changed`。
  - 同身份不同 parser profile 返回 `parser_profile_changed`。
  - 只有 error cache 返回 `error_cache`。
  - 完全无 cache 返回 `new_file`。
- scanner 测试：
  - text-like 文件在 `worker_lane_mode = "direct"` 时不调用 `_run_extract_subprocess()`。
  - PDF / Excel 等重格式仍调用 `_run_extract_subprocess()`。
  - direct parse 失败时写入 error cache，并进入可审计结果。
- benchmark 测试：
  - JSON 输出包含 `reparse_details`。
  - Markdown 输出包含重解析文件表。

## 验收条件

1. `conda run -n test python -m pytest tests -q` 通过。
2. `conda run -n test python -m compileall main.py src tests` 通过。
3. 连续两次 warm benchmark 中，若扫描样本无变动且无运行产物进入范围，`reparsed_count` 应降为 0。
4. 若仍有 `reparsed_count > 0`，benchmark 必须能列出具体文件和 `cache_miss_reason`。
5. `.md` / `.json` / `.log` 等 text-like 文件重解析时，单文件 parse duration 应明显低于 Windows spawn 路径。

## 伪代码草案

```python
# [伪代码草案]
# 目标：净化扫描范围，解释 cache miss，并为纯文本类文件启用低开销解析路径
# 输入：
# - scanner_cfg: 从 settings.toml 读取并转换后的 scanner 配置
# - discovered_files: discovery 阶段返回的候选文件元数据
# - parser_profile_key: 当前解析预算序列化结果
# - scan_index_store: 负责库存、cache 与 scan run 指标的 SQLite 存储层
# 输出：
# - scan_result: 与现有上层调用兼容的 ScanResult
# - run_metrics: 当前扫描运行级指标
# - reparse_details: 每个重解析文件的原因、耗时和结果

def build_scanner_config(settings):
    cfg = {
        "allowed_extensions": to_builtin(settings.scanner.allowed_extensions),
        "ignored_patterns": to_builtin(settings.scanner.ignored_patterns),
        # 为什么必须透传：discovery 已经支持 excluded_dirs，但没有配置输入就无法生效
        "excluded_dirs": to_builtin(getattr(settings.scanner, "excluded_dirs", [])),
        "worker_lane_mode": getattr(settings.scanner, "worker_lane_mode", "direct"),
        "text_max_chars": settings.scanner.text_max_chars,
    }
    return cfg


def probe_cache_status(store, item, parser_profile_key):
    # 1. 成功缓存完全匹配时才能复用，保证 source_version 变化不会误用旧内容
    if store.has_fresh_cache(
        item.file_identity,
        parser_profile_key,
        source_version=item.source_version,
    ):
        return CacheProbe(status="fresh", reason="")

    # 2. 查历史 cache 是为了给 benchmark 解释原因，不改变 freshness 规则
    history = store.list_cache_history(item.file_identity)
    if not history:
        return CacheProbe(status="miss", reason="new_file")

    if history.has_error_for(parser_profile_key, item.source_version):
        return CacheProbe(status="miss", reason="error_cache")

    if history.has_profile(parser_profile_key):
        previous = history.latest_success_for_profile(parser_profile_key)
        return CacheProbe(
            status="miss",
            reason="source_version_changed",
            previous_source_version=previous.source_version,
        )

    return CacheProbe(status="miss", reason="parser_profile_changed")


def should_parse_direct(file_type, scanner_cfg):
    # text-like 文件读取有字符数上限，直接解析可以避开 Windows spawn 固定成本
    return (
        scanner_cfg.get("worker_lane_mode") == "direct"
        and file_type in {".txt", ".md", ".csv", ".json", ".log"}
    )


def parse_uncached_file(scanner, item, limits, probe):
    started_at = now()
    try:
        if should_parse_direct(item.extension, scanner.scanner_cfg):
            context = scanner._extract_content(item.path, limits)
        else:
            context = scanner._extract_content_with_timeout(item.path, limits)

        duration_ms = elapsed_ms(started_at)
        scanner._write_parse_cache(item, context)

        return ReparseDetail(
            path=str(item.path),
            extension=item.extension,
            file_identity=item.file_identity,
            source_version=item.source_version,
            cache_status="miss",
            cache_miss_reason=probe.reason,
            previous_source_version=probe.previous_source_version,
            parse_duration_ms=duration_ms,
            parse_status="success" if context.error is None else "error",
            parse_error=context.error or "",
        )

    except Exception as exc:
        # 未知异常仍要进入 error cache 和 detail，避免 benchmark 只看到失败计数
        scanner.scan_index_store.upsert_parse_cache(
            file_identity=item.file_identity,
            parser_profile=scanner.parser_profile_key,
            content_excerpt="",
            parse_status="error",
            parse_error=str(exc),
            source_version=item.source_version,
        )
        return ReparseDetail(
            path=str(item.path),
            extension=item.extension,
            file_identity=item.file_identity,
            source_version=item.source_version,
            cache_status="miss",
            cache_miss_reason=probe.reason,
            parse_duration_ms=elapsed_ms(started_at),
            parse_status="error",
            parse_error=str(exc),
        )
```
