# Scanner / Index 拆分重构收尾设计

日期：2026-06-11
项目：`/home/george/Python_program/ai_daily_report`

## 目标

把当前已经开始的 scanner/index 拆分 WIP 收敛成一次小而完整、可验证、可提交的内部重构。

本次目标不是重写 scanner 架构，也不是继续扩大拆分范围，而是：

- 保留 `ScanIndexStore` 作为扫描索引对外入口。
- 将 schema/migration、inventory SQL、共享模型、scanner item 适配、parse cache/reparse audit 构造分别放在清晰模块中。
- 让 `FileScanner` 保留现有 public/测试兼容入口，但把已拆出的 helper 细节委托给新模块。
- 不改变 scanner 业务行为、parser backend 策略、fallback 策略、cache freshness 规则和 benchmark evidence 语义。

## 输入

### 关键入参

当前工作区已有 WIP 代码：

- `src/services/scan_index_store.py`
- `src/services/scan_index_schema.py`
- `src/services/scan_index_inventory.py`
- `src/services/scan_index_models.py`
- `src/services/scanner_items.py`
- `src/services/scanner_parse_cache.py`
- `src/services/file_scanner.py`
- `src/services/cold_scanner_run.py`

当前相关测试：

- `tests/test_scan_index_store.py`
- `tests/test_scan_index_inventory.py`
- `tests/test_scanner_items.py`
- `tests/test_scanner_parse_cache.py`
- `tests/test_file_scanner.py`
- `tests/test_cold_scanner_run.py`
- `tests/test_scan_planner.py`
- `tests/test_office_parser.py`
- `tests/test_rust_cli_contract.py`
- `tests/test_rust_discovery_contract.py`
- `tests/test_schemas.py`

### 上下文信息

当前仓库已有未提交 scanner/index 拆分改动。本轮应优先收尾这些改动，不另起一条大重构线。

本项目 scanner/backend 相关行为必须保持：

- `parser_backend` 和 `worker_lane` 继续分离。
- Office parser 默认保持 `rust_office_oxide_v1`。
- `.xlsx` Rust CLI fast path 运行时 evidence 继续报告 `rust_xlsx_bounded_v1`。
- `office_fallback_after_timeout` 默认保持 `false`。
- parser profile/cache key 继续覆盖 backend、fallback、Rust binary path、parser budgets 等影响解析行为的字段。

### 外部依赖

- Python 3.10+
- SQLite
- pytest
- Rust discovery / Rust Office parser CLI contract 测试

### 运行环境约束

默认 Linux 本地开发环境。优先按项目现有 Python 环境运行测试；如果 `conda run -n test` 可用，优先使用该环境；否则使用当前 `python -m pytest`。

## 输出

### 成功输出

- 当前 WIP 被整理为边界清楚的内部重构。
- 新增 helper 模块职责清晰，导入关系无明显反向依赖或循环依赖风险。
- `FileScanner` 和 `ScanIndexStore` 对外方法保持兼容。
- 关键测试通过，并记录验证命令和结果。
- 如无使用方式或配置变化，不强行改 README。

### 失败输出

如验证失败，需要明确输出：

- 失败的命令。
- 失败原因。
- 是当前 WIP 原有问题、本轮整理引入问题，还是环境问题。
- 是否需要用户决策后继续。

### 副作用

本次会修改项目内源码与测试文件；不会修改密钥、本机私有配置或远端状态。不会 push、不会创建 PR。

## 运行环境

- OS：Linux
- Shell：bash/zsh
- 语言：Python，少量 Rust contract 测试相关代码保持现状
- 数据库：SQLite
- 测试框架：pytest

## 方案选择

采用“最小收敛当前 WIP”方案。

不采用继续大拆 repository/service 层的原因：当前 WIP 尚未收敛，继续扩大拆分会增加 diff 和回归风险。

不采用只跑测试哪里坏修哪里方案的原因：当前已经暴露 helper 模块导入边界不够干净，单纯修测试可能留下可维护性隐患。

## 模块设计

### `scan_index_models.py`

只放 scan index 共享 typed models：

- `InventoryItem`
- `CacheProbe`

约束：

- 不依赖 `ScanIndexStore`。
- 不依赖 `FileScanner`。
- 不依赖 SQL helper。
- 作为 `scan_index_store.py`、`scan_index_inventory.py`、`scanner_items.py`、`scanner_parse_cache.py` 的模型来源。

### `scan_index_schema.py`

只负责 SQLite schema 和 migration：

- 初始化当前表结构。
- 迁移旧 `file_inventory` schema。
- 迁移旧 `parse_cache` schema。
- 补齐 scan metrics schema。
- 提供表名、列名、主键 introspection helper。

约束：

