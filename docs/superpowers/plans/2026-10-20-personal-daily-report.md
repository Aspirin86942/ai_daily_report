# 个人日报精简实施计划

> 执行方式：由主会话按依赖顺序完成下列任务，每项用相关测试验证；无需自动委派、逐项审批或逐项提交。代码片段为接口约定或明确标注的伪代码，实施时遵循现有 Rust 风格。

**目标：** 自动生成供个人回顾和安排的简短日报，同时限制选材、模型请求、处理时间和临时存储。

**架构：** 复用现有 Rust 采集、解析器、模型传输、调用账本与原子发布。新增日报格式保存选出的重点及独立依据；日常处理使用隔离的短期缓存。旧报告、冻结输入和归档保持原有读取语义。

**技术：** Rust、SQLite、Serde、MiniJinja、现有 Windows Worker；不新增生产依赖。

**设计依据：** [个人日报精简设计](../specs/2026-10-20-personal-daily-report-design.md)。本计划给出实现选择与验证方法，产品取舍以该设计为准。

## 全局约束

- 正文通常 3～7 项、最多 10 项，目标 500～1,000 字，最多 1,500 个 Unicode 字符；必要依据与用户原文单独计算。
- 输入业务文字最多 20,000 个 Unicode 字符，其中人工补充合计最多 2,000 字。UTF-8 请求字节和模型上下文限制同时生效。
- 通常一次模型生成；同一工作范围、目标日期累计最多 3 次自动请求。失败、超时、结果未知及重试计数，重启不刷新额度。
- 日报正常目标 180 秒内，总预算 300 秒，预留 30 秒收尾。采集及独立补解析每次最多 120 秒。
- 日报及关键依据文件合计最多 100,000 字节。处理缓存最多 7 天、合计不超过 1 GiB；详细运行记录最多 30 天、10 MiB。
- 时区 Asia/Shanghai。保持现有工作目录、模型发送权限、工作日采集窗口及 OCR 关闭状态。
- 复用已有依赖、固定构建目录及匹配 Worker；保持旧格式读取和恢复，不改写已发布历史报告。
- 本地代码与隔离验证是一阶段；真实模型调用、正式任务/配置切换、历史删除和 Git 发布按各自明确授权执行。此计划不自动执行这些操作。

## 实现边界与文件职责

采用显式配置 `report.profile: personal` 和 `archive.daily_source: archive` 开启新流程；缺省 profile 继续解析为 `evidence`，新安装示例在完整链路验收后同时设置上述两项。personal 模式拒绝显式 `daily --source scan`，避免绕过短期采集规则。旧配置序列化时省略缺省 profile，保持旧恢复指纹不变。日报、采集、补解析和健康任务需要使用一致的 profile。

新日报/冻结输入版本为 5，模型计划版本为 6。数据库继续复用现有表；目标日期累计额度放入 `report_metadata`，无需改动正式报告库结构。

| 文件 | 职责 |
| --- | --- |
| 新增 `rust/report_app/src/reports/personal.rs` | v5 日报、来源材料、选择统计与验证；渲染所需数据 |
| 新增 `rust/report_app/src/processing_cache.rs` | 处理区锁、共享容量预留、文件登记与故障后核对 |
| 新增 `rust/report_app/src/archive/retention.rs` | 仅清理新处理区的过期对象、解析结果与索引历史 |
| 新增 `rust/report_app/src/archive/selection.rs` | 按目标日期选择版本、进行有界比较并返回有限材料 |
| 新增 `rust/report_app/src/model/personal.rs` | 单次摘要计划与响应验证，使用现有 ModelRunner 执行 |
| 新增 `rust/report_app/src/store/daily_budget.rs` | 同一目标日期持久额度，在既有请求预占事务中更新 |
| 新增 `rust/report_app/src/run/personal.rs` | 新流程编排、补充信息、截止时间、失败结果与发布 |
| 新增 `templates/personal_daily_prompt_v5.md`、`templates/personal_daily_template_v5.md` | 个人日报提示词和排版 |

