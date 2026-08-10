# Deep Report Run Module 设计规格

> 状态：Ready for implementation  
> 日期：2026-07-17  
> 决策范围：daily、weekly、monthly 报告命令的应用层编排  
> 首要目标：建立一个高杠杆测试 seam，并保持现有 CLI、scanner 与报告产物契约  
> 不涉及：Rust scanner、parser backend、cache、Hybrid Office fallback、报告模板内容重设计

## Problem Statement

### 当前问题

当前 daily、weekly、monthly 三条报告命令分别在 CLI 层完成完整运行编排。每条路径都直接知道：

- 如何构造 scanner 请求；
- 如何判断 scanner 的 ok、partial、error；
- 何时读取日报数据库或昨日计划；
- 何时构造 LLM client；
- 如何拼接用户补充内容；
- 如何选择报告 schema 与模板；
- 如何先写 SQLite、再写 Markdown；
- 如何处理 --no-save、预览、warning 与退出码。

这形成了三个 shallow orchestration：

| 现有路径 | 重复知识 | 特有分支 |
|---|---|---|
| daily | scanner、LLM、render、publish、preview | 固定 scan、昨日计划、交互输入、日期覆盖 |
| weekly | scanner 或 DB、LLM、render、publish、preview | ISO 周解析、缺失工作日、补充输入 |
| monthly | scanner 或 DB、LLM、render、publish、preview | 月份解析、缺失工作日、补充输入 |

重复不只存在于生产代码。当前 CLI 测试需要反复替换 ContextScheduler、SQLiteStore、ReportGenerator 与 LLMClient，并断言 CLI 内部调用顺序。结果是：

1. CLI interface 很宽，调用方和测试都必须理解内部依赖。
2. scanner error/partial、--no-save 和发布顺序的规则需要在三处保持一致。
3. 修改任何共享规则都会同时影响三套实现与大量 shallow 测试。
4. 失败目前主要压缩为 bool 或通用异常，调用方无法稳定区分失败阶段。
5. SQLite 已提交但 Markdown 写入失败的非原子状态没有一等表达。
6. 真实外部依赖与本地可替代依赖混在同一层，测试容易走向全量 mock。

### 目标

本规格要把三条报告运行路径收进一个 deep module，使调用方只学习一个 interface，内部隐藏 source acquisition、LLM generation、render、publication 与 failure checkpoint。

完成后的可观察目标：

1. daily、weekly、monthly 全部通过同一个 ReportRunner.run seam。
2. CLI 只负责参数映射、交互输入适配、结果展示和退出码。
3. 一次 scan 报告运行最多构建一次 context；一次成功运行最多调用一次 LLM。
4. scanner error 必须在 LLM adapter 构造之前终止。
5. scanner partial 必须保留 warning 并继续生成。
6. --no-save 必须阻止报告 SQLite 与 Markdown 的发布写入。
7. source=db 必须零次调用 scanner。
8. 预期运行失败使用稳定的 error_code、message、retryable 和 phase 表达。
9. SQLite 成功、Markdown 失败时必须显式返回部分发布状态，不做伪事务。
10. 主要测试从 CLI 内部调用顺序转移到 ReportRunner interface。

### 成功边界

本规格是应用层架构收敛，不是 scanner 性能重构。成功标准是：

- scanner 请求次数不增加；
- LLM 请求次数不增加；
- Rust CLI JSON contract 不变；
- scanner DB 继续由 Rust 独占；
- Cold scanner run、warm cache identity 与 Hybrid Office fallback policy 不变；
- CLI 参数、退出码、语义提示和 Markdown 预览保持兼容；
- 报告 schema、模板内容和报告数据库 schema 不变。

## Solution

### 方案结论

建立一个 Report Run deep module，公开一个行为 interface：

**ReportRunner.run(ReportRunRequest) -> ReportRunOutcome**

ReportRunRequest 是 daily、weekly、monthly 三个封闭 variant 的联合类型；ReportRunOutcome 是 success、failure 两个封闭 variant 的联合类型。

module 内部保留一条公共 execution pipeline，并使用私有 recipe 表达三种报告的真实差异。禁止公开 run_daily、run_weekly、run_monthly 三个平行 interface，也禁止引入 mode registry、middleware、step plugin 或通用 workflow engine。

### 三种候选 interface 的比较

| 候选 | 核心形态 | 优点 | 主要代价 | 结论 |
|---|---|---|---|---|
| 最小 interface | 一个 run，加一个 CLI interaction adapter | interface 最小，能保留交互时点 | 结果若过度压缩会丢失 source 与发布证据 | 采用其单一行为 interface |
| 灵活 interface | 一个 run，封闭 request/outcome，显式 as-of date | 非法组合少、可复现、扩展点有事实依据 | 每个请求多一个基准日期 | 采用其 typed union 与显式日期 |
| daily-first interface | DailyRun 默认最短，clock 与 interaction 注入 | 日常调用简单、交互兼容 | clock seam 可由值输入替代；事件 interface 易扩大 | 采用其延迟输入，拒绝额外 clock/event seam |

最终混合方案的取舍：

