# Python Wheel With Bundled Rust Binaries Design

Status: ARCHIVED - NOT PLANNED FOR EXECUTION
Date: 2026-05-26
Archived Date: 2026-05-27
Archive Note: User decided not to execute the Python wheel / bundled Rust binary release path now. Keep this design as reference material only.

## 目标

把 `ai_daily_report` 打成完整 Python wheel，让没有 Rust / Cargo 环境的 Linux 和 Windows 用户也能安装运行 CLI。

第一版采用平台 wheel 内置预编译 Rust CLI 二进制的方案：

- Linux wheel 内置 Linux x86_64 Rust binaries。
- Windows wheel 内置 Windows amd64 Rust `.exe` binaries。
- Python 运行时自动选择包内 binary；开发者仍可用配置覆盖到源码树 `target/release`。
- GitHub Actions matrix 构建两个平台 wheel，并上传到 GitHub Release。

版本策略统一为：

- Python package version: `0.5.0`
- Git tag: `v0.5.0`
- GitHub Release title: `ver 0.5`
- Wheel version segment: `0.5.0`

使用 `0.x` 是因为当前工具仍处于内部试用和打磨阶段，Rust parser、配置初始化、跨平台安装体验和发布流程都还会继续迭代，不应使用 `5.x` 传达成熟稳定大版本的信号。

## 输入

- 当前仓库 `/home/george/Python_program/ai_daily_report`。
- Python 代码、模板、示例配置和 CLI 入口。
- Rust crates:
  - `rust/discovery`
  - `rust/office_parser`
- GitHub Actions runner:
  - `ubuntu-latest`
  - `windows-latest`
- Rust stable toolchain。
- Python 3.10+ 构建环境。
- GitHub Release 发布权限，使用默认 `GITHUB_TOKEN` 或后续配置的发布 token。

## 输出

成功输出：

- 可安装的 Linux wheel：
  - `ai_daily_report-0.5.0-py3-none-linux_x86_64.whl`
- 可安装的 Windows wheel：
  - `ai_daily_report-0.5.0-py3-none-win_amd64.whl`
- GitHub Release:
  - tag: `v0.5.0`
  - title: `ver 0.5`
  - assets: 上述两个 wheel
- 安装后 CLI：
  - `ai-daily-report --help`
  - `ai-daily-report doctor`
  - `ai-daily-report daily -i "..."`
  - `ai-daily-report weekly --source db`
  - `ai-daily-report monthly --source db`

失败输出：

- Rust discovery 不可用时，记录 warning 并 fallback 到 Python discovery。
- Rust Office parser 不可用时，进入现有 `RUST_OFFICE_START_FAILED` / Python fallback / 可审计错误链路。
- 不支持平台时，resolver 返回结构化不可用状态，不抛出模糊异常。
- 缺少本机配置时，CLI 输出清晰提示，不假装默认配置可用。

外部副作用：

- 新增 Python packaging 配置。
- 调整包内资源读取方式。
- 新增 GitHub Actions workflow。
- GitHub Actions 在 tag 发布时创建或更新 Release assets。

## 运行环境

用户运行环境：

- Linux x86_64，Python 3.10+。
- Windows amd64，Python 3.10+。
- 不要求安装 Rust、Cargo、Visual Studio Build Tools 或 MinGW。
- 不要求用户打开 GitHub Actions。
- 不要求用户从源码目录运行 `main.py`。

维护者构建环境：

- GitHub Actions `ubuntu-latest` 和 `windows-latest`。
- Rust stable。
- Python build tooling: `build`, `wheel`, `setuptools` 或等价后端。

第一版非目标：

- 不发布 PyPI。
- 不支持 macOS wheel。
- 不做 PyO3 / maturin 扩展模块。
- 不做安装时本机编译 Rust。
- 不做一个同时塞 Linux + Windows 二进制的通用 wheel。
- 不改变 scanner/parser 的业务行为和 fallback 口径。

## 方案比较

### 方案 A：平台 wheel 内置 Rust CLI 二进制（采用）

Python 包随 wheel 携带当前平台的 Rust CLI，运行时通过 resolver 找到包内 binary，并继续通过 `subprocess` 调用。

优点：

- 保持现有 Rust/Python 边界，改动风险最低。
- 用户不需要 Rust 环境。
- Rust CLI 崩溃不会直接带崩 Python 进程。
- 现有 Python fallback、benchmark、审计字段基本可复用。
- 后续可以平滑迁移到 GitHub Release 或 PyPI 多平台 wheel。

缺点：

- 需要新增 package data 和 platform wheel 构建。
- 需要 GitHub Actions 构建矩阵。
- Linux wheel 需要处理可执行权限。