现有文件按下列任务作局部修改。除明确写出的仓库相对路径外，下文 Rust 源文件相对 `rust/report_app/src/`，应用测试相对 `rust/report_app/tests/`。新模块在所属 `mod.rs`、`lib.rs` 或父模块声明；不建立平行 HTTP 客户端、第二套文件解析器或通用任务框架。

## 任务 1：确定新格式、配置和来源约束

**修改：** `rust/report_app/src/config.rs`、`config.defaults.json`、`reports.rs`；新增 `reports/personal.rs`。测试使用新增 `tests/personal_reports.rs` 和既有 `tests/cli_config.rs`、`tests/legacy_recovery.rs`。

**接口约定：** 以下类型放在 `reports/personal.rs`；时间、来源元数据及原文引用复用现有类型。`EvidenceRef` 的原文与位置随材料复制，长期阅读不依赖版本数据库。

```rust
pub struct Material {
    pub source: ReportSource,
    pub text: String,
    pub evidence: Vec<EvidenceRef>,
    pub priority: u8,
}
pub struct SelectionSummary {
    pub observed_files: u64,
    pub inspected_files: u64,
    pub selected_files: u64,
    pub selected_chars: usize,
    pub incomplete_inventory: bool,
    pub omissions: BTreeMap<String, u64>,
    pub omitted_paths: Vec<String>,
}
pub struct Selection {
    pub materials: Vec<Material>,
    pub summary: SelectionSummary,
    pub gaps: Vec<CoverageGap>,
}
```

- [x] 定义 `ReportProfile::{Evidence, Personal}`，将新 profile 纳入个人配置指纹；旧 profile 序列化缺省值省略。测试旧配置指纹与既有冻结样例一致，个人配置不能误复用旧计划。
- [x] 定义 `PersonalItem`：`title`、`text`、`kind`、`statement_kind`、`source_ids`。`kind` 取 `progress/attention/next_step`，`statement_kind` 取 `observed/user_statement/uncertain/suggestion`。定义 `PersonalReport`，含版本 5、`report_kind=personal_daily`、现有日期窗口字段、`quality`、`sources`、`items`、`evidence`、`selection`、`gaps`、`user_notes`；`evidence` 为来源编号到原文引用列表的映射。
- [x] 实现 `PersonalReport::validate()`：检查日期、编号唯一性、来源和原文指纹、不可用来源不得支持确定结论；最多 10 项，正文总字符数最多 1,500。人工原文与依据不计入正文字符数。只检查输出项，不套用旧 `ComposedReport` 的全事实覆盖要求。
- [x] 来源验证覆盖完整段落及表格的表头、单位、条件和前后配对。模型返回来源编号；保存的摘录由程序从已选输入复制，禁止使用模型自行改写的“引用”。用户来源复用 `ReportSource::user`，完整补充原文保存在 `user_notes`。
- [x] 固定内部默认限制，测试允许通过构造器传入更小限制；只新增 profile 配置入口，避免把每个内部上限都暴露成用户选项。

**验证：** `cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --locked --test personal_reports --test cli_config --test legacy_recovery`。

测试应覆盖有效数值变更、未知来源、篡改摘录、把未确认输入写成已确认、11 项输出、正文超限、仅用户补充、旧格式样例保持有效。语义真实性仍需在任务 8 检查可读样例。

## 任务 2：建立有总配额的处理区

**修改：** `snapshots.rs`、`archive/store.rs`、`archive/mod.rs`、`lib.rs`；新增 `processing_cache.rs`、`archive/retention.rs` 和 `tests/personal_cache.rs`。

**目录与接口：** `data_dir/daily_cache/` 存放 `archive/`、`snapshots/` 和暂存文件。`ArchiveStore::open` 保持旧归档读取；新增 `ArchiveStore::open_collection(config, writable)` 为 personal 选择 `daily_cache/archive`，否则选择原归档。显式历史查询/恢复使用旧入口，当前采集、队列和健康检查使用新入口。

`ProcessingSession::acquire(config, now, deadline)` 持有操作系统文件锁并提供 `Arc<CacheQuota>`。`CacheQuota::reserve(bytes)` 返回 `CacheReservation`；其 `commit(paths)` 核对实际大小并登记，释放时归还未使用预留。所有路径相对固定处理区，拒绝重定向。

