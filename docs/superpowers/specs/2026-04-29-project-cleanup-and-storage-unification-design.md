# 项目整理与存储统一设计

## 1. 目标

本次整理聚焦两个目标：

1. 删除已经失效或不再需要维护的历史包袱，降低目录噪音。
2. 将当前事实上的主存储实现统一为 `SQLiteStore`，去掉兼容壳和一次性迁移逻辑，使代码结构与实际运行方式一致。

本次工作以“小幅收口”为原则，不做大规模重构，不改变已有业务行为，不调整数据库内容。

## 2. 已确认边界

### 2.1 需要删除

- `scripts/migrate_json_to_sqlite.py`
- `scripts/migrate_daily_schema.py`
- `src/services/history_mgr.py`
- 与当前项目运行无直接关系的一次性过程文档：
  - `docs/superpowers/specs/2026-04-03-report-text-simplification-design.md`
- 本地运行产物：
  - `logs/` 下日志文件
  - `data/reports/` 下已生成的日报、周报、月报
  - `__pycache__/` 等缓存目录

### 2.2 需要保留

- `data/db/` 下数据库文件，尤其是 `reports.sqlite3`
- `config/` 配置文件
- `src/` 正式源码
- `templates/` 模板
- `tests/` 测试

## 3. 当前问题判断

### 3.1 存储层命名与实现不一致

当前 `HistoryManager` 已经只是 `SQLiteStore` 的兼容封装，CLI 和测试继续依赖旧名称，会带来两个问题：

- 阅读代码时难以快速判断真正的存储入口；
- 后续继续保留兼容层，会让“历史实现”和“当前实现”的边界一直模糊。

因此，本次应直接统一为 `SQLiteStore`，并让入口、测试、文档全部收敛到同一命名。

### 3.2 迁移脚本已完成历史使命

仓库中保留了 JSON 到 SQLite 的迁移脚本，以及旧表结构到新表结构的升级脚本。用户已经明确表示这两条迁移链路都不再需要。

继续保留的成本高于收益：

- 容易让维护者误以为项目仍支持旧存储形态；
- README 和说明文档会继续背负过时信息；
- 测试和认知负担会被历史兼容逻辑放大。

因此应彻底移除一次性迁移能力，而不是继续保留“可能以后会用”的入口。

### 3.3 文档与实现存在漂移

当前仓库中至少存在以下漂移：

- README 仍包含 `gemini` 相关描述，但代码实现已只支持 `deepseek` / `openai`。
- README 仍强调 JSON 数据迁移链路，但当前默认存储已经是 SQLite。
- `HistoryManager` 仍出现在文档和目录说明中，但它并非必要的业务层抽象。

这类漂移不会马上导致运行错误，但会直接降低项目可维护性，属于应当同步修正的问题。

### 3.4 架构不需要大改，但需要收口

从现状看，主线职责已经基本清楚：

- `main.py`：CLI 编排
- `src/core/`：配置、日志、LLM 客户端
- `src/models/`：Pydantic 数据模型
- `src/services/file_scanner.py`：文件扫描与内容提取
- `src/services/sqlite_store.py`：历史存储
- `src/services/report_gen.py`：Markdown 渲染与保存

因此当前阶段没有必要重拆目录或引入新的层次。真正的问题不是“层次不够多”，而是“历史遗留还没清干净”。本次应优先收口，而不是为了整洁感做大规模重组。

## 4. 设计方案

### 4.1 清理策略

采用“删除历史兼容，保留当前运行核心”的策略：

- 删除一次性迁移脚本
- 删除 `HistoryManager` 兼容层
- 清空本地日志和生成报表
- 保留数据库文件
- 同步修正文档和测试，使仓库表达的结构与当前实现一致

### 4.2 存储统一方案

统一后的存储入口为 `src/services/sqlite_store.py` 中的 `SQLiteStore`。

所有原本通过 `HistoryManager` 完成的行为，直接由 `SQLiteStore` 提供：

- 读取昨日日报计划
- 保存日报 / 周报 / 月报
- 查询周报或月报所需的日报集合
- 列出已有日报

这样做的原因不是“少一个文件更好看”，而是为了确保调用方看到的类型名称，就是系统真实在用的存储实现。

### 4.3 文档策略

文档只保留当前有效的信息：

- LLM provider 描述只保留实际支持的 provider
- 存储说明只保留 SQLite 方案
- 删除不再需要的迁移说明
- 目录结构示意中移除 `HistoryManager`

文档目标不是“记录历史”，而是帮助下一位维护者正确运行和修改当前系统。

## 5. 影响范围

### 5.1 代码文件

预期会修改：

- `main.py`
- `src/services/__init__.py`
- `tests/test_history_mgr.py`
- `README.md`
- `CLAUDE.md`
- `AGENTS.md`

预期会删除：

- `src/services/history_mgr.py`
- `scripts/migrate_json_to_sqlite.py`
- `scripts/migrate_daily_schema.py`
- `docs/superpowers/specs/2026-04-03-report-text-simplification-design.md`

### 5.2 本地文件

预期会清理：

- `logs/`
- `data/reports/`
- 仓库内缓存目录

预期会保留：

- `data/db/`

## 6. 风险与控制

### 6.1 风险

- CLI 若仍引用 `HistoryManager`，删除兼容层后会直接报错。
- 测试若继续依赖旧类名，会造成失败。
- README / CLAUDE / AGENTS 若不同步，后续使用者会继续被旧信息误导。

### 6.2 控制措施

- 先统一替换代码入口，再删除兼容层文件。
- 运行全量测试确认回归面。
- 用全文检索确认仓库内不再残留 `HistoryManager` 和迁移脚本说明。

## 7. 非目标

以下内容不在本次范围内：

- 不修改数据库 schema
- 不清空或删除 SQLite 数据库
- 不重做 CLI 参数设计
- 不重写 `FileScanner`、`LLMClient` 或模板体系
- 不进行 `services/` 子目录重组

## 8. 验收标准

本次整理完成后，应满足：

1. 仓库中不再存在 `HistoryManager` 兼容层和两份迁移脚本。
2. `main.py` 与测试直接使用 `SQLiteStore`。
3. README 与项目说明不再出现过时的 Gemini / 迁移链路描述。
4. `data/db/` 数据库保留，`logs/` 与 `data/reports/` 被清理。
5. 全量测试通过。
