"""日志模块"""

import logging
from datetime import datetime
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[2]


def setup_logger(
    name: str = "ai_daily_report",
    log_dir: str | Path | None = None,
) -> logging.Logger:
    """配置日志系统

    Args:
        name: 日志器名称
        log_dir: 日志目录；未指定时使用仓库根目录下的 ``logs``

    Returns:
        配置好的 Logger 实例
    """
    logger = logging.getLogger(name)
    logger.setLevel(logging.INFO)

    # 避免重复添加 handler
    if logger.handlers:
        return logger

    # 创建日志目录
    log_path = PROJECT_ROOT / "logs" if log_dir is None else Path(log_dir)
    log_path.mkdir(parents=True, exist_ok=True)

    # 文件 handler
    log_file = log_path / f"{datetime.now().strftime('%Y-%m-%d')}.log"
    file_handler = logging.FileHandler(log_file, encoding="utf-8")
    file_handler.setLevel(logging.INFO)

    # 控制台 handler
    console_handler = logging.StreamHandler()
    console_handler.setLevel(logging.INFO)

    # 格式化
    formatter = logging.Formatter(
        "%(asctime)s - %(name)s - %(levelname)s - %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    )
    file_handler.setFormatter(formatter)
    console_handler.setFormatter(formatter)

    logger.addHandler(file_handler)
    logger.addHandler(console_handler)

    return logger
