"""An explicitly requested synthetic gateway check, counted in the shared call budget."""

from datetime import UTC, datetime
from pathlib import Path
import json
import os

from .context import RunContext
from .errors import ReportRunError
from .evidence_daily import EvidenceDailyService
from .locking import ReportLock
from .model_calls import AuditedModel, Deadline
from .model_plan import plan_evidence


def check_model(runtime_config, *, transport_factory=None, clock=lambda: datetime.now(UTC)) -> dict:
    app = EvidenceDailyService(runtime_config, transport_factory=transport_factory, clock=clock)
    limits = runtime_config.recovery_settings
    expected = "合成检查通过：SYNTH-731，2 条记录。"
    with ReportLock(app.db_path, wait_seconds=limits.lock_wait_seconds):
        app._open()
        context = RunContext.create(now=clock(), config_fingerprint=app._config_fingerprint())
        app.runs.create(context, kind="model_check")
        try:
            plan = plan_evidence(context, (), user_input="合成样本编号 SYNTH-731，记录数量 2。",
                system_prompt=('只返回 JSON：{"summary_items":[{"text":"' + expected
                    + '","source_ids":["user_input"]}],"verification_items":[]}。核对输入样本后按此格式原样回答。'),
                llm_config=runtime_config.llm_config, settings=runtime_config.model_request_settings)
            reference = app.snapshots.put(context.run_id, "input", {"synthetic": True,
                "batches": [batch.model_dump(mode="json") for batch in plan.batches]})
            app.runs.prepare(context.run_id, reference, plan.input_fingerprint, list(plan.sources))
            model = AuditedModel(app.runs, app.snapshots, transport=app._transport(),
                settings=runtime_config.model_request_settings, limits=limits, deadline=Deadline(limits.run_budget_seconds))
            result = model.generate(context.run_id, plan.batches[0], synthetic_check=True)
            if len(result.summary_items) != 1 or result.summary_items[0].text != expected or result.verification_items:
                raise ReportRunError("MODEL_CHECK_FACTS_INVALID")
            calls = app.runs.calls(context.run_id)
            evidence = {"run_id": context.run_id, "gateway": runtime_config.llm_config["base_url"],
                    "model": runtime_config.llm_config["model_id"], "calls": len(calls),
                    "checked_at": clock().astimezone(UTC).isoformat(), "config_fingerprint": context.config_fingerprint,
                    "request_max_tokens": runtime_config.llm_config["max_tokens"],
                    "json_verified": True, "source_references_verified": True,
                    "usage_available": any(type(value) is int and value >= 0 for key, value in
                        json.loads(calls[-1]["usage_json"] or "{}").items()
                        if key in {"prompt_tokens", "completion_tokens", "total_tokens", "input_tokens", "output_tokens"}),
                    "context_limits_verified": runtime_config.model_request_settings.context_window_tokens is not None
                        and runtime_config.model_request_settings.verified_max_output_tokens is not None}
            directory = Path(runtime_config.data_dir) / "diagnostics"
            directory.mkdir(parents=True, exist_ok=True)
            path = directory / f"model-check-{context.run_id}.json"
            with path.open("x", encoding="utf-8", newline="\n") as stream:
                stream.write(json.dumps(evidence, ensure_ascii=False, indent=2) + "\n")
                stream.flush()
                os.fsync(stream.fileno())
            evidence["evidence_path"] = str(path)
            app.runs.finish_preview(context.run_id, "complete")
            return evidence
        except KeyboardInterrupt:
            app.runs.fail(context.run_id, "RUN_CANCELLED", cancelled=True)
            raise
        except Exception as error:
            app.runs.fail(context.run_id, error.code if isinstance(error, ReportRunError) else "MODEL_CHECK_FAILED")
            raise