- [x] 用现有 `ReportLock` 对整个处理操作串行加锁：采集/补解析遇占用返回 `skipped`，日报最多等待 5 秒且计入总预算。先取得处理区锁，再取得报告库锁；内部函数接收已持有的会话，不重复获取同一锁。
- [x] 在进入操作时核对实际文件和残留暂存，计入完整逻辑大小。只清理属于已登记、已结束操作的临时文件；未知文件计入容量并报错，不猜测其用途后删除。核对本身受时间限制，未完成时停止新写入。
- [x] 将原件复制、Worker 输出、快照、模型请求及响应统一纳入预留。Worker 使用预留量作为真实输出上限；写入达到上限即终止该对象。快照创建硬链接期间按两个路径的逻辑大小保守预留，避免临时超额。
- [x] 新处理区索引仍复用归档 schema v1，但单独设置 SQLite：使用 DELETE journal，`temp_store=MEMORY`，主库最多 32 MiB，预留 72 MiB 作为主库、回滚日志和控制文件的容量池；其余容量用于材料。首次建库前设置增量自动回收与页数上限，每次至多 128 个对象的小事务写入，单条诊断最多 4 KiB；启动时核对 PRAGMA 实际值及日志大小。已建立的新处理库发现不符时停止写入，不静默扩大预算。旧归档 WAL 配置保持。
- [x] 新增 `SnapshotStore::with_quota(root, max_bytes, Arc<CacheQuota>)`，保留旧构造器；新运行快照走共享配额，旧快照仍使用原目录。处理区满额时返回 `PROCESSING_CACHE_FULL`，不能回退写入旧归档。
- [x] 实现只针对新处理区的保留规则：每份正文最多 7 天，容量不足优先释放已结束的最旧材料。保留当前文件指纹所需的最小版本元数据，正文可到期；清理前解除过期的版本前驱关联及变化引用，再按外键顺序移除历史记录。排队任务到期结束，失败重试标记同步移除。旧归档原件和 `report_versions` 保护规则保持原样。

**伪代码：**

```text
取得处理区独占锁
核对已有文件与预留；回收已结束且过期的材料
写入前：已占用 + 活动预留 + 本次上限 <= 1 GiB
不足：尝试回收最旧可释放材料；仍不足则返回明确缺口
成功发布：最终报告已包含依据 -> 解除临时材料保护
异常退出：下次取得锁后按实际文件重算，不沿用失效预留
```

**验证：** `cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --locked --test personal_cache --test retention --test archive_storage`。用小配额验证两个同时申请的写入不会超过总量、输出上限实际生效、中断残留计数、外键完整、旧目录及链接目标不被清理。

## 任务 3：让采集、解析和目录遍历都受预算控制

**修改：** `rust/discovery/src/lib.rs`；`archive/collect.rs`、`parse.rs`、`queue.rs`、`diff.rs`、`resources.rs`。测试使用新增 `rust/discovery/tests/bounded_discovery.rs`、`report_app/tests/personal_collection.rs`，并复用采集时段、队列测试。

**接口：** 新增 `discover_files_bounded(request, deadline, max_entries)`，返回 `DiscoveryReport` 并以新增问题类型表达时间/数量中断；旧 discovery API 保持原行为。所有当前采集入口接收任务 2 的处理会话。新增 `diff::compare_bounded(..., deadline, max_units)`，复用原比较算法。

