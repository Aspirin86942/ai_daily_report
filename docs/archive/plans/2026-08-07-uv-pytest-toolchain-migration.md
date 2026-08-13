# Python 3.13.13 与 pytest 工具链最终状态

根 `.python-version`、`requires-python`、release wheel 和部署验证统一为精确
CPython 3.13.13。pytest 配置位于 `pyproject.toml`，测试通过仓库 `.venv` 运行。

Rust release 夹具只需要：

- `rust/target/release/ai_daily_scanner_native.dll`；
- `rust/target/release/ai-daily-office-parser.exe`。

原生 wheel 由固定 maturin 构建工具生成，不启用 abi3。Python 业务运行依赖不因
原生 scanner 增加新的包。

```powershell
.\.venv\Scripts\python.exe -m pytest tests -v
$env:PYO3_PYTHON = (Resolve-Path '.\.venv\Scripts\python.exe').Path
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
```

测试临时数据只使用 `tmp_path` 或显式临时目录，不读取本机配置、业务文件或真实
数据库。
