# Rust Discovery Backend Design

Status: APPROVED
Mode: Builder

## 目标

用 Rust 替换当前 scanner 的文件发现边界，解决 warm benchmark 中 discovery 阶段占主要耗时的问题。

本阶段只替换 `FileDiscoveryService.bootstrap_full_scan()` 的底层遍历实现。Python 继续负责 scanner 编排、inventory/cache、parser profile、parse cache、文件内容解析、聚合、SQLite 审计和 ContextScheduler。

当前证据：

- warm scanner benchmark：总耗时 `1461ms`，discovery `1374ms`，parse `23ms`。
- warm context scheduler benchmark：scan 总耗时 `1498ms`，discovery `1382ms`，parse `57ms`。

这说明当前主要瓶颈已经从 parser 转移到 Python discovery：`os.walk()`、逐目录排除判断、逐文件扩展名/忽略模式过滤、`stat()`、日期过滤和元数据构造。

## 输入

Rust discovery CLI 接收一份结构化请求，字段来自 Python `FileDiscoveryService` 当前上下文：

- `work_dir`: 扫描根目录。
- `start_date`: 起始日期，格式 `YYYY-MM-DD`。
- `end_date`: 结束日期，格式 `YYYY-MM-DD`。
- `allowed_extensions`: 允许扩展名列表，例如 `.md`、`.xlsx`、`.log`。
- `ignored_patterns`: 文件名忽略 glob，例如 `~$*`、`*.tmp`。
- `excluded_dirs`: 需要跳过的目录列表。

运行环境约束：

- Linux 本地开发环境。
- Rust stable toolchain，由 `rustup` 提供。
- Python 仍使用当前 Conda `test` 环境运行测试。
- 第一阶段不要求 Windows 编译产物，但输出契约必须兼容 Windows 路径语义，避免后续扩展时重写接口。

外部依赖：

- Rust CLI 位于仓库内，例如 `rust/discovery/`。
- Python 使用 `subprocess` 调用 Rust CLI。
- Rust 输出 `stdout` JSON，Python 读取并转换为现有 `DiscoveredFile`。

## 输出

成功输出：

- `list[DiscoveredFile]`，字段与当前 Python discovery 完全一致：
  - `file_identity`
  - `path`
  - `extension`
  - `modified_at`
  - `size_bytes`
  - `source_version`

失败输出：

- Rust CLI 进程失败、JSON 解析失败、字段缺失或契约校验失败时，Python 记录 warning，并回退到现有 Python discovery。
- 单个文件 `stat` 失败时，Rust 不应让整次 discovery 失败；应返回结构化 warning 或 stderr 行。Python 第一阶段可以先记录 warning，不写入 `DiscoveredFile`。

副作用：

- Rust discovery 不写 SQLite。
- Rust discovery 不解析文件内容。
- Rust discovery 不修改配置文件。
- Rust discovery 不直接写 benchmark 文件。
- Python scanner 仍按现有流程写 `file_inventory`、`scan_runs` 和 parse cache。

## 设计原则

1. Correctness 优先。Rust 输出必须与当前 Python discovery 的业务语义一致。
2. 最小替换面。只替换 discovery，不碰 parser/cache/aggregator/ContextScheduler。
3. 可回退。Rust 失败时回到 Python discovery，避免新增 native 工具导致 CLI 不可用。
4. 可学习。Rust 代码保持直线流程和清晰类型，避免第一阶段引入复杂 async、FFI 或宏抽象。
5. 可观测。benchmark 必须能区分 `python` / `rust` discovery backend 和 discovery 阶段耗时。

## 方案选择

### 推荐方案：Rust CLI + Python Fallback

仓库新增一个 Rust CLI，例如：

```text
rust/discovery/
  Cargo.toml
  src/main.rs
```

Python `FileDiscoveryService` 增加 backend 选择：

- `scanner.discovery_backend = "rust"`：默认优先调用 Rust CLI，失败则 fallback 到 Python。
- `scanner.discovery_backend = "python"`：显式强制使用现有 `os.walk()`，用于对照 benchmark 或临时排障。
- 缺省值也按 `rust` 处理，让真实扫描优先验证 Rust discovery；fresh clone 未构建二进制时仍通过 fallback 保持可用。

推荐原因：

