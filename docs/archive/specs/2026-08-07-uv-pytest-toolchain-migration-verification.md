# uv + pytest 工具链迁移验收记录

> 日期：2026-08-07
> 对应设计：`docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-design.md`

## 迁移前基线（阶段 0）

| 指标 | 实测值 | 命令 |
|---|---|---|
| 启动 `--help` | 3 次分别为 3.060s、2.424s、2.220s；中位数 2.424s | `.venv/Scripts/python.exe -c "…main.py --help…"` |
| 启动 `doctor` | 6.902s | `.venv/Scripts/python.exe -c "…main.py doctor…"` |
| pytest 全量 | 236 passed、1 skipped，36.00s | `.venv/Scripts/python.exe -m pytest tests/ -q` |
| 扫描基准测试 | 5 passed，1.83s；该命令未输出文件吞吐量 | `.venv/Scripts/python.exe -m pytest tests/test_benchmark_scanner.py -v` |

### 基线口径说明

- PowerShell 7.6.4 就绪后，两个指定的 PowerShell 契约测试实测为 `2 passed in 3.23s`。
- 实际全量门禁为 236 passed、1 skipped、0 failed，与计划草案中的 220 passed、15 skipped 不同；本次迁移以本次实测的全绿结果作为等价性基准。
- `tests/test_benchmark_scanner.py -v` 只输出测试通过数与测试耗时，没有输出文件数/秒等扫描吞吐指标。本阶段保留同命令、同口径进行迁移前后比较，不虚构吞吐数据。

## 迁移后基线（阶段 2）

| 指标 | 实测值 | 命令 |
|---|---|---|
| 启动 `--help` | 3 次分别为 2.294s、2.130s、2.175s；中位数 2.175s | `uv run python -c "…main.py --help…"` |
| 启动 `doctor` | 3.694s | `uv run python -c "…main.py doctor…"` |
| pytest 全量 | 236 passed、1 skipped，34.40s | `uv run pytest` |
| 扫描基准测试 | 5 passed，1.76s；该命令未输出文件吞吐量 | `uv run pytest tests/test_benchmark_scanner.py -v` |

### 等价性与清理验证

- `uv run python main.py doctor` 与 `uv run python main.py --help` 均以退出码 0 完成；`--help` 的命令、参数与提示文案保持不变。
- `uv run pytest` 保持 236 passed、1 skipped、0 failed，与迁移前全绿结果一致。
- 清理了 `data/.pytest-tmp/` 与 `.tmp/` 共 18,025 个历史文件、约 1.47 GiB；全量测试后两目录均未重建。
- uv 在当前主机提示 `SSL_CERT_DIR` 中没有有效证书，以及同步时 hardlink 回退为复制；依赖解析、锁文件校验、测试与 CLI 均成功。这两个主机环境警告不通过修改项目依赖或业务代码规避。

## 对比（迁移前 → 迁移后）

| 指标 | 迁移前 | 迁移后 | 变化 |
|---|---|---|---|
| 启动 `--help`（3 次中位数） | 2.424s | 2.175s | 缩短 0.249s（10.3%） |
| 启动 `doctor` | 6.902s | 3.694s | 缩短 3.208s（46.5%） |
| pytest 全量 | 36.00s | 34.40s | 缩短 1.60s（4.4%） |
| 扫描基准测试 | 1.83s | 1.76s | 缩短 0.07s（3.8%）；两侧命令均未暴露真实扫描吞吐量 |

阶段 0–2 的验收目标是工具链等价迁移，不把本次单机耗时波动解释为业务性能优化成果。后续启动与扫描性能阶段应继续以相同命令、固定语料和多次重复测量为准。

## 阶段 6：CLI 启动收口

Plan 3 拆分后的轻量入口按相同子进程计时口径各重复 3 次，结果如下：

| 命令 | 3 次实测 | 中位数 | 退出码 |
|---|---|---:|---|
| `main.py --help` | 0.087s、0.082s、0.090s | 0.087s | 3 次均为 0 |
| `main.py doctor` | 3.657s、3.490s、3.567s | 3.567s | 3 次均为 0 |
| `main.py list` | 2.283s、2.327s、2.317s | 2.317s | 3 次均为 0 |

- `--help` 相对迁移前中位数 2.424s 缩短 2.337s（96.4%），低于
  1.697s 的验收上限，达到“至少改善 30%”目标。
- `doctor` 相对迁移前 6.902s 缩短 3.335s（48.3%）；迁移前没有
  `list` 的独立计时基线，因此只记录当前绝对值，不构造变化百分比。
- `python -X importtime main.py --help` 未命中 `openai`、`jinja2`、
  `pandas`、`pptx`、`docx` 或 `pdfplumber`。
- `src/cli/common.py` 的 production runner 依赖已全部位于
  `build_default_report_runner()` 函数内；help/list/doctor 不导入报告命令模块。
  本轮未发现残留重 import，因此没有为制造性能 diff 再改代码。