- 不构造 `FileContext`。
- 不做业务查询。
- 不保存 scanner run metrics 或 context audit 数据。

### `scan_index_inventory.py`

只负责 `file_inventory` 表的读写：

- `replace_inventory(conn, items)`
- `query_inventory(conn, start_date, end_date)`

约束：

- 可以依赖 `scan_index_models.InventoryItem`。
- 不依赖 `ScanIndexStore`。
- 保持按 `modified_date` 闭区间查询和现有排序语义。

### `scan_index_store.py`

继续作为外部 facade：

- 管理 SQLite 连接。
- 开启 `PRAGMA foreign_keys = ON`。
- 调用 `init_scan_index_schema(conn)` 初始化 schema。
- 暴露现有 scan index、parse cache、scan metrics、context audit 方法。
- 将 inventory 查询和替换委托给 `scan_index_inventory.py`。

本轮不强行把所有 SQL 都拆走；尤其是 scan run metrics、context run、context decisions 和 parse cache facade API 可以继续留在 `ScanIndexStore` 中，以减少范围和风险。

### `scanner_items.py`

只负责 scanner item adapter：

- `Path`
- `InventoryItem`
- `DiscoveredFile`

核心函数：

- `normalize_discovered_files`
- `item_path`
- `item_identity`
- `item_extension`
- `item_source_version`

约束：

- `InventoryItem` 必须从 `scan_index_models` 导入，而不是从 `scan_index_store` 导入。
- 继续兼容旧测试中 monkeypatch 返回 `Path` 的情况。
- `Path` 的 `file_identity` 和 `source_version` 生成规则不变。

### `scanner_parse_cache.py`

只负责 scanner 层 parse cache 和 reparse audit helper：

- `get_cached_contexts`
- `write_parse_cache`
- `build_reparse_detail`
- `build_reparse_exception_detail`

约束：

- 可以通过协议或窄接口描述依赖的 store 方法，避免长期使用无边界 `Any`。
- 不直接控制 worker lane 规则；`worker_lane` 仍通过注入的 `infer_worker_lane` 函数计算。
- Office parse audit 字段继续合并到 `ReparseDetail`：
  - `attempted_backend`
  - `fallback_backend`
  - `fallback_reason`
  - `rust_duration_ms`
  - `fallback_duration_ms`
  - `failure_class`

### `file_scanner.py`

本轮只做兼容性瘦身：

- 保留 `_get_cached_contexts`、`_write_parse_cache`、`_record_reparse_detail`、`_record_reparse_exception` 等 wrapper。
- wrapper 内部委托给 helper 模块。
- 不改变 parser 选择顺序：
  1. too large guard
  2. direct text parser
  3. Rust Office parser orchestration
  4. document parser subprocess
  5. generic subprocess fallback
- 不改变 timeout/fallback 行为。

### `cold_scanner_run.py`

保留将 `assert` 改为显式 `RuntimeError` 的方向。

原因：运行时一致性错误不能依赖 `assert`，因为 Python `-O` 会禁用 assert。

## 伪代码草案