- 保留一个 run interface；
- 使用封闭 request union，拒绝万能可空 DTO；
- 请求显式携带 as_of_date，拒绝新增 Clock port；
- daily 的交互输入通过一个极小的延迟 input adapter 获取，以保持“source gate 后再询问”的既有顺序；
- CLI 直接解释最终 outcome，不建立 presenter 或 progress event bus；
- LLM 是唯一新增的 true external port；
- scanner 复用现有 ContextScheduler interface；
- SQLite 与 filesystem 使用真实本地 substitute 测试，不建立公开 repository/writer port。

### 目标架构

~~~mermaid
flowchart LR
    CLI["CLI adapter<br/>参数、交互、展示、退出码"] --> Runner["ReportRunner<br/>唯一应用 seam"]
    Input["Daily input adapter"] --> Runner
    Runner --> Scheduler["ContextScheduler<br/>现有 scanner interface"]
    Runner --> Store["Report SQLite store<br/>日报读取与报告发布"]
    Runner --> ModelPort["Report model port<br/>延迟构造"]
    ModelPort --> LLMAdapter["LLM provider adapter"]
    Runner --> Renderer["Jinja renderer<br/>内存 Markdown"]
    Runner --> Publisher["Markdown publisher<br/>按需写文件"]
    Scheduler --> Rust["Rust scanner CLI<br/>JSON contract"]
    CLI <-->|"ReportRunOutcome"| Runner
~~~

调用方向必须保持单向：

- CLI 可以依赖 ReportRunner interface 和请求/结果类型；
- ReportRunner implementation 可以依赖现有 scheduler、store、renderer 与 module-owned model port；
- scheduler、store、renderer、LLM adapter 不得反向依赖 CLI；
- ReportRunner 不得读取或解释 scanner DB；
- ReportRunner 不得根据 parser backend、worker lane、deadline 或 fallback 重新决策。

### Public Interface

#### 1. ReportRunner

ReportRunner 只公开一个同步行为：run。

输入是一个 request variant；输出总是一个 typed outcome。预期业务失败不抛给 CLI。只有未知 request variant、返回 report 类型与 mode 不匹配等 implementation invariant 被破坏时，才抛出编程错误，由顶层通用异常处理映射为退出码 1。

#### 2. Request variants

| Request variant | 必填字段 | 可选字段 | 类型不变量 |
|---|---|---|---|
| DailyReportRunRequest | as_of_date、save | user_input、report_date_override | source 隐含为 scan，不能选择 db |
| WeeklyReportRunRequest | as_of_date、source、save | week_label、supplemental_input | source 只能是 db 或 scan |
| MonthlyReportRunRequest | as_of_date、source、save | year_month、supplemental_input | source 只能是 db 或 scan |

字段语义：

- as_of_date：本次运行的基准日期。CLI 在进入 module 前提供系统当天日期；测试和未来重放可提供固定日期。
- save：正向发布开关。CLI 将 --no-save 映射为 save=false。
- user_input：daily 的显式输入。为 null 时，module 在 source 被接受且昨日计划读取完成后调用 daily input adapter。
- report_date_override：只覆盖 LLM 返回后的日报 date，不改变 scan 窗口、昨日计划日期或 as_of_date。
- week_label：可空 ISO 周标签。为空时以 as_of_date 所在周为目标周。
- year_month：可空 YYYY-MM。为空时以 as_of_date 所在月为目标月。
- supplemental_input：周报或月报的可选补充文本。空白文本按未提供处理。

禁止使用一个同时包含 week、month、daily input、source 等大量条件字段的万能 request。新增季度报告时，应在真实需求出现后新增第四个 request variant，而不是提前开放字符串注册。

#### 3. Resolved period

module 内部将 request 解析为只读的 resolved period，至少包含：

- mode；
- source；
- start_date；
- end_date；
- display_label；
- as_of_date。

resolved period 是 implementation detail，不对 CLI 公开。其作用是让 source acquisition、generation 与 publication 使用同一组已验证日期，避免各阶段重复解析。

#### 4. Success outcome

成功 outcome 至少包含：

| 字段 | 语义 |
|---|---|
| outcome | 固定为 success |
| status | ok 或 partial |
| mode / source | 实际运行模式与证据来源 |
| period | 已解析的日期范围与展示标签 |
| report | DailyReportData、WeeklyReportData 或 MonthlyReportData |
| markdown | 已在内存中渲染的 Markdown，用于预览 |
| warnings | 按原顺序保留的诊断集合 |
| source_evidence | scan summary 或 DB evidence summary |
| publication | 本次发布请求与两个载体的最终状态 |

partial 是成功状态，不是 failure。它表示 source acquisition 有可审计 warning，但已有足够上下文继续 LLM、render 和可选 publication。

source_evidence 使用封闭 variant：

- ScanEvidence：scanner status、source file count、success count、scan_run_id、context_run_id；
- DatabaseEvidence：读取的日报数量、按日期升序的 missing_days。

#### 5. Failure outcome

失败 outcome 至少包含：

| 字段 | 语义 |
|---|---|
| outcome | 固定为 failure |
| mode / source | 已知的运行模式与来源 |
| period | 如果 request 已成功解析，则返回解析结果 |
| phase | request、source、generation、render、sqlite_publish 或 markdown_publish |
| error | error_code、message、retryable 与可选 cause metadata |
| warnings | 失败前已产生的 warning |
| source_evidence | 失败前已取得的可审计证据 |
| publication | 两个载体的实际发布 checkpoint |