- [x] 对目录枚举、元数据读取之间、候选队列和比较循环增加剩余时间检查。每次最多检查 10,000 个新条目，页内按路径稳定排序并保存遍历续点；后续从未完成位置继续，到末尾后重新开始。标准文件系统 API 不能跨进程定位目录游标，恢复时需要重放此前名称，重放也检查截止时间但不计为新的元数据检查。目录变化造成遗漏时如实记缺口；中断或分页根目录不能进入 `complete_roots`，不能据此推断删除或重命名。
- [x] personal 采集总时限为 120 秒，复制、Worker 启动、哈希核对和解析使用剩余时间。复制前检查单输入上限；默认单输入最多 50 MiB、单解析产物最多 16 MiB。容量不足以预留固定上限时暂缓该对象，不根据实时空闲量改变同一解析身份的输出范围。超限按对象说明。有限队列按最近变化优先，不在内存中展开全量正文。
- [x] 使用 `open_collection` 以及处理区导出的锁/暂存路径，替换当前采集流程中写死的 `data_dir/archive`。历史 `archive recover/verify/prune` 保持显式旧目录用途。
- [x] personal 复用原 Worker 请求及解析器，仅降低时间和输出预算。新增输出影响参数必须进入解析配置身份；若修改 `parse_request.rs`，按现有构建身份规则重建匹配 Worker。保持 OCR 关闭，不以旧身份复用受影响产物。
- [x] personal 采集记录观察版本和解析状态，不对每个中间版本计算完整差异。日报在任务 4 对所选的日期首尾版本比较。单次比较最多读取每侧 5,000 个单元；超限返回部分范围和缺口，不能声明完整无变化。
- [x] 首次观察状态逐轮建立，初次清单不被当作业务进展；持久化基线起点。已清理正文但保留指纹的文件继续识别是否变化；需要比较而旧文缺失时产生 `PREVIOUS_CONTENT_UNAVAILABLE`，不重新制造整份“新增”事件。
- [x] 队列仅重试仍在 7 天范围内的材料；同一对象最多沿用现有 3 次解析尝试。超时/配额不足保留有效队列，过期后关闭。工作日窗口判断继续先于锁、资源申请和任何写入。

**验证：** 分别运行 discovery 的 `bounded_discovery` 测试，以及 report_app 的 `personal_collection`、`archive_queue`、`collection_workday_window`、`collection_policy`。重点断言遍历不完整不报文件消失、夜间不创建处理区、解析超时后 Worker 实际退出、无变化重复采集不产生新正文。

## 任务 4：准备目标日材料并合并人工补充

**修改：** `run/input.rs`、`archive/model_input.rs`、`archive/diff.rs`；新增 `archive/selection.rs`；选材验收合并在 `tests/personal_collection.rs`，复用真实采集与解析样例。

**接口：** `selection::prepare(config, request, deadline) -> Result<Selection, ReportError>`，读取任务 2 的当前处理区；复用现有 `permitted`、`diff::unit/text` 和表格主键规则。`run/input.rs` 提供有限读取日期备注的方法，供新流程使用。

- [x] SQL 按 `window_start_utc <= observed_at < window_end_utc` 选择目标日文件；每文件取窗口前最近可用版本及窗口内最后观察版本，并以本次有界补解析完成后的冻结时刻限制可用解析结果。观察窗口仍严格属于目标日。使用有索引的分页查询，每页 128 条，不沿用“所有未发布变化”的查询。
- [x] 首次发现、缺失前版、前后解析不完整分别标记。当天改动后恢复原样，仅在两侧完整可比时判定无有效变化；窗口中间状态不作为最终进展。
- [x] 命令行与日期备注统一正规化换行、按完整重复段落去重，合计超 2,000 字返回 `USER_INPUT_TOO_LARGE`，保留原文件。作为一个 `user_input` 来源优先纳入，避免同一编号多次出现。
- [x] 候选材料按明确截止/阻塞提示、状态或关键字段变化、一般变化排序。同级按观察时间和路径稳定排序；不同文件轮流获得机会，再补充同文件。关键词只作为优先级信号，不作为已确认事实。
- [x] 逐单元计算文字量；完整保留变更前后值、标题、表头、单位和条件。先扣除人工补充，再填入最多 20,000 字。真实请求超过字节或上下文容量时继续整单元减量，不增加事实提取批次。
- [x] `SelectionSummary` 分别记录发现、检查、选入数量。`omissions` 使用 `duplicate/out_of_scope/budget/read_failed/comparison_unavailable`；路径例子最多 10 条。遍历未完成时只能报告已观察下限，不能写成精确遗漏总数。

**伪代码：**

