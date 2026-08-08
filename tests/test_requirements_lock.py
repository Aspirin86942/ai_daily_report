import subprocess
from pathlib import Path

def test_lock_regenerates_byte_identical():
    root = Path(__file__).resolve().parents[1]
    expected = (root / "requirements.lock").read_bytes()
    export = subprocess.run(
        ["uv", "export", "--frozen", "--no-dev", "--no-emit-project", "--no-header",
         "--format", "requirements.txt"],
        cwd=root, capture_output=True, check=True,
    ).stdout
    assert export == expected, "requirements.lock 与 uv export 不一致"