调用方不得通过 message 文本推断失败阶段；必须使用 phase 和 error_code。

#### 6. Publication receipt

publication 必须显式表示：

- requested：请求是否要求发布；
- sqlite_state：not_attempted、committed 或 failed；
- markdown_state：not_attempted、written 或 failed；
- markdown_path：仅写入成功时存在。

这比 attempted=true/false 或两个可空 path 更准确，能表达：

- no-save：两个 state 都是 not_attempted；
- SQLite 失败：sqlite_state=failed，markdown_state=not_attempted；
- Markdown 失败：sqlite_state=committed，markdown_state=failed；
- 完全成功：sqlite_state=committed，markdown_state=written。

### Execution Pipeline

ReportRunner implementation 只有一条公共 pipeline：

~~~mermaid
flowchart TD
    A["Validate request"] --> B["Resolve period"]
    B --> C{"Source"}
    C -->|scan| D["Build context exactly once"]
    C -->|db| E["Read reports and missing days"]
    D --> F{"Scanner status"}
    F -->|error| X["Typed source failure"]
    F -->|partial or ok| G["Prepare generation input"]
    E --> H{"Any source reports?"}
    H -->|no| Y["Typed no-source failure"]
    H -->|yes| G
    G --> I["Resolve deferred daily input if needed"]
    I --> J["Lazily create model adapter"]
    J --> K["Generate typed report exactly once"]
    K --> L["Apply daily date override"]
    L --> M["Render Markdown in memory"]
    M --> N{"save?"}
    N -->|no| S["Success with no publication"]
    N -->|yes| O["Commit report SQLite"]
    O --> P["Write Markdown file"]
    P --> S
~~~

固定执行顺序：

1. 校验 request variant 与字段格式。
2. 解析目标周期。
3. 获取 source evidence。
4. 执行 scanner status 或 DB data gate。
5. 准备 daily 昨日计划，或 period reports/missing_days。
6. 必要时延迟读取 daily 交互输入。
7. 延迟构造 model adapter。
8. 调用 LLM 一次。
9. daily 在生成后应用 report_date_override。
10. 在内存中渲染 Markdown。
11. save=true 时先提交报告 SQLite。
12. SQLite 成功后写 Markdown。
13. 返回 typed outcome。

禁止事项：

- ReportRunner 不自动重试 scanner 或 LLM；
- ReportRunner 不并行执行 SQLite 与 Markdown publication；
- ReportRunner 不在 scanner error 后构造 model adapter；
- ReportRunner 不在 render 失败后尝试 publication；
- ReportRunner 不用通用异常吞掉 phase 信息；
- ReportRunner 不公开三个 mode handler interface。

### Mode Behavior Matrix

| 行为 | Daily | Weekly / DB | Weekly / Scan | Monthly / DB | Monthly / Scan |
|---|---|---|---|---|---|
| 目标周期 | as_of_date-1 至 as_of_date 的 scan window | ISO 周 | ISO 周 | 自然月 | 自然月 |
| scanner 调用 | 1 | 0 | 1 | 0 | 1 |
| 报告 DB 作为生成来源 | 否；只读取昨日计划 | 是 | 否 | 是 | 否 |
| 无 DB 报告 | 不适用 | source failure | 不适用 | source failure | 不适用 |
| scanner error | source failure | 不适用 | source failure | 不适用 | source failure |
| scanner partial | warning 后继续 | 不适用 | warning 后继续 | 不适用 | warning 后继续 |
| 用户文本 | daily user_input | supplemental | supplemental | supplemental | supplemental |
| LLM 调用 | 最多 1 | 最多 1 | 最多 1 | 最多 1 | 最多 1 |
| render | 成功生成后 1 次 | 同左 | 同左 | 同左 | 同左 |
| publication | save 控制 | save 控制 | save 控制 | save 控制 | save 控制 |

#### Daily invariants

1. source 永远是 scan。
2. scan window 保持 as_of_date-1 至 as_of_date。
3. scanner error 立即失败；不得读取交互输入、不得构造 LLM。
4. scanner partial 保留全部 warning 并继续。
5. source 被接受后，读取 as_of_date-1 的昨日计划。
6. 显式 user_input 优先；未提供时才调用 daily input adapter。
7. 去除首尾空白后为空，返回 EMPTY_DAILY_INPUT，LLM factory 调用次数为零。
8. report_date_override 只在 LLM 成功后应用，不改变 scan window。

#### Weekly and monthly DB invariants

1. 不调用 ContextScheduler。
2. 日报按 date 升序进入 generation input。
3. missing_days 按日期升序。
4. 没有任何日报时返回 NO_SOURCE_REPORTS，LLM factory 调用次数为零。
5. file context 保持当前语义“无文件证据”。
6. supplemental_input 使用现有精确格式追加：

   <base context>

   ---

   用户补充: <supplemental input>

#### Weekly and monthly scan invariants

1. 使用完整目标周期调用 ContextScheduler 一次。
2. 不读取日报 DB 作为 generation input。
3. reports 与 missing_days 传空集合。
4. scanner 的 file_context 顺序保持 Rust implementation 输出，不在 Python 二次排序。
5. supplemental_input 使用与 DB 路径相同的拼接规则。