## 阶段 6：pytest-xdist 评估

| 模式 | 结果 | pytest 内部耗时 | 相对单进程 |
|---|---|---:|---:|
| 默认单进程 `uv run pytest` | 269 passed、1 skipped | 35.48s | 100% |
| 显式 `uv run pytest -n auto` | 269 passed、1 skipped | 19.82s | 55.9% |

- xdist 在当前完整套件上全绿并缩短 15.66s（44.1%），低于计划规定的
  单进程 70% 上限 24.84s，判定为可用。
- `tests/test_windows_release_package.py --collect-only` 正常收集 16 项；
  `rust_release_binaries` 是 session fixture，且只解析已有 release 二进制路径，
  不在每条测试中重复构建 Rust workspace。
- `pytest-xdist>=3.6,<4` 保留为 dev 依赖，便于显式并行运行；
  `[tool.pytest.ini_options].addopts` 仍为 `-q --tb=short`，不默认加 `-n`。
- 当前全绿证明固定测试集在本机可并行，不把它扩大解释为 SQLite/Rust
  子进程共享资源已经完成深度隔离验证，因此日常与发布门禁仍使用默认单进程。

## Windows 环境风险收口

| 检查项 | 调整前 | 调整后 |
|---|---|---|
| Conda base | 新 PowerShell 自动激活，并注入无效的 `SSL_CERT_DIR` | 用户级 `auto_activate: false`；Miniforge 保留并按需显式激活 |
| uv cache | `C:\Users\<user>\AppData\Local\uv\cache`，与 D 盘项目跨卷 | 项目内 `.uv/cache`，与 `.venv` 同盘 |
| uv 安装方式 | hardlink 失败后回退复制 | `[tool.uv] link-mode = "hardlink"`；`fsutil hardlink list` 确认 cache 与 `.venv` 共享文件 |
| Python 环境 | 迁移过程中复用既有 `.venv` | 使用 Miniforge CPython 3.13.13 清空重建，并按 `uv.lock` frozen 安装 58 个包 |
| warm sync | 未形成同盘证据 | `uv sync --frozen` 外层实测 0.071s，uv 内部检查 58 个包耗时 6ms |

- 修改前的系统级 Conda 配置已备份到 `%LOCALAPPDATA%\ai-daily-report\backups` 并核对 SHA-256；修改前用户级 `.condarc` 不存在。
- 在退出旧的 Conda base 进程环境后，`SSL_CERT_DIR` / `SSL_CERT_FILE` 均不再注入，uv 同步、锁文件检查、pytest 与 CLI 全程无证书警告。
- 当前已启动的 Codex/终端进程仍可能继承修改前的 `(base)` 环境；关闭后重新打开即可使用新的不自动激活设置。
- 干净环境首次填充新 cache 用时约 2 分 45 秒，属于一次性下载与准备成本；随后使用 hardlink 安装 58 个包仅耗时 1.10s。

## 固定脱敏语料 scanner 吞吐门禁

基准语料为仓库跟踪的 `tests/fixtures/worker_documents`，日期窗口固定为
2000-01-01 至 2100-01-01，确保 checkout 文件时间变化时仍覆盖全部允许格式。
每组使用全新 v2 scanner DB 跑 cold，再复用同一 DB 跑 warm；不访问本机业务目录，
不调用 LLM，原始 JSON/Markdown 存放在已忽略的 `.uv/benchmarks/`。

| 指标 | cold | warm |
|---|---:|---:|
| status | ok | ok |
| 发现/成功/错误/超时 | 3 / 3 / 0 / 0 | 3 / 3 / 0 / 0 |
| reused / reparsed | 0 / 3 | 3 / 0 |
| total_duration_ms | 4,956 | 41 |
| files_per_second | 0.605 | 73.171 |

- warm 吞吐约为 cold 的 120.9 倍。
- 两次内容 SHA-256、parser backend 汇总和 worker lane 汇总完全一致。
- backend 为 `rust_office_oxide_v1: 2`、`python_office_v1: 1`；lane 为 `rust_office_process: 2`、`python_document_process: 1`。
- 门禁条件：两次均 `status=ok` 且零错误/超时；warm 全量复用且零重解析；内容哈希、backend、lane 一致；cold/warm 吞吐均为正且 warm 高于 cold。本次实测全部通过。
- 新增吞吐测试后全量门禁为 237 passed、1 skipped、0 failed（34.21s）；`uv lock --check`、`uv sync --frozen` 与 `doctor --strict` 均以退出码 0 完成。

## 阶段 6：扫描外层调用审查与复测

使用同一固定脱敏语料、全新 scanner DB 和 2000-01-01 至 2100-01-01
日期窗口再次执行 cold/warm；证据位于已忽略的
`.uv/benchmarks/scanner-fixture-20260808-011131/`。

