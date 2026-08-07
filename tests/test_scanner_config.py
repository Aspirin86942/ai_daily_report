"""scanner_config 拆分与模块边界等价性测试。"""

from __future__ import annotations

from src.services.context_engine import ContextEngine


def test_context_engine_protocol_is_public_name() -> None:
    """跨模块引用的 Protocol 必须是公开名。"""
    from src.services.context_scheduler import ContextScheduler

    assert ContextEngine is not None
    assert hasattr(ContextScheduler, "_engine_from_config")
