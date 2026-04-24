"""配置管理模块"""

import os
from pathlib import Path
from typing import Dict, Any
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
            "allowed_extensions": self._settings.scanner.allowed_extensions,
            "ignored_patterns": self._settings.scanner.ignored_patterns,
            "max_workers": self._settings.scanner.max_workers,
            "excel_max_rows": self._settings.scanner.excel_max_rows,
            "pdf_max_pages": self._settings.scanner.pdf_max_pages,
            "text_max_chars": self._settings.scanner.text_max_chars,
        }
        # 可选的 summary 模式配置
        scanner = self._settings.scanner
        for key in (
            "summary_excel_max_rows",
            "summary_pdf_max_pages",
            "summary_text_max_chars",
            "total_max_chars",
        ):
            if hasattr(scanner, key):
                cfg[key] = getattr(scanner, key)
        return cfg

    @property
    def llm_provider(self) -> str:
        """LLM provider (deepseek/gemini/openai)"""
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
        return getattr(self._settings.api, "deepseek_api_key", "")

    @property
    def google_api_key(self) -> str:
        """Google API Key"""
        # 优先读取环境变量，避免把密钥写入配置文件
        key = os.getenv("GOOGLE_API_KEY")
        if key:
            return key
        return getattr(self._settings.api, "google_api_key", "")

    @property
    def openai_api_key(self) -> str:
        """OpenAI API Key"""
        key = os.getenv("OPENAI_API_KEY")
        if key:
            return key
        return getattr(self._settings.api, "openai_api_key", "")

    @property
    def api_key(self) -> str:
        """Backward compatible Google API Key"""
        return self.google_api_key

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
