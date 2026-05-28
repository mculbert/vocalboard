"""NDJSON message parsing and command dispatch."""
from __future__ import annotations

import json
from typing import Any, Callable

import structlog

Handler = Callable[[dict[str, Any], bool], Any]

_log = structlog.get_logger()


def parse_message(line: str) -> dict[str, Any]:
    """Parse one NDJSON line into a message dict."""
    line = line.strip()
    if not line:
        raise ValueError("empty line")
    return json.loads(line)


def make_log_msg(
    msg: str, level: str = "info", request_id: str | None = None
) -> dict[str, Any]:
    return {"type": "log", "request_id": request_id, "level": level, "msg": msg}


def make_result_msg(request_id: str, payload: Any) -> dict[str, Any]:
    return {"type": "result", "request_id": request_id, "payload": payload}


def make_error_msg(
    request_id: str | None, code: str, message: str
) -> dict[str, Any]:
    return {
        "type": "error",
        "request_id": request_id,
        "code": code,
        "message": message,
    }


def dispatch(
    msg: dict[str, Any],
    registry: dict[str, Handler],
    cancel_flags: dict[str, bool],
) -> dict[str, Any] | None:
    """Route a parsed message to a handler and return the response.

    Returns None for cancel messages (no response is emitted).
    """
    msg_type = msg.get("type")

    if msg_type == "cancel":
        request_id = msg.get("request_id")
        if request_id:
            cancel_flags[request_id] = True
        return None

    if msg_type != "request":
        return make_error_msg(
            msg.get("request_id"),
            "unknown_command",
            f"unexpected message type: {msg_type!r}",
        )

    request_id = msg.get("request_id") or ""
    command = msg.get("command") or ""
    payload = msg.get("payload") or {}

    handler = registry.get(command)
    if handler is None:
        return make_error_msg(
            request_id, "unknown_command", f"unknown command: {command!r}"
        )

    cancelled = cancel_flags.get(request_id, False)
    result_payload = handler(payload, cancelled)
    return make_result_msg(request_id, result_payload)


def handle_message(
    msg: dict[str, Any],
    registry: dict[str, Handler],
    cancel_flags: dict[str, bool],
) -> dict[str, Any] | None:
    """Dispatch a parsed message, converting handler exceptions to internal_error responses.

    Keeps the sidecar loop alive when a handler raises unexpectedly.
    Returns None for cancel messages (same as dispatch).
    """
    try:
        return dispatch(msg, registry, cancel_flags)
    except Exception as exc:
        request_id = msg.get("request_id")
        _log.error("handler_exception", request_id=request_id, exc_info=exc)
        return make_error_msg(request_id, "internal_error", str(exc))
