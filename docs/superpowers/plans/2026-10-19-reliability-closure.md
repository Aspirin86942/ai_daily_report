# 自动运行可靠性收敛实现计划

> **面向 AI 代理的工作者：** 使用 executing-plans 连续执行；范围独立的健康通知可交给一个子代理，Lead 验收并在最后安排全新上下文审查。步骤使用复选框记录。

**目标：** 补解析可自动恢复，晨间日报等待材料有界，状态如实表达，中文健康结果可靠落盘。

**架构：** 复用现有归档队列、解析器、SQLite 运行账本和 Windows 调度；补解析不访问原目录。保留现有冻结输入和发布合同，以明确的 partial/deferred 和截止时间连接阶段。

**技术栈：** Rust、rusqlite/serde/chrono/tempfile、Windows PowerShell 5.1、现有 MSVC 工具链。无新增依赖。

**规格：** ../specs/2026-10-19-reliability-closure-design.md

## 全局约束

- 保留工作日 09:00–18:00 自动采集边界、内存保护、全文覆盖、OCR 关闭、模型/预算策略、历史证据和用户修改。
- 显式人工补解析可以处理现有积压；本轮不手动生成生产日报，不新增模型请求，不执行 Git 提交/推送，不删除旧包。
- 测试只用隔离配置/数据库/本地模型替身；包构建三 EXE 同版，固定 PDFium。

## 审查重点（Review Focus）

- 补解析记录绝不能让没有真实采集的时段变成连续；任务1用仅有 parse_complete 的时段核验缺口。
- 晨间刚完成的解析必须进入新冻结输入，且当日材料不可混入昨日；任务3覆盖 ready_at、observed_at 两种边界。
- 归档锁占用、低内存、周末与18:01不得造成失控重试、模型请求或源目录访问；任务1/3覆盖。
- 健康输出包含中文、PowerShell CP936、健康非零退出时仍应有新鲜可读文件；任务2运行真实程序和脚本。
- 一次失败只有被同 scope/日期更晚的正式发布恢复后才降为历史；任务2覆盖未恢复、跨日期、晚于发布的失败。

## 文件职责与接口

- archive/queue.rs：补解析入口、待处理摘要及目标日就绪检查；archive/parse.rs：可选目标截止筛选；collect.rs：共享恢复和结果字段；stats.rs：分列补解析运行。
- operations.rs：统一调度日志和 parse 任务定义；cli.rs/archive/cli.rs/main.rs：新增命令参数和主调度衔接。
- health.rs：当前故障判断和原子 UTF-8 文件；scripts/health_notify.ps1：读取文件、任务回读和通知。
- scripts/daily_task_common.ps1、install/check/remove wrappers：支持 parse kind；既有安装和激活事务保持。
- tests/archive_queue.rs、scheduled_log.rs、health.rs、health_persistence.rs、operations.rs、cli_runtime.rs 及 Windows 脚本：行为回归。
- 文档只同步当前入口、运行含义、任务和新验收结果，不改历史验收结论。

### 任务 1：归档补解析和真实材料状态

- [ ] 写 queue-only、目标日期、窗口外零副作用、锁冲突和非采集连续性测试；先执行确认缺失行为失败。
- [ ] 实现 `queue::execute(config, scheduled, target_date, max_seconds, now) -> Result<Value>` 和 `queue::pending(config, before) -> Result<Value>`。
- [ ] 复用 parse::drain，增加可选 before 截止筛选；抽出 collect 中已中断解析恢复，不更改解析器身份。
- [ ] parse-only 账本用 parse_* 状态及 operation=parse，stats 分列；普通采集时间查询排除 parse 运行。
- [ ] collect/parse 输出 execution_status/material_status/queued/errors/reason_codes，调度日志同步。
- [ ] 执行 archive_queue、scheduled_log 及既有 archive/window 相关测试，读取结果并记账。

### 任务 2：健康持久化、通知与已恢复失败

- [ ] 写原子 UTF-8 持久化和同 scope/日期恢复告警测试，运行 RED。
- [ ] 实现 `health::persist(report: &HealthReport) -> Result<PathBuf>` 与 `health::write_report(report: &HealthReport, path: &Path) -> Result<()>`，使用现有 tempfile。
- [ ] TaskNames 增加 parse；run_attention 保留历史失败，但仅未由更晚正式发布恢复的运行作为当前告警；证据保留 resolved_failures。
- [ ] PowerShell 用新 `health --output` 文件接口、显式 UTF-8、原子更新，通知失败保留健康结果；添加真实脚本行为检查。
- [ ] Lead 在 cli/main 中接入 output/persist，并运行 health、health_persistence 和 Windows 通知检查，记录结果。

### 任务 3：自动补解析与晨间就绪

- [ ] 写 parse 任务触发、自动跳过窗口、晨间 deferred 与截止后 partial、既有报告/冻结恢复不重处理测试，先 RED。
- [ ] CLI 开放 archive drain；operations 支持 parse task，窗口内半小时、错开源采集5分钟并补18:00，任务运行上限留收尾余量。
- [ ] scheduled archive 新日报在模型冻结前做最多120秒目标日 drain；09:30之前延期，截止后带 ARCHIVE_PARSE_PENDING，准备时间刷新，零新增模型路径保持。
- [ ] PowerShell 安装/回读支持第四个任务，沿用既有归属和安全回读。
- [ ] 测试 queue、operations、daily、cli、Windows 安装/通知脚本，更新操作文档。

### 任务 4：最终门禁和独立审查

- [ ] fmt check、clippy -D warnings、locked workspace tests；保留完整输出，只回读摘要及失败定位。
- [ ] 通过全新 GPT 子代理审查整个 diff，按实际风险修正重要发现并完成回归。
- [ ] 构建独立静态 CRT 发布包，核验清单、固定 DLL、同版 Worker、独立包测试及 doctor。

### 任务 5：部署与现有积压验收

- [ ] 保存实际任务、有效配置、报告/归档一致快照、原件/报告 SHA、调用账本计数和源码差异。
- [ ] 新包使用既有两个正式配置；仅更新所需 Worker 路径，安装并回读 daily/collect/parse/health 四任务，失败按保存定义回退，不覆盖业务库。
- [ ] 显式手动 drain 处理现有153项，核验剩余队列、失败原因、原件和报告哈希、模型账本零增量。
- [ ] 手动运行健康落盘/任务上下文核验；如真实时间触发已发生单独核验，不将手动结果记为真实调度。
- [ ] 更新 current-state 与本轮验收结果，保留源码工作区、旧材料和全部证据；交付实际结果与观察期限制。
