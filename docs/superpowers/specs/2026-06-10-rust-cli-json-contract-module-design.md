# Rust CLI JSON Contract Module Design

Status: REVIEW_READY
Mode: improve-codebase-architecture + grilling
Date: 2026-06-10

## 目标

把 Rust discovery 和 Rust Office parser 共同的 stdin/stdout JSON 执行契约收敛为一个 deep Module: **Rust CLI JSON contract**。

这个 Module 只负责 Rust helper CLI 的执行和契约机械部分:

- binary path 解析。
- request payload JSON 序列化。
- subprocess 执行、超时和启动失败处理。
- stdout JSON 解码。
- stderr、return code 和错误类型归一。
- 调用 Adapter 提供的 response payload validator。
- 返回包含 payload、contract error、耗时和调试信息的结构化结果。

这个 Module 不负责 discovery、Office fallback、parser profile、cache、inventory、metrics、report 或 benchmark 的业务判断。业务策略继续留在对应 Adapter 和 scanner Module 中。

相关 glossary:

- `Rust CLI JSON contract`
- `Cold scanner run`
- `Hybrid Office fallback policy`
- `Office parser contract failure`

相关 ADR:

- `docs/adr/0001-performance-first-hybrid-office-fallback.md`

## 输入

Module Interface 接收:

- `binary_path`: Rust helper CLI 路径，可以是绝对路径，也可以是相对仓库根目录的路径。
- `request_payload`: Adapter 构造的 JSON request object。
- `timeout_seconds`: 单次 CLI 执行预算。
- `contract_name`: 调试和错误归因用名称，例如 `rust_discovery` 或 `rust_office_parser`。
- `validator`: Adapter 提供的 callable，输入 decoded JSON，输出受信任的领域 payload。
- `project_root`: 可选，测试或特殊运行环境可显式传入；默认从 `src/services/rust_cli_contract.py` 反推仓库根目录。
- `json_indent`: 可选，默认不要求固定值；Office 当前用 `indent=2`，discovery 当前不用缩进。除非 Rust 端依赖格式，格式差异不应成为 contract。

Adapter 继续提供:

- Discovery request:
  - `work_dir`
  - `start_date`
  - `end_date`
  - `allowed_extensions`
  - `ignored_patterns`
  - `excluded_dirs`
- Office request:
  - `path`
  - `file_path`
  - `file_type`
  - `limits`
  - `parser_backend`
- Discovery response validator:
  - 校验 stdout JSON 是 list。
  - 校验每个 item 可以转换为 `DiscoveredFile`。
- Office response validator:
  - 校验 stdout JSON 可以转换为 `FileContext`。
  - 校验 `file_path`、`file_type`、`parser_backend` 满足本次 request 预期。

## 输出

Module 输出一个结构化结果，建议形状如下:

```python
@dataclass(frozen=True, slots=True)
class RustCliContractError:
    kind: Literal[
        "request_serialization_failed",
        "timeout",
        "start_failed",
        "nonzero_exit",
        "invalid_stdout_encoding",
        "invalid_json",
        "invalid_payload",
    ]
    message: str
    returncode: int | None
    stderr: str
    stdout_excerpt: str


@dataclass(frozen=True, slots=True)
class RustCliJsonResult(Generic[T]):
    payload: T | None
    error: RustCliContractError | None
    duration_ms: int
    binary_path: Path
```

成功输出:

- `payload` 为 validator 返回的受信任对象。
- `error` 为 `None`。
- `duration_ms` 为 CLI 执行和 contract validation 的总耗时。
- `binary_path` 为解析后的实际路径。

失败输出:

- `payload` 为 `None`。
- `error.kind` 表示 contract failure 类型。
- `error.message` 是短错误原因，供 Adapter 映射到当前公开错误。
- `stderr` 和 `stdout_excerpt` 用于日志和测试，不应承载业务策略。
- `duration_ms` 即使失败也必须填充，供 Office audit 继续记录 Rust attempt 耗时。

Adapter 对外输出保持不变:

- `RustDiscoveryRunner.discover()` 仍返回 `list[DiscoveredFile]`，失败时抛出 `RustDiscoveryError`，由 `FileDiscoveryService.bootstrap_full_scan()` fallback 到 Python discovery。
- `RustOfficeParserRunner.parse()` 仍返回 `tuple[FileContext, int]`，失败时返回带现有 `RUST_OFFICE_*` 前缀的 error `FileContext`。
- `parse_office_with_fallback()` 和 `classify_office_failure()` 的 fallback policy 不迁入本 Module。