### Source Acceptance Policy

| source 结果 | ReportRunner 行为 |
|---|---|
| scan ok | 继续，status=ok |
| scan partial | 原序保留 warning，继续，最终 success status=partial |
| scan error | 返回 source failure；停止于 model factory 之前 |
| db 有报告 | 继续，记录 report_count 与 missing_days |
| db 无报告 | 返回 NO_SOURCE_REPORTS；停止于 model factory 之前 |
| db 读取异常 | 返回 SOURCE_READ_FAILED |

scanner diagnostic 的 error_code、message、retryable 与关联 run id 应尽量原样保留。ReportRunner 可以增加自己的高层 error_code，但不得用一条泛化文本抹掉 Rust contract 提供的原因。

### LLM Port

LLM 是 true external dependency，因此 module 定义一个窄的 ReportModelPort：

- 输入为封闭的 daily、weekly、monthly generation request union；
- 输出为对应的 typed report union；
- production adapter 包装现有 LLMClient；
- test adapter 返回固定 report，并记录调用；
- ReportRunner 持有 lazy factory，而不是已构造 client。

lazy factory 是行为要求，不只是测试技巧。它证明 scanner error、无 DB 报告和空 daily 输入不会初始化外部 provider，也不会触发其配置校验或网络副作用。

ReportRunner 不在该层实现 provider retry。retry、timeout 和 provider error 分类由现有 LLM adapter 负责；ReportRunner 只把结果归一为 LLM_GENERATION_FAILED，并保留可审计 cause metadata。

### Local Dependencies

#### ContextScheduler

复用现有 ContextScheduler interface，不新增 ScannerPort。

- production 使用现有 Rust scanner engine；
- module 测试使用 ContextScheduler 与确定性 fake engine；
- contract 集成测试继续使用现有 Rust CLI JSON fixture 或合成文件；
- ReportRunner 不复制 scanner DTO、profile normalization、cache identity 或 fallback 逻辑。

#### Report SQLite

报告 SQLite 是本地可替代依赖，不新增公开 repository protocol。

- module 测试使用临时目录中的真实 SQLite；
- DB source 与昨日计划读取必须按真实 schema 验证；
- write path 必须延迟到 save=true 且 render 成功之后；
- read path 必须避免隐式 schema 初始化或迁移。

--no-save 的“无 SQLite 发布写入”特指 Python 报告数据库。scan 路径仍允许 Rust scanner 按现有契约写入其独占的 scanner run/cache DB；本规格不改变 scanner 可观测性和 cache 行为。

#### Renderer and Markdown filesystem

renderer 与 filesystem publication 必须是两个阶段：

1. render 总是在内存完成，成功结果携带 Markdown 供 CLI 预览；
2. filesystem write 只在 save=true 且 SQLite 已提交后执行。

测试使用真实 Jinja 模板与临时目录。构造 renderer 不得仅因预览而创建输出目录；目录应在实际 Markdown write 时按需创建。

#### Daily input adapter

daily input adapter 只提供一个同步 read 操作：

- production adapter 从现有 CLI console 读取多行输入；
- test adapter 返回固定文本或空文本；
- 只有 user_input 未提供且 source 已被接受时才调用；
- adapter 不负责 scanner、LLM、render、publication 或结果展示。

这是真实的 in-process adapter，不是第二套业务 interface。ReportRunner.run 仍是唯一应用层测试 seam。

#### CLI presentation

CLI 直接把 outcome 映射为现有语义提示、warning、Markdown preview 与退出码，不新增 Presenter Protocol 或事件总线。

兼容要求：

- 成功为退出码 0；
- expected failure 为退出码 1；
- KeyboardInterrupt 为退出码 130；
- scanner summary、partial warning、生成失败、保存失败与预览仍可见；
- transient spinner 的精确帧、持续时间和阶段切换时点不作为兼容 contract。

### Publication Semantics

#### save=false

必须满足：

- 仍可读取昨日计划或 source=db 日报；
- 仍可执行 scanner，并允许 Rust scanner DB 的既有 run/cache 写入；
- 仍调用 LLM；
- 仍在内存渲染 Markdown；
- 不调用任何报告 SQLite save 方法；
- 不创建、迁移或修改报告 SQLite；
- 不创建 Markdown 输出目录；
- 不写 Markdown 文件；
- outcome publication 的两个 state 都是 not_attempted。

#### save=true

保持现有 publication 顺序：

1. 报告 SQLite commit；
2. Markdown filesystem write。

首轮不实现跨 SQLite 与 filesystem 的原子事务、补偿或回滚：

| 失败点 | 结果 |
|---|---|
| SQLite commit 失败 | 返回 sqlite_publish failure；Markdown 不尝试 |
| Markdown write 失败 | 返回 markdown_publish failure；明确 SQLite 已 committed |
| 两者成功 | success receipt |

不得静默重试，也不得把 Markdown 失败伪装成完全成功。

### Error Model

稳定 error_code 集合：

