"""Complete input units, original excerpt offsets, and conservative prefix revisions."""
from __future__ import annotations

from dataclasses import dataclass
import re

from ...models.content_audit import ContentUnit, SourceRange

ESSENTIAL = {"heading", "table_header", "unit", "placeholder"}


def union_chars(ranges: list[dict]) -> int:
    end = total = 0
    for item in sorted(ranges, key=lambda value: (value["start"], value["end"])):
        total += max(0, item["end"] - max(end, item["start"]))
        end = max(end, item["end"])
    return total


@dataclass(frozen=True)
class InputUnit:
    start: int
    end: int
    kind: str
    ranges: tuple[SourceRange, ...] = ()


def input_units(text: str, audit: dict | None) -> list[InputUnit]:
    declared = (audit or {}).get("content_units")
    if declared is not None:
        units = [ContentUnit.model_validate(value) for value in declared]
        original = [InputUnit(unit.text_start, unit.text_end, unit.kind, tuple(unit.ranges)) for unit in units]
    else:
        original = [InputUnit(match.start(), match.end(), "heading" if match.group().lstrip().startswith("#")
                             else "paragraph") for match in re.finditer(r".+?(?:[。！？]|\n+|(?<=\.)\s+|$)", text, re.DOTALL)
                    if match.group().strip()]
    output = []
    for unit in original:
        if unit.end - unit.start > 512 and unit.kind not in ESSENTIAL | {"table_row", "total"}:
            spans = [match for match in re.finditer(r".+?(?:[。！？]|\n+|(?<=\.)\s+|$)",
                                                   text[unit.start:unit.end], re.DOTALL) if match.group().strip()]
            output.extend(InputUnit(unit.start + span.start(), unit.start + span.end(), unit.kind, unit.ranges)
                          for span in spans)
        else:
            output.append(unit)
    return output


def unit_chunks(units: list[InputUnit], full_chars: int | None = None) -> list[tuple[int, int]]:
    """One meaningful first piece per source; depth blocks retain complete units."""
    first = next((index for index, unit in enumerate(units) if unit.kind not in ESSENTIAL), None)
    if first is None:
        return []
    end = units[first].end
    index = first + 1
    while index < len(units) and units[index].end <= 512:
        end = units[index].end
        index += 1
    result = [(0, end)]
    cursor = end
    while index < len(units):
        end = units[index].end
        index += 1
        while index < len(units) and units[index].end - cursor <= 512:
            end = units[index].end
            index += 1
        result.append((cursor, end))
        cursor = end
    if full_chars is not None:
        result[-1] = (result[-1][0], full_chars)
    return result


def make_entry(base: dict, text: str, units: list[InputUnit], chunk: tuple[int, int], part: int) -> dict:
    start, end = chunk
    essential = [unit for unit in units if unit.end <= start and unit.kind in ESSENTIAL - {"placeholder"}]
    last_heading = next((unit.start for unit in reversed(essential) if unit.kind == "heading"), 0)
    essential = [unit for unit in essential if unit.start >= last_heading]
    context, size = [], 0
    for unit in reversed(essential):
        if size + unit.end - unit.start > 512:
            break
        context.append({"start": unit.start, "end": unit.end})
        size += unit.end - unit.start
    spans = sorted(context + [{"start": start, "end": end}], key=lambda span: span["start"])
    merged = []
    for span in spans:
        if merged and span["start"] <= merged[-1]["end"] + 2:
            merged[-1]["end"] = max(merged[-1]["end"], span["end"])
        else:
            merged.append(dict(span))
    rendered, mapped, local = [], [], 0
    for span in merged:
        if rendered:
            local += 1
        rendered.append(text[span["start"]:span["end"]])
        for unit in units:
            from_, to = max(span["start"], unit.start), min(span["end"], unit.end)
            if from_ < to:
                item = {"start": local + from_ - span["start"], "end": local + to - span["start"],
                        "kind": unit.kind, "ranges": [range_.model_dump(mode="json", exclude_none=True)
                                                        for range_ in unit.ranges]}
                prior = mapped[-1] if mapped else None
                same_ranges = prior is not None and prior["ranges"] == item["ranges"]
                adjacent_ranges = (prior is not None and len(prior["ranges"]) == len(item["ranges"]) == 1
                    and prior["ranges"][0]["end"] == item["ranges"][0]["start"]
                    and {k: v for k, v in prior["ranges"][0].items() if k not in {"start", "end"}}
                        == {k: v for k, v in item["ranges"][0].items() if k not in {"start", "end"}})
                if (prior and item["kind"] not in ESSENTIAL and prior["kind"] == item["kind"]
                        and len(prior.get("boundaries", [prior["end"]])) < 512
                        and (same_ranges or adjacent_ranges) and item["start"] <= prior["end"] + 2):
                    prior = mapped[-1]
                    if adjacent_ranges or "range_ends" in prior:
                        prior.setdefault("range_ends", [prior["ranges"][0]["end"]]
                            * len(prior.get("boundaries", [prior["end"]]))).append(item["ranges"][0]["end"])
                        prior["ranges"] = [{**prior["ranges"][0], "end": item["ranges"][0]["end"]}]
                    prior.setdefault("boundaries", [prior["end"]]).append(item["end"])
                    prior["end"] = item["end"]
                else:
                    mapped.append(item)
        local += span["end"] - span["start"]
    return {**base, "part": part, "text": "\n".join(rendered), "text_ranges": merged, "input_units": mapped}


