"""Python 文档 worker 命令行入口。"""

from __future__ import annotations

import json
import sys
from collections.abc import Sequence

from src.models.scanner_contract import WorkerParseRequest

from .contracts import (
    invalid_request_response,
    parse_worker_request,
    python_worker_version_response,
)


def main(argv: Sequence[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if args == ["version"]:
        _emit_json(python_worker_version_response().model_dump(mode="json"))
        return 0
    if args == ["parse"]:
        try:
            request_json = sys.stdin.buffer.read().decode(
                "utf-8",
                errors="strict",
            )
            request = WorkerParseRequest.model_validate(json.loads(request_json))
        except (UnicodeError, ValueError):
            _emit_json(invalid_request_response().model_dump(mode="json"))
            return 2
        response = parse_worker_request(request)
        _emit_json(response.model_dump(mode="json"))
        return 0 if response.status == "ok" else 1

    _emit_json(invalid_request_response().model_dump(mode="json"))
    return 2


def _emit_json(payload: object) -> None:
    """绕过环境文本编码，保证进程合同始终输出 UTF-8 字节。"""
    response = json.dumps(payload, ensure_ascii=False).encode(
        "utf-8",
        errors="strict",
    )
    sys.stdout.buffer.write(response + b"\n")


if __name__ == "__main__":
    raise SystemExit(main())