```text
material = 合并后的用户补充
遍历目标日有限文件候选 -> 读取首尾版本 -> 有界比较
按优先级分组，各文件轮流提供完整材料单元
能够容纳：加入材料并累计字符、字节、来源范围
不能容纳：记录 budget 原因，不把该单元标为已总结
返回 Selection；没有变化和读取失败分别返回不同状态
```

**验证：** `cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --locked --test personal_collection --test personal_pipeline --test archive`。合成 200 行表格更新、跨日积压、同日多次修改、缺失前版、只有补充及中文/emoji 输入；断言材料上限、完整配对、选择确定性和零模型请求。

## 任务 5：持久化每日额度并接入一次生成

**修改：** `store/calls.rs`、`store/mod.rs`、`model/plan.rs`、`model/mod.rs`；新增 `store/daily_budget.rs`、`model/personal.rs`、提示词和 `tests/personal_model.rs`。复用本地 HTTP 模拟服务。

**接口：** 新增 `StepKind::PersonalSummary` 和 `StepOutput::Personal`，模型计划版本 6。`model::personal::prepare(llm, context, selection)` 生成一个 `PlannedStep`；调用现有 `ModelRunner::execute`，不进入 `generate_with_workers` 的提取/成稿循环。

- [x] 在准备新运行时，将运行类型登记到 `report_metadata` 的 `personal_run:{run_id}`。在 `reserve_call` 的同一 IMMEDIATE 事务内，根据持久运行类型执行目标日额度检查，不依赖可关闭的 `enforce_model_budgets`。
- [x] 额度键为 `personal_calls:{scope_id}:{target_date}`，JSON 值保存 `used` 及结束摘要。新键起始为 0；每次请求预占先检查 `used < 3`，递增与调用行插入共同提交。任何回滚不消耗额度，提交后即使尚未完成网络发送也保守计入。
- [x] 同一天的新 run、恢复、预览和手动普通重跑共享额度；新版本自动运行不追赶旧版本未完成运行。清理详细调用行时保留日期累计值。额外额度不实现隐式重置入口。
- [x] 新提示词要求直接生成最多 10 个事项，并区分用户陈述、观察、待确认和行动建议。输出仅引用输入提供的编号。检查结构与引用失败才在剩余额度/时间内重试；个人步骤不执行旧版自动拆批、再次成稿或长文本润色。
- [x] 更新 `PlannedStep` 的构造、请求身份验证和输出类型分派，覆盖 `rebuild/validate_identity/check_capacity` 等分支。成功响应仅在来源、提示词、schema、日期和请求字节完全匹配时复用。
- [x] 来源内容作为数据传入提示词；文档中要求忽略规则、发送额外材料的文字不能改变程序权限或请求边界。真实发送前继续重读允许目录配置。

**额度事务伪代码：**

```text
BEGIN IMMEDIATE
读取 run 的 scope、target_date 和持久 profile
若 personal：读取日期累计值；used >= 3 -> 返回 PERSONAL_CALL_LIMIT
更新日期累计值；插入 reserved 调用记录
COMMIT
执行一次网络请求；结果写 succeeded / failed / unknown，均不退还额度
```

**验证：** `cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --locked --test personal_model --test model_execution --test state_recovery`。构造三个分别失败、未知、成功的调用；重启并删除隔离调用明细后，第 4 次仍被拒绝且服务器收到的总请求不增加。验证并发预占、旧预算开关为 false 和不同目标日期。

## 任务 6：连接截止时间、独立依据与发布恢复

**修改：** `main.rs`、`run.rs`、`run/daily.rs`、`reports.rs`、`store/reports.rs`、`publish.rs`、`operations.rs`；新增 `run/personal.rs`、模板与 `tests/personal_pipeline.rs`。

**接口：** `run::personal::run(request, app, started)` 处理 personal 新运行；`PersonalInput` 版本 5 保存 `context`、`selection`、可选 `PlannedStep`、模板及输入/恢复指纹。新格式在原始 JSON 分派阶段识别，不能先按旧 `FrozenInput` 解码。用持久运行 profile 选择快照目录，不根据当前配置猜测旧运行位置。