- 构建和调用模型直观，适合初学 Rust。
- 不需要 Python native extension 打包。
- Rust 和 Python 契约可以通过 fixture 测试直接比较。
- 出问题时不会影响 parser 和上层报告生成。

### 备选方案：Rust CLI 直写 SQLite Inventory

Rust 直接扫描并写入 `file_inventory`，Python 后续从 SQLite 读取。

不推荐第一阶段：

- 会让 Rust 同时承担 discovery 和持久化副作用。
- 需要把 SQLite schema、迁移边界和事务策略同步到 Rust。
- 一旦出错，排查会跨 Rust、Python、SQLite 三层。

### 备选方案：PyO3 / maturin Native Extension

Rust 编译为 Python 扩展，Python 直接 import 调用。

不推荐第一阶段：

- 接口漂亮，但构建、发布和 Conda 环境集成复杂度更高。
- 初学 Rust 时不利于区分 Rust 逻辑本身和 Python FFI 问题。
- 后续稳定后可以再评估迁移。

## 契约细节

Rust 输出的每个文件必须满足：

```json
{
  "file_identity": "bootstrap:/home/george/work/report.md",
  "path": "/home/george/work/report.md",
  "extension": ".md",
  "modified_at": "2026-05-25T10:00:00",
  "size_bytes": 1234,
  "source_version": "mtime_ns=1779674400000000000:size=1234"
}
```

字段规则：

- `path`: 使用文件实际路径字符串。Linux 第一阶段输出绝对路径更稳定；Python 转成 `Path` 后继续使用。
- `file_identity`: 继续使用 `bootstrap:{resolved_path_lower}`。
- `extension`: 小写后缀，包含前导点。
- `modified_at`: ISO-like datetime 字符串，Python 用 `datetime.fromisoformat()` 还原。
- `size_bytes`: 文件大小。
- `source_version`: 继续使用 `mtime_ns={mtime_ns}:size={size_bytes}`。

过滤规则必须保持：

- 扩展名匹配大小写不敏感。
- `ignored_patterns` 只匹配文件名，不匹配完整路径。
- `ignored_patterns` 保持 glob 语义。
- `excluded_dirs` 按目录前缀排除，目录自身和子目录都跳过。
- 日期过滤覆盖 `start_date 00:00:00` 到 `end_date 23:59:59.999999`。
- 不主动跟随目录 symlink，保持接近 Python `os.walk(..., followlinks=False)` 的默认行为。

排序规则：

- 第一阶段保持稳定排序，按 `path` 字符串升序输出。
- 这样 Python 和 Rust fixture 测试更容易比较，也减少 benchmark 和快照输出抖动。

## 配置

新增 scanner 配置项：

```yaml
scanner:
  discovery_backend: "rust"  # rust | python
  rust_discovery_bin: "rust/discovery/target/release/ai-daily-discovery"
```

建议第一阶段默认值：

- `discovery_backend = "rust"`，优先验证 Rust discovery 的真实收益。
- 未构建 Rust CLI 或路径配置错误时自动 fallback 到 Python，避免 fresh clone 直接不可用。
- 需要做 Python baseline benchmark 或临时排障时，在 `config/settings.linux.yaml` 显式改成 `python`。

## 数据流

1. `FileScanner.scan_files()` 调用 `FileDiscoveryService.bootstrap_full_scan(start_date, end_date)`。
2. `FileDiscoveryService` 根据 `scanner_cfg["discovery_backend"]` 选择 backend。
3. Python backend 走现有 `os.walk()`。
4. Rust backend 构建 JSON 请求，通过 stdin 传给 Rust CLI。
5. Rust CLI 扫描目录，stdout 输出 JSON。
6. Python 校验并转换为 `DiscoveredFile`。
7. 如果 Rust 调用失败，记录 warning，回退 Python backend。
8. 后续 inventory/cache/parser/aggregator 完全沿用当前 Python 流程。

## 错误处理

Rust CLI 进程级错误：

- 非零退出码。
- stdout 不是合法 JSON。
- JSON 字段缺失或类型不对。
- CLI 文件不存在或没有执行权限。

处理策略：

- Python warning 日志写明 Rust discovery 失败原因。
- 回退到 Python discovery。
- scanner 不返回成功假象；如果 fallback 也失败，则按现有 Python 异常路径处理。

