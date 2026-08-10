# Repository Guidelines

## Project Structure & Module Organization
- `main.py` is the CLI entry point.
- `src/` holds the Python application shell: `core/` (config, health checks, logging, LLM client), `models/` (Pydantic contracts), `services/` (Rust-core orchestration, report storage and generation), `workers/` (crash-isolated Python document worker), and `utils/` (text helpers).
- `templates/` contains LLM prompts and Jinja2 report templates.
- `config/` stores YAML settings. Track only `settings.example.yaml`; keep `settings.windows.yaml` and `.secrets.yaml` local.
- `data/` stores the SQLite database and generated Markdown reports; `logs/` holds runtime logs.
- `rust/` is the Cargo workspace for the production scanner/context core: `scanner_core/`, `scanner_contract/`, `scanner_cli/`, the reusable `discovery/` library, and the crash-isolated `office_parser/` worker.
- `scripts/benchmark_scanner.py` runs the real scanner path and writes parser/discovery benchmark evidence.
- `tests/` contains pytest test modules.

## Build, Test, and Development Commands
- The only supported runtime is CPython `3.13.13`; `.python-version` is the source of truth for development, release, and deployment.
- `uv sync` installs dependencies from `pyproject.toml` / `uv.lock` into the project `.venv`.
- `uv run python main.py doctor --strict` validates the production config, templates, dependencies, workers, and Rust core.
- `uv run python main.py daily -i "..."` generates a daily report. Examples: add `--no-save` or `--date 2026-02-05` as needed.
- `uv run python main.py weekly --source db` aggregates weekly reports; `--source scan` scans files instead.
- `uv run python main.py monthly --source db` aggregates monthly reports.
- `uv run pytest` runs the Python test suite.
- `cargo test --manifest-path rust/Cargo.toml --workspace --locked` verifies the Rust workspace.
- `cargo build --manifest-path rust/Cargo.toml --workspace --release --locked` builds both required production executables.
- `uv run python scripts/benchmark_scanner.py --start-date YYYY-MM-DD --end-date YYYY-MM-DD --json-out .artifacts\scanner.json --markdown-out .artifacts\scanner.md` captures scanner performance and backend evidence.

## Coding Style & Naming Conventions
- Python style: follow PEP 8 with 4-space indentation.
- Use `snake_case` for functions/variables, `PascalCase` for classes, and `UPPER_SNAKE_CASE` for constants.
- Keep modules focused; prefer adding new services under `src/services/` and schemas under `src/models/`.

### Architecture Naming Conventions
- `Scheduler`: 表示一次运行中的策略编排，不表示后台常驻任务、定时器或 daemon。
- `Planner`: 表示生成计划、预算或 profile，不执行重 I/O、解析或持久化副作用。
- `Classifier`: 表示对文件、上下文或 workload 做确定性分类，不做 I/O 副作用。
- `Parser`: 表示具体文件内容解析，输入文件和预算，输出可审计的解析结果。
- `Backend`: 只用于 parser backend 命名，例如 `light_text_v1`、`office_v1`、`pdf_text_v1`，不用于业务服务命名。
- `Compressor`: 表示确定性上下文压缩，不调用 LLM，不重新解析文件。
- `Store` / `IndexStore`: 表示 SQLite 持久化、索引和 cache 载体，不承载业务流程判断。
- `Run`: 表示一次 CLI 命令执行、一次 scanner run 或一次 context 构建。
- `Decision`: 表示单个文件在一次 context run 中的策略选择和审计原因。
- 产品讨论中可以把 scanner、parser、compressor 理解成“能力代理”，但代码命名优先使用 `Scheduler`、`Planner`、`Parser`、`Compressor`、`Store` 等确定性术语，避免把职责边界写虚。
- Scanner backend changes must keep `parser_backend` separate from `worker_lane`; benchmark summaries should prove both the parser that produced content and the lane where it executed.
- `.xlsx` reports the bounded `rust_xlsx_bounded_v1` backend; `.docx` / `.pptx` report `rust_office_oxide_v1`. PDF and explicitly enabled legacy document formats use the Python document worker. Keep `office_fallback_after_timeout` false unless the user explicitly chooses slower timeout fallback.
- Backend, fallback, timeout, and parser-budget changes must participate in the Rust-normalized scanner profile and cache identity to prevent stale parse-cache reuse.

## Testing Guidelines
- Framework: pytest.
- Test files are named `tests/test_*.py`.
- Add tests for new parsing logic, schema changes, and report templates; run `pytest tests/ -v` before submitting.

## Commit & Pull Request Guidelines
- Git history is minimal; no strict convention yet. Use short, imperative commit messages (e.g., "Add weekly scan limits").
- PRs should include a clear summary, linked issues (if any), and notes on config changes or template updates.

## Configuration & Security
- Do not commit API keys. Use `config/.secrets.yaml`, `DEEPSEEK_API_KEY`, or `OPENAI_API_KEY`.
- Update `config/settings.windows.yaml` for path, model, and scanner limits changes.
 - LLM backend is selectable via `llm.provider` (`deepseek` or `openai`). For DeepSeek, set `DEEPSEEK_API_KEY` (or `api.deepseek_api_key` in `config/.secrets.yaml`). For OpenAI, set `OPENAI_API_KEY` (or `api.openai_api_key` in `config/.secrets.yaml`) and use a model that supports JSON schema output (e.g., `gpt-4o-mini`).
