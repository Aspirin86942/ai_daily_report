"""Append-only budget revisions; the original reservations remain immutable."""
import json

from ..models.model_budget import BudgetReservation, JobBudgetRevision, JobCallsRevision
from .report_runner.errors import ReportRunError
from .report_runner.snapshots import canonical_json, fingerprint


def job_rows(connection, job_id):
    return [dict(row) for row in connection.execute("""SELECT call_id,run_id,status,error_code,
        usage_json,request_sha256,response_sha256 FROM report_model_calls
        WHERE json_extract(usage_json,'$.reserved_budget.job_id')=? ORDER BY call_id""", (job_id,))]


def digest(value):
    return fingerprint(value)


def read_revision_chain(connection, job_id):
    """Validate the complete chain and the retained rows before allocating more calls."""
    if job_id is None:
        return []
    try:
        base_row = connection.execute("SELECT value FROM report_metadata WHERE key=?",
            ("job_calls_revision:" + job_id,)).fetchone()
        head_row = connection.execute("SELECT value FROM report_metadata WHERE key=?",
            ("job_budget_revision_head:" + job_id,)).fetchone()
        if base_row is None:
            if head_row is not None:
                raise ValueError("missing anchor")
            return []
        base = json.loads(base_row["value"])
        anchor = JobCallsRevision.model_validate(base["revision"])
        if digest(base["revision"]) != base["sha256"] or anchor.job_id != job_id:
            raise ValueError("changed anchor")
        reverse = []
        identity = json.loads(head_row["value"])["sha256"] if head_row else base["sha256"]
        seen = set()
        while identity != base["sha256"]:
            if identity in seen or len(seen) >= 32:
                raise ValueError("invalid revision chain")
            seen.add(identity)
            row = connection.execute("SELECT value FROM report_metadata WHERE key=?",
                ("job_budget_revision:" + job_id + ":" + identity,)).fetchone()
            if row is None:
                raise ValueError("missing revision")
            envelope = json.loads(row["value"])
            revision = JobBudgetRevision.model_validate(envelope["revision"])
            if digest(envelope["revision"]) != identity or envelope["sha256"] != identity:
                raise ValueError("changed revision")
            reverse.append((revision, identity))
            identity = revision.previous_revision_sha256
        chain = [(anchor, base["sha256"]), *reversed(reverse)]
        rows = job_rows(connection, job_id)
        for index, (revision, _) in enumerate(chain):
            retained = [row for row in rows if row["call_id"] in revision.retained_call_ids]
            if ([row["call_id"] for row in retained] != revision.retained_call_ids
                    or digest(retained) != revision.retained_calls_sha256):
                raise ValueError("changed retained calls")
            if index:
                parent, parent_hash = chain[index - 1]
                if (revision.previous_revision_sha256 != parent_hash
                        or revision.previous_calls_limit != parent.calls_limit
                        or revision.previous_tokens_limit != parent.tokens_limit
                        or revision.job_id != parent.job_id or revision.plan_sha256 != parent.plan_sha256
                        or revision.pricing != parent.pricing
                        or revision.cost_limit_microunits != parent.cost_limit_microunits
                        or not set(parent.retained_call_ids).issubset(revision.retained_call_ids)):
                    raise ValueError("revision changed unrelated scope")
        return chain
    except (ValueError, KeyError, TypeError):
        raise ReportRunError("MODEL_JOB_BUDGET_CHANGED") from None


def reservation_is_approved(prior, call_id, chain, current):
    identity = prior.request_audit.get("job_calls_revision_sha256")
    if (prior.pricing != current.pricing
            or prior.job_cost_limit_microunits != current.job_cost_limit_microunits):
        return False
    if not chain:
        return (identity == current.request_audit.get("job_calls_revision_sha256")
            and prior.job_calls_limit == current.job_calls_limit
            and prior.job_tokens_limit == current.job_tokens_limit)
    anchor = chain[0][0]
    scopes = [(None, anchor.previous_calls_limit, anchor.tokens_limit, anchor.retained_call_ids)]
    scopes += [(key, revision.calls_limit, revision.tokens_limit,
        chain[index + 1][0].retained_call_ids if index + 1 < len(chain) else None)
        for index, (revision, key) in enumerate(chain)]
    return any(identity == key and prior.job_calls_limit == calls and prior.job_tokens_limit == tokens
        and (retained is None or call_id in retained) for key, calls, tokens, retained in scopes)


def append_budget_revision(connection, revision: JobBudgetRevision):
    value = revision.model_dump(mode="json")
    identity = digest(value)
    key = "job_budget_revision:" + revision.job_id + ":" + identity
    chain = read_revision_chain(connection, revision.job_id)
    if not chain:
        raise ReportRunError("MODEL_JOB_CALL_REVISION_CHANGED")
    if any(key_hash == identity for _, key_hash in chain):
        return identity
    parent, parent_hash = chain[-1]
    if (revision.previous_revision_sha256 != parent_hash
            or revision.previous_calls_limit != parent.calls_limit
            or revision.previous_tokens_limit != parent.tokens_limit
            or revision.plan_sha256 != parent.plan_sha256 or revision.pricing != parent.pricing
            or revision.cost_limit_microunits != parent.cost_limit_microunits):
        raise ReportRunError("MODEL_JOB_CALL_REVISION_CHANGED")
    rows = job_rows(connection, revision.job_id)
    if ([row["call_id"] for row in rows] != revision.retained_call_ids
            or digest(rows) != revision.retained_calls_sha256):
        raise ReportRunError("MODEL_JOB_CALL_REVISION_CHANGED")
    current = BudgetReservation.model_validate(json.loads(rows[-1]["usage_json"])["reserved_budget"])
    for row in rows:
        prior = BudgetReservation.model_validate(json.loads(row["usage_json"])["reserved_budget"])
        if (row["status"] not in {"failed", "succeeded"}
                or not reservation_is_approved(prior, row["call_id"], chain, current)):
            raise ReportRunError("MODEL_JOB_CALL_REVISION_CHANGED")
    connection.execute("INSERT INTO report_metadata(key,value) VALUES (?,?)",
        (key, canonical_json({"revision": value, "sha256": identity}).decode("utf-8")))
    connection.execute("INSERT INTO report_metadata(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        ("job_budget_revision_head:" + revision.job_id, canonical_json({"sha256": identity}).decode("utf-8")))
    return identity
