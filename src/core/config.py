"""配置管理模块"""

import os
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any, Dict
from dynaconf import Dynaconf


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

        # 加载配置
        self._settings = Dynaconf(
            envvar_prefix="DAILY_REPORT",
            settings_files=[
                str(config_dir / "settings.toml"),
                str(config_dir / ".secrets.toml"),
            ],
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
