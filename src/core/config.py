"""应用配置管理；scanner wire 校验位于 services.scanner_config。"""

import os
import platform
from collections.abc import Mapping
from pathlib import Path
from typing import Any, Dict

from dynaconf import Dynaconf

DEFAULT_OFFICE_WORKER_PATH = "rust/target/release/ai-daily-office-parser"
DEFAULT_INDEX_DB_PATH = "data/db/scan_index_v3.sqlite3"

INSTALLED_PATH_ENV_VARS = {
    "install_root": "DAILY_REPORT_INSTALL_ROOT",
    "config_dir": "DAILY_REPORT_CONFIG_DIR",
    "data_dir": "DAILY_REPORT_DATA_DIR",
    "reports_dir": "DAILY_REPORT_REPORTS_DIR",
    "db_dir": "DAILY_REPORT_DB_DIR",
    "log_dir": "DAILY_REPORT_LOG_DIR",
}

_SCANNER_CONFIG_EXPORTS = frozenset(
    {"SCANNER_SETTINGS_FIELDS", "UnknownScannerSettingsError"}
)


def __getattr__(name: str) -> Any:
    """按需导出 scanner 配置门禁，避免 core/services 循环加载。"""
    if name not in _SCANNER_CONFIG_EXPORTS:
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}")

    from ..services import scanner_config

    value = getattr(scanner_config, name)
    globals()[name] = value
    return value


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
        self._project_root = Path(__file__).resolve().parents[2]
        installed_paths = self._resolve_installed_paths()
        self._install_root = (
            installed_paths["install_root"] if installed_paths else None
        )
        self._config_dir = (
            installed_paths["config_dir"]
            if installed_paths
            else self._project_root / "config"
        )
        self._data_dir = installed_paths["data_dir"] if installed_paths else None
        self._reports_dir = (
            installed_paths["reports_dir"] if installed_paths else None
        )
        self._db_dir = installed_paths["db_dir"] if installed_paths else None
        self._log_dir = installed_paths["log_dir"] if installed_paths else None
        self._settings = self._build_settings(self._config_dir)

    @classmethod
    def _resolve_installed_paths(
        cls,
        environ: Mapping[str, str] | None = None,
    ) -> dict[str, Path] | None:
        """验证 launcher 提供的 installed-mode 外部路径。"""

        values = os.environ if environ is None else environ
        install_text = str(values.get(INSTALLED_PATH_ENV_VARS["install_root"], ""))
        if not install_text.strip():
            return None

        resolved: dict[str, Path] = {}
        for name, env_name in INSTALLED_PATH_ENV_VARS.items():
            raw = str(values.get(env_name, "")).strip()
            if not raw:
                raise ValueError(f"installed mode requires {env_name}")
            path = Path(raw)
            if not path.is_absolute():
                raise ValueError(f"{env_name} must be absolute")
            try:
                path = path.resolve(strict=True)
            except OSError as exc:
                raise ValueError(f"{env_name} must be an existing directory") from exc
            if not path.is_dir():
                raise ValueError(f"{env_name} must be an existing directory")
            resolved[name] = path

        shared_root = (resolved["install_root"] / "shared").resolve()
        for name in ("config_dir", "data_dir", "reports_dir", "db_dir", "log_dir"):
            path = resolved[name]
            if not path.is_relative_to(shared_root):
                env_name = INSTALLED_PATH_ENV_VARS[name]
                raise ValueError(f"{env_name} must stay under install-root/shared")
        return resolved

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
        """返回本机配置读取顺序，后加载的文件拥有更高优先级。"""
        return [
            str(config_dir / "settings.yaml"),
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

        通用本机配置与系统配置递归合并，敏感配置最后加载，确保其优先级最高。
        """
        return Dynaconf(
            envvar_prefix="DAILY_REPORT",
            settings_files=cls._settings_files(config_dir, system_name),
            environments=False,
            load_dotenv=True,
            merge_enabled=True,
        )

    @staticmethod
    def _non_blank_string(value: Any, default: str) -> str:
        """把 YAML null/空白值收敛到默认值，避免下游 cache key 漂移。"""
        if value is None:
            return default
        text = str(value).strip()
        return text or default

    @property
    def work_dir(self) -> Path:
        """工作目录"""
        path = Path(self._settings.paths.work_dir)
        if self.installed_mode and not path.is_absolute():
            raise ValueError("installed-mode paths.work_dir must be absolute")
        return path

    @property
    def project_root(self) -> Path:
        """当前源码 checkout 或已选中 release 的绝对根目录。"""
        return getattr(
            self,
            "_project_root",
            Path(__file__).resolve().parents[2],
        )

    @property
    def installed_mode(self) -> bool:
        """是否由 side-by-side launcher 启动。"""
        return getattr(self, "_install_root", None) is not None

    @property
    def install_root(self) -> Path | None:
        """已安装根目录；源码运行返回 ``None``。"""
        return getattr(self, "_install_root", None)

    @property
    def config_dir(self) -> Path:
        """唯一配置目录，不读取 release/version-local 配置。"""
        return getattr(self, "_config_dir", self.project_root / "config")

    @property
    def config_source(self) -> Path:
        """报告当前平台实际优先使用的非敏感配置路径。"""
        platform_file = self.config_dir / self._settings_file_name()
        generic_file = self.config_dir / "settings.yaml"
        return platform_file if platform_file.is_file() else generic_file

    @property
    def data_dir(self) -> Path:
        """数据目录"""
        installed = getattr(self, "_data_dir", None)
        if installed is not None:
            return installed
        return self.project_root / self._settings.paths.data_dir

    @property
    def reports_dir(self) -> Path:
        """报告目录"""
        installed = getattr(self, "_reports_dir", None)
        if installed is not None:
            return installed
        return self.project_root / self._settings.paths.reports_dir

    @property
    def db_dir(self) -> Path:
        """数据库目录"""
        installed = getattr(self, "_db_dir", None)
        if installed is not None:
            return installed
        return self.project_root / self._settings.paths.db_dir

    @property
    def log_dir(self) -> Path:
        """日志目录；installed mode 始终位于 shared。"""
        installed = getattr(self, "_log_dir", None)
        if installed is not None:
            return installed
        return self.project_root / "logs"

    @property
    def llm_config(self) -> Dict[str, Any]:
        """LLM 配置"""
        return {
            "model_id": self._settings.llm.model_id,
            "temperature": self._settings.llm.temperature,
            "max_tokens": self._settings.llm.max_tokens,
            "max_retries": self._settings.llm.max_retries,
            # API base URL；旧本机配置缺省为空串，由客户端回退 provider 默认。
            "base_url": str(
                getattr(self._settings.llm, "base_url", "") or ""
            ).strip(),
        }

    def scanner_settings(self) -> Dict[str, Any]:
        """提取调用方显式配置的 scanner settings。"""
        from ..services.scanner_config import extract_scanner_settings

        return extract_scanner_settings(self._settings.scanner)

    @property
    def office_worker_path(self) -> str:
        """隔离 Office worker 路径。"""
        if self.installed_mode:
            return str(
                (self.project_root / DEFAULT_OFFICE_WORKER_PATH).resolve()
            )
        return self._non_blank_string(
            getattr(
                self._settings.scanner,
                "office_worker_path",
                DEFAULT_OFFICE_WORKER_PATH,
            ),
            DEFAULT_OFFICE_WORKER_PATH,
        )

    @property
    def index_db_path(self) -> str:
        """Fresh-only v3 scanner 数据库路径。"""
        if self.installed_mode:
            return str((self.db_dir / "scan_index_v3.sqlite3").resolve())
        return self._non_blank_string(
            getattr(
                self._settings.scanner,
                "index_db_path",
                DEFAULT_INDEX_DB_PATH,
            ),
            DEFAULT_INDEX_DB_PATH,
        )

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
        key = (os.getenv("DEEPSEEK_API_KEY") or "").strip()
        if key:
            return key
        api_settings = getattr(self._settings, "api", None)
        key = str(getattr(api_settings, "deepseek_api_key", "") or "").strip()
        if key:
            return key

        # 兼容既有本机 settings.yaml；新部署仍推荐环境变量或 .secrets.yaml。
        llm_settings = getattr(self._settings, "llm", None)
        return str(
            getattr(llm_settings, "DEEPSEEK_API_KEY", "") or ""
        ).strip()

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