## 非目标

- 不改 Rust discovery CLI stdout schema。
- 不改 Rust Office parser stdout schema。
- 不改 scanner 输出 contract。
- 不改 Office fallback policy。
- 不改 `office_fallback_after_timeout` 默认值。
- 不改 parser profile key。
- 不改 scan cache、inventory 或 benchmark schema。
- 不做 batch CLI、long-running worker、PyO3、wheel bundled binaries。
- 不启动 Phase B 的 Cold scanner run Module 收敛。

## 当前事实

当前代码里 discovery 和 Office parser 已经分别实现了相似的 contract mechanics。

Discovery 当前事实:

- `src/services/scan_discovery.py::RustDiscoveryRunner.discover()` 构造 request dict。
- 它通过 `subprocess.run()` 执行 Rust discovery binary。
- 它使用 `json.dumps(..., ensure_ascii=False)` 写 stdin。
- 它使用 `text=True`、`encoding="utf-8"`、`errors="strict"`、`capture_output=True`。
- 它处理 non-zero return code。
- 它处理 `json.loads(completed.stdout)` 的 invalid JSON。
- 它在 `_to_discovered_file()` 中校验 payload item。
- `FileDiscoveryService.bootstrap_full_scan()` 捕获 `OSError`、`subprocess.SubprocessError`、`RustDiscoveryError` 后 fallback 到 Python discovery。

Office 当前事实:

- `src/services/office_parser.py::RustOfficeParserRunner.parse()` 构造 request dict。
- 它通过 `subprocess.run()` 执行 Rust Office parser binary。
- 它使用 `json.dumps(..., ensure_ascii=False, indent=2)` 写 stdin。
- 它处理 timeout、`OSError`、non-zero return code、invalid JSON 和 invalid payload。
- 它返回 `_error_context()` 而不是抛出给 scanner 主流程。
- `_validate_rust_payload_context()` 校验 `file_path`、`file_type` 和 `parser_backend`。
- `classify_office_failure()` 根据 `RUST_OFFICE_TIMEOUT`、`RUST_OFFICE_START_FAILED`、`RUST_OFFICE_INVALID_JSON`、`RUST_OFFICE_INVALID_PAYLOAD` 等 error prefix 决定 failure class 和 fallback。

这些行为都应该保留。重构目标不是改变结果，而是把重复的 execution contract 做成一个有 Leverage 的 deep Module。

## 设计原则

1. Correctness 优先。Module 必须让 Adapter 明确知道 payload 是否可信，不能把 invalid JSON 或 invalid payload 当作空结果。
2. Interface 小。Adapter 只需要传 binary、request、timeout 和 validator，不需要理解 subprocess 细节。
3. Implementation 深。path 解析、JSON 编码、执行、超时、stdout/stderr 捕获、JSON 解码和 validator 调用都在 Module 内部完成。
4. Policy 外置。fallback、cache、metrics、parser backend 选择、benchmark 分类继续在现有 scanner/Office parser Module。
5. 输出 shape-preserving。重构前后 scanner 用户看到的 discovery 和 Office parse 结果不变。
6. Locality 强。未来新增 Rust helper CLI 时，只新增 Adapter request/validator/error mapping，不复制 subprocess JSON 模板。

## Module 边界

新增文件:

- `src/services/rust_cli_contract.py`

该 Module 拥有:

- `resolve_binary_path()`
- `run_rust_json_cli()`
- `RustCliJsonResult`
- `RustCliContractError`
- 内部 `_elapsed_ms()`
- 内部 stdout excerpt 截断逻辑

该 Module 不拥有:

- `DiscoveredFile`
- `FileContext`
- `RustDiscoveryError`
- `OfficeParseAudit`
- `classify_office_failure()`
- `parse_office_with_fallback()`
- scanner config policy
- parser profile key policy

Adapter 仍拥有:

- request construction。
- response validator。
- public error mapping。
- fallback 调用。
- logging context。
- Rust helper backend name。

## Interface 草案

推荐 Interface:

```python
T = TypeVar("T")
PayloadValidator = Callable[[Any], T]


def run_rust_json_cli(
    *,
    binary_path: str | Path,
    request_payload: Mapping[str, Any],
    timeout_seconds: float,
    validator: PayloadValidator[T],
    contract_name: str,
    project_root: Path | None = None,
    json_indent: int | None = None,
) -> RustCliJsonResult[T]:
    ...
```