| error_code | phase | retryable 规则 | 后续阶段 |
|---|---|---|---|
| INVALID_WEEK | request | false | 全部停止 |
| INVALID_MONTH | request | false | 全部停止 |
| EMPTY_DAILY_INPUT | request | false | LLM、render、publish 停止 |
| NO_SOURCE_REPORTS | source | false | LLM、render、publish 停止 |
| SCANNER_FAILED | source | 继承 scanner diagnostic | LLM factory 之前停止 |
| SOURCE_READ_FAILED | source | 根据底层 I/O 分类 | generation 之前停止 |
| LLM_GENERATION_FAILED | generation | 继承 provider 分类 | render、publish 停止 |
| MARKDOWN_RENDER_FAILED | render | 通常 false | publish 停止 |
| SQLITE_PUBLISH_FAILED | sqlite_publish | 根据底层 I/O 分类 | Markdown 不尝试 |
| MARKDOWN_PUBLISH_FAILED | markdown_publish | 根据底层 I/O 分类 | 返回 SQLite 已提交状态 |

规则：

- deterministic validation error 一律 retryable=false；
- scanner error 尽量继承原始 retryable；
- LLM provider error 由 adapter 分类；
- 本地 I/O 只有明确可重试时才标 retryable=true；
- message 面向用户，cause metadata 面向审计；
- 不把 token、路径中的敏感信息或 provider credential 放入 error。

### Ordering Invariants

以下顺序属于 contract，必须测试：

1. source gate 在 model factory 之前。
2. daily 昨日计划读取在 source gate 之后、LLM 之前。
3. daily deferred input 在 source gate 和昨日计划之后。
4. report_date_override 在 LLM 之后、render 之前。
5. render 在任何 publication 之前。
6. SQLite commit 在 Markdown write 之前。
7. scanner warning 保持原顺序。
8. DB reports 与 missing_days 按日期升序。
9. scanner file_context 不在 Python 重排。

以下顺序不是 contract：

- Rich spinner 的刷新帧；
- 普通日志与 console 文案在同一阶段内的微小先后；
- 本地测试临时文件的目录枚举顺序。

## User Stories

1. 作为 daily CLI 用户，我希望继续使用相同命令生成日报，而不需要理解 scanner、LLM 或存储实现。
2. 作为 daily CLI 用户，我希望显式输入存在时不再被交互询问。
3. 作为 daily CLI 用户，我希望未提供输入时仍能在 source 被接受后输入工作内容。
4. 作为 daily CLI 用户，我希望空输入清晰失败，且不会产生 LLM 调用或报告文件。
5. 作为 daily CLI 用户，我希望 --date 只改变报告日期，不改变扫描范围。
6. 作为 weekly 用户，我希望 source=db 只聚合已有日报，不启动 scanner。
7. 作为 weekly 用户，我希望 source=scan 扫描完整 ISO 周并生成报告。
8. 作为 monthly 用户，我希望 source=db 只聚合目标月已有日报并报告缺失工作日。
9. 作为 monthly 用户，我希望 source=scan 扫描完整自然月并生成报告。
10. 作为 period report 用户，我希望补充输入沿用当前格式，不改变 LLM prompt 语义。
11. 作为用户，我希望 scanner partial 时看到所有 warning，但仍能得到报告。
12. 作为用户，我希望 scanner error 时立即停止，避免无证据的 LLM 调用。
13. 作为用户，我希望 source=db 没有日报时收到明确错误，而不是空报告。
14. 作为用户，我希望 --no-save 仍显示 Markdown 预览，但不发布报告数据库记录或文件。
15. 作为用户，我希望保存成功时仍先写报告 SQLite、再写 Markdown。
16. 作为用户，我希望 Markdown 写入失败时知道 SQLite 已经提交，避免重复生成造成误判。
17. 作为用户，我希望成功、普通失败和 Ctrl+C 的退出码保持 0、1、130。
18. 作为运维人员，我希望每个预期失败都有稳定 error_code、phase 和 retryable。
19. 作为运维人员，我希望 scanner 的原始诊断和 run id 不被通用异常文本吞掉。
20. 作为开发者，我希望修改 scanner acceptance policy 时只改一个 locality。
21. 作为开发者，我希望修改 --no-save 或 publication 顺序时只改一个 locality。
22. 作为开发者，我希望 request 类型在构造时排除 daily/db 等非法组合。
23. 作为开发者，我希望 period 默认值可用固定 as_of_date 重放，不依赖测试机器当天日期。
24. 作为测试人员，我希望通过 ReportRunner interface 验证完整行为，而不是 mock CLI 内的每个内部对象。
25. 作为测试人员，我希望 SQLite、Jinja 和 filesystem 使用真实临时 substitute，减少虚假 mock 通过。
26. 作为测试人员，我希望只有 true external LLM 使用 mock adapter。
27. 作为 scanner 维护者，我希望 ReportRunner 不读取 scanner DB，也不复制 Rust JSON contract。
28. 作为 parser 维护者，我希望 ReportRunner 不改变 backend、worker lane、timeout 或 Hybrid Office fallback。
29. 作为维护者，我希望最终实现直接替换三套重复编排，不保留长期兼容 shim。
30. 作为未来扩展者，我希望真正增加新报告 mode 时扩展封闭 union，而不是依赖无类型字符串插件。

## Implementation Decisions

### ID-1：一个最高测试 seam

所有报告生成路径必须通过 ReportRunner.run。CLI 报告命令不得再直接编排 scheduler、store、model、renderer 和 publisher。

