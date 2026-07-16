"""Python 文档 worker 命令行入口。"""

from __future__ import annotations

import sys

from .python_worker_identity import python_worker_version_json


def main(argv: list[str] | tuple[str, ...] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv)
    if args == ["version"]:
        sys.stdout.buffer.write(python_worker_version_json() + b"\n")
        return 0

    import json

    from .contracts import invalid_request_response, parse_worker_request

    if args == ["parse"]:
        from src.models.scanner_contract import WorkerParseRequest

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
    import json

    response = json.dumps(payload, ensure_ascii=False).encode(
        "utf-8",
        errors="strict",
    )
    sys.stdout.buffer.write(response + b"\n")


if __name__ == "__main__":
    exit_code = main()
    if sys.platform == "win32":
        # This worker serves exactly one request. Flush the contract bytes and
        # skip CPython's process-wide finalizer walk; Windows reclaims every
        # remaining process handle after the already-complete request exits.
        sys.stdout.flush()
        sys.stderr.flush()
        import nt

        nt._exit(exit_code)
    raise SystemExit(exit_code)
