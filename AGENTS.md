# Repository Guidelines

## Project Structure & Module Organization
- `main.py` is the CLI entry point.
- `src/` holds core logic: `core/` (config, health checks, logging, LLM client), `models/` (Pydantic schemas), `services/` (file scanning, SQLite storage, report generation), and `utils/` (text helpers).
- `templates/` contains LLM prompts and Jinja2 report templates.
- `config/` stores YAML settings. Track only `settings.example.yaml`; keep `settings.linux.yaml`, `settings.windows.yaml`, and `.secrets.yaml` local.
- `data/` stores the SQLite database and generated Markdown reports; `logs/` holds runtime logs.
- `rust/` contains Rust CLI helpers: `discovery/` for file discovery and `office_parser/` for Office parsing.
- `scripts/benchmark_scanner.py` runs the real scanner path and writes parser/discovery benchmark evidence.
- `tests/` contains pytest test modules.

## Build, Test, and Development Commands
- `pip install -r requirements.txt` installs runtime dependencies.
- `python main.py doctor` validates config, templates, dependencies, and API setup.
- `python main.py daily -i "..."` generates a daily report (CLI mode). Examples: `python main.py daily --no-save -i "..."`, `python main.py daily --date 2026-02-05 -i "..."`.
- `python main.py weekly --source db` aggregates weekly reports; `--source scan` scans files instead.
- `python main.py monthly --source db` aggregates monthly reports.
- `python -m pytest tests/ -v` runs the test suite.
- `cd rust/discovery && cargo test && cargo build --release` verifies the Rust discovery CLI.
- `cd rust/office_parser && cargo test && cargo build --release` verifies the Rust Office parser CLI.
- `python scripts/benchmark_scanner.py --start-date YYYY-MM-DD --end-date YYYY-MM-DD --json-out /tmp/scanner.json --markdown-out /tmp/scanner.md` captures scanner performance and backend evidence.

## Coding Style & Naming Conventions
- Python style: follow PEP 8 with 4-space indentation.
- Use `snake_case` for functions/variables, `PascalCase` for classes, and `UPPER_SNAKE_CASE` for constants.
- Keep modules focused; prefer adding new services under `src/services/` and schemas under `src/models/`.

### Architecture Naming Conventions
- `Scheduler`: 表示一次运行中的策略编排，不表示后台常驻任务、定时器或 daemon。
- `Planner`: 表示生成计划、预算或 profile，不执行重 I/O、解析或持久化副作用。
- `Classifier`: 表示对文件、上下文或 workload 做确定性分类，不做 I/O 副作用。
- `Parser`: 表示具体文件内容解析，输入文件和预算，输出可审计的 `FileContext`。
- `Backend`: 只用于 parser backend 命名，例如 `light_text_v1`、`office_v1`、`pdf_text_v1`，不用于业务服务命名。
- `Compressor`: 表示确定性上下文压缩，不调用 LLM，不重新解析文件。
- `Store` / `IndexStore`: 表示 SQLite 持久化、索引和 cache 载体，不承载业务流程判断。
- `Run`: 表示一次 CLI 命令执行、一次 scanner run 或一次 context 构建。
- `Decision`: 表示单个文件在一次 context run 中的策略选择和审计原因。
- 产品讨论中可以把 scanner、parser、compressor 理解成“能力代理”，但代码命名优先使用 `Scheduler`、`Planner`、`Parser`、`Compressor`、`Store` 等确定性术语，避免把职责边界写虚。
- Scanner backend changes must keep `parser_backend` separate from `worker_lane`; benchmark summaries should prove both the parser that produced content and the lane where it executed.
- Office parser config defaults to `rust_office_oxide_v1` with Python fallback. Inside the Rust CLI, `.xlsx` reports the dedicated bounded preview backend `rust_xlsx_bounded_v1`; `.docx` / `.pptx` continue to report `rust_office_oxide_v1`. Keep `office_fallback_after_timeout` false unless the user explicitly chooses slower timeout fallback.
- Backend, fallback, timeout, and parser budget changes must be included in `ScanPlanner` parser profile keys to avoid stale parse cache reuse.

## Testing Guidelines
- Framework: pytest.
- Test files are named `tests/test_*.py`.
- Add tests for new parsing logic, schema changes, and report templates; run `pytest tests/ -v` before submitting.

## Commit & Pull Request Guidelines
- Git history is minimal; no strict convention yet. Use short, imperative commit messages (e.g., "Add weekly scan limits").
- PRs should include a clear summary, linked issues (if any), and notes on config changes or template updates.

## Configuration & Security
- Do not commit API keys. Use `config/.secrets.yaml`, `DEEPSEEK_API_KEY`, or `OPENAI_API_KEY`.
- Update the current OS local settings file (`config/settings.linux.yaml` on Linux, `config/settings.windows.yaml` on Windows) for path, model, and scanner limits changes.
 - LLM backend is selectable via `llm.provider` (`deepseek` or `openai`). For DeepSeek, set `DEEPSEEK_API_KEY` (or `api.deepseek_api_key` in `config/.secrets.yaml`). For OpenAI, set `OPENAI_API_KEY` (or `api.openai_api_key` in `config/.secrets.yaml`) and use a model that supports JSON schema output (e.g., `gpt-4o-mini`).
