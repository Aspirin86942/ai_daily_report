# 最终验证口径

验证必须来自当前 checkout，而不是历史提交标签或旧性能样本。

- Python：daily/weekly/monthly 矩阵、source gate、lazy LLM、日期覆盖、发布顺序、
  failure receipt、no-save、native adapter、worker v2、release 工具。
- Rust：workspace fmt、clippy、tests、release build、Scanner domain interface、v3
  store、worker pool 生命周期和源文件变化。
- Native：CPython 3.13.13 导入、请求/结果转换、Unicode/Windows path、错误映射、
  panic 隔离、GIL 释放、重复调用、并发顶层保护和资源释放。
- Fixed corpus：context/status/decisions/backend/lane/cache/evidence 确定性，cold/warm
  median、p95、throughput 和 peak RSS。
- Structure：一个 Python scanner adapter、一次 PyO3 call、零 scanner 子进程、
  零 `services → src.cli` 依赖。

任何必需门禁失败都停在未完成状态，不用旧结果替代当前证据。