原因：这是 leverage 最高的 seam。一个 interface 测试可以覆盖三种 caller 共用的 source、generation、render、publication 与 failure policy。

### ID-2：封闭 request union

daily、weekly、monthly 使用独立 request variant。daily 类型不暴露 source；weekly/monthly 明确要求 db 或 scan。

原因：非法状态在 interface 层消失，优于一个带大量可空字段的通用 DTO。

### ID-3：显式 as-of date

每个 request 携带 as_of_date，不新增 Clock port。

原因：日期是本次运行的领域输入；值输入比可替换 clock implementation 更小、更确定。

### ID-4：单一 pipeline、私有 recipes

mode 差异可以通过私有 recipe 或 discriminated match 表达，但不得形成三个公开 handler interface。

原因：共享排序只维护一次，同时保留真实领域差异。

### ID-5：typed outcome 替代 bool

预期失败返回 typed failure；只有 implementation invariant 破坏才抛编程错误。

原因：CLI、日志和测试可以依赖稳定 phase/error_code，而不是解析 message。

### ID-6：LLM lazy factory

ReportRunner 只在 source 和 daily input gate 通过后构造 model adapter。

原因：scanner error、无源数据和空输入必须零次初始化 true external dependency。

### ID-7：复用 ContextScheduler

不新增第二个 scanner port，不复制 scanner contract。

原因：现有 interface 已是合适 seam；再包装会形成 shallow pass-through。

### ID-8：真实本地 substitute

SQLite、Jinja renderer 与 filesystem 在测试中使用真实临时资源，不新增公开 repository 或 writer interface。

原因：它们快速、确定、可隔离，mock 反而会隐藏 schema、模板与路径问题。

### ID-9：延迟 daily input adapter

仅在 request 未含 user_input 时调用一个单方法 input adapter。

原因：保留 source gate 后再读取交互输入的行为，又不把 console 放入业务 request。

### ID-10：不建立 presenter event bus

CLI 根据最终 outcome 展示语义信息；精确 spinner 时间点不进入 contract。

原因：当前只有一个 presentation，不存在第二个真实 adapter；事件总线会扩大 interface 和测试表面。

### ID-11：render 与 write 分离

Markdown 先在内存渲染，再按 save policy 决定是否写文件。

原因：--no-save 仍需预览，且 render failure 必须发生在任何 publication 之前。

### ID-12：保留非原子发布顺序

首轮继续 SQLite 后 Markdown，不引入跨载体事务、补偿或重试。

原因：保持现有行为与范围；通过 publication receipt 消除状态歧义。

### ID-13：严格 no-save

save=false 禁止报告数据库和 Markdown 的持久化副作用，但不禁用 Rust scanner 自身的 run/cache 持久化。

原因：报告发布与 scanner 可观测性/cache 属于不同所有权。

### ID-14：直接替换，不保留双轨

迁移完成后删除三套旧编排和重复测试，不长期保留旧路径转发到新路径的 compatibility shim。

原因：双轨会降低 locality，并继续允许新增代码绕开最高 seam。

### ID-15：Rust contract 完全冻结

本次 implementation 不修改 Rust CLI JSON contract、scanner profile/cache identity、scanner DB schema、parser backend、worker lane、timeout 与 fallback policy。

原因：这些属于另一个 module 的行为与性能范围。

## Testing Decisions

### 最高测试面

ReportRunner.run 是主要测试 surface。

module 测试应从 request 到 outcome 观察：

- source 请求与接受规则；
- LLM 调用次数；
- report 类型；
- Markdown 内容；
- report SQLite 状态；
- Markdown 文件状态；
- warning/error/publication receipt；
- 失败后的未执行阶段。

除 true external LLM 与现有 scheduler engine seam 外，不应通过替换所有内部对象来证明行为。

### Test doubles and local substitutes

| 依赖 | 测试策略 | 原因 |
|---|---|---|
| LLM | recording/failing mock adapter，经 lazy factory 注入 | true external，需零网络且可断言构造次数 |
| ContextScheduler | 真实 scheduler + deterministic fake engine | 复用现有 seam，保留 request/result contract |
| Report SQLite | 临时目录中的真实 SQLite | 验证真实 schema、排序与 commit |
| Jinja renderer | 真实模板 | 验证 template/schema 兼容 |
| Markdown filesystem | 临时目录 | 验证真实路径、目录创建和文件内容 |
| Daily input | fixed/empty input adapter | 保留延迟调用行为 |
| 日期 | request 中固定 as_of_date | 无 clock mock |
| CLI presentation | stub ReportRunner，捕获 console | CLI 只测映射和展示 |

### ReportRunner acceptance tests

1. Daily success：
   - scan window 为 as_of_date-1 至 as_of_date；
   - scanner 一次；
   - LLM 一次；
   - report、Markdown 与 publication receipt 正确。

2. Daily report date override：
   - scan window不变；
   - LLM input 日期语义不变；
   - override 仅影响最终 report、render 与保存键。

3. Daily explicit input：
   - input adapter 零调用；
   - 显式输入进入 generation request。

4. Daily deferred input：
   - scanner 被接受、昨日计划读取后才调用 input adapter；
   - fixed input 进入 generation request。

5. Daily empty input：
   - 返回 EMPTY_DAILY_INPUT；
   - model factory 构造次数为零；
   - render 与 publication 均未发生。

