"""配置管理模块"""

import os
import platform
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any, Dict
from dynaconf import Dynaconf

DEFAULT_RUST_OFFICE_PARSER_BIN = (
    "rust/target/release/ai-daily-office-parser"
)
DEFAULT_SCANNER_ENGINE = "rust_v2"
DEFAULT_RUST_SCANNER_BIN = "rust/target/release/ai-daily-scanner"
DEFAULT_RUST_INDEX_DB_PATH = "data/db/scan_index_v2.sqlite3"
DEFAULT_RUST_PROCESS_TIMEOUT_SECONDS = 900.0

INSTALLED_PATH_ENV_VARS = {
    "install_root": "DAILY_REPORT_INSTALL_ROOT",
    "config_dir": "DAILY_REPORT_CONFIG_DIR",
    "data_dir": "DAILY_REPORT_DATA_DIR",
    "reports_dir": "DAILY_REPORT_REPORTS_DIR",
    "db_dir": "DAILY_REPORT_DB_DIR",
    "log_dir": "DAILY_REPORT_LOG_DIR",
}

SCANNER_CONTRACT_FIELDS = (
    "allowed_extensions",
    "ignored_patterns",
    "excluded_dirs",
    "max_workers",
    "max_file_size_mb",
    "discovery_timeout_seconds",
    "file_timeout_seconds",
    "file_timeout_by_extension",
    "total_max_chars",
    "parser_profile_version",
    "office_parser_backend",
    "pdf_parser_backend",
    "office_fallback_policy_version",
    "office_parser_fallback_enabled",
    "office_fallback_after_timeout",
    "office_legacy_extensions_enabled",
    "pptx_include_notes",
    "office_parser_fallback_order",
    "direct_text_max_bytes",
    "direct_text_read_bytes",
    "log_tail_read_bytes",
    "text_excerpt_max_chars",
    "excel_max_rows",
    "pdf_max_pages",
    "text_max_chars",
    "excel_max_sheets",
    "excel_max_columns",
    "docx_max_paragraphs",
    "docx_max_tables",
    "docx_table_max_rows",
    "docx_table_max_cols",
    "pptx_max_slides",
    "document_excerpt_max_chars",
    "summary_excel_max_rows",
    "summary_pdf_max_pages",
    "summary_text_max_chars",
    "summary_excel_max_sheets",
    "summary_excel_max_columns",
    "summary_docx_max_paragraphs",
    "summary_docx_max_tables",
    "summary_docx_table_max_rows",
    "summary_docx_table_max_cols",
    "summary_pptx_max_slides",
    "summary_document_excerpt_max_chars",
)

SCANNER_INFRASTRUCTURE_FIELDS = frozenset(
    {
        "rust_office_parser_bin",
        "engine",
        "rust_scanner_bin",
        "rust_index_db_path",
        "rust_process_timeout_seconds",
    }
)


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
    def _to_builtin_value(value: Any) -> Any:
        """递归转成原生容器，避免 Dynaconf 容器在 Windows spawn 下无法 pickle。"""
        if isinstance(value, Mapping):
            return {
                str(key): Config._to_builtin_value(item)
                for key, item in value.items()
            }
        if isinstance(value, Sequence) and not isinstance(
            value,
            (str, bytes, bytearray),
        ):
            return [Config._to_builtin_value(item) for item in value]
        return value

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
        }

    def scanner_contract_profile(self) -> Dict[str, Any]:
        """提取调用方显式配置的 scanner v1 wire 叶子。

        Rust 是默认值和归一化的唯一所有者，因此这里不补默认值，
        也不携带 worker、数据库或进程路径。
        """
        scanner = self._settings.scanner
        if isinstance(scanner, Mapping):
            raw_items = scanner.items()
        elif hasattr(scanner, "__dict__"):
            raw_items = vars(scanner).items()
        else:
            raise ValueError("scanner settings must expose explicit leaves")

        present = {
            str(key).strip().lower(): self._to_builtin_value(value)
            for key, value in raw_items
        }
        unknown = sorted(
            set(present)
            - set(SCANNER_CONTRACT_FIELDS)
            - SCANNER_INFRASTRUCTURE_FIELDS
        )
        if unknown:
            raise ValueError(
                "unknown scanner contract fields: " + ", ".join(unknown)
            )

        profile: Dict[str, Any] = {"schema_version": "scanner_profile_v1"}
        for key in SCANNER_CONTRACT_FIELDS:
            if key in present:
                profile[key] = present[key]
        return profile

    @property
    def scanner_engine(self) -> str:
        """选择一次完整 scanner/context engine；Windows 默认 Rust v2。"""
        value = str(
            getattr(self._settings.scanner, "engine", DEFAULT_SCANNER_ENGINE)
        ).strip().lower()
        if value != "rust_v2":
            raise ValueError(f"unsupported scanner engine: {value!r}")
        return value

    @property
    def rust_scanner_bin(self) -> str:
        """Rust v2 context binary 路径，不注入 scanner wire profile。"""
        if self.installed_mode:
            return str((self.project_root / DEFAULT_RUST_SCANNER_BIN).resolve())
        return self._non_blank_string(
            getattr(
                self._settings.scanner,
                "rust_scanner_bin",
                DEFAULT_RUST_SCANNER_BIN,
            ),
            DEFAULT_RUST_SCANNER_BIN,
        )

    @property
    def rust_office_parser_bin(self) -> str:
        """Rust Office worker 路径；worker 始终由 Rust core 隔离启动。"""
        if self.installed_mode:
            return str(
                (self.project_root / DEFAULT_RUST_OFFICE_PARSER_BIN).resolve()
            )
        return self._non_blank_string(
            getattr(
                self._settings.scanner,
                "rust_office_parser_bin",
                DEFAULT_RUST_OFFICE_PARSER_BIN,
            ),
            DEFAULT_RUST_OFFICE_PARSER_BIN,
        )

    @property
    def rust_index_db_path(self) -> str:
        """Rust v2 独占数据库路径；已退役的 v1 数据库保持不变。"""
        if self.installed_mode:
            return str((self.db_dir / "scan_index_v2.sqlite3").resolve())
        return self._non_blank_string(
            getattr(
                self._settings.scanner,
                "rust_index_db_path",
                DEFAULT_RUST_INDEX_DB_PATH,
            ),
            DEFAULT_RUST_INDEX_DB_PATH,
        )

    @property
    def rust_process_timeout_seconds(self) -> float:
        """Python 外层 watchdog 总预算，不替代 Rust 的逐文件 deadline。"""
        raw = getattr(
            self._settings.scanner,
            "rust_process_timeout_seconds",
            DEFAULT_RUST_PROCESS_TIMEOUT_SECONDS,
        )
        try:
            value = float(raw)
        except (TypeError, ValueError) as exc:
            raise ValueError("rust_process_timeout_seconds must be numeric") from exc
        if not 1.0 <= value <= 86_400.0:
            raise ValueError(
                "rust_process_timeout_seconds must be between 1 and 86400"
            )
        return value

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
