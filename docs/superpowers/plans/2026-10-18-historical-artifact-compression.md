# 历史测量与验收产物透明压缩实施计划

> **面向 AI 代理的工作者：** 必需子技能：使用 subagent-driven-development 或 executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）跟踪进度。

**目标：** 在保留历史产物内容、路径、哈希与可读取性的前提下，对两处历史解析输出实施可暂停、可续跑、可回退的 NTFS 透明压缩；第一阶段争取释放约 8 GB。

**架构：** 增加一个离线维护脚本，仅使用 Python 标准库和 Windows 原生文件系统能力。冻结清单约束操作范围；每次执行重新核对文件身份、内容和压缩状态，独立保存本次运行证据，不用历史日志驱动业务状态。不调用日报、采集、解析或模型流程。

**技术栈：** Windows x64 / NTFS、Python 3.13 标准库、NTFS LZNT1、SHA-256、unittest。产品仍是无需 Python 的 Rust 发布包；此脚本只用于维护。

**规格：** 本文件的[设计基线](#设计基线)固化本会话已讨论的第一阶段方案；实现者须先读本节和[当前运行状态](../../current-state.md)、[仓库规则](../../../AGENTS.md)。用户审阅后已追加“可以通过子代理执行”，授权按本计划实现、审查并执行限定范围内的压缩。

**状态：** 已完成（2026-10-18 18:39，Asia/Shanghai）。78 项维护测试、逐任务及最终独立审查通过；16,641 个真实文件压缩和全量 verify 通过，释放 8,877,867,378 字节。正式包、任务、报告与保留 Worker 对比通过，生产模型调用 0。18:39 验收阶段未进行 Git 写入；20:32 用户追加提交、合并与发布授权。[实施结果与证据](2026-10-18-historical-artifact-compression-results.md)。

## 设计基线

规划依据为 `main` 的 `e21a1cd7ce7889c822a4283b6392d87bb1ff05ad`。原始项目根路径为 `D:\python_program\ai_daily_report`，下文称 `SourceRoot`。实施代码可以在隔离工作树中，称 `CodeRoot`；两者必须显式区分，不能根据当前目录猜测历史数据位置。

第一阶段的两个允许目录，均相对 `SourceRoot`：

| 允许目录 | 2026-10-18 只读盘点文件数 | 压缩前文件数据占用 |
| --- | ---: | ---: |
| `.artifacts/rust-migration/archive-benchmark-final-20261017-04/incremental/data/archive/extractions` | 9,129 | 7,328,993,694 字节 |
| `.artifacts/rust-migration/archive-benchmark-final-20261017/incremental/data/archive/extractions` | 7,512 | 6,950,832,416 字节 |
| 合计 | 16,641 | 14,279,826,110 字节 |

- 只允许这两个目录内名称恰为 `body.md`、`units.jsonl`、`units.idx` 的普通文件。目录压缩继承属性不改。
- 此次盘点的上述文件均未压缩、硬链接数为 1；执行前必须重新检查。多硬链接、重解析点、加密、已压缩和扫描期间变化的文件跳过并登记。
- 临时独立副本的 36 个分层样本：77,390,875 字节压至 30,711,792 字节，节省 60.32%；SHA-256、随机读取及解压回退均通过。分层外推节省 8,770,820,278 字节，仅为估计，不是验收保证。
- 项目 2026-10-18 14:53 的去重文件数据占用为 73,912,816,777 字节；若外推成立，约降至 65.1 GB。文件系统元数据与全卷其他活动另计。
- 原始抽样记录位于本机临时目录 `C:\Users\lcf\AppData\Local\Temp\artifact-compression-probe-20261018-151600\result.json`；36 份临时副本已清理。临时记录若已不存在，保留本节数字作为历史依据，先在新的临时副本上重做验证。

## 全局约束

- 规划阶段仅新增计划及导航链接；后续实现、测试与真实操作以本文件状态和已批准的限定范围为准。
- 不修改 `data/`、`.releases/`、`.venv/`、配置、模板、旧 Python/native Worker、数据库及 SQLite 的 WAL/SHM 文件。
- 不删除、搬移或重新生成历史证据；不改旧性能数字、原始日志、失败样本或保存的配置。
- 不调用模型、重新扫描工作目录、重新解析文件、触发/禁用/改写正式任务，不新增自动跟进任务。
- NTFS 设置沿用 [现有实现](../../../rust/report_app/src/archive/storage.rs)的 `FSCTL_SET_COMPRESSION` / LZNT1；保留逻辑字节与随机访问能力，不引入新的压缩包格式。
- 单个压缩进程；哈希流式读取缓冲为 1 MiB。试批及字节批次阈值均为 268,435,456 字节；另在每 100 个文件边界检查任务上下文。单个大文件不截断。
- 上海时区的下一次正式任务触发前 300 秒暂停；任务正在运行、查询失败或任一任务缺失时也暂停。查询的是实际三任务，不能写死某一天的 16:00。
- 完成工具隔离测试后才允许进入真实目录。操作必须使用显式 `SourceRoot` 和审阅过的清单；不得扩大到整个 `.artifacts/`。
- Python 使用会话 `load_workspace_dependencies` 返回的绝对路径，不安装依赖、不替换原生产 `.venv`。独立环境可使用已有 Python 3.13。
- 不为本脚本重建 Rust 工作区或发布包，避免重新生成刚清理的构建缓存。Rust 源码、配置、模板如超出本计划被改动，先停止并重新评估范围与验证。
- 本计划不授权 commit、合并、推送或部署；各任务保留可审查 diff。后续已有明确提交授权时才做相应本地提交。

## 审查重点（Review Focus）

1. 路径逃逸、父目录重解析点或共享硬链接：不得使范围外文件获得压缩属性；对应 `ScopeTests`。
2. 清单生成后文件被替换、修改、占用，或压缩后哈希异常：停止后续写操作，不自动覆盖或重建；对应 `IntegrityTests`。
3. 文件已成功压缩但记录尚未落盘时中断：续跑须核对同一文件及哈希，不能依赖旧日志末行或重复盲写；对应 `ResumeTests`。
4. 恰好进入触发前 300 秒、任务运行中或任务查询失败：暂停而非继续，不注册或改写任务；对应 `WindowTests`。
5. 回退重复执行、清单被改动、原本已压缩的文件：仅处理本清单候选，不影响跳过项或外部文件；对应 `RollbackTests`。

## 文件与职责

| 文件 | 未来动作与职责 |
| --- | --- |
| `scripts/compress_historical_artifacts.py` | 新增；清单、四个 CLI 子命令、Win32 文件操作、逐文件核验、任务窗口与运行证据。保持单文件，不建立通用维护框架 |
| `tests/test_historical_artifact_compression.py` | 新增；标准库 unittest，所有变更在系统临时目录的合成文件中完成 |
| `scripts/README.md` | 实现完成后新增维护脚本入口，更新实际数量，并说明不是生产运行依赖 |
| 本计划 | 实施后登记真实结果、未完成项和证据路径；不要把预计值改写成实测 |
| `docs/plans/README.md` | 本次仅增加本计划导航；实施后才更新其状态 |

## 操作与证据契约

CLI 固定为 `plan`、`apply`、`verify`、`rollback`。不提供任意 glob、任意目标目录或忽略哈希的开关。

```text
plan --project-root <SourceRoot> --evidence-dir <全新证据目录>
apply --manifest <manifest.json>
verify --manifest <manifest.json>
rollback --manifest <manifest.json>
```

- 证据目录必须是全新目录，位于 `SourceRoot/.artifacts/rust-migration/historical-artifact-compression/` 下，且不在目标目录中。现有目录不得覆盖。
- `manifest.json` 发布后不可修改；另存其原始 UTF-8 字节的 `manifest.sha256`。记录 schema_version=1、plan_id、UTC 时间、绝对 project_root、两个允许相对目录、eligible、skipped 和总数。
- eligible 每项字段固定为 `relative_path`、`file_id`（volume/file index）、`nlink`、`logical_bytes`、`mtime_ns`、`attributes`、`was_compressed`、`sha256`、`allocated_before_bytes`；文件路径使用 `/` 分隔，拒绝绝对路径、`..`、反斜杠和 ADS。候选的 `nlink=1`、`was_compressed=false`。
- `plan` 只读源文件。使用真实 `os.stat` 或锁定句柄查询文件身份；不能使用 Windows 上可能返回零身份的 `DirEntry.stat`。允许目录及其祖先出现重解析点直接拒绝；目标目录内部的子节点重解析点不进入并登记。清单完成前也遵守任务窗口，未完成清单不可进入 apply。
- `apply/rollback` 每次创建新的 `runs/<run_id>/`，保存事件 JSONL、任务回读与最终 summary；不覆盖、截断或修复旧日志。续跑不解析旧日志来判断文件是否完成，而以清单、当前压缩属性和重新校验的哈希为准。
- 修改前以同一 R/W 句柄锁定文件，允许共享读、不允许并发写/删除；重新核对最终路径、身份、链接数、大小、修改时间和哈希。续跑及 verify 比较属性时，只允许压缩位相对原始清单不同，其余属性保持原样；否则已完成文件会被误判为变更。文件被占用记为失败并停止；清单生成阶段的跳过项不作为执行候选。
- 在同一锁定句柄上设置压缩、再次流式哈希，并比对开始位置、1/3 位置、末尾最多 4,096 字节的读取结果。文件长度与身份不变；修改时间如被属性操作改变，在该句柄上恢复后复核。
- 第一段累计达到 256 MiB 后形成试批小结；哈希、读取、元数据全部通过且任务窗口允许，才继续剩余批次。小数据集不足 256 MiB 时全体就是试批。
- 输出状态为 `completed/paused/failed`，进程退出码分别为 `0/1/2`。summary 包含 `plan_sha256`、`run_id`、`files_verified`、`files_changed`、`files_already`、`logical_bytes`、`allocated_before_bytes`、`allocated_after_bytes`、有符号 `reclaimed_bytes` 和 `pause_reason`。
- verify 必须核对全部 eligible 项，报告内容/元数据匹配以及当前压缩、未压缩数量。完成 apply 后候选须全部通过且处于压缩状态；rollback 后须恢复原始压缩状态。
- 多硬链接一律跳过，不尝试自行去重或改造链接。回退前按当前分配空间与原始逻辑大小计算所需余量；保留 1 GiB 额外余量，不足则不开始解压。

## 任务 1：只读清单与范围保护

**文件：** 创建维护脚本、创建上述 unittest 文件；此任务只交付 `plan` 子命令。

本任务开始前须已有工具实现授权并选定执行方式，再建立或选用隔离工作区、固定 `CodeRoot` 与 `SourceRoot`。本次写计划不建立工作树；实现时也不安装依赖或重建 Rust。

**接口：**

```python
def scan_manifest(project_root: Path) -> dict: ...
def write_manifest(manifest: dict, evidence_dir: Path) -> Path: ...
def load_manifest(manifest_path: Path) -> dict: ...
def read_task_window(now_utc: datetime) -> tuple[bool, datetime]: ...
```

read_task_window 返回“是否有任务运行”和“最近下一次触发 UTC”；异常表示上下文不可核验。只读查询 `AI Daily Report`、`AI Daily Report Collect`、`AI Daily Report Health`，三者均须存在。PowerShell 查询通过结构化进程参数传入，stdout/stderr 使用临时文件，避免依赖捕获管道及本地化文本；不安装模块。plan 在开始和批次间检查窗口，不允许时返回 paused，不发布可执行清单。

- [x] **步骤 1：编写失败测试。** 测试文件内的 `Fixture` 用 `TemporaryDirectory` 构造两个允许目录、三种文件、范围外文件及证据目录；不复制真实业务材料。Fixture 默认替换任务查询为“空闲、距当前 UTC 一小时后触发”，WindowTests 单独覆盖时钟和任务边界；所有测试均不查询真实任务。至少钉住下列断言：

```python
# ScopeTests.test_plan_has_exact_scope_and_never_mutates_sources
m = tool.scan_manifest(f.root)
self.assertEqual({e['relative_path'] for e in m['eligible']}, f.expected_paths)
self.assertTrue(all(e['nlink'] == 1 and not e['was_compressed'] for e in m['eligible']))
self.assertEqual(f.source_fingerprints(), before)
# ScopeTests.test_distinct_files_have_nonzero_distinct_ids
self.assertNotEqual(ids[0], ids[1]); self.assertTrue(all(ids))
# ScopeTests.test_hardlink_encrypted_and_precompressed_files_are_skipped
self.assertEqual(set(skip_reasons), {'hardlink', 'encrypted', 'already_compressed'})
# ScopeTests.test_reparse_parent_and_path_escape_are_rejected
self.assertEqual(error_code, 'SCOPE_REJECTED')
# ScopeTests.test_modified_manifest_is_rejected
self.assertEqual(error_code, 'MANIFEST_CHANGED')
```

测试重解析点使用无需管理员权限的临时目录 junction；junction 创建失败在本机验收中视为未完成覆盖，不能计作通过。加密场景用文件属性查询替身，硬链接用临时目录真实 `os.link`。

- [x] **步骤 2：运行该测试文件确认失败。** 命令见[本地测试命令](#本地测试命令)；预期新脚本/入口尚不存在，测试失败。
- [x] **步骤 3：实现精确范围、稳定文件身份、1 MiB 流式哈希及不可覆盖的清单。** skip reason 固定为 `hardlink/encrypted/already_compressed/reparse/unstable`；初始枚举遇到重解析点不进入，手工构造的逃逸清单必须被 load_manifest 拒绝。损坏或失败样本只保留实际字节，不解析、不修复。
- [x] **步骤 4：再次运行测试，要求 ScopeTests 全部通过。** 检查源文件内容、大小、mtime 与属性指纹未变；未知清单字段、版本和哈希错误均拒绝。
- [x] **步骤 5：检查本任务 diff。** 仅脚本与合成测试；按已获得的提交授权处理，否则保持未提交。

## 任务 2：压缩、核验、任务窗口与安全续跑

**文件：** 修改维护脚本、同一 unittest 文件。

**接口：**

```python
def set_compression(file: BinaryIO, enabled: bool) -> None: ...
def apply_manifest(manifest_path: Path) -> dict: ...
def verify_manifest(manifest_path: Path) -> dict: ...
```

任务窗口接口沿用任务 1；本任务扩展到写操作前的检查、300 秒暂停与续跑，不改变任务查询的只读性质。

- [x] **步骤 1：补失败测试，覆盖下列行为。** native 测试必须在当前 Windows NTFS 上真正执行，任务状态/时钟用 unittest.mock 替身，不操作实际任务。

```python
# NativeTests.test_compression_preserves_hash_seek_identity_and_mtime
self.assertEqual(after.sha256, before.sha256)
self.assertEqual((after.file_id, after.logical_bytes, after.mtime_ns),
                 (before.file_id, before.logical_bytes, before.mtime_ns))
self.assertEqual(random_blocks_after, random_blocks_before)
self.assertLess(after.allocated_bytes, before.allocated_bytes)  # 用 2 MiB 重复文本
# IntegrityTests.test_changed_file_and_corrupt_compressor_stop_before_next_file
self.assertIn(result['error_code'], {'SOURCE_CHANGED', 'HASH_MISMATCH'})
self.assertEqual(f.second_file_fingerprint(), second_before)
# IntegrityTests.test_writer_lock_is_not_bypassed
self.assertEqual(result['error_code'], 'SOURCE_LOCKED')
# ResumeTests.test_resume_after_native_success_before_receipt
self.assertEqual(result['status'], 'completed')
self.assertGreaterEqual(result['files_already'], 1)
self.assertEqual(writes_to_completed_file, 0)
# WindowTests.test_cutoff_running_missing_and_failed_probe_pause
self.assertEqual(status_at_300_seconds, 'paused')
self.assertEqual(status_at_301_seconds, 'completed')
self.assertTrue(all(s == 'paused' for s in running_missing_failed_statuses))
self.assertEqual(blocked_set_compression_calls, 0)
```

- [x] **步骤 2：运行新增测试确认失败。** 不以副本抽样试验替代对新脚本的测试。
- [x] **步骤 3：实现原生文件句柄、压缩与逐文件核验。** 复用项目现有 Win32 控制码、LZNT1 格式；ctypes 句柄必须按指针宽度声明。同一句柄完成保护、压缩、读取和时间恢复。实现前后哈希异常、文件被替换/新增硬链接等路径，错误码固定为 `SOURCE_CHANGED/SOURCE_LOCKED/HASH_MISMATCH/IO_FAILED`。
- [x] **步骤 4：实现批次与续跑。** 每批开始重新查询三任务，文件开始前还要检查已知暂停时间；`now >= next_trigger - 300s` 即暂停。已压缩且与清单身份/内容匹配的文件记 `files_already`，不重复设置属性；旧日志不作为可信状态。清单不匹配、原生异常或哈希失败停止后续文件。
- [x] **步骤 5：运行全部本地测试，检查真压缩、零后续误写、暂停退出码 1、失败退出码 2。** `files_changed` 只计本次实际操作，禁止将跳过项当作压缩成功。
- [x] **步骤 6：检查 diff。** 不改日报、归档 schema、生产配置、Rust 代码或依赖锁文件；提交沿用已获得的授权。

## 任务 3：可重复回退与完整 CLI 交付

**文件：** 修改维护脚本、同一 unittest 文件、`scripts/README.md`。

**接口：**

```python
def rollback_manifest(manifest_path: Path) -> dict: ...
def main(argv: list[str] | None = None) -> int: ...
```

- [x] **步骤 1：补失败测试。**

```python
# RollbackTests.test_only_manifest_candidates_are_restored_and_repeat_is_noop
self.assertEqual(restored_hashes, manifest_hashes)
self.assertEqual(restored_compression_states, original_states)
self.assertEqual(outside_and_skipped_fingerprints, outside_before)
self.assertEqual(second_rollback['files_changed'], 0)
# RollbackTests.test_modified_manifest_changed_file_or_low_space_prevents_writes
self.assertEqual(set_compression_calls, 0)
# CliTests.test_plan_apply_verify_rollback_exit_codes_and_json
self.assertEqual(exit_codes, [0, 0, 0, 0])
self.assertEqual(model_or_scheduler_mutation_calls, 0)
```

- [x] **步骤 2：运行测试确认失败。** 回退测试必须包含原本已压缩但被排除的文件、范围外文件和清单被改动的情况。
- [x] **步骤 3：实现只针对本清单的解压。** 先校验清单与当前目标身份/内容，再在同一锁定句柄取消本次压缩。恢复元数据后再核验；不覆盖文件内容、不尝试“修复”哈希错误。回退遵守同一任务窗口及空间余量，支持同一命令重复执行。
- [x] **步骤 4：完成四个子命令与标准 JSON 输出。** 操作证据与目标目录分离，错误和跳过原因完整保留；重复 apply、verify、rollback 均须有明确输出。
- [x] **步骤 5：运行完整新测试文件。** 真压缩/解压、中文与空格路径、不可压缩随机字节、零长度文件、文件替换、多硬链接和任务窗口均覆盖。当前 Windows 本机的 native 测试不允许被平台 skip 后计作通过。
- [x] **步骤 6：更新脚本导航与使用说明，核对数量和链接。** 明确此维护脚本不加入 Rust 生产发布包，现有 Python 回退环境不改。检查 diff 后按已有提交授权处理。

## 任务 4：真实目录只读清单、试批与第一阶段验收

**交付物：** 新的压缩证据目录；本计划实施结果记录。此任务须在用户审阅计划、选择执行方式并授权实际压缩后开始。

- [x] **步骤 1：核对既有 SourceRoot/CodeRoot，确认任务 1–3 的测试和独立代码审查已通过。** 实现工作区已在任务 1 前准备；真实操作仍只指向明确的 SourceRoot，不以 CodeRoot 推断数据位置。
- [x] **步骤 2：回读正式任务、程序包、运行进程和磁盘余量，确认没有基准/采集正在使用目标目录。** 保存三任务定义与状态摘要，运行正式包清单校验，记录业务报告哈希；不手动触发任务。
- [x] **步骤 3：执行 plan 生成新清单。** 对照 16,641 文件 / 14,279,826,110 字节的规划基线及两处允许目录；有差异先登记并停在写操作之前，由用户审阅更新后的范围，不能用旧数字强行放行。
- [x] **步骤 4：按该清单执行 apply。** 工具先完成约 256 MiB 试批并保存结果，验证通过后在同一次授权范围内继续；任务窗口不允许时退出 paused，之后用同一清单续跑，不重建或放宽清单。
- [x] **步骤 5：执行全量 verify 并重新测量项目占用。** 候选全部哈希、逻辑大小、身份、mtime 相符，随机读取通过，未触碰项保持原样。记录实际净节省、真实压缩状态和全部跳过/失败项；未完成不能宣称全量成功。
- [x] **步骤 6：运行正式包离线 doctor 与 health，回读任务并对照步骤 2。** health 可仍有原历史失败 warning；要求没有本次操作引入的新错误。严格自检不加 `--check-model`。
- [x] **步骤 7：保存清单、清单哈希、每次运行日志、全量核验、前后空间、任务回读与回退命令。** 记录旧基准数据仍是原测量时的性能；以后复测须标注压缩属性。只在任务全部完成后更新本计划及索引状态。

## 本地测试命令

实施者先通过 `load_workspace_dependencies` 确认 `$Python` 的绝对路径；`$CodeRoot` 指包含新增脚本与测试的工作区。以下是未来验证命令，当前编写计划时不运行：

```powershell
& $Python -m unittest discover -s "$CodeRoot/tests" -p 'test_historical_artifact_compression.py' -v
# 预期：上述所有测试通过；本机 Windows 原生测试无 skip。
& $Python "$CodeRoot/scripts/compress_historical_artifacts.py" --help
# 预期：仅列出 plan、apply、verify、rollback，退出码 0。
```

不执行全仓 pytest；它们主要是迁移保留测试。本工具未改 Rust 时不执行 cargo 构建/测试。实现过程若改变原定范围，先重新决定对应验证。

## 真实操作命令与回退入口

以下命令必须在任务 4 获准后执行。`$Python` 和 `$CodeRoot` 使用实施时确认值，程序包必须先从实际任务回读；这里的版本只是规划基线。

```powershell
$SourceRoot = 'D:\python_program\ai_daily_report'
$Tool = Join-Path $CodeRoot 'scripts/compress_historical_artifacts.py'
$Evidence = Join-Path $SourceRoot ('.artifacts/rust-migration/historical-artifact-compression/' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
& $Python $Tool plan --project-root $SourceRoot --evidence-dir $Evidence
# 审阅清单与基线，确认本次实际操作仍被授权后：
$Manifest = Join-Path $Evidence 'manifest.json'
& $Python $Tool apply --manifest $Manifest
# paused 时，在任务空闲并有足够余量的窗口重用同一命令；不注册续跑任务。
& $Python $Tool verify --manifest $Manifest
```

获准回退后使用同一清单：

```powershell
& $Python $Tool rollback --manifest $Manifest
& $Python $Tool verify --manifest $Manifest
# 预期：内容核验通过，eligible 项恢复原压缩状态，跳过项与外部文件未变。
```

正式程序检查使用回读出的路径：

```powershell
& "$Bundle/ai-daily-report.exe" --config-dir "$SourceRoot/config-rust" doctor --strict | Out-Host
& "$Bundle/ai-daily-report.exe" --config-dir "$SourceRoot/config-rust" health --format json | Out-Host
```

凭据只在离线自检进程中使用已存在的用户环境；不得输出凭据。记录原生退出码，health 的已有 warning=1 不能被当成新故障，也不能掩盖新增检查异常。

## 执行交接与计划自检

- 规划交接后，用户已授权子代理执行；任务 1–4 及最终审查修复均完成，真实结果和证据见[实施结果](2026-10-18-historical-artifact-compression-results.md)。
- 工具 78 项隔离测试及独立审查通过；真实试批、apply、全量 verify、空间重测和正式运行对比分别保存。18:39 验收时隔离分支尚未提交，后续 Git 集成已由用户另行授权；实际 rollback 未执行。
- 本计划只覆盖第一阶段两处解析输出。其他历史日志/文本、数据库、原件、共享硬链接及程序包不自动进入下一阶段。

| 自检项 | 计划覆盖 |
| --- | --- |
| 两处范围、类型、估算与实测区分 | 设计基线、任务 1、任务 4 |
| 文件身份、重解析点、多硬链接与清单篡改 | ScopeTests、任务 1 |
| 哈希、随机访问、mtime、锁与失败停止 | IntegrityTests / NativeTests、任务 2 |
| 300 秒窗口、任务查询异常与安全续跑 | WindowTests / ResumeTests、任务 2 |
| 精确回退、重复执行与余量 | RollbackTests、任务 3 |
| 真实任务、正式包、零模型与证据保留 | 全局约束、任务 4 |
| 规划、实现与实际操作的授权及进度区分 | 状态、任务 4 入口、执行交接 |