```python
# 目标：把当前 scanner/index 拆分 WIP 收敛到稳定边界，不改变业务行为。

@dataclass(slots=True)
class InventoryItem:
    file_identity: str
    path: Path
    extension: str
    modified_date: date
    size_bytes: int
    source_version: str


@dataclass(frozen=True, slots=True)
class CacheProbe:
    file_identity: str
    parser_profile: str
    source_version: str
    cache_status: str
    cache_miss_reason: str
    previous_source_version: str | None = None


def init_scan_index_schema(conn: sqlite3.Connection) -> None:
    # 先迁移旧表，避免旧库缺 source_version / parser_backend 等字段。
    migrate_file_inventory_schema(conn)
    migrate_parse_cache_schema(conn)

    # 再创建当前版本需要的表。
    create_current_tables(conn)

    # 最后补齐 scan metrics 兼容列。
    migrate_scan_metrics_schema(conn)


def replace_inventory(conn: sqlite3.Connection, items: list[dict[str, object]]) -> None:
    # inventory 是 bootstrap 快照，本轮保持整体替换语义。
    conn.execute("DELETE FROM file_inventory")
    conn.executemany("INSERT INTO file_inventory ...", normalize_items(items))


def query_inventory(
    conn: sqlite3.Connection,
    start_date: date,
    end_date: date,
) -> list[InventoryItem]:
    # 只按 modified_date 闭区间查询，不改变排序和过滤规则。
    rows = conn.execute(
        "SELECT ... FROM file_inventory WHERE modified_date >= ? AND modified_date <= ?",
        (start_date.isoformat(), end_date.isoformat()),
    ).fetchall()
    return [InventoryItem(...row...) for row in rows]


def normalize_discovered_files(items: list[Path | DiscoveredFile]) -> list[DiscoveredFile]:
    normalized = []
    for item in items:
        if isinstance(item, DiscoveredFile):
            normalized.append(item)
            continue

        # 兼容旧测试里 monkeypatch 返回 Path 的情况。
        stat_result = item.stat()
        normalized.append(
            DiscoveredFile(
                file_identity=f"bootstrap:{str(item.resolve()).lower()}",
                path=item,
                extension=item.suffix.lower(),
                modified_at=datetime.fromtimestamp(stat_result.st_mtime),
                size_bytes=stat_result.st_size,
                source_version=f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}",
            )
        )
    return normalized


def get_cached_contexts(
    store: ParseCacheStore,
    cached_files: list[Path | InventoryItem],
    parser_profile: str,
) -> list[FileContext]:
    contexts = []
    for item in cached_files:
        cached = store.load_parse_cache(
            item_identity(item),
            parser_profile,
            source_version=item_source_version(item),
        )

        # 保持原行为：缓存中的 error 恢复为 FileContext.error。
        parse_status = cached["parse_status"]
        parse_error = cached["parse_error"] or None
        contexts.append(
            FileContext(
                file_path=str(item_path(item)),
                file_type=item_extension(item),
                content=cached["content_excerpt"],
                error=parse_error if parse_status != "success" else None,
                parser_backend=cached["parser_backend"] or None,
                truncated=bool(cached["truncated"]),
            )
        )
    return contexts


def write_parse_cache(
    store: ParseCacheStore,
    item: Path | InventoryItem,
    parser_profile: str,
    context: FileContext,
) -> None:
    is_success = context.error is None

    # 保持原行为：失败时不缓存正文，只缓存错误信息和 backend。
    store.upsert_parse_cache(
        file_identity=item_identity(item),
        parser_profile=parser_profile,
        content_excerpt=context.content if is_success else "",
        parse_status="success" if is_success else "error",
        parse_error=context.error or "",
        source_version=item_source_version(item),
        parser_backend=context.parser_backend or "",
        truncated=context.truncated,
    )


class FileScanner:
    def _get_cached_contexts(self, cached_files, parser_profile):
        # wrapper 保留，降低调用方变动。
        return get_cached_contexts(self.scan_index_store, cached_files, parser_profile)

    def _write_parse_cache(self, item, parser_profile, context):
        # wrapper 保留，降低调用方变动。
        write_parse_cache(self.scan_index_store, item, parser_profile, context)

    def _record_reparse_detail(self, item, cache_probe, duration_ms, context):
        # 通过注入 infer_worker_lane 保持 worker_lane 规则仍由 FileScanner 决定。
        detail = build_reparse_detail(
            item=item,
            cache_probe=cache_probe,
            duration_ms=duration_ms,
            context=context,
            office_parse_audits=self._office_parse_audits,
            infer_worker_lane=self._infer_worker_lane,
        )
        self.last_reparse_details.append(detail)
```

## 风险点 / 边界条件

- 不做大规模 repository 化拆分。
- 不改 scanner 配置默认值。
- 不改 Rust parser contract。
- 不改 `.xlsx` fast path 语义。
- 不改 `office_fallback_after_timeout=false` 默认策略。
- 不改 benchmark 输出字段语义。
- 不引入新依赖。
- 不 push、不 PR。
- 不碰 `config/.secrets.yaml`。
- 如测试暴露历史问题，先记录和报告，不顺手扩大修复范围。

## 验收方式

优先按小范围到全量的顺序验证：

```bash
conda run -n test python -m pytest \
  tests/test_scan_index_inventory.py \
  tests/test_scanner_items.py \
  tests/test_scanner_parse_cache.py \
  -v
```

```bash
conda run -n test python -m pytest tests/test_scan_index_store.py -v
```

```bash
conda run -n test python -m pytest \
  tests/test_file_scanner.py \
  tests/test_cold_scanner_run.py \
  tests/test_scan_planner.py \
  -v
```

```bash
conda run -n test python -m pytest \
  tests/test_office_parser.py \
  tests/test_rust_cli_contract.py \
  tests/test_rust_discovery_contract.py \
  tests/test_schemas.py \
  -v
```

如果 `conda run -n test` 不可用，则使用当前 Python：

```bash
python -m pytest tests/ -v
```

完成标准：

- `git diff` 只包含当前 scanner/index WIP 范围内的收敛改动。
- helper 模块导入边界清楚。
- `FileScanner` / `ScanIndexStore` 兼容原有调用。
- 上述关键测试通过；如果全量测试能跑通，则以全量 pytest 作为最终证据。
- 若验证失败，报告失败命令、错误原因和下一步建议。
