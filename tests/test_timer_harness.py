# tests/test_timer_harness.py
import json, subprocess, sys, time
from pathlib import Path
from benchmark_harness import wall_clock_ms, BenchmarkResult

def test_timer_covers_child_spawn_and_response_validation(tmp_path):
    script = tmp_path / "sleeper.py"
    script.write_text("import json,time,sys\nprint(json.dumps({'ok':1}))\ntime.sleep(0.2)\n", encoding="utf-8")
    payload = b"{}"
    started = time.perf_counter()
    result = wall_clock_ms([sys.executable, str(script)], payload, response_validator=lambda b: json.loads(b)["ok"] == 1)
    elapsed = time.perf_counter() - started
    # 必须 >= child 内部 sleep 时长（证明覆盖 response validation 之后）
    assert result.wall_ms >= 200, result.wall_ms
    assert result.validated is True
    assert elapsed >= 0.2

def test_timer_rejects_unvalidated_response(tmp_path):
    script = tmp_path / "bad.py"
    script.write_text("import sys\nsys.stdout.write('NOT_JSON')\n", encoding="utf-8")
    result = wall_clock_ms([sys.executable, str(script)], b"{}", response_validator=lambda b: json.loads(b))
    assert result.validated is False
    assert result.exit_code == 0  # 子进程退出码正常，但验证失败由 harness 捕获
