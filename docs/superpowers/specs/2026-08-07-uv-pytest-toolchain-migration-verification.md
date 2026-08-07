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
- 实际全量门禁为 236 passed、1 skipped、0 failed，与计划草案中的 220 passed、15 skipped 不同；本次迁移以后者的实际全绿结果作为等价性基准。
- `tests/test_benchmark_scanner.py -v` 只输出测试通过数与测试耗时，没有输出文件数/秒等扫描吞吐指标。本阶段保留同命令、同口径进行迁移前后比较，不虚构吞吐数据。

## 迁移后基线（阶段 5 填）