6. Scanner error：
   - 返回 SCANNER_FAILED；
   - 保留 diagnostic 与 run id；
   - input adapter、model factory、render、report DB save、Markdown write 均为零调用。

7. Scanner partial：
   - warning 原序保留；
   - status=partial；
   - LLM、render 与可选 publication 继续。

8. Weekly DB success：
   - scanner 零调用；
   - reports 和 missing_days 升序；
   - DB evidence 正确。

9. Monthly DB success：
   - scanner 零调用；
   - 自然月边界准确；
   - reports 和 missing_days 升序。

10. DB no reports：
    - weekly/monthly 参数化；
    - 返回 NO_SOURCE_REPORTS；
    - model factory、render、publication 均不发生。

11. DB read failure：
    - 返回 SOURCE_READ_FAILED；
    - generation 不发生；
    - 不泄露敏感路径或配置。

12. Weekly scan success：
    - ISO 周起止日期准确；
    - report DB 不作为 generation source；
    - scanner 一次。

13. Monthly scan success：
    - 自然月起止日期准确；
    - report DB 不作为 generation source；
    - scanner 一次。

14. Supplement format：
    - weekly/monthly、db/scan 参数化；
    - 使用既有分隔符；
    - 空白 supplement 不追加分隔块。

15. LLM failure：
    - 返回 LLM_GENERATION_FAILED；
    - render、SQLite 与 Markdown 均未发生；
    - ReportRunner 不自行重试。

16. Wrong typed report：
    - adapter 返回与 mode 不匹配的 report；
    - 触发 implementation invariant error，而不是生成错误模板。

17. Render failure：
    - 返回 MARKDOWN_RENDER_FAILED；
    - 两个 publication state 都为 not_attempted。

18. save=false：
    - daily/weekly/monthly 参数化；
    - 内存 Markdown 存在；
    - 报告 DB 无新增、无 schema 初始化或迁移；
    - 输出目录和 Markdown 文件不存在；
    - publication state 均为 not_attempted。

19. SQLite publication failure：
    - 返回 SQLITE_PUBLISH_FAILED；
    - Markdown 文件不存在；
    - markdown_state=not_attempted。

20. Markdown publication failure：
    - 返回 MARKDOWN_PUBLISH_FAILED；
    - 真实 SQLite 中报告已存在；
    - sqlite_state=committed、markdown_state=failed。

21. Full publication success：
    - 真实 SQLite 中报告存在；
    - Markdown 文件存在且等于 outcome.markdown；
    - receipt 两个 state 都成功。

22. Scanner single-call invariant：
    - 所有 scan mode 每次运行恰好一次 context build；
    - ReportRunner 不进行 fallback scan 或 retry。

23. DB zero-scanner invariant：
    - weekly/monthly source=db 的 scheduler 调用次数为零。

24. Scanner DB ownership：
    - 合成文件集成测试证明 ReportRunner 只消费 ContextBuildResult；
    - 不直接打开 scanner DB。

25. Sorting：
    - DB reports 和 missing_days 升序；
    - scanner file_context 保持 engine 返回顺序。

### CLI characterization tests

CLI 测试只替换 ReportRunner，并保留：

1. daily、weekly、monthly 参数解析。
2. weekly/monthly 的 db 和 scan source。
3. --no-save 到 save=false 的映射。
4. --date 只进入 daily report_date_override。
5. week/month 缺省值由 request as_of_date 解析。
6. success 退出 0。
7. typed failure 退出 1。
8. KeyboardInterrupt 退出 130。
9. scanner summary 与 partial warning 可见。
10. Markdown preview 可见。
11. publication failure 能区分 SQLite 与 Markdown。
12. 报告命令不再直接构造 scheduler、store、renderer 或 LLM client。

现有 CLI 测试中，凡是只为断言这些内部对象调用顺序而存在的重复 fake，应在对应 ReportRunner interface 测试建立后删除，而不是保留两套测试。

### Rust and contract regression

虽然本次不改 Rust，仍需运行现有回归门禁以证明 application refactor 没有改变请求或契约：

- Rust workspace tests；
- scanner contract fixture/schema tests；
- Hybrid Office fallback routing/fault tests；
- 一次匹配 profile 的 Cold scanner run 与 warm run smoke；
- 确认 diff 不含 Rust contract、profile、fallback 或 scanner DB schema 功能修改。

不要求本次产生 scanner 性能提升，也不以微小 benchmark 波动作为验收条件。性能相关 invariant 是“无额外 scanner/LLM 调用”和“DB source 零 scanner 调用”。

### Verification commands

在 Windows 项目环境中至少运行：

1. .\.venv\Scripts\python.exe -m pytest tests/ -v
2. cargo test --manifest-path rust/Cargo.toml --workspace --locked
3. .\.venv\Scripts\python.exe main.py doctor --strict
4. git diff --check

如果运行环境没有有效 LLM credential，不影响 deterministic model adapter 的自动化测试；不得用 live provider 作为单元测试前提。

## Acceptance Criteria

本规格只有在以下条件全部满足时才算实现完成：