def prefix_ends(entry: dict) -> list[int]:
    if "input_units" in entry:
        return [end for unit in entry["input_units"] if unit["kind"] not in ESSENTIAL
                for end in unit.get("boundaries", [unit["end"]])]
    return [match.end() for match in re.finditer(r".+?(?:[。！？\n]|(?<=\.)\s+|$)", entry["text"], re.DOTALL)
            if match.group().strip()]


def clip_entry(entry: dict, end: int) -> dict:
    result = {**entry, "text": entry["text"][:end]}
    if "text_ranges" in entry:
        spans, left = [], end
        for span in entry["text_ranges"]:
            if spans:
                left -= 1
            if left <= 0:
                break
            amount = min(left, span["end"] - span["start"])
            spans.append({"start": span["start"], "end": span["start"] + amount})
            left -= amount
        result["text_ranges"] = spans
        mapped = []
        for unit in entry["input_units"]:
            if unit["start"] >= end:
                break
            item = {**unit, "end": min(end, unit["end"])}
            if "boundaries" in item:
                item["boundaries"] = [boundary for boundary in item["boundaries"] if boundary <= end]
                if "range_ends" in item:
                    item["range_ends"] = item["range_ends"][:len(item["boundaries"])]
                    item["ranges"] = [{**item["ranges"][0], "end": item["range_ends"][-1]}]
            mapped.append(item)
        result["input_units"] = mapped
    return result


def validate_entry_ranges(entry: dict) -> None:
    spans = entry.get("text_ranges")
    if not isinstance(spans, list) or not spans or len(spans) > 512:
        raise ValueError("versioned model input requires bounded excerpt ranges")
    previous = size = 0
    for index, span in enumerate(spans):
        if set(span) != {"start", "end"} or any(type(span[key]) is not int for key in span):
            raise ValueError("model excerpt offsets must be integers")
        if span["start"] < previous or span["end"] <= span["start"]:
            raise ValueError("model excerpt offsets must be ordered and nonoverlapping")
        previous = span["end"]
        size += span["end"] - span["start"] + bool(index)
    if size != len(entry["text"]):
        raise ValueError("model excerpt ranges must match message text length")
    prior_end = 0
    for index, unit in enumerate(entry.get("input_units", [])):
        ContentUnit(unit_id=f"u{index}", kind=unit["kind"], text_start=unit["start"], text_end=unit["end"],
                    ranges=[SourceRange.model_validate(value) for value in unit["ranges"]])
        if unit["start"] < prior_end or unit["end"] > len(entry["text"]):
            raise ValueError("model units must stay within their input")
        if "boundaries" in unit:
            bounds = unit["boundaries"]
            if (not bounds or len(bounds) > 512 or any(type(value) is not int for value in bounds)
                    or bounds != sorted(set(bounds)) or bounds[0] <= unit["start"] or bounds[-1] != unit["end"]):
                raise ValueError("semantic boundaries must stay within their unit")
        if "range_ends" in unit:
            ends = unit["range_ends"]
            ranges = unit["ranges"]
            if (len(ranges) != 1 or "boundaries" not in unit or len(ends) != len(unit["boundaries"])
                    or any(type(value) is not int for value in ends) or ends != sorted(ends)
                    or not ends or ends[0] <= ranges[0]["start"] or ends[-1] != ranges[0]["end"]):
                raise ValueError("physical range boundaries must match complete input units")
        prior_end = unit["end"]


def is_prefix_revision(entry: dict, original: dict) -> bool:
    if not original["text"].startswith(entry["text"]):
        return False
    if "text_ranges" in original:
        return (len(entry["text"]) in prefix_ends(original)
                and entry == clip_entry(original, len(entry["text"])))
    return {key: value for key, value in entry.items() if key != "text"} == {
        key: value for key, value in original.items() if key != "text"}


def wire_entry(entry: dict, version: int = 2) -> dict:
    result = {key: value for key, value in entry.items() if key not in {"text_ranges", "input_units"}}
    if version == 3:
        # Physical ranges, encoding and file modification time remain local.
        # The model needs source identity, readable text and uncertainty flags.
        result.pop("modified_at_utc", None)
        limits = result.pop("coverage_limits", {})
        if limits.get("text_unverified") is True:
            result["coverage_limits"] = {"text_unverified": True}
    return result


def batch_entries(batch) -> list[dict]:
    if batch.input_range_version in {2, 3}:
        if batch.input_audit is None:
            raise ValueError("local model input audit is required")
        return batch.input_audit
    import json
    return json.loads(batch.messages[1].content)["sources"]


def batch_ranges(batch) -> dict[str, list[dict]] | None:
    if batch.input_range_version not in {1, 2, 3}:
        return None
    result: dict[str, list[dict]] = {}
    for entry in batch_entries(batch):
        result.setdefault(entry["source_id"], []).extend(entry["text_ranges"])
    return result