单文件错误：

- 无法读取 metadata。
- 权限不足。
- 文件在遍历和 `stat` 之间被删除。

处理策略：

- Rust 记录 warning。
- 跳过该文件。
- 不让单文件错误中断整次扫描。

## 测试策略

Python 单元测试：

- `FileDiscoveryService` 默认优先使用 Rust backend。
- `discovery_backend = "rust"` 时会调用 Rust runner。
- Rust runner 成功时转换为 `DiscoveredFile`。
- Rust runner 失败时 fallback 到 Python discovery。
- 配置项缺失时默认使用 Rust backend，并保留失败 fallback。

契约测试：

- 同一组 fixture 文件，Python backend 和 Rust backend 输出同一批 `file_identity`、`extension`、`size_bytes`、`source_version`。
- 覆盖大小写扩展名、ignored patterns、excluded dirs、日期过滤。

Rust 单元测试：

- 扩展名大小写匹配。
- glob 忽略规则。
- excluded dirs 前缀过滤。
- source_version 格式。
- 日期边界包含起止日。

集成 benchmark：

- 跑 Python discovery benchmark 作为 baseline。
- 构建 Rust release binary。
- 切 `discovery_backend = "rust"` 后跑同样日期范围。
- 比较 discovery 阶段耗时、发现文件数、成功/失败数。

## 风险点 / 边界条件

- Rust 和 Python 时间精度可能不同。必须确认 Linux 下能拿到 nanosecond mtime，并生成与 Python `st_mtime_ns` 一致的 `mtime_ns`。
- 路径大小写规则在 Linux 和 Windows 不同。第一阶段在 Linux 验证，仍保留 `lower()` 的 `file_identity` 契约。
- Rust CLI 路径配置错误会触发 fallback。benchmark 需要明确输出当前 discovery backend 和 fallback warning，避免误以为 Rust 二进制真实生效。
- 如果扫描根目录过大，stdout JSON 数组会占用内存。第一阶段 Python 仍需要完整列表，因此 JSON 数组可接受；后续如要极大规模扫描，再改 JSON Lines 流式处理。
- `ignored_patterns` 的 glob 实现要和 Python `fnmatch` 足够接近。第一阶段只承诺覆盖项目已有模式，先不扩展复杂 glob 语义。

## 非目标

- 不重写 parser。
- 不把 scan planner 搬到 Rust。
- 不让 Rust 写 SQLite。
- 不引入常驻 daemon。
- 不做 PyO3/maturin 扩展。
- 不改变 `ScanResult`、`FileContext`、`DiscoveredFile` 的上层业务契约。
- 不改变 ContextScheduler 压缩策略。

## 验收方式

1. `cargo test` 在 `rust/discovery/` 下通过。
2. `cargo build --release` 生成 Rust discovery CLI。
3. `conda run -n test python -m pytest tests/test_scan_discovery.py tests/test_file_scanner.py -q` 通过。
4. `conda run -n test python -m pytest tests/ -q` 通过。
5. Rust backend 与 Python backend 在 fixture 中输出一致。
6. 本机 benchmark 明确显示 `discovery_backend = rust`。
7. Rust backend 的 scanner benchmark 中 `discovery_duration_ms` 明显低于当前 Python baseline，且 `discovered_count` 不减少。

## 伪代码草案

### Python 侧