关键约束:

- `binary_path` 相对路径从仓库根目录解析，保持 discovery 和 Office 现在的行为。
- `request_payload` 序列化失败时返回 `request_serialization_failed`，这是 Python Adapter bug，不应调用 Rust CLI。
- `TimeoutExpired` 和 `TimeoutError` 归为 `timeout`。
- `OSError` 归为 `start_failed`。
- return code 非 0 归为 `nonzero_exit`，`message` 优先使用 trimmed stderr，否则使用 `exit code N`。
- stdout 解码失败归为 `invalid_stdout_encoding`。这在 Office Adapter 中应映射为 contract failure，而不是 parser content failure。
- `json.loads()` 失败归为 `invalid_json`。
- validator 抛错归为 `invalid_payload`。
- validator 必须只做 contract validation 和领域对象构造，不做 fallback policy。

## Adapter 映射

### Discovery Adapter

`RustDiscoveryRunner.discover()` 应变成 thin Adapter:

1. 构造 discovery request。
2. 调用 `run_rust_json_cli(..., validator=self._validate_discovery_payload)`。
3. 成功时返回 `result.payload`。
4. 失败时抛出 `RustDiscoveryError`。
5. `FileDiscoveryService.bootstrap_full_scan()` 保持现有 fallback 行为。

Discovery public error 不需要新增稳定 code，因为上层当前只把它作为 fallback trigger 和 warning 日志内容。实现时要保留 warning 的上下文: `rust_discovery_bin`、`work_dir`、`start_date`、`end_date`。

### Office Adapter

`RustOfficeParserRunner.parse()` 应变成 thin Adapter:

1. 构造 Office request。
2. 调用 `run_rust_json_cli(..., validator=validate_file_context_for_request)`。
3. 成功时返回 `(context, result.duration_ms)`。
4. 失败时把 `result.error.kind` 映射为现有 error prefix:

| Contract error kind | Office public error |
|---|---|
| `request_serialization_failed` | `RUST_OFFICE_INVALID_PAYLOAD: ...` |
| `timeout` | `RUST_OFFICE_TIMEOUT: file parse exceeded {timeout_seconds:g}s` |
| `start_failed` | `RUST_OFFICE_START_FAILED: ...` |
| `nonzero_exit` | `RUST_OFFICE_PARSE_FAILED: ...` |
| `invalid_stdout_encoding` | `RUST_OFFICE_INVALID_JSON: ...` |
| `invalid_json` | `RUST_OFFICE_INVALID_JSON: ...` |
| `invalid_payload` | `RUST_OFFICE_INVALID_PAYLOAD: ...` |

这个映射必须保持 `classify_office_failure()` 的现有语义:

- timeout 默认属于 deterministic failure，不 fallback，除非 `office_fallback_after_timeout=true`。
- start failed 属于 environment-unavailable failure，可以 fallback。
- invalid JSON 和 invalid payload 属于 contract failure，可以 fallback。
- nonzero exit 属于 Rust parser failure，由 Office policy 判断是否可 fallback。

## 测试策略

新增测试文件:

- `tests/test_rust_cli_contract.py`

该测试文件覆盖 Module 本身:

- 相对 binary path 从仓库根目录解析。
- request payload 使用 UTF-8 JSON 写入 stdin。
- success path 调用 validator，并返回 validator 产物。
- `subprocess.TimeoutExpired` -> `timeout`。
- `TimeoutError` -> `timeout`。
- `OSError` -> `start_failed`。
- return code 非 0 -> `nonzero_exit`，stderr 优先。
- stdout 不是 UTF-8 -> `invalid_stdout_encoding`。
- stdout 不是 JSON -> `invalid_json`。
- stdout JSON 可以解析但 validator 抛错 -> `invalid_payload`。
- request payload 不可 JSON 序列化 -> `request_serialization_failed`，且不调用 subprocess。
- failure result 仍有 `duration_ms`。

更新现有测试:

- `tests/test_scan_discovery.py`
  - 保持 Rust backend 成功 request contract 不变。
  - 保持 Rust 失败 fallback 到 Python discovery。
  - 保持 invalid discovery JSON / invalid item fallback 到 Python discovery。
- `tests/test_rust_discovery_contract.py`
  - 继续验证 Rust discovery CLI 和 Python discovery 的 contract 等价。
