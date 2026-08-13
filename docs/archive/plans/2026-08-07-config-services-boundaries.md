# 配置与 services 分层最终状态

## 配置接口

`Config.scanner_settings()` 委托 `src/services/scanner_config.py`，只输出已显式配置的
可变 scanner 叶子。Rust 负责默认值、校验、单位换算、路由和标准化 identity。

scanner 路径字段只有：

- `index_db_path`；
- `office_worker_path`。

Python executable、module root 和 Python worker module 从当前精确运行环境推导。
未知或已删除键抛出 `UnknownScannerSettingsError`，不提供 alias。

## services 依赖

```text
src/cli → ReportRunner → NativeScanner
                      ↘ report store / renderer / model port
```

`NativeScanner` 是唯一 Python scanner adapter。`ReportRunner` 不读取 scanner DB，
不复制 backend/lane/cache/fallback 逻辑。`src/services` 不反向依赖 `src.cli`。

## 验收

- 配置 unknown-key 与路径测试通过；
- production runner 只装配一个 scanner adapter；
- AST 审计证明 `services → src.cli` 为零。
