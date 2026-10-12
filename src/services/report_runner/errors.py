"""Stable error codes; exception messages never contain business text or SDK bodies."""


class ReportRunError(RuntimeError):
    def __init__(self, code: str):
        self.code = code
        super().__init__(code)