- `tests/test_office_parser.py`
  - 保持 `RUST_OFFICE_INVALID_JSON`。
  - 保持 `RUST_OFFICE_INVALID_PAYLOAD`。
  - 保持 `RUST_OFFICE_TIMEOUT`。
  - 保持 `RUST_OFFICE_START_FAILED`。
  - 保持 fallback classification 和 audit 字段。

推荐局部验证命令:

```powershell
conda run -n test python -m pytest tests/test_rust_cli_contract.py tests/test_scan_discovery.py tests/test_rust_discovery_contract.py tests/test_office_parser.py -q
```

推荐完整 Python 验证命令:

```powershell
conda run -n test python -m pytest tests -q
conda run -n test python -m compileall main.py src tests
```

如果实现不改 Rust 代码，不要求每次都重跑 Cargo。但如果 Adapter contract 修改触及 Rust request/response schema，应补跑:

```powershell
cd rust/discovery && cargo test
cd rust/office_parser && cargo test
```

## 验收条件

- `src/services/scan_discovery.py` 不再直接调用 `subprocess.run()`。
- `src/services/office_parser.py` 不再直接调用 `subprocess.run()`。
- `src/services/rust_cli_contract.py` 是唯一拥有 Rust helper subprocess JSON mechanics 的 Python Module。
- Discovery success path 返回的 `DiscoveredFile` 字段和排序语义不变。
- Discovery failure path 仍 fallback 到 Python discovery。
- Office success path 返回的 `FileContext` 不变。
- Office failure path 的 `RUST_OFFICE_*` error prefix 不变。
- `classify_office_failure()` 的 failure class 和 fallback decision 不变。
- `OfficeParseAudit.rust_duration_ms` 仍能记录失败耗时。
- 现有 benchmark 不需要改字段名即可读取 parser/discovery evidence。

## 风险点 / 边界条件

- Discovery 当前把 `subprocess.TimeoutExpired` 作为 fallback trigger 间接处理；重构后如果直接映射成 `RustDiscoveryError`，必须确认仍被 `FileDiscoveryService.bootstrap_full_scan()` 捕获并 fallback。
- Office 的 timeout message 现在包含 `{timeout_seconds:g}s`。实现时不能把它替换成通用 `timeout`，否则现有测试和用户日志会漂移。
- Office invalid stdout encoding 应归为 contract failure，映射到 `RUST_OFFICE_INVALID_JSON`，避免被误判成 file content parse failure。
- Module 不应吞掉 validator 的错误细节，但要避免把过长 stdout 全量写入日志。
- request payload 序列化失败是 Python Adapter bug，不是 Rust binary failure；它不应启动 subprocess。
- binary path resolution 必须保持 Windows `.exe` 路径可用，也必须保持 Linux release binary 路径可用。
- `json_indent` 只影响可读性，不应被测试为业务 contract，除非 Rust CLI 明确要求。
- Validator 只能判断 stdout payload 是否满足 Python scanner contract，不能偷偷执行 fallback。
- 不能把 `RustCliContractError.kind` 直接暴露给 scanner 输出；对外仍使用现有领域错误。

## 伪代码草案