- [x] 从 CLI 创建日报请求开始传递 `started`，包含 App 打开、锁等待和材料准备。旧 `prepare_scheduled_archive` 的独立 120 秒准备不能在新日报预算之外执行；personal 在同一截止时间内准备，截止后直接说明缺口。
- [x] 以 `started + 270s` 作为新工作的截止时间，`started + 300s` 作为收尾截止时间。请求超时不超过剩余工作时间，循环、SQLite 等待及 Worker 使用同一剩余预算。锁不可得返回明确运行错误，不能并行覆盖另一日报。
- [x] 只有成功检查且无材料、无补充、无缺口时才生成 `no_changes`。有材料但模型失败时本地生成 `partial`，只包含原文补充、已核实变化概况和说明；无可用内容时生成可读失败状态，不把它标为健康或已总结。
- [x] 模型输出有效后，将每项使用的完整依据从 Selection 复制到报告。采用单份 Markdown 内嵌关键摘录，数据库保存结构化副本，简化双文件提交一致性。超过 100,000 字节时从最低优先级整项移除并清除无引用摘录，保留完整用户原文；重新验证后发布。
- [x] `store/reports.rs` 按版本调用对应验证器，允许 v5，`report_kind` 从已验证报告读取，不再固定写入 `file_evidence`。复用 `Publisher` 原子写入、外部修改检测和 `db_committed` 恢复。
- [x] v5 不调用旧 `model_input::pin/published`，不登记全部变化覆盖或无限期原件引用。只有结构化依据和文件均通过校验后，才发布并解除临时材料引用。已发布有限结果的后续自动触发直接返回 existing。
- [x] `repair_run/repair_output` 按持久格式分派：v5 只需数据库中的报告和正文，无需缓存；旧格式继续按原归档发布恢复。临时输入已过期的新运行标为不可继续该输入，不能靠反复恢复永久保护文件。
- [x] 注册新提示词与模板到 `operations.rs` 的内嵌模板校验清单，包内文件必须与编译字节一致。

**验证：** `cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --locked --test personal_pipeline --test publication --test templates --test scheduled_log`。验证缓存删除后修复输出、写文件前/后中断、人工改动冲突、重复定时调用零新增请求、剩余时间不足时不新发请求。验证金额、条件和建议标签的最终渲染。

## 任务 7：接通历史读取、维护与实际任务配置

**修改：** `store/reports.rs`、`run/period.rs`、`search.rs`、`health.rs`、`maintenance/retention.rs`、`operations.rs`、`archive/cli.rs`、`config/settings.example.yaml`；任务计划由 `operations.rs::task_plan/task_plan_kind` 生成，安装及回读复用 `scripts/daily_task_common.ps1`、`install_daily_task.ps1`、`check_daily_task.ps1`。测试使用既有 `search/period_pipeline/health/retention` 及 Windows 调度测试。

- [x] `reports_in_range` 接受并验证 v5；`daily_context` 将 v5 `items` 与必要来源和质量状态传给周月报，保留用户陈述/建议标志。搜索按事项索引 v5，并提高索引实现版本触发可重建索引更新；原报告库只读。
- [x] 当前健康检查根据 profile 使用新采集库；将“报告已发布”和“内容仅部分可用”分开，显示缓存用量、过期队列和读取缺口。历史失败不反复覆盖较新的成功运行，也不抹去原失败记录。新流程在 CLI 错误出口、运行结果和调度日志统一提供 `error_code/message/retryable`：锁冲突、临时读取或连接失败可重试；预算耗尽、配置无效、输入超限及快照过期不可自动重试。修改 `main.rs` 与 `operations.rs`，内部继续复用 `ReportError`，不改写无关错误体系。
- [x] 清理 v5 的 7 天输入/请求/响应后，解除数据库中的快照路径引用，按日期保留额度和最终报告依据。超过 30 天或 10 MiB 时收缩 personal 详细调用、重复运行和日志，保留日报关联汇总。保留旧运行的引用保护，不能把旧运行缺快照误当作可删除。
- [x] 本地候选配置采用 `report.profile=personal`，日报和采集配置的工作目录、模型权限、处理目录与 profile 必须一致。安装验证明确拒绝混用两种处理目录的任务组合；运行进程中不复制新材料到旧归档。
- [x] 日报应用预算为 300 秒，计划任务外部兜底保留至少 60 秒退出余量；采集/补解析应用预算 120 秒，调度兜底至少 180 秒。调用既有安装脚本参数与回读流程实现，不仅修改配置文本。时间检查只能约束程序主动启动的新工作；Windows 文件系统调用卡死时由外部调度终止。分别记录正常应用退出和调度终止，不能宣称外部强制终止前必然已保存结果。
- [x] 更新新安装示例和 README、AGENTS、日报使用/恢复与 Windows 部署说明。记录模型请求与处理期缓存边界、补充入口、首日基线和回退限制。旧二进制可能不能读取新增 v5 报告：回退需隔离其数据目录或保留新版本只读访问，不能把旧数据库备份覆盖新增记录。

