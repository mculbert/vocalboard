"""Command handler registry (stub for M0; ML handlers added in later milestones)."""
from __future__ import annotations

from typing import Any

from vocalboard_sidecar.dispatch import Handler


def _handle_ping(_payload: dict[str, Any], _cancelled: bool) -> dict[str, Any]:
    return {"pong": True}


REGISTRY: dict[str, Handler] = {
    "ping": _handle_ping,
}
