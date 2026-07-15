"""Tests for application logging setup."""

import logging
from pathlib import Path

from src.core import logger as logger_module


def test_setup_logger_anchors_default_log_directory_to_project_root(
    tmp_path,
    monkeypatch,
):
    project_root = tmp_path / "project"
    cwd = tmp_path / "cwd"
    cwd.mkdir()
    monkeypatch.setattr(logger_module, "PROJECT_ROOT", project_root)
    monkeypatch.chdir(cwd)
    logger = logger_module.setup_logger("test_project_root_default_log")

    try:
        file_handlers = [
            handler
            for handler in logger.handlers
            if isinstance(handler, logging.FileHandler)
        ]
        assert len(file_handlers) == 1
        assert Path(file_handlers[0].baseFilename).parent == project_root / "logs"
        assert not (cwd / "logs").exists()
    finally:
        for handler in list(logger.handlers):
            logger.removeHandler(handler)
            handler.close()
