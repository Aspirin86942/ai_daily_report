# ReportRunner 最终实现

## Interface

```python
ReportRunner.run(ReportRunRequest) -> ReportRunOutcome
```

这是 daily、weekly、monthly 唯一公开报告 seam。封闭 request union 与 typed
outcome 保留 source、周期、错误阶段、warnings、evidence 和 publication receipt。

## 内部依赖

`ReportRunner` 接受 scanner port、report store、renderer、model port 和 daily
input adapter。production scanner adapter 是 `NativeScanner`；测试用同一窄接口的
确定性 adapter，不需要原生扩展。

规则：

- daily 固定 scan；weekly/monthly 可选 db 或 scan；
- scan source 每次 recipe 只调用一次 `NativeScanner.build_context`；
- scanner error 阻止 LLM 构造和调用；
- `save=False` 仍允许 scanner run/cache 副作用，但不写报告 SQLite/Markdown；
- 保存时先提交报告 SQLite，再写 Markdown；SQLite 失败不写 Markdown；
- 不重试 scanner/LLM，不解释 backend、lane、cache 或 fallback。

## 测试 surface

业务矩阵测试位于 `tests/test_report_runner.py`；CLI 测试只验证 request 映射、展示
和退出码。测试不穿透 `ReportRunner` interface 断言内部实现细节。
