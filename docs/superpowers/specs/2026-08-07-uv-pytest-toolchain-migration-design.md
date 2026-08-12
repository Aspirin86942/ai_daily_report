# 工具链与应用结构最终设计

## Runtime

- Windows x64；
- exact CPython 3.13.13；
- `.python-version` 为唯一版本来源；
- PyO3 wheel 为 `cp313-win_amd64`，不使用 abi3。

## Application structure

```text
lazy CLI → ReportRunner → NativeScanner → PyO3 Scanner
```

CLI 只映射和展示，ReportRunner 负责报告业务，NativeScanner 负责唯一原生 seam。
配置只传显式 mutable leaves；Rust `Scanner` 拥有默认值、store、worker pools 和完整
evidence。

## Test structure

- pytest 业务测试通过 ReportRunner/NativeScanner interface；
- Rust 测试通过 `Scanner` 和 worker-v2 seam；
- 临时目录隔离 config、scanner DB 和 release pointer；
- benchmark 使用多组成对 cold/warm；
- tests、fmt、clippy、locked build、wheel install、doctor、CLI help、compileall 和
  static audits 共同构成总门禁。
