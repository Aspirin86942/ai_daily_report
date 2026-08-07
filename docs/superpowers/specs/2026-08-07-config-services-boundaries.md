# Config / Services 分层边界

本说明冻结阶段 5 重构后的模块归属和依赖方向。它只描述现有职责，不改变
运行接口、错误语义或持久化行为。

## 分层归属

| 分层 | 模块 | 职责 |
|---|---|---|
| interface | `src/models/scanner_contract.py` | Rust/Python wire DTO 与 JSON contract |
| interface | `src/services/context_engine.py` | 应用结果 DTO 与公开 `ContextEngine` Protocol |
| interface | `src/services/scanner_config.py` | scanner v1 profile 的纯提取与字段校验 |
| adapter | `src/services/json_process_client.py` | UTF-8 JSON 单请求/单响应进程边界 |
| adapter | `src/services/rust_context_client.py` | Rust scanner 子进程与 Python 应用结果的适配 |
| adapter | `src/services/document_parser.py` | crash-isolated Python fallback worker 的 bounded 解析 |
| orchestration | `src/services/context_scheduler.py` | 一次运行的引擎选择与 context 调度 |
| orchestration | `src/services/report_runner/` | daily/weekly/monthly 配方、错误模型和发布结果编排 |
| orchestration | `src/services/report_gen.py` | Jinja 渲染与 Markdown 发布 |
| orchestration | `src/services/sqlite_store.py` | 报告历史与聚合数据持久化入口 |

`src/models/scanner_contract.py` 和 `src/services/sqlite_store.py` 属于阶段 5 的
禁改模块，因此其分层归属只在本说明中记录，不修改模块 docstring。

## 依赖方向

允许的主方向为：

```text
src/cli -> report_runner/context_scheduler -> interfaces + adapters
                                           -> report_gen/sqlite_store/model port
```

- `src/services` 不得反向依赖 `src/cli`。
- `report_runner` 不得导入 CLI presenter、argparse parser 或 command handler。
- `context_scheduler` 只依赖公开的 `ContextEngine` Protocol，不跨模块引用私有名。
- `Config.scanner_contract_profile()` 委托 `scanner_config.extract_scanner_profile()`；
  `config` 单例和既有 scanner 常量/异常导入继续兼容。
- Rust core 继续拥有 scanner profile 默认值和归一化；Python 只透传显式 wire
  叶子并排除二进制路径、数据库路径和进程超时等基础设施字段。

## 静态门禁

阶段 5 验收通过 AST import 检查确认：services 到 CLI 的反向依赖为空，且
`report_runner` 的依赖集合不包含 `src.cli`。运行行为继续由全量 pytest 门禁
覆盖。
