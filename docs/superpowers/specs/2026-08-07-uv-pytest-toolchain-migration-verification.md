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
