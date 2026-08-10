# Weekly Seven-Section Template Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Upgrade weekly reports from the legacy 4-section structure to the confirmed 7-section structure across schema, prompt, Markdown rendering, SQLite persistence, and compatibility loading.

**Architecture:** Keep the weekly generation flow unchanged at the CLI boundary, but replace the weekly data contract with seven explicit text sections. Use `raw_json` as the compatibility source of truth for legacy weekly rows, migrate the `weekly_reports` table in-place when the old schema is detected, and keep rendering logic consuming only the new model.

**Tech Stack:** Python 3.10+, Pydantic v2, SQLite3, Jinja2, pytest, PowerShell, conda (`test` environment)

---

## File Structure

### Existing files to modify

- `src/models/schemas.py`
  - Replace the weekly report model fields with the seven-section contract.
- `templates/weekly_prompt.md`
  - Update the weekly JSON output contract and field-specific generation guidance.
- `templates/weekly_template.md`
  - Render the seven numbered weekly sections in the user-approved order.
- `src/services/sqlite_store.py`
  - Add weekly schema detection, table migration, legacy JSON compatibility loading, and seven-section persistence.
- `tests/test_schemas.py`
  - Lock the new `WeeklyReportData` shape.
- `tests/test_report_gen.py`
  - Lock the new weekly Markdown headings and prompt schema fields.
- `tests/test_sqlite_store.py`
  - Lock the new weekly table shape, save/load behavior, and legacy migration behavior.
- `tests/test_main.py`
  - Keep the weekly CLI smoke test aligned with the new weekly model constructor.

### No new production modules expected

- This feature should fit in the existing weekly report files.
- Do not refactor unrelated daily/monthly code in this rollout.

---

### Task 1: Lock the Seven-Section Weekly Contract in Tests and Implement It

**Files:**
- Modify: `tests/test_schemas.py`
- Modify: `tests/test_report_gen.py`
- Modify: `tests/test_main.py`
- Modify: `src/models/schemas.py`
- Modify: `templates/weekly_prompt.md`
- Modify: `templates/weekly_template.md`

- [ ] **Step 1: Write the failing tests for the new weekly schema, prompt, rendering, and CLI stub**

Update `tests/test_schemas.py` so `WeeklyReportData` only accepts the new fields:

```python
def test_weekly_report_data():
    report = WeeklyReportData(
        week_label="2026-W14",
        date_range="2026-03-30 ~ 2026-04-05",
        completed_work="本周完成了日报模板梳理、周报字段收敛和生成链路范围确认。",
        self_growth="在梳理周报结构时，更清楚地理解了输出模板、数据契约和历史兼容之间的边界。",
        improvement_actions="需要继续收敛提示词中的空泛表达，并通过测试锁住新字段，避免模板与数据再次错位。",
        work_summary="本周工作重点从表层模板替换下沉到数据契约统一，主要风险已经明确。",
        next_plan="下周完成 SQLite 迁移、周报模板联调和回归验证。",
        support_needed="当前需要团队确认历史周报兼容口径，以便迁移后输出风格保持一致。",
        other_notes="建议后续评估月报是否也采用同类结构化段落收口。",
    )
    assert report.week_label == "2026-W14"
    assert report.self_growth.startswith("在梳理周报结构时")
```

Update `tests/test_report_gen.py` weekly rendering test and weekly prompt smoke test:

```python
def test_render_weekly_markdown():
    gen = ReportGenerator()

    report = WeeklyReportData(
        week_label="2026-W05",
        date_range="2026-01-27 ~ 2026-02-02",
        completed_work="本周完成项目A底稿复核、项目B资料清单初审，并同步补齐问题跟踪记录。",
        self_growth="在多项目并行推进中，进一步提升了材料收口和进度归纳的能力。",
        improvement_actions="需要继续提高补件跟踪的前置提醒力度，并把问题分类口径写得更稳定。",
        work_summary="本周工作以阶段性收口和新项目准备并行为主，关键依赖已经基本识别清楚。",
        next_plan="下周继续推进项目B补件核验，并启动项目C现场访谈准备。",
        support_needed="如项目B补件继续延迟，需要业务接口人协助加快资料回传。",
        other_notes="建议后续统一各项目问题跟踪表的字段命名。",
    )

    markdown = gen.render_weekly_markdown(report)
    assert "## 1、本周主要工作完成情况（例行及专项，体现关键数据及进度情况）" in markdown
    assert "## 2、自我成长（结合本周工作开展，收获了什么成长和领悟）" in markdown
    assert "## 3、有待改善的地方及相关措施" in markdown
    assert "## 4、本周工作小结（整体回顾分析本周的工作，直面问题进行思考，有评有论）" in markdown
    assert "## 5、下周主要工作目标及计划（提炼关键目标，做好任务计划管理）" in markdown
    assert "## 6、需要的协助与支持（针对工作过程中出现的困难等）" in markdown
    assert "## 7、其他（建议等）" in markdown
    assert "统一各项目问题跟踪表的字段命名" in markdown
```