```python
# [伪代码草案]
# 目标：
# - 把 Rust helper CLI 的 JSON 执行契约集中到一个 deep Module。
# - 让 discovery 和 Office parser Adapter 只负责 request、validator 和 public error mapping。
# - 保持 scanner 对外输出和 fallback policy 不变。
#
# 输入：
# - binary_path: 配置中的 Rust CLI 路径，可能是相对仓库根目录的路径
# - request_payload: Adapter 构造的 JSON request object
# - timeout_seconds: 本次 Rust CLI 执行预算
# - validator: Adapter 提供的 stdout payload validator
# - contract_name: 日志和错误归因名称
#
# 输出：
# - success_result: 包含 validator 产物、resolved binary path、duration_ms
# - error_result: 包含 contract error kind、message、stderr、stdout excerpt、duration_ms

def run_rust_json_cli(
    *,
    binary_path,
    request_payload,
    timeout_seconds,
    validator,
    contract_name,
    project_root=None,
    json_indent=None,
):
    # 1. 路径解析集中处理，避免每个 Adapter 都复制 project root 计算逻辑
    resolved_binary = resolve_binary_path(binary_path, project_root)
    started_at = perf_counter()

    try:
        # 2. stdin JSON 在启动进程前生成；失败说明 Adapter 传入了非 contract payload
        request_json = json.dumps(
            request_payload,
            ensure_ascii=False,
            indent=json_indent,
        )
    except (TypeError, ValueError) as exc:
        return error_result(
            kind="request_serialization_failed",
            message=str(exc),
            binary_path=resolved_binary,
            duration_ms=elapsed_ms(started_at),
        )

    try:
        # 3. 统一 subprocess 参数，保证所有 Rust helper 都按 UTF-8 JSON contract 运行
        completed = subprocess.run(
            [str(resolved_binary)],
            input=request_json,
            text=True,
            encoding="utf-8",
            errors="strict",
            capture_output=True,
            timeout=float(timeout_seconds),
            check=False,
        )
    except (subprocess.TimeoutExpired, TimeoutError) as exc:
        return error_result(
            kind="timeout",
            message=str(exc) or f"{contract_name} exceeded {timeout_seconds:g}s",
            binary_path=resolved_binary,
            duration_ms=elapsed_ms(started_at),
        )
    except UnicodeDecodeError as exc:
        # stdout/stderr 不是 UTF-8 时，Rust CLI 已经违反 JSON stdout contract
        return error_result(
            kind="invalid_stdout_encoding",
            message=str(exc),
            binary_path=resolved_binary,
            duration_ms=elapsed_ms(started_at),
        )
    except OSError as exc:
        return error_result(
            kind="start_failed",
            message=str(exc),
            binary_path=resolved_binary,
            duration_ms=elapsed_ms(started_at),
        )

    if completed.returncode != 0:
        message = completed.stderr.strip() or f"exit code {completed.returncode}"
        return error_result(
            kind="nonzero_exit",
            message=message,
            returncode=completed.returncode,
            stderr=completed.stderr,
            stdout_excerpt=excerpt(completed.stdout),
            binary_path=resolved_binary,
            duration_ms=elapsed_ms(started_at),
        )

    try:
        decoded_payload = json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        return error_result(
            kind="invalid_json",
            message=str(exc),
            stderr=completed.stderr,
            stdout_excerpt=excerpt(completed.stdout),
            binary_path=resolved_binary,
            duration_ms=elapsed_ms(started_at),
        )

    try:
        # validator 是 Adapter 提供的，因为只有 Adapter 知道领域 payload 是否可信
        trusted_payload = validator(decoded_payload)
    except Exception as exc:
        return error_result(
            kind="invalid_payload",
            message=str(exc),
            stderr=completed.stderr,
            stdout_excerpt=excerpt(completed.stdout),
            binary_path=resolved_binary,
            duration_ms=elapsed_ms(started_at),
        )

    return success_result(
        payload=trusted_payload,
        binary_path=resolved_binary,
        duration_ms=elapsed_ms(started_at),
    )


def discovery_adapter_discover(work_dir, start_date, end_date):
    # Discovery Adapter 只构造 request 和映射失败，不再拥有 subprocess JSON mechanics
    request = build_discovery_request(work_dir, start_date, end_date, scanner_cfg)
    result = run_rust_json_cli(
        binary_path=scanner_cfg["rust_discovery_bin"],
        request_payload=request,
        timeout_seconds=scanner_cfg["discovery_timeout_seconds"],
        validator=validate_discovery_payload,
        contract_name="rust_discovery",
    )
    if result.error is None:
        return result.payload

    # 抛出 RustDiscoveryError 是为了保留 FileDiscoveryService 的 Python fallback 入口
    raise RustDiscoveryError(result.error.message)


def office_adapter_parse(file_path, file_type, limits, timeout_seconds):
    # Office Adapter 保留 RUST_OFFICE_* public error mapping，供 fallback policy 分类
    request = build_office_request(file_path, file_type, limits)
    result = run_rust_json_cli(
        binary_path=rust_office_parser_bin,
        request_payload=request,
        timeout_seconds=timeout_seconds,
        validator=lambda payload: validate_office_context(
            payload,
            expected_file_path=str(file_path),
            expected_file_type=file_type.lower(),
        ),
        contract_name="rust_office_parser",
        json_indent=2,
    )
    if result.error is None:
        return result.payload, result.duration_ms

    public_error = map_contract_error_to_rust_office_error(
        result.error,
        timeout_seconds=timeout_seconds,
    )
    return _error_context(file_path, file_type.lower(), public_error), result.duration_ms
```