### 方案 B：PyO3 / maturin 原生扩展（暂不采用）

把 Rust 编成 Python extension module，例如 `ai_daily_report_rust`。

优点：

- 调用更原生，少一次 subprocess。
- wheel 生态成熟。

缺点：

- 要重写 stdin/stdout JSON CLI 契约。
- Rust panic / native crash 更贴近 Python 主进程。
- 第一版目标是解决分发，不是重做架构。

### 方案 C：安装时本机编译 Rust（不采用）

把 Rust 源码放进 sdist，安装时编译。

缺点：

- 直接违背“无 Rust 环境也能运行”。
- Windows 用户安装失败概率高。
- 构建耗时、错误复杂、支持成本高。

## 包结构设计

目标结构：

```text
ai_daily_report/
  pyproject.toml
  README.md
  config/
    settings.example.yaml
  src/
    ai_daily_report/
      __init__.py
      __main__.py
      main.py
      core/
      models/
      services/
      utils/
      templates/
      rust_bins/
        linux-x86_64/
          ai-daily-discovery
          ai-daily-office-parser
        win-amd64/
          ai-daily-discovery.exe
          ai-daily-office-parser.exe
```

CLI 入口：

```toml
[project.scripts]
ai-daily-report = "ai_daily_report.main:main"
```

兼容策略：

- 当前根目录 `main.py` 可以保留为薄 wrapper，方便开发者继续执行 `python main.py ...`。
- 正式安装入口以 `ai-daily-report` 为准。
- `templates/` 迁入包内，并用 `importlib.resources` 读取。
- 示例配置可保留根目录副本，同时包内提供可读取资源。

## Rust Binary Resolver

新增模块：

```text
src/ai_daily_report/core/rust_binaries.py
```

职责：

- 将当前平台映射为稳定 platform key：
  - Linux x86_64 -> `linux-x86_64`
  - Windows AMD64/x86_64 -> `win-amd64`
- 根据 binary 类型返回包内文件名：
  - discovery:
    - Linux: `ai-daily-discovery`
    - Windows: `ai-daily-discovery.exe`
  - office parser:
    - Linux: `ai-daily-office-parser`
    - Windows: `ai-daily-office-parser.exe`
- 校验文件存在和基本可执行性。
- 返回结构化 resolution，供 runner、doctor、benchmark 和 parser profile 使用。

配置策略：

```yaml
scanner:
  discovery_backend: "rust"
  rust_discovery_bin: null

  office_parser_backend: "rust_office_oxide_v1"
  rust_office_parser_bin: null
```

查找顺序：

```text
用户显式配置 binary 路径
  -> 使用配置路径，source = config
配置为空
  -> 使用包内 binary，source = package
包内 binary 不存在或不可执行
  -> 返回 unavailable，并让上层 fallback
```

缓存 profile 规则：

- parser profile 不能写入虚拟环境里的绝对路径。
- 包内 binary 使用稳定值：
  - `package:linux-x86_64/ai-daily-office-parser`
  - `package:win-amd64/ai-daily-office-parser.exe`
- 用户配置路径可以进入 profile：
  - `config:/custom/path/ai-daily-office-parser`

这样可以避免不同 venv/site-packages 路径导致 parse cache 无意义失效。

## GitHub Actions 构建与发布

新增 workflow：

```text
.github/workflows/release-wheels.yml
```

触发方式：

```yaml
on:
  workflow_dispatch:
  push:
    tags:
      - "v0.5.0"
      - "v*.*.*"
```

构建矩阵：

```yaml
strategy:
  matrix:
    include:
      - os: ubuntu-latest
        platform: linux-x86_64
      - os: windows-latest
        platform: win-amd64
```

每个平台执行：

1. Checkout。
2. Setup Python 3.11。
3. Setup Rust stable。
4. 安装 build tooling。
5. `cargo test && cargo build --release` for `rust/discovery`。
6. `cargo test && cargo build --release` for `rust/office_parser`。
7. 复制当前平台 release binary 到 `src/ai_daily_report/rust_bins/<platform>/`。
8. 构建平台 wheel。
9. 安装 wheel。
10. 运行不依赖 API key 的 smoke test。
11. 上传 wheel 到 workflow artifacts。
12. tag 触发时上传 wheel 到 GitHub Release。

Release 规则：

```text
tag: v0.5.0
title: ver 0.5
assets:
  ai_daily_report-0.5.0-py3-none-linux_x86_64.whl
  ai_daily_report-0.5.0-py3-none-win_amd64.whl
```

普通同事流程：

```bash
pip install ai_daily_report-0.5.0-py3-none-linux_x86_64.whl
ai-daily-report --help
```

