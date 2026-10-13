"""Immutable references to existing model calls; reuse never creates a charged call."""

from typing import Literal

from pydantic import BaseModel, ConfigDict, Field


class FrozenModelCall(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True, strict=True)
    version: Literal[1] = 1
    call_id: int = Field(ge=1)
    run_id: str = Field(pattern=r"^[0-9a-f]{32}$")
    generation_id: str = Field(pattern=r"^[0-9a-f]{32}$")
    config_fingerprint: str = Field(max_length=64)
    logical_step: str = Field(pattern=r"^batch_[0-9]{3}$")
    status: Literal["succeeded", "failed", "unknown", "reserved"]
    request_fingerprint: str = Field(pattern=r"^[0-9a-f]{64}$")
    request_path: str
    request_sha256: str = Field(pattern=r"^[0-9a-f]{64}$")
    response_path: str | None
    response_sha256: str | None = Field(pattern=r"^[0-9a-f]{64}$")
    row_sha256: str = Field(pattern=r"^[0-9a-f]{64}$")

    @classmethod
    def from_record(cls, run: dict, call: dict, *, row_sha256: str):
        if call["run_id"] != run["run_id"] or call["generation_id"] != run["generation_id"]:
            raise ValueError("a pinned call must belong to the stated run")
        fields = {key: call[key] for key in ("call_id", "run_id", "generation_id", "logical_step", "status",
            "request_fingerprint", "request_path", "request_sha256", "response_path", "response_sha256")}
        return cls(**fields, config_fingerprint=run["config_fingerprint"], row_sha256=row_sha256)
