# Repository Guidelines

## Project Structure & Module Organization
- `main.py` is the CLI entry point.
- `src/` holds core logic: `core/` (config, logging, LLM client), `models/` (Pydantic schemas), `services/` (file scanning, history, report generation), and `utils/` (text helpers).
- `templates/` contains LLM prompts and Jinja2 report templates.
- `config/` stores settings (`settings.toml`) and secrets (`.secrets.toml`, not committed).
- `data/` stores generated JSON databases and Markdown reports; `logs/` holds runtime logs.
- `tests/` contains pytest test modules.

## Build, Test, and Development Commands
- `pip install -r requirements.txt` installs runtime dependencies.
- `python check_config.py` validates config and API setup.
- `python main.py daily -i "..."` generates a daily report (CLI mode). Examples: `python main.py daily --no-save -i "..."`, `python main.py daily --date 2026-02-05 -i "..."`.
- `python main.py weekly --source db` aggregates weekly reports; `--source scan` scans files instead.
- `python main.py monthly --source db` aggregates monthly reports.
- `python -m pytest tests/ -v` runs the test suite.

## Coding Style & Naming Conventions
- Python style: follow PEP 8 with 4-space indentation.
- Use `snake_case` for functions/variables, `PascalCase` for classes, and `UPPER_SNAKE_CASE` for constants.
- Keep modules focused; prefer adding new services under `src/services/` and schemas under `src/models/`.

## Testing Guidelines
- Framework: pytest.
- Test files are named `tests/test_*.py`.
- Add tests for new parsing logic, schema changes, and report templates; run `pytest tests/ -v` before submitting.

## Commit & Pull Request Guidelines
- Git history is minimal; no strict convention yet. Use short, imperative commit messages (e.g., "Add weekly scan limits").
- PRs should include a clear summary, linked issues (if any), and notes on config changes or template updates.

## Configuration & Security
- Do not commit API keys. Use `config/.secrets.toml` or `GOOGLE_API_KEY`.
- Update `config/settings.toml` for path, model, and scanner limits changes.
 - LLM backend is selectable via `llm.provider` (`gemini` or `openai`). For OpenAI, set `OPENAI_API_KEY` (or `api.openai_api_key` in `config/.secrets.toml`) and use a model that supports JSON schema output (e.g., `gpt-4o-mini`).