**验证：** `cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --locked --test search --test period_pipeline --test health --test health_persistence --test retention --test reliability_status`。对实际修改的调度脚本运行 `tests/windows_operations.ps1`、`tests/collection_window_operations.ps1`、`tests/windows_health_notify.ps1`，这些测试使用替身，不注册真实任务。

## 任务 8：完整隔离验收、发布准备与历史占用清单

**修改：** 扩展 `tests/bundle_acceptance.rs` 支持 personal；更新 `tests/support/model_mock.rs`，增加 v5 回复而保留旧格式分支。实施结果以测试输出及现有 `docs/current-state.md` 的简短记录说明，不强制新建结果文档。

- [x] 准备合成工作目录：一个小文本、一个 200 行变更表格、同日多次更新、前版缺失、跨日积压、超过容量的文件及可选补充。用本地 HTTP 服务生成日报，检查文件与数据库一致、已用额度、耗时、缓存大小、来源及遗漏提示。
- [x] 将缓存配额和时间通过测试构造器缩小，验证写入中断、容量不足、异常进程退出和过期清理。容量不足使用隔离的小配额模拟；真实 Worker 及其子进程超时终止已有运行证据，不据此声称复现了 Windows 文件系统调用卡死或实际磁盘写满。
- [x] 人工检查固定样例：金额 100→120、仍待审批、条件生效、未确认截止、程序建议；确认没有把变化直接写成成果，没有将旧内容称为新增。
- [x] 复用开发 target；先构建与源码匹配的 Office/PDF Worker 及所需 examples，把固定且核对过哈希的 PDFium 放在对应 Worker 目录，设置既有 `AI_DAILY_TEST_WORKER_DIR` 后执行需要 Worker 的测试。保留旧运行 Worker。
- [x] 局部检查通过后执行一次完整门禁：

```powershell
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked
```

- [x] 使用 `scripts/build_windows_release.ps1` 和其默认共享 release target 生成一个新候选目录，核对 manifest。按现有 `AI_DAILY_TEST_BUNDLE`、`AI_DAILY_ACCEPTANCE_OUTPUT` 指向该候选包及新隔离输出目录，执行：

```powershell
cargo test --manifest-path rust/Cargo.toml -p ai-daily-report --locked --test bundle_acceptance -- --ignored
```

此验收使用本地 HTTP 模型替身，远程请求为零。真实模型服务耗时和真实日历触发需要在正式授权后独立验证，不能用本地替身结果宣称完成。

- [x] 准备正式切换的具体差异：程序包绝对路径、两份配置、四个任务定义、处理区路径、首日基线、备份与哈希、恢复步骤。候选四任务组一致，日报外部时限 6 分钟，采集/补解析各 3 分钟；已生成与现存任务的差异，未注册任务。
- [x] 正式授权后刷新备份，执行模型自检和四任务切换，回读实际程序路径/profile/Worker/任务窗口，核对旧报告、旧归档与新增记录。2026-10-21 01:15 已切换到 `rust-20261021-personal-02`，四任务启用且回读匹配，历史数据保持。
- [ ] 验收一次切换后的真实日历日报：下一次触发为 2026-10-21 09:00。当前记录仅覆盖切换回读、手动接续采集和模型自检，真实日历验收尚未发生。
- [x] 历史空间处理只生成只读清单：区分可再生构建/测试目录、当前与回退包、被旧报告或未完成运行引用的业务归档。列绝对路径、实际占用、引用关系和恢复方式；硬链接/压缩按实际占用核算。删除前取得对应范围授权，不能从新缓存期限推导旧数据删除权限。