- [ ] 三种报告命令都通过 ReportRunner.run。
- [ ] request 是 daily/weekly/monthly 的封闭联合类型。
- [ ] outcome 是 success/failure 的 typed 联合类型。
- [ ] CLI 不再直接编排 ContextScheduler、SQLiteStore、ReportGenerator 和 LLMClient。
- [ ] scanner error 在 LLM factory 构造前终止。
- [ ] scanner partial 继续生成并保留 warning。
- [ ] weekly/monthly source=db 零 scanner 调用。
- [ ] daily source 在类型层面固定为 scan。
- [ ] period 默认值只依赖 request.as_of_date。
- [ ] daily date override 不改变 scan window。
- [ ] save=false 不修改报告 SQLite、不创建 Markdown 输出目录或文件。
- [ ] save=false 不错误禁止 Rust scanner DB 的既有 run/cache 写入。
- [ ] save=true 保持 SQLite 后 Markdown。
- [ ] SQLite 成功、Markdown 失败有明确 receipt。
- [ ] 所有预期失败提供 error_code、message、retryable 和 phase。
- [ ] 主要 behavior 测试位于 ReportRunner interface。
- [ ] 本地依赖使用真实临时 substitute。
- [ ] LLM 使用 deterministic mock adapter 且保持 lazy construction。
- [ ] 重复 CLI internal-order 测试已删除或降为薄 mapping 测试。
- [ ] Python 全量测试、Rust workspace 测试与 doctor --strict 通过。
- [ ] Rust CLI JSON contract、scanner DB ownership 和 Hybrid Office fallback 不变。
- [ ] 最终代码没有旧/新双轨 compatibility shim。

## Out of Scope

以下内容明确不在本次 implementation：

1. Rust scanner active-run lifecycle 重构。
2. ParserScheduler worker lifecycle 重构。
3. Python context runtime 的进一步拆分。
4. Rust CLI JSON contract 变更。
5. scanner DB schema、所有权或 Python 直读。
6. scanner profile normalization 或 cache identity 变更。
7. parser backend、worker lane、timeout 或 fallback 策略变更。
8. Hybrid Office fallback 开关或顺序变更。
9. 新 LLM provider、模型选择或 prompt 重设计。
10. DailyReportData、WeeklyReportData、MonthlyReportData schema 重设计。
11. Jinja 模板版式与报告章节内容重设计。
12. 报告数据库 schema migration。
13. SQLite 与 filesystem 的跨载体原子事务、补偿或自动重试。
14. Web、GUI、daemon、queue 或后台 scheduler。
15. 通用 workflow engine、plugin registry、middleware 或 dependency container。
16. list、doctor 等非报告命令重构。
17. scanner benchmark 优化或新的性能预算。
18. 为假设中的云数据库、对象存储或第二种 presentation 预建 adapter。

## Further Notes

### 建议实施顺序

1. 先补 characterization tests，冻结现有 CLI 参数、退出码、source policy、日期范围、warning、preview 与 publication 顺序。
2. 建立 request/outcome 类型和空的 ReportRunner interface 测试。
3. 先迁移 daily，证明 scanner gate、deferred input、date override 和 no-save。
4. 迁移 weekly 的 db/scan 两条 source recipe。
5. 迁移 monthly 的 db/scan 两条 source recipe。
6. 将 CLI 收窄为 request mapping、daily input adapter、outcome presentation 与 exit code。
7. 删除旧三套 orchestration 和重复 internal-order 测试。
8. 运行 Python、Rust、doctor 与 cold/warm contract smoke。

每一步应保持主测试集可运行；最终不保留双轨。

### 建议提交切分

1. Freeze report-run behavior with characterization tests
2. Add typed report-run interface and outcome model
3. Move daily flow behind report-run seam
4. Move weekly and monthly flows behind report-run seam
5. Simplify CLI and replace shallow tests
6. Verify report-run architecture and contracts

提交切分是实施建议，不要求为了切分保留临时兼容层。

### 主要风险与缓解

| 风险 | 缓解 |
|---|---|
| CLI 文案或退出码漂移 | 先建立 characterization tests，CLI 只映射 typed outcome |
| scanner error 后仍初始化 LLM | lazy factory + 构造次数断言 |
| no-save 仍因构造 store/renderer 产生写副作用 | read path side-effect-free，writer 延迟构造，检查 DB/目录不存在 |
| Markdown 失败被误报成功 | publication receipt 显式记录 SQLite 与 Markdown state |
| 过度抽象成本高于收益 | 禁止 registry、middleware、通用 pipeline 和假 adapter |
| 新旧路径长期并存 | 最终验收要求删除旧 orchestration 和重复测试 |
| 无意改变 Rust scanner 行为 | Rust diff freeze + workspace/contract/fallback 回归 |
| 测试全 mock 导致虚假通过 | SQLite、Jinja、filesystem 使用真实临时 substitute |

### Freeze decision

本规格完成实现前，建议冻结另外三个架构候选方向：Rust active-run lifecycle、ParserScheduler worker lifecycle、Python context runtime。它们有独立价值，但同时推进会扩大验证矩阵，削弱本次最高 seam 的收益归因。

本规格实现并验证后，再根据新的复杂度与性能证据决定是否解冻下一项；不以“架构更纯”为理由自动继续重构。

### Issue tracking

本文件是本地 implementation source of truth。未获得明确授权前，不发布到外部 issue tracker，也不创建远程 issue 或 PR。
