# ReportRunner 深模块设计（最终）

`ReportRunner` 用一个小 interface 承载完整报告行为：source gate、周期解析、LLM
生成、模板渲染、publication 顺序、错误阶段和 evidence receipt。

```python
ReportRunner.run(ReportRunRequest) -> ReportRunOutcome
```

调用者只需理解 request variant 与 outcome。daily/weekly/monthly 的差异是模块内部
recipe，不扩展公开方法。

Scanner seam 为私有 port，production adapter 是 `NativeScanner`。ReportRunner
只消费 `ScanResult.envelope` 的报告语义，不读取 scanner SQLite，不复制完整 evidence
DTO，也不做 parser、worker、cache 或 fallback 决策。

发布不并行：先 SQLite，后 Markdown。`save=False` 跳过这两步，但 source scan 已经
发生的 run/cache 副作用保留。所有预期失败返回 typed outcome；未知 request variant
抛出 `TypeError`。

测试通过同一 interface 覆盖三种 mode、两种 period source、lazy LLM、日期覆盖、
partial/error、SQLite-before-Markdown、失败 receipt 和 no-save 行为。CLI 只替换
ReportRunner，不重复业务 pipeline 测试。