**本地验收结果（2026-10-21）：** 完整工作区 607 项测试通过，0 失败、3 项默认忽略；fmt 与严格 Clippy 通过。其中两项独立包验收另行通过，剩余一项为需要指定历史语料的回放测试。最终中文模板相关 13 项回归通过；安装包测试调用修正后，相关严格 Clippy 通过。Windows 运维 26 项、采集安装 6 类失败场景、健康 109 项及 personal 四任务一致性检查通过，均未修改真实任务。

候选包 `rust-20261021-personal-01` 使用默认共享发布缓存构建，460 个文件清单校验通过。[旧流程包验收](../../../.artifacts/personal-20261020/bundle-01/acceptance.json)和[个人日报包验收](../../../.artifacts/personal-20261020/bundle-02-personal/acceptance.json)均保留。个人样例一次本地请求，生成耗时 6,185 ms，正文含依据 1,410 字节；输入选材 13,275 字，清理前缓存 3,048,919 字节，过期清理后 167,939 字节。重复调度零新增请求，处理材料清理后正文恢复字节一致。固定样例故意保留采集缺口：[健康结果](../../../.artifacts/personal-20261020/bundle-02-personal/health.json)分别显示发布成功、内容 partial 和采集覆盖错误，整体 unhealthy 符合该样例，不能当成生产健康结论。

[切换准备](../../../.artifacts/personal-20261020/cutover/README.md)包含候选配置、四任务拟议 XML、当前任务差异、备份和回退说明；模型自检尚未执行，候选配置的自检状态仍为 false。[历史占用清单](../../../.artifacts/personal-20261020/history-inventory.json)已完成，检查范围硬链接去重后的 NTFS 实际占用约 92.9 GiB，本轮释放空间 0 字节。正式生产切换、远程模型调用、历史删除及 Git 发布均未执行。

**正式切换结果（2026-10-21 01:15）：** 首次切换在单文件复制回执的容量边界处失败并自动回退，数据库未从备份覆盖。修复为复制阶段预留 16 MiB 解析空间，回执空间不足时按文件暂缓并完成本轮记录；70 项相关回归、严格 Clippy 和 fmt 通过。新包 `rust-20261021-personal-02` 的 460 文件校验及两项独立包验收通过。第二次切换接续已有基线，采集 46,033 ms 正常退出、材料为 `partial`，最终缓存 929,618,262 字节；1,039 个解析任务排队，读取缺口保留。四任务同包、同 profile 和处理目录，日报时限 6 分钟、采集/补解析各 3 分钟。两次真实模型自检共 2 次请求；202 份既有报告、原有数据库记录和旧归档索引保持，历史数据删除量为 0。健康为 `unhealthy`，原因是 10 月 20 日日报未发布及解析积压。详见[正式运行状态](../../current-state.md)及[切换核验](../../../.artifacts/personal-20261020/deployment-02/runtime-verification.json)。上文“未切换/未调用远程模型”的结果仅指切换前的本地候选验收阶段。

## 依赖与完成标准

执行顺序：任务 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8。每项只运行其影响的验证；完整门禁通过后，仅在新改动、失败或仍有疑点时重复相关检查。

| 设计要求 | 对应任务 |
| --- | --- |
| 个人用途、可选补充、简短结构与可信来源 | 1、4、5、6 |
| 目标日范围、首尾比较、本地筛选和遗漏提示 | 3、4 |
| 跨运行请求额度、截止时间、失败结果 | 5、6 |
| 7 天/1 GiB、写入预留、队列到期、清理后依据可读 | 2、3、6、7 |
| 旧格式、搜索周月报、任务切换与回退 | 1、6、7、8 |
| 整体速度、输出大小、真实运行及历史空间回收 | 8 |

分别报告三个结论：本地实现及隔离验证是否通过、正式运行是否已生效、历史空间是否实际释放。只有对应证据存在时才声明该项完成。
