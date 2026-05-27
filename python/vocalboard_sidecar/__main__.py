"""Vocalboard sidecar entry point: NDJSON dispatch loop over stdio."""
from __future__ import annotations

import json
import sys

import structlog

from vocalboard_sidecar.dispatch import (
    dispatch,
    make_log_msg,
    parse_message,
)
from vocalboard_sidecar.registry import REGISTRY

# Route structlog to stderr so it never pollutes the NDJSON stdout stream.
structlog.configure(logger_factory=structlog.PrintLoggerFactory(file=sys.stderr))
_log = structlog.get_logger()


def _emit(msg: dict) -> None:  # type: ignore[type-arg]
    print(json.dumps(msg, separators=(",", ":")), flush=True)


def main() -> None:
    _log.info("sidecar_starting")
    _emit(make_log_msg("sidecar ready"))
    cancel_flags: dict[str, bool] = {}
    for raw_line in sys.stdin:
        try:
            msg = parse_message(raw_line)
        except (ValueError, json.JSONDecodeError):
            continue
        response = dispatch(msg, REGISTRY, cancel_flags)
        if response is not None:
            _emit(response)


if __name__ == "__main__":
    main()
