"""Host side of the Execution X-Ray receipt query: read receipts.jsonl and
calls.jsonl without a kernel, and join a kernel call to the host receipts it
caused. Read-only."""
from __future__ import annotations

import json
from pathlib import Path

NOTE = ("Receipts are stamped with the request in flight when the host handled them; "
        "work a call left running in the background is attributed to the call in flight at the time.")
LIMIT_MAX = 500

# palette_control's fields for {"action": "receipts"}.
QUERY_SCHEMA = {"call": {"type": "integer"}, "epoch": {"type": "string", "maxLength": 64},
                "kind": {"type": "string", "maxLength": 64}, "since": {"type": "number"}, "until": {"type": "number"},
                "limit": {"type": "integer", "minimum": 1, "maximum": LIMIT_MAX}}


def _is_int(value):
    return isinstance(value, int) and not isinstance(value, bool)


def _is_number(value):
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def _read_lines(path):
    """The JSON objects in a JSONL file, and how many non-blank lines were not one."""
    objects, unreadable = [], 0
    try:
        text = Path(path).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return objects, unreadable
    for line in text.splitlines():
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except ValueError:
            value = None
        if isinstance(value, dict):
            objects.append(value)
        else:
            unreadable += 1
    return objects, unreadable


def query(receipts_dir, *, call=None, epoch=None, kind=None, since=None, until=None, limit=50):
    """Host receipts from receipts_dir, filtered and joined to a kernel call.

    call: join to the calls.jsonl record with this call number (and this epoch,
          when given; else the last such record). Keeps receipts stamped with
          that record's session_request. Raises ValueError if there is none.
    epoch: only used with call (kernel call numbers restart in every epoch).
    kind: receipts whose category equals kind.
    since, until: inclusive bounds on receipt timestamp (unix seconds).
    limit: return the last `limit` matches (1..500).
    """
    if call is not None and not _is_int(call):
        raise ValueError("call must be an integer")
    if epoch is not None and not isinstance(epoch, str):
        raise ValueError("epoch must be a string")
    if kind is not None and not isinstance(kind, str):
        raise ValueError("kind must be a string")
    for name, value in (("since", since), ("until", until)):
        if value is not None and not _is_number(value):
            raise ValueError(f"{name} must be a number (unix seconds)")
    if not _is_int(limit):
        raise ValueError("limit must be an integer")
    if not 1 <= limit <= LIMIT_MAX:
        raise ValueError(f"limit must be between 1 and {LIMIT_MAX}")

    receipts_dir = Path(receipts_dir)
    receipts, unreadable_r = _read_lines(receipts_dir / "receipts.jsonl")

    call_record, unreadable_c = None, 0
    if call is not None:
        calls, unreadable_c = _read_lines(receipts_dir / "calls.jsonl")
        call_record = next((r for r in reversed(calls)
                            if _is_int(r.get("call")) and r.get("call") == call
                            and (epoch is None or r.get("epoch") == epoch)), None)
        if call_record is None:
            where = f" in epoch {epoch}" if epoch is not None else ""
            raise ValueError(f"no record of call {call}{where}")

    matched = []
    for r in receipts:
        if call_record is not None:
            join = call_record.get("session_request")
            if not isinstance(join, str) or r.get("session_request") != join:
                continue
        if kind is not None and r.get("category") != kind:
            continue
        ts = r.get("timestamp")
        if since is not None and not (_is_number(ts) and ts >= since):
            continue
        if until is not None and not (_is_number(ts) and ts <= until):
            continue
        matched.append(r)

    returned = matched[-limit:]
    return {
        "receipts": returned,
        "matched": len(matched),
        "returned": len(returned),
        "call": call_record,
        "unreadable_lines": unreadable_r + unreadable_c,
        "note": NOTE,
    }
