# Native scanner 性能验证

## 口径

使用合成固定语料、临时 state 目录和多组成对 cold/warm 样本。每对使用一个新的
`scan_index_v3_pair_N.sqlite3`，并在同一个 `NativeScanner` 实例上先 cold 后 warm。

报告必须包含：

- cold/warm median 与 nearest-rank p95；
- median throughput；
- peak worker RSS；
- warm 全量复用；
- `native_call_count=1`（每个样本）；
- `scanner_process_start_count=0`；
- `scanner_transport_serialized_bytes=0`。

历史单样本只能标记为参考，不能冒充当前 p95。若提供 cold/warm reference，门禁
使用 reference × 1.05；不提供 reference 时只判定运行完整性与 warm 复用。

## 命令

```powershell
.\.venv\Scripts\python.exe scripts\benchmark_scanner.py `
  --work-dir (Resolve-Path 'tests\fixtures\worker_documents') `
  --state-dir $temporaryState `
  --start-date 2000-01-01 `
  --end-date 2100-01-01 `
  --iterations 5
```

Benchmark 不加载本机配置，不触碰真实 scanner DB，不调用 LLM。
