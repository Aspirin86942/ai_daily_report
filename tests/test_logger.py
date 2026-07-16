"""Tests for application logging setup."""

import logging
from pathlib import Path
from types import SimpleNamespace

from src.core import logger as logger_module


def test_setup_logger_uses_the_resolved_config_log_directory(
    tmp_path,
    monkeypatch,
):
    log_dir = tmp_path / "安装 根" / "shared" / "logs"
    cwd = tmp_path / "cwd"
    cwd.mkdir()
    monkeypatch.setattr(
        logger_module,
        "config",
        SimpleNamespace(log_dir=log_dir),
    )
    monkeypatch.chdir(cwd)
    logger = logger_module.setup_logger("test_resolved_config_log")

    try:
        file_handlers = [
            handler
            for handler in logger.handlers
            if isinstance(handler, logging.FileHandler)
        ]
        assert len(file_handlers) == 1
        assert Path(file_handlers[0].baseFilename).parent == log_dir
        assert not (cwd / "logs").exists()
    finally:
        for handler in list(logger.handlers):
            logger.removeHandler(handler)
            handler.close()