```python
# [伪代码草案]
# 目标：让 FileDiscoveryService 可以选择 Python 或 Rust discovery，但保持返回 DiscoveredFile 列表不变

class FileDiscoveryService:
    def bootstrap_full_scan(self, start_date: date, end_date: date) -> list[DiscoveredFile]:
        backend = self.scanner_cfg.get("discovery_backend", "rust")

        if backend == "rust":
            try:
                # 为什么先走独立 runner：把 subprocess、JSON 校验和 fallback 边界集中起来，
                # 避免 FileDiscoveryService 主流程被进程调用细节打散。
                return RustDiscoveryRunner(self.scanner_cfg).discover(
                    work_dir=self.work_dir,
                    start_date=start_date,
                    end_date=end_date,
                )
            except RustDiscoveryError as exc:
                logger.warning("Rust discovery failed, fallback to Python: %s", exc)

        return self._bootstrap_full_scan_python(start_date, end_date)


class RustDiscoveryRunner:
    def discover(self, work_dir: Path, start_date: date, end_date: date) -> list[DiscoveredFile]:
        request = {
            "work_dir": str(work_dir),
            "start_date": start_date.isoformat(),
            "end_date": end_date.isoformat(),
            "allowed_extensions": self.scanner_cfg["allowed_extensions"],
            "ignored_patterns": self.scanner_cfg["ignored_patterns"],
            "excluded_dirs": self.scanner_cfg.get("excluded_dirs", []),
        }

        completed = subprocess.run(
            [self._resolve_binary_path()],
            input=json.dumps(request, ensure_ascii=False),
            text=True,
            capture_output=True,
            timeout=self.scanner_cfg.get("discovery_timeout_seconds", 30),
            check=False,
        )

        if completed.returncode != 0:
            raise RustDiscoveryError(completed.stderr)

        raw_items = json.loads(completed.stdout)
        return [self._to_discovered_file(item) for item in raw_items]

    def _to_discovered_file(self, item: dict[str, object]) -> DiscoveredFile:
        # 这里做字段校验，是为了把 Rust/Python 契约错误尽早暴露，
        # 避免坏数据进入 SQLite inventory 后才表现成 cache 异常。
        return DiscoveredFile(
            file_identity=str(item["file_identity"]),
            path=Path(str(item["path"])),
            extension=str(item["extension"]),
            modified_at=datetime.fromisoformat(str(item["modified_at"])),
            size_bytes=int(item["size_bytes"]),
            source_version=str(item["source_version"]),
        )
```

### Rust 侧

```rust
// [伪代码草案]
// 目标：读取 Python 传入的 discovery 请求，遍历目录，输出与 DiscoveredFile 对齐的 JSON。

struct DiscoveryRequest {
    work_dir: PathBuf,
    start_date: NaiveDate,
    end_date: NaiveDate,
    allowed_extensions: Vec<String>,
    ignored_patterns: Vec<String>,
    excluded_dirs: Vec<PathBuf>,
}

struct DiscoveredFileOut {
    file_identity: String,
    path: String,
    extension: String,
    modified_at: String,
    size_bytes: u64,
    source_version: String,
}

fn main() -> Result<()> {
    let request = read_json_request_from_stdin()?;
    let files = discover_files(&request)?;
    print_json_to_stdout(files)?;
    Ok(())
}

fn discover_files(request: &DiscoveryRequest) -> Result<Vec<DiscoveredFileOut>> {
    let mut files = Vec::new();
    let start_dt = request.start_date.and_hms_nano(0, 0, 0, 0);
    let end_dt = request.end_date.and_hms_nano(23, 59, 59, 999_999_999);

    for entry in walk_directory_without_following_links(&request.work_dir) {
        let path = entry.path();

        if entry.is_dir() && is_excluded_dir(path, &request.excluded_dirs) {
            skip_current_dir();
            continue;
        }

        if !entry.is_file() {
            continue;
        }

        let file_name_lower = lower_file_name(path);
        if !has_allowed_extension(&file_name_lower, &request.allowed_extensions) {
            continue;
        }
        if matches_ignored_pattern(&file_name_lower, &request.ignored_patterns) {
            continue;
        }

        let metadata = match std::fs::metadata(path) {
            Ok(value) => value,
            Err(error) => {
                // 单文件读取失败不应中断整次 discovery。
                eprintln!("warning: cannot stat {}: {}", path.display(), error);
                continue;
            }
        };

        let modified_at = metadata_modified_datetime(&metadata)?;
        if modified_at < start_dt || modified_at > end_dt {
            continue;
        }

        let resolved_path = canonicalize_path(path)?;
        let size_bytes = metadata.len();
        let mtime_ns = metadata_modified_time_nanos(&metadata)?;

        files.push(DiscoveredFileOut {
            file_identity: format!("bootstrap:{}", resolved_path.to_string_lossy().to_lowercase()),
            path: resolved_path.to_string_lossy().to_string(),
            extension: lower_extension(path),
            modified_at: modified_at.to_string(),
            size_bytes,
            source_version: format!("mtime_ns={}:size={}", mtime_ns, size_bytes),
        });
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}
```
