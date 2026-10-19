#!/usr/bin/env python3
"""Normalize Palette execution arguments at the host transport boundary.

`source` is opaque program text carried by the MCP/JSON transport. It is never
embedded in another Julia string literal. The adapter converts it to the
worker's existing `code` field only after JSON decoding.

`code` remains supported for compatibility. Exactly one of `code` or `source`
must be supplied.

`timeout_s` is a call's own time limit, within the host's maximum.

`context` and `budget` choose the call's execution context; neither applies to
an ephemeral call. They are refused here, before the call starts, because a
rejected request must not stop the kernel. Same wording as the host (cli.rs)
and the kernel (contexts.jl).
"""
from __future__ import annotations

import os


class SourceTransportError(ValueError):
    pass


def normalize_palette_call(args: dict) -> dict:
    if not isinstance(args, dict):
        raise SourceTransportError("Palette arguments must be an object")

    value = dict(args)
    has_code = "code" in value
    has_source = "source" in value

    if has_code == has_source:
        raise SourceTransportError("exactly one of code or source is required")

    field = "source" if has_source else "code"
    text = value[field]
    if not isinstance(text, str):
        raise SourceTransportError(f"{field} must be a string")

    if has_source:
        value["code"] = value.pop("source")
    check_context(value)
    return value


CONTEXT_NAMES = ("normal", "traced", "reference", "memoized", "budgeted")

CONTEXT_SCHEMA = {
    "context": {"type": "string", "enum": list(CONTEXT_NAMES)},
    "budget": {"type": "object", "properties": {"seconds": {"type": "number", "exclusiveMinimum": 0},
                                                "bytes": {"type": "integer", "minimum": 1}},
               "additionalProperties": False},
}


def check_context(args: dict) -> None:
    for field in ("context", "budget"):
        if args.get("ephemeral") is True and args.get(field) is not None:
            raise SourceTransportError(f"request '{field}' does not apply to an ephemeral call: "
                                       "a scratch worker always runs in the normal context")
    context = args.get("context")
    if args.get("budget") is not None and context != "budgeted":
        raise SourceTransportError('a budget applies only to the budgeted context: send context "budgeted" with it'
                                   if context is None else
                                   f'a budget applies only to the budgeted context, not "{context}"')


# The hosts refuse a longer --max-turn-timeout.
LONGEST_TURN_TIMEOUT = 604800


def turn_limits(environ=os.environ) -> tuple[float, float]:
    """A call's time limit when it names none, and the longest it may name:
    PALETTE_TURN_TIMEOUT (default 60) and PALETTE_MAX_TURN_TIMEOUT (default
    1800), passed to the host as --turn-timeout and --max-turn-timeout."""
    default = float(environ.get("PALETTE_TURN_TIMEOUT", "60"))
    longest = float(environ.get("PALETTE_MAX_TURN_TIMEOUT", "1800"))
    if not 0 < default <= longest <= LONGEST_TURN_TIMEOUT:
        raise ValueError("PALETTE_TURN_TIMEOUT must be positive and no more than PALETTE_MAX_TURN_TIMEOUT, "
                         f"which is at most {LONGEST_TURN_TIMEOUT} (7 days)")
    return default, longest


def seconds_text(t: float) -> str:
    """A limit as short text that parses back to the same number."""
    short = f"{t:g}"
    return short if float(short) == t else repr(float(t))


def call_limit(args: dict, limits: tuple[float, float]) -> float:
    """The limit a call runs under. A `timeout_s` outside (0, max], or on a
    scratch call, is refused by the host at once, so it waits the default."""
    value = args.get("timeout_s")
    valid = (isinstance(value, (int, float)) and not isinstance(value, bool) and 0 < value <= limits[1]
             and args.get("ephemeral") is not True)
    return float(value) if valid else limits[0]


def timeout_schema(limits: tuple[float, float]) -> dict:
    return {"type": "number", "exclusiveMinimum": 0, "maximum": limits[1],
            "description": f"This call's time limit in seconds; default {limits[0]:g}, at most {limits[1]:g}. "
                           "Not for an ephemeral call, which runs under the default. "
                           "Longer work runs as a job: Palette.host_command(name; wait=false)."}
