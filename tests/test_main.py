"""Smoke tests for CLI entrypoints in main.py."""

import main


def test_list_reports_uses_sqlite_store(monkeypatch):
    calls: dict[str, int] = {"init": 0, "list_all_reports": 0}

    class StubSQLiteStore:
        def __init__(self) -> None:
            calls["init"] += 1

        def list_all_reports(self) -> list[str]:
            calls["list_all_reports"] += 1
            return []

    printed: list[str] = []

    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(
        main.console,
        "print",
        lambda *args, **kwargs: printed.append(args[0] if args else ""),
    )

    main.list_reports()

    assert calls == {"init": 1, "list_all_reports": 1}
    assert any("已有日报列表" in text for text in printed)
    assert any("暂无日报数据" in text for text in printed)