| 指标 | cold | warm |
|---|---:|---:|
| status | ok | ok |
| 发现/成功/错误/超时 | 3 / 3 / 0 / 0 | 3 / 3 / 0 / 0 |
| reused / reparsed | 0 / 3 | 3 / 0 |
| total_duration_ms | 805 | 49 |
| files_per_second | 3.727 | 61.224 |

- cold/warm 文件集合与内容 SHA-256 全部一致；backend 都是
  `rust_office_oxide_v1: 2`、`python_office_v1: 1`，worker lane 都是
  `rust_office_process: 2`、`python_document_process: 1`。
- `tests/test_benchmark_scanner.py -v` 为 6 passed（1.77s）。
- `ReportRunner._build_scan_context()` 每个 scan recipe 只调用一次
  `scheduler.build_context()`；`ContextScheduler` 再只调用一次 engine；
  `RustContextClient` 对应一次 `build-context` 子进程。
- 周/月 `source=db` 分支直接读取报告存储；行为测试明确断言 scheduler
  调用为零。周/月 scan 行为测试均断言 scheduler 调用恰好一次。
- 未发现同周期重复扫描或额外 scanner 往返。900s 是进程级安全上限，
  不是已观测到的耗时瓶颈；没有证据支持收紧它。本 Task 不修改生产代码。

## 最终验收（阶段 8）

最终启动、默认 pytest 与 benchmark test 均重复 3 次取中位数。固定语料
scanner 使用 3 个互相独立的全新 DB，各跑一组 cold 后复用该 DB 跑 warm。

| 指标 | 迁移前/参考值 | 最终实测 | 变化 | 结论 |
|---|---:|---:|---:|---|
| 启动 `--help` | 2.424s | 0.101s | 缩短 2.323s（95.8%） | 达标（目标 ≤1.697s） |
| 启动 `doctor` | 6.902s | 3.691s | 缩短 3.211s（46.5%） | 达标 |
| 启动 `list` | 迁移前未单独测量 | 2.443s | 不计算 | 记录绝对值 |
| pytest 默认单进程 | 236 passed、1 skipped，36.00s | 269 passed、1 skipped，35.86s | 缩短 0.14s（0.4%） | **未达标**（目标 ≤27.00s，差 8.86s） |
| pytest 显式 xdist | 无同模式迁移前值 | 269 passed、1 skipped，19.82s | 相对迁移前单进程缩短 44.9%（不同模式） | 可用，但非默认门禁 |
| scanner benchmark test | 5 passed，1.83s | 6 passed，1.82s | 缩短 0.01s（0.5%） | 全绿 |
| 固定语料 cold 吞吐 | 阶段 2 单次 0.605 files/s | 3 次中位数 3.667 files/s | 增加 506.1% | 记录实测，不归因于本阶段代码 |
| 固定语料 warm 吞吐 | 阶段 2 单次 73.171 files/s | 3 次中位数 61.224 files/s | 降低 16.3% | 全量复用门禁仍通过 |

### 最终采样明细

- `--help`：0.101s、0.091s、0.118s；`doctor`：3.869s、3.640s、
  3.691s；`list`：2.682s、2.320s、2.443s。九次退出码均为 0。
- 默认 pytest：36.19s、35.86s、35.54s，三次均为 269 passed、
  1 skipped。测试面相对迁移前增加 33 个通过项，因此当前时长不是同测试数
  的纯性能对比；即便如此也不能把 0.4% 写成达到 25% 目标。
- benchmark test：1.98s、1.82s、1.80s，三次均为 6 passed。
- fixed corpus cold：805ms / 3.727 files/s、896ms / 3.348 files/s、
  818ms / 3.667 files/s；warm：49ms / 61.224 files/s、
  44ms / 68.182 files/s、51ms / 58.824 files/s。
- 三组 scanner run 均为 3 个文件全部成功、零错误/超时；cold 都是
  reused=0 / reparsed=3，warm 都是 reused=3 / reparsed=0；每组 cold/warm
  内容哈希、parser backend 与 worker lane 完全一致。

### 验收判定与阶段边界

- CLI 启动目标达成，收益来自 Plan 3 的轻入口和按命令延迟加载；阶段 6
  复核没有发现还需下沉的重 import。
- 默认 `uv run pytest` 的 25% 提速目标未达成。显式 xdist 实测可把完整套件
  降到 19.82s，但因 SQLite/Rust 并发隔离未做深度保证，保持默认关闭，
  不用它把默认门禁包装成达标。
- scanner 外层没有重复扫描，固定语料正确性与 cache 门禁全部通过。本阶段
  未修改 Rust scanner，cold/warm 的单机波动只作为实测记录，不宣称算法收益。
- 阶段 0–2 是 uv/pytest 等价迁移，阶段 3–5 是结构收敛，阶段 6 是启动、
  测试与扫描外层收口。PDF 质量/吞吐不在本表判定，由阶段 7 的独立门禁
  证据决定迁移或维持 pdfplumber。
