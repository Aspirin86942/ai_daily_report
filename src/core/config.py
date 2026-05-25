"""配置管理模块"""

import os
import platform
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any, Dict
from dynaconf import Dynaconf

DEFAULT_OFFICE_PARSER_BACKEND = "rust_office_oxide_v1"
DEFAULT_RUST_OFFICE_PARSER_BIN = (
    "rust/office_parser/target/release/ai-daily-office-parser"
)
DEFAULT_OFFICE_FALLBACK_ORDER = [
    "python_office_v1",
    "python_sharepoint_text_v1",
]


class Config:
    """全局配置管理器 (单例模式)"""

    _instance = None
    _settings = None

    def __new__(cls):
        if cls._instance is None:
            cls._instance = super().__new__(cls)
            cls._instance._initialize()
        return cls._instance

    def _initialize(self):
        """初始化配置"""
        # 获取项目根目录
        root_dir = Path(__file__).parent.parent.parent
        config_dir = root_dir / "config"

        self._settings = self._build_settings(config_dir)

    @staticmethod
    def _settings_file_name(system_name: str | None = None) -> str:
        """按运行系统选择本机配置文件名。"""
        normalized = (system_name or platform.system()).strip().lower()
        if normalized.startswith("win"):
            return "settings.windows.yaml"
        return "settings.linux.yaml"

    @classmethod
    def _settings_files(
        cls,
        config_dir: Path,
        system_name: str | None = None,
    ) -> list[str]:
        """返回 Dynaconf 的配置文件读取顺序。"""
        return [
            str(config_dir / cls._settings_file_name(system_name)),
            str(config_dir / ".secrets.yaml"),
        ]

    @classmethod
    def _build_settings(
        cls,
        config_dir: Path,
        system_name: str | None = None,
    ) -> Dynaconf:
        """构建配置对象。

        非敏感配置按系统拆分，敏感配置最后加载，确保本机密钥不会写进示例文件。
        """
        return Dynaconf(
            envvar_prefix="DAILY_REPORT",
            settings_files=cls._settings_files(config_dir, system_name),
            environments=False,
            load_dotenv=True,
        )

    @staticmethod
    def _to_builtin_value(value: Any) -> Any:
        """递归转成原生容器，避免 Dynaconf 容器在 Windows spawn 下无法 pickle。"""
        if isinstance(value, Mapping):
            return {
                str(key): Config._to_builtin_value(item)
                for key, item in value.items()
            }
        if isinstance(value, Sequence) and not isinstance(value, (str, bytes, bytearray)):
            return [Config._to_builtin_value(item) for item in value]
        return value

    @property
    def work_dir(self) -> Path:
        """工作目录"""
        return Path(self._settings.paths.work_dir)

    @property
    def data_dir(self) -> Path:
        """数据目录"""
        root_dir = Path(__file__).parent.parent.parent
        return root_dir / self._settings.paths.data_dir

    @property
    def reports_dir(self) -> Path:
        """报告目录"""
        root_dir = Path(__file__).parent.parent.parent
        return root_dir / self._settings.paths.reports_dir

    @property
    def db_dir(self) -> Path:
        """数据库目录"""
        root_dir = Path(__file__).parent.parent.parent
        return root_dir / self._settings.paths.db_dir

    @property
    def llm_config(self) -> Dict[str, Any]:
        """LLM 配置"""
        return {
            "model_id": self._settings.llm.model_id,
            "temperature": self._settings.llm.temperature,
            "max_tokens": self._settings.llm.max_tokens,
            "max_retries": self._settings.llm.max_retries,
        }

    @property
    def scanner_config(self) -> Dict[str, Any]:
        """扫描器配置"""
        cfg: Dict[str, Any] = {
            "allowed_extensions": self._to_builtin_value(
                self._settings.scanner.allowed_extensions
            ),
            "ignored_patterns": self._to_builtin_value(
                self._settings.scanner.ignored_patterns
            ),
            "excluded_dirs": self._to_builtin_value(
                getattr(self._settings.scanner, "excluded_dirs", [])
            ),
            "max_workers": self._settings.scanner.max_workers,
            "excel_max_rows": self._settings.scanner.excel_max_rows,
            "pdf_max_pages": self._settings.scanner.pdf_max_pages,
            "text_max_chars": self._settings.scanner.text_max_chars,
            "index_db_path": getattr(
                self._settings.scanner,
                "index_db_path",
                "data/db/scan_index.sqlite3",
            ),
            "parser_profile_version": getattr(
                self._settings.scanner,
                "parser_profile_version",
                "v1",
            ),
            "worker_lane_mode": getattr(
                self._settings.scanner,
                "worker_lane_mode",
                "direct",
            ),
            "discovery_backend": str(
                getattr(self._settings.scanner, "discovery_backend", "rust")
            ).strip().lower(),
            "rust_discovery_bin": getattr(
                self._settings.scanner,
                "rust_discovery_bin",
                "rust/discovery/target/release/ai-daily-discovery",
            ),
            "office_parser_backend": str(
                getattr(
                    self._settings.scanner,
                    "office_parser_backend",
                    DEFAULT_OFFICE_PARSER_BACKEND,
                )
            ).strip(),
            "rust_office_parser_bin": getattr(
                self._settings.scanner,
                "rust_office_parser_bin",
                DEFAULT_RUST_OFFICE_PARSER_BIN,
            ),
            "office_parser_fallback_enabled": bool(
                getattr(self._settings.scanner, "office_parser_fallback_enabled", True)
            ),
            "office_parser_fallback_order": self._to_builtin_value(
                getattr(
                    self._settings.scanner,
                    "office_parser_fallback_order",
                    DEFAULT_OFFICE_FALLBACK_ORDER,
                )
            ),
            "office_fallback_after_timeout": bool(
                getattr(self._settings.scanner, "office_fallback_after_timeout", False)
            ),
            "office_external_fallback": str(
                getattr(self._settings.scanner, "office_external_fallback", "disabled")
            ).strip().lower(),
            "office_legacy_extensions_enabled": bool(
                getattr(self._settings.scanner, "office_legacy_extensions_enabled", False)
            ),
            "discovery_timeout_seconds": getattr(
                self._settings.scanner,
                "discovery_timeout_seconds",
                30,
            ),
        }
        # 可选的 summary 模式配置
        scanner = self._settings.scanner
        for key in (
            "summary_excel_max_rows",
            "summary_pdf_max_pages",
            "summary_text_max_chars",
            "total_max_chars",
            "max_file_size_mb",
            "file_timeout_seconds",
            "file_timeout_by_extension",
            "direct_text_max_bytes",
            "direct_text_read_bytes",
            "log_tail_read_bytes",
            "text_excerpt_max_chars",
            "excel_max_sheets",
            "excel_max_columns",
            "docx_max_paragraphs",
            "docx_max_tables",
            "docx_table_max_rows",
            "docx_table_max_cols",
            "pptx_max_slides",
            "pptx_include_notes",
            "document_excerpt_max_chars",
            "summary_excel_max_sheets",
            "summary_excel_max_columns",
            "summary_docx_max_paragraphs",
            "summary_docx_max_tables",
            "summary_docx_table_max_rows",
            "summary_docx_table_max_cols",
            "summary_pptx_max_slides",
            "summary_document_excerpt_max_chars",
        ):
            if hasattr(scanner, key):
                cfg[key] = self._to_builtin_value(getattr(scanner, key))
        return cfg

    @property
    def llm_provider(self) -> str:
        """LLM provider (deepseek/openai)"""
        provider = "deepseek"
        if hasattr(self._settings, "llm") and hasattr(self._settings.llm, "provider"):
            provider = self._settings.llm.provider
        return str(provider).strip().lower()

    @property
    def deepseek_api_key(self) -> str:
        """DeepSeek API Key"""
        key = os.getenv("DEEPSEEK_API_KEY")
        if key:
            return key
        api_settings = getattr(self._settings, "api", None)
        return getattr(api_settings, "deepseek_api_key", "")

    @property
    def google_api_key(self) -> str:
        """Google API Key"""
        # 优先读取环境变量，避免把密钥写入配置文件
        key = os.getenv("GOOGLE_API_KEY")
        if key:
            return key
        api_settings = getattr(self._settings, "api", None)
        return getattr(api_settings, "google_api_key", "")

    @property
    def openai_api_key(self) -> str:
        """OpenAI API Key"""
        key = os.getenv("OPENAI_API_KEY")
        if key:
            return key
        api_settings = getattr(self._settings, "api", None)
        return getattr(api_settings, "openai_api_key", "")

    @property
    def proxy_config(self) -> Dict[str, str]:
        """代理配置"""
        proxy = getattr(self._settings, "proxy", None)
        return {
            "http": getattr(proxy, "http_proxy", ""),
            "https": getattr(proxy, "https_proxy", ""),
        }


# 全局配置实例
config = Config()