或 Windows：

```bash
pip install ai_daily_report-0.5.0-py3-none-win_amd64.whl
ai-daily-report --help
```

普通同事不需要：

- 安装 Rust。
- 运行 `cargo build`。
- 打开 GitHub Actions。
- 从源码目录运行。
- 手工配置 Rust binary 路径。

## 配置与错误处理

### Discovery

如果 `discovery_backend: "rust"` 但 Rust binary 不可用：

- 记录 warning，包含：
  - source: `config` / `package` / `unavailable`
  - platform key
  - path
  - error
- fallback 到 Python discovery。
- `doctor` 报告 Rust discovery 状态。

### Office Parser

如果 `office_parser_backend: "rust_office_oxide_v1"` 但 Rust binary 不可用：

- 返回结构化 Rust start failure。
- 如果 `office_parser_fallback_enabled: true`，继续 Python fallback。
- `reparse_details` 保留：
  - `attempted_backend`
  - `fallback_backend`
  - `fallback_reason`
  - `rust_duration_ms`
  - `fallback_duration_ms`

### Doctor

增强 `doctor`，显示 Rust binary 状态：

```text
Rust binaries:
  discovery:
    source: package
    platform: linux-x86_64
    path: .../site-packages/ai_daily_report/rust_bins/linux-x86_64/ai-daily-discovery
    executable: yes
  office_parser:
    source: package
    platform: linux-x86_64
    path: .../site-packages/ai_daily_report/rust_bins/linux-x86_64/ai-daily-office-parser
    executable: yes
```

CI smoke test 不应依赖真实 API key。可新增：

```bash
ai-daily-report doctor --no-api
```

或者至少使用：

```bash
ai-daily-report --help
python -c "from ai_daily_report.core.rust_binaries import describe_rust_binaries; print(describe_rust_binaries())"
```

## 版本同步

当前版本必须统一为 `0.5.0` / `0.5`。

需要同步：

- `pyproject.toml`
- `README.md`
- `AGENTS.md`
- `CLAUDE.md`
- `config/settings.example.yaml`
- CLI `--version` 输出（如新增）
- GitHub Actions workflow
- Release title / tag / wheel 文件名
- 新增设计文档

验收搜索：

```bash
rg -n -P "(?<!0\\.)\\b5[.]0(?:[.]0)?\\b|\\bv5[.]0(?:[.]0)?\\b|审计日报生成器\\x20v5" .
```

规则：

- 当前对外文档和运行元数据必须改成 `0.5.0` / `0.5`。
- 历史归档 plan/spec 如果只是过去上下文，不做大面积重写。
- 新增 release 和 packaging 文档不得继续使用旧大版本号。

## 伪代码草案

```python
# [伪代码草案]
# 目标：安装后的 Python 包自动找到当前平台随 wheel 分发的 Rust CLI。
# 输入：
# - scanner_cfg: 用户配置，可能显式指定 rust_discovery_bin / rust_office_parser_bin
# - runtime_platform: 当前系统和 CPU 架构
# - package_resources: wheel 内 rust_bins 目录
# 输出：
# - BinaryResolution: binary 路径、来源、平台 key、是否可执行、稳定 profile 值、错误原因

@dataclass
class BinaryResolution:
    name: str
    source: Literal["config", "package", "unavailable"]
    platform_key: str
    path: Path | None
    available: bool
    profile_value: str
    error: str | None


def resolve_rust_binary(
    *,
    binary_name: Literal["discovery", "office_parser"],
    configured_path: str | None,
    runtime_platform: RuntimePlatform,
) -> BinaryResolution:
    # 用户显式配置路径时优先使用，便于开发者测试本地 target/release。
    if configured_path:
        return validate_binary(
            name=binary_name,
            source="config",
            path=Path(configured_path),
            profile_value=f"config:{configured_path}",
        )

    platform_key = map_platform(runtime_platform)
    if platform_key is None:
        return BinaryResolution(
            name=binary_name,
            source="unavailable",
            platform_key="unsupported",
            path=None,
            available=False,
            profile_value="package:unsupported",
            error="当前平台没有随包 Rust binary",
        )

    path = package_resource_path(
        "ai_daily_report",
        f"rust_bins/{platform_key}/{binary_filename(binary_name, runtime_platform)}",
    )

    return validate_binary(
        name=binary_name,
        source="package",
        path=path,
        profile_value=f"package:{platform_key}/{path.name}",
    )


def run_discovery(scanner_cfg):
    resolution = resolve_rust_binary(
        binary_name="discovery",
        configured_path=scanner_cfg.get("rust_discovery_bin"),
        runtime_platform=current_platform(),
    )

    if not resolution.available:
        log_warning("Rust discovery unavailable, fallback to Python", resolution)
        return run_python_discovery()

    try:
        return run_rust_discovery_subprocess(resolution.path)
    except RustDiscoveryError as error:
        log_warning("Rust discovery failed, fallback to Python", error)
        return run_python_discovery()


def run_office_parser(scanner_cfg, file_path, file_type, limits):
    resolution = resolve_rust_binary(
        binary_name="office_parser",
        configured_path=scanner_cfg.get("rust_office_parser_bin"),
        runtime_platform=current_platform(),
    )

    if not resolution.available:
        rust_error = f"RUST_OFFICE_START_FAILED: {resolution.error}"
        return run_python_fallback_if_enabled(rust_error)

    rust_result = run_rust_office_subprocess(
        resolution.path,
        file_path,
        file_type,
        limits,
    )
    if rust_result.ok:
        return rust_result

    return run_python_fallback_if_enabled(rust_result.error)
```