```python
def fake_call(prompt: str, response_model: type[object]) -> WeeklyReportData:
    captured["prompt"] = prompt
    return WeeklyReportData(
        week_label="2026-W14",
        date_range="2026-03-30 ~ 2026-04-05",
        completed_work="本周主要工作完成情况。",
        self_growth="本周自我成长。",
        improvement_actions="本周待改善事项。",
        work_summary="本周工作小结。",
        next_plan="下周工作计划。",
        support_needed="需要的协助支持。",
        other_notes="其他补充。",
    )

assert '"self_growth"' in prompt
assert '"improvement_actions"' in prompt
assert '"support_needed"' in prompt
assert '"other_notes"' in prompt
```

Update the weekly smoke stub in `tests/test_main.py`:

```python
return WeeklyReportData(
    week_label=f"{year}-W{week:02d}",
    date_range="2026-01-26 ~ 2026-02-01",
    completed_work="完成周报",
    self_growth="形成了新的周报结构认知",
    improvement_actions="需要继续完善历史兼容策略",
    work_summary="周报总结",
    next_plan="下周计划",
    support_needed="需要确认迁移口径",
    other_notes="无其他补充",
)
```

- [ ] **Step 2: Run the targeted tests to verify they fail on the old weekly contract**

Run:

```powershell
conda run -n test python -m pytest tests/test_schemas.py tests/test_report_gen.py tests/test_main.py -v
```

Expected:

- `tests/test_schemas.py::test_weekly_report_data` fails with weekly field validation errors.
- `tests/test_report_gen.py::test_render_weekly_markdown` fails because the numbered headings are missing.
- `tests/test_main.py::test_generate_weekly_report_db_uses_sqlite_store` fails because the old `WeeklyReportData` constructor does not accept the new fields.

- [ ] **Step 3: Implement the new weekly schema, prompt, and Markdown template**

Replace the weekly model in `src/models/schemas.py`:

```python
class WeeklyReportData(BaseModel):
    model_config = ConfigDict(extra="forbid")
    week_label: str = Field(description="ISO 周标签，例如 2026-W14")
    date_range: str = Field(description="日期范围，例如 2026-03-30 ~ 2026-04-05")
    completed_work: str = Field(description="本周主要工作完成情况，使用自然段")
    self_growth: str = Field(description="自我成长，使用自然段")
    improvement_actions: str = Field(description="有待改善的地方及相关措施，使用自然段")
    work_summary: str = Field(description="本周工作小结，使用自然段")
    next_plan: str = Field(description="下周主要工作目标及计划，使用自然段")
    support_needed: str = Field(description="需要的协助与支持，使用自然段")
    other_notes: str = Field(description="其他补充说明，使用自然段")
```

Replace `templates/weekly_template.md` with:

```markdown
# {{ report.week_label }} 审计周报

> {{ report.date_range }}

## 1、本周主要工作完成情况（例行及专项，体现关键数据及进度情况）
{{ report.completed_work }}

## 2、自我成长（结合本周工作开展，收获了什么成长和领悟）
{{ report.self_growth }}

## 3、有待改善的地方及相关措施
{{ report.improvement_actions }}

## 4、本周工作小结（整体回顾分析本周的工作，直面问题进行思考，有评有论）
{{ report.work_summary }}

## 5、下周主要工作目标及计划（提炼关键目标，做好任务计划管理）
{{ report.next_plan }}

## 6、需要的协助与支持（针对工作过程中出现的困难等）
{{ report.support_needed }}

## 7、其他（建议等）
{{ report.other_notes }}

---
*报告生成时间: {{ now }}*
```

Replace the weekly output contract in `templates/weekly_prompt.md` with:

```markdown
你是专业的内审日报整理助手。请根据本周日报上下文和文件证据，生成结构化周报。

## 输出要求
1. 严格输出 JSON，且必须符合 WeeklyReportData schema。
2. 只输出以下字段：`week_label`、`date_range`、`completed_work`、`self_growth`、`improvement_actions`、`work_summary`、`next_plan`、`support_needed`、`other_notes`。
3. 所有正文字段都必须使用自然段，不要使用项目符号、编号列表或表格。
4. 不要臆造量化结论。只有输入中明确出现的数字、比例、日期或数量，才可以保留在输出里。
5. 缺失信息不要编造，可用审慎表述概括，但不得补造不存在的成果、风险、统计口径、协助需求或建议。
6. 风格保持客观、简洁、专业，聚焦已经发生的工作事实与后续安排。

## JSON Schema
{schema}

## 上下文信息

### 周标签
{week_label}

### 数据来源
{data_source}

### 日报聚合上下文
{reports_summary}

### 缺失日报日期
{missing_days}

### 文件证据
{file_context}

## 生成指南
1. `week_label` 直接使用提供值。
2. `date_range` 填写本周一至周日的日期范围，格式为 `YYYY-MM-DD ~ YYYY-MM-DD`。
3. `completed_work` 说明本周主要工作完成情况，兼顾例行工作与专项工作，能保留的关键数据和进度要保留，但不能编造。
4. `self_growth` 说明本周在推进工作过程中形成的成长、收获或领悟，不写空泛口号。
5. `improvement_actions` 说明当前仍有待改善的地方，以及已经明确的改进措施，不只写问题，不只写态度。
6. `work_summary` 对本周工作做整体回顾分析，直面问题，有评有论，但保持客观。
7. `next_plan` 提炼下周主要工作目标及计划，突出关键目标和任务安排。
8. `support_needed` 说明当前需要的协助与支持；如果输入中没有明确信息，可用审慎表述说明当前暂无明确协助需求。
9. `other_notes` 填写其他补充建议；如果确无补充，可用简短保守表述说明暂无其他事项。

请直接输出 JSON，不要包含任何额外说明。
```

- [ ] **Step 4: Re-run the targeted tests to verify the new weekly contract passes**

Run:

```powershell
conda run -n test python -m pytest tests/test_schemas.py tests/test_report_gen.py tests/test_main.py -v
```

Expected:

- `test_weekly_report_data` passes with the new model fields.
- `test_render_weekly_markdown` passes with all 7 numbered headings.
- `test_generate_weekly_report_prompt_contains_period_context_and_metadata` passes and captures the new schema field names.
- `test_generate_weekly_report_db_uses_sqlite_store` passes with the updated weekly stub.

- [ ] **Step 5: Commit the contract and rendering change**

Run:

```powershell
git add tests/test_schemas.py tests/test_report_gen.py tests/test_main.py src/models/schemas.py templates/weekly_prompt.md templates/weekly_template.md
git commit -m "feat: upgrade weekly report to seven-section template"
```

Expected:

- One commit containing only weekly schema, prompt, template, and smoke-test alignment changes.

---

### Task 2: Add Weekly SQLite Migration and Legacy Weekly Compatibility

**Files:**
- Modify: `tests/test_sqlite_store.py`
- Modify: `src/services/sqlite_store.py`

- [ ] **Step 1: Write the failing persistence and migration tests**

Update the expected weekly column set in `tests/test_sqlite_store.py`:

```python
assert weekly_columns == {
    "week_label",
    "date_range",
    "completed_work",
    "self_growth",
    "improvement_actions",
    "work_summary",
    "next_plan",
    "support_needed",
    "other_notes",
    "raw_json",
    "created_at",
    "updated_at",
}
```

Update the weekly save/load fixture:

```python
weekly = WeeklyReportData(
    week_label="2026-W06",
    date_range="2026-02-02 ~ 2026-02-08",
    completed_work="完成了日报和周报七段式结构设计。",
    self_growth="在统一模板和存储结构时，对历史兼容风险有了更清晰的判断。",
    improvement_actions="需要继续验证旧周报迁移后的第一段合并效果。",
    work_summary="整体工作集中在去掉旧 overview 并补齐新增段落。",
    next_plan="下周继续修改 SQLite 存储和兼容读取逻辑。",
    support_needed="需要确认历史周报空白字段保持为空字符串是否可接受。",
    other_notes="建议迁移后保留 raw_json 作为审计追溯依据。",
)
```

Add a legacy migration test:

```python
def test_sqlite_store_migrates_legacy_weekly_reports(tmp_path):
    db_path = tmp_path / "reports.sqlite3"
    legacy_payload = {
        "week_label": "2026-W05",
        "date_range": "2026-01-26 ~ 2026-02-01",
        "overview": "本周围绕周报模板调整推进。",
        "completed_work": "完成了字段梳理和模板确认。",
        "work_summary": "本周完成了旧结构摸底。",
        "next_plan": "下周继续处理 SQLite 兼容。",
    }

    with sqlite3.connect(db_path) as conn:
        conn.execute(
            '''
            CREATE TABLE weekly_reports (
                week_label TEXT PRIMARY KEY,
                date_range TEXT NOT NULL,
                overview TEXT NOT NULL,
                completed_work TEXT NOT NULL,
                work_summary TEXT NOT NULL,
                next_plan TEXT NOT NULL,
                raw_json TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            '''
        )
        conn.execute(
            """
            INSERT INTO weekly_reports (
                week_label, date_range, overview, completed_work, work_summary, next_plan, raw_json
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            """,
            (
                "2026-W05",
                "2026-01-26 ~ 2026-02-01",
                "本周围绕周报模板调整推进。",
                "完成了字段梳理和模板确认。",
                "本周完成了旧结构摸底。",
                "下周继续处理 SQLite 兼容。",
                json.dumps(legacy_payload, ensure_ascii=False),
            ),
        )
        conn.commit()

    store = SQLiteStore(db_path=db_path)
    loaded = store.get_weekly_report("2026-W05")

    assert loaded is not None
    assert loaded.completed_work == "本周围绕周报模板调整推进。\n\n完成了字段梳理和模板确认。"
    assert loaded.self_growth == ""
    assert loaded.improvement_actions == ""
    assert loaded.support_needed == ""
    assert loaded.other_notes == ""
```

- [ ] **Step 2: Run the SQLite tests to verify they fail on the old weekly schema**

Run:

```powershell
conda run -n test python -m pytest tests/test_sqlite_store.py -v
```

Expected:

- The weekly column assertion fails because `overview` still exists and the new columns do not.
- The weekly save/load test fails because the model constructor requires the new fields.
- The legacy migration test fails because no weekly schema migration exists yet.

- [ ] **Step 3: Implement weekly table migration, legacy JSON mapping, and seven-section persistence**

Add the new weekly table definition and migration hooks in `src/services/sqlite_store.py`:

```python
CREATE TABLE IF NOT EXISTS weekly_reports (
    week_label TEXT PRIMARY KEY,
    date_range TEXT NOT NULL,
    completed_work TEXT NOT NULL,
    self_growth TEXT NOT NULL,
    improvement_actions TEXT NOT NULL,
    work_summary TEXT NOT NULL,
    next_plan TEXT NOT NULL,
    support_needed TEXT NOT NULL,
    other_notes TEXT NOT NULL,
    raw_json TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
```

Add a legacy payload loader and a text join helper:

```python
    @staticmethod
    def _join_non_empty_parts(*parts: str) -> str:
        return "\n\n".join(part for part in parts if part)

    def _load_weekly_report_from_raw_json(
        self,
        raw_json: str,
        fallback_week_label: str = "",
        fallback_date_range: str = "",
    ) -> WeeklyReportData:
        payload = json.loads(raw_json)

        if "self_growth" in payload:
            return WeeklyReportData(**payload)

        # 为什么要把 overview 合并到 completed_work：
        # 因为旧总览通常承载本周推进背景，放进第一段最接近用户确认的新模板语义。
        return WeeklyReportData(
            week_label=payload.get("week_label", fallback_week_label),
            date_range=payload.get("date_range", fallback_date_range),
            completed_work=self._join_non_empty_parts(
                payload.get("overview", ""),
                payload.get("completed_work", ""),
            ),
            self_growth="",
            improvement_actions="",
            work_summary=payload.get("work_summary", ""),
            next_plan=payload.get("next_plan", ""),
            support_needed="",
            other_notes="",
        )
```

Add weekly schema detection and migration in `_init_schema()`:

```python
            weekly_columns = {
                row[1] for row in conn.execute("PRAGMA table_info(weekly_reports)").fetchall()
            }
            expected_weekly_columns = {
                "week_label",
                "date_range",
                "completed_work",
                "self_growth",
                "improvement_actions",
                "work_summary",
                "next_plan",
                "support_needed",
                "other_notes",
                "raw_json",
                "created_at",
                "updated_at",
            }
            if weekly_columns and weekly_columns != expected_weekly_columns:
                self._migrate_weekly_reports_table(conn)
```

Add the migration routine:

```python
    def _migrate_weekly_reports_table(self, conn: sqlite3.Connection) -> None:
        rows = conn.execute(
            """
            SELECT week_label, date_range, raw_json, created_at, updated_at
            FROM weekly_reports
            """
        ).fetchall()

        conn.execute("ALTER TABLE weekly_reports RENAME TO weekly_reports_legacy")
        conn.execute(
            """
            CREATE TABLE weekly_reports (
                week_label TEXT PRIMARY KEY,
                date_range TEXT NOT NULL,
                completed_work TEXT NOT NULL,
                self_growth TEXT NOT NULL,
                improvement_actions TEXT NOT NULL,
                work_summary TEXT NOT NULL,
                next_plan TEXT NOT NULL,
                support_needed TEXT NOT NULL,
                other_notes TEXT NOT NULL,
                raw_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            """
        )

        for row in rows:
            report = self._load_weekly_report_from_raw_json(
                row["raw_json"],
                fallback_week_label=row["week_label"],
                fallback_date_range=row["date_range"],
            )
            payload = report.model_dump()
            conn.execute(
                """
                INSERT INTO weekly_reports (
                    week_label, date_range, completed_work, self_growth,
                    improvement_actions, work_summary, next_plan,
                    support_needed, other_notes, raw_json, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    report.week_label,
                    report.date_range,
                    report.completed_work,
                    report.self_growth,
                    report.improvement_actions,
                    report.work_summary,
                    report.next_plan,
                    report.support_needed,
                    report.other_notes,
                    self._to_json(payload),
                    row["created_at"],
                    row["updated_at"],
                ),
            )

        conn.execute("DROP TABLE weekly_reports_legacy")
        conn.commit()
```

Update `save_weekly_report()` and `get_weekly_report()` to use the seven-section columns and shared loader:

```python
                INSERT INTO weekly_reports (
                    week_label, date_range, completed_work, self_growth,
                    improvement_actions, work_summary, next_plan,
                    support_needed, other_notes, raw_json, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
```

```python
        try:
            return self._load_weekly_report_from_raw_json(row["raw_json"])
        except Exception as exc:
            logger.warning("Failed to parse weekly report %s: %s", week_label, exc)
            return None
```

- [ ] **Step 4: Re-run the SQLite tests to verify migration and compatibility pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_sqlite_store.py -v
```

Expected:

- The weekly column assertion passes with the seven-section columns.
- Saving and loading a new weekly report passes.
- The legacy weekly table migrates on store init and `get_weekly_report()` returns the merged first section plus blank new sections.

- [ ] **Step 5: Commit the persistence migration**

Run:

```powershell
git add tests/test_sqlite_store.py src/services/sqlite_store.py
git commit -m "feat: migrate weekly storage to seven-section schema"
```

Expected:

- One commit containing only the weekly SQLite migration and compatibility logic plus its tests.

---

### Task 3: Run the Weekly Regression Suite Before Hand-Off

**Files:**
- Test: `tests/test_schemas.py`
- Test: `tests/test_report_gen.py`
- Test: `tests/test_sqlite_store.py`
- Test: `tests/test_main.py`

- [ ] **Step 1: Run the full weekly regression suite**

Run:

```powershell
conda run -n test python -m pytest tests/test_schemas.py tests/test_report_gen.py tests/test_sqlite_store.py tests/test_main.py -v
```

Expected:

- All weekly-related tests pass.
- There are no remaining failures caused by references to `overview` in weekly code paths.

- [ ] **Step 2: Confirm the rendered weekly template headings from production files**

Run:

```powershell
Get-Content .\templates\weekly_template.md
```

Expected:

- The file contains exactly 7 numbered weekly headings.
- The headings match the user-approved wording, including the parenthetical notes.

- [ ] **Step 3: Check the worktree for unintended edits**

Run:

```powershell
git status --short
```

Expected:

- Only the planned weekly report files are modified.
- No unrelated daily, monthly, scanner, or config files are touched.

---

## Self-Review

### Spec coverage

- Seven-section weekly schema: covered by Task 1.
- Updated weekly prompt and Markdown template: covered by Task 1.
- SQLite weekly storage upgrade: covered by Task 2.
- Legacy weekly compatibility mapping: covered by Task 2.
- Regression verification: covered by Task 3.

### Placeholder scan

- No placeholder markers remain in the plan body.
- Every code-changing step includes concrete snippets.
- Every verification step includes exact PowerShell commands and expected outcomes.

### Type consistency

- The same weekly field names are used consistently across model, prompt, template, tests, and SQLite migration:
  - `completed_work`
  - `self_growth`
  - `improvement_actions`
  - `work_summary`
  - `next_plan`
  - `support_needed`
  - `other_notes`
