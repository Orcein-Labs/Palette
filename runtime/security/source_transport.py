#!/usr/bin/env python3
"""Normalize Palette execution arguments at the host transport boundary.

`source` is opaque program text carried by the MCP/JSON transport. It is never
embedded in another Julia string literal. The adapter converts it to the
worker's existing `code` field only after JSON decoding.

`code` remains supported for compatibility. Exactly one of `code` or `source`
must be supplied.
"""
from __future__ import annotations


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
    return value
