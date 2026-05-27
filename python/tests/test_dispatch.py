"""Tests for the sidecar NDJSON parse and dispatch functions."""
from __future__ import annotations

import json

import pytest

from vocalboard_sidecar.dispatch import (
    dispatch,
    make_error_msg,
    make_log_msg,
    make_result_msg,
    parse_message,
)
from vocalboard_sidecar.registry import REGISTRY


class TestParseMessage:
    def test_valid_request(self) -> None:
        line = '{"type":"request","request_id":"r1","command":"ping","version":1,"payload":{}}'
        msg = parse_message(line)
        assert msg["type"] == "request"
        assert msg["command"] == "ping"

    def test_strips_surrounding_whitespace(self) -> None:
        msg = parse_message('  {"type":"cancel"}  \n')
        assert msg["type"] == "cancel"

    def test_empty_line_raises_value_error(self) -> None:
        with pytest.raises(ValueError, match="empty"):
            parse_message("")

    def test_whitespace_only_raises_value_error(self) -> None:
        with pytest.raises(ValueError):
            parse_message("   \n")

    def test_malformed_json_raises(self) -> None:
        with pytest.raises(json.JSONDecodeError):
            parse_message("{not json}")


class TestDispatch:
    def test_ping_returns_pong(self) -> None:
        msg = {
            "type": "request",
            "request_id": "r1",
            "command": "ping",
            "version": 1,
            "payload": {},
        }
        response = dispatch(msg, REGISTRY, {})
        assert response == {
            "type": "result",
            "request_id": "r1",
            "payload": {"pong": True},
        }

    def test_unknown_command_returns_error(self) -> None:
        msg = {
            "type": "request",
            "request_id": "r2",
            "command": "no_such_command",
            "version": 1,
            "payload": {},
        }
        response = dispatch(msg, REGISTRY, {})
        assert response is not None
        assert response["type"] == "error"
        assert response["request_id"] == "r2"
        assert response["code"] == "unknown_command"

    def test_cancel_sets_flag_and_returns_none(self) -> None:
        cancel_flags: dict[str, bool] = {}
        msg = {"type": "cancel", "request_id": "r3"}
        response = dispatch(msg, REGISTRY, cancel_flags)
        assert response is None
        assert cancel_flags == {"r3": True}

    def test_cancel_without_request_id_does_not_crash(self) -> None:
        cancel_flags: dict[str, bool] = {}
        response = dispatch({"type": "cancel"}, REGISTRY, cancel_flags)
        assert response is None
        assert cancel_flags == {}

    def test_unexpected_message_type_returns_error(self) -> None:
        msg = {"type": "bogus", "request_id": "r4"}
        response = dispatch(msg, REGISTRY, {})
        assert response is not None
        assert response["type"] == "error"
        assert "bogus" in response["message"]


class TestMessageBuilders:
    def test_make_log_msg_defaults(self) -> None:
        msg = make_log_msg("sidecar ready")
        assert msg == {
            "type": "log",
            "request_id": None,
            "level": "info",
            "msg": "sidecar ready",
        }

    def test_make_log_msg_with_request_id(self) -> None:
        msg = make_log_msg("loaded model", level="debug", request_id="r1")
        assert msg["level"] == "debug"
        assert msg["request_id"] == "r1"

    def test_make_result_msg(self) -> None:
        msg = make_result_msg("r1", {"pong": True})
        assert msg == {"type": "result", "request_id": "r1", "payload": {"pong": True}}

    def test_make_error_msg(self) -> None:
        msg = make_error_msg("r1", "unknown_command", "bad command")
        assert msg == {
            "type": "error",
            "request_id": "r1",
            "code": "unknown_command",
            "message": "bad command",
        }

    def test_make_error_msg_null_request_id(self) -> None:
        msg = make_error_msg(None, "sidecar_not_ready", "startup failed")
        assert msg["request_id"] is None
