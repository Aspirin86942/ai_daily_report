# CLAUDE.md

项目开发约定、结构、工具链、命名和安全边界以 [AGENTS.md](AGENTS.md) 为准。
运行方式、主要接口与代码阅读入口见 [README.md](README.md)。

## 按需阅读

- 日报、调度、补跑及修复：[日报操作说明](docs/daily-report-operations.md)。
- 扫描路由、缓存和性能验证：[扫描后端说明](docs/scanner-backends.md)。
- 发布包、安装与回滚：[Windows 部署说明](docs/windows-deployment.md)。
- 当前覆盖优化验收：[V27 状态](docs/coverage-optimization-v27-status.md)。
- 专用术语：[CONTEXT.md](CONTEXT.md)。
- 历史设计与实施计划：`docs/archive/`。

## 修改前确认

- Python 运行时固定为 CPython 3.13.13；依赖和测试使用项目声明的 uv 工具链。
- 日报的证据生成由 `EvidenceDailyService` 承担；`ReportRunner` 保留历史日报接口并编排周报、月报。
- 扫描入口为 `NativeScanner`，通过 PyO3 在当前进程调用 Rust 核心；文件解析继续使用隔离 worker。
- SQLite 保存程序状态，Markdown 提供阅读输出。重构需保留错误代码、审计记录、恢复行为和发布顺序。
- 根据目标文件和必要调用关系加载上下文。仓库已有 `.codegraph/`，需要理解符号关系时优先使用 CodeGraph；索引过期时以磁盘源码为准。
- 保留当前未提交改动。生产配置、真实数据库、调度切换及 Git 发布遵循 AGENTS.md 中的授权边界。