## 测试策略

### Python packaging

```bash
python -m build --wheel
python -m pip install --force-reinstall dist/*.whl
python -c "import ai_daily_report; print(ai_daily_report.__version__)"
ai-daily-report --help
```

验收：

- `ai_daily_report` 可 import。
- `ai-daily-report` 命令存在。
- 模板和示例配置通过包资源读取。
- 安装包环境不依赖源码根目录。

### Rust binary resolver

新增 `tests/test_rust_binaries.py`：

- Linux x86_64 -> `linux-x86_64`。
- Windows AMD64/x86_64 -> `win-amd64`。
- 用户配置路径优先。
- 配置为空时使用包内 binary。
- 不支持平台返回 unavailable。
- package binary profile marker 稳定，不包含 venv 绝对路径。

### Existing regression

```bash
cd rust/discovery && cargo test && cargo build --release
cd ../../rust/office_parser && cargo test && cargo build --release
cd ../..
/home/george/miniconda3/bin/conda run -n test python -m pytest tests -q
/home/george/miniconda3/bin/conda run -n test python -m compileall main.py src tests
git diff --check
```

验收：

- discovery Rust CLI 合同不变。
- office parser Rust CLI 合同不变。
- `.xlsx` 仍报告 `rust_xlsx_bounded_v1`。
- Python fallback 行为不变。

### GitHub Actions

Linux 和 Windows job 都必须：

- 构建 Rust binaries。
- 构建平台 wheel。
- 安装刚构建的 wheel。
- 运行不依赖 API key 的 smoke test。
- 上传 wheel artifact。
- tag release 时上传到 GitHub Release。

## 风险点 / 边界条件

- Linux wheel 使用 `linux_x86_64` 是内部发布可接受的第一版做法，不等同于 PyPI manylinux 兼容承诺；后续要公开发布 PyPI 时应迁移到 `cibuildwheel` / manylinux。
- Windows runner 构建出的 `.exe` 必须在安装 wheel 后从 package data 调用，不能依赖源码 `target/release`。
- 包内 binary 的执行权限必须在 Linux wheel 安装后可用。
- `importlib.resources` 在 wheel/zip 安装场景下可能返回临时资源路径；如果 subprocess 需要真实文件路径，应使用 `as_file()` 或确保 wheel 解包安装。
- 当前 `doctor` 可能检查 API key；CI smoke test 不能依赖真实密钥。
- 把 `src/core` 等模块迁到 `src/ai_daily_report` 会影响 import 路径，必须一次性配套改测试和入口。
- 本机配置文件位置在 wheel 安装场景下需要清晰规则，否则用户会不知道该把 `settings.linux.yaml` / `settings.windows.yaml` 放哪里。

## 验收标准

- `pyproject.toml` 存在，包名、版本、入口正确。
- `ai-daily-report --help` 在安装 wheel 后可运行。
- Linux wheel 内含 Linux Rust binaries。
- Windows wheel 内含 Windows Rust `.exe` binaries。
- 未安装 Rust 的用户能运行 help、doctor no-api 和不触发 LLM 的 resolver smoke test。
- `discovery_backend: "rust"` 且 package discovery binary 可用时，实际调用包内 discovery。
- Rust discovery 不可用时 fallback 到 Python discovery。
- Office parser package binary 可用时，`.xlsx` 仍返回 `rust_xlsx_bounded_v1`。
- Office parser package binary 不可用时，按现有 Python fallback 和审计字段处理。
- GitHub Release `ver 0.5` 包含 Linux / Windows 两个 wheel。
- 全仓当前版本叙述收敛到 `0.5.0` / `0.5`。
