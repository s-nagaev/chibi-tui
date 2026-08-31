#!/usr/bin/env python3
"""Deterministic fake Chibi IDE backend (stdlib only) for pipeline tests.

Speaks IDE protocol v1 over JSONL on stdin/stdout, mirroring the canonical
fixtures in tests/fixtures/ide_protocol/:

* ``initialize`` -> ``ready`` (canonical fixture shape)
* ``request``    -> status queued -> status running -> result (after a short,
  cancel-able delay)
* ``cancel``     -> per fixture: status running + error cancelled for
  in-flight ids, error unknown_request otherwise
* ``shutdown``   -> clean exit 0

Optional behaviour flags (used by the integration tests):

``--garbage-on-start``   emit one non-JSON line right after ``ready``
``--crash-after-running`` emit ``status running`` for a request, then hard-exit
                         with code 69 via ``os._exit`` (broken-pipe scenario)
``--result-without-model`` omit the optional ``model``/``provider`` fields on
                         result frames (feat_agent_model_label fallback: old
                         backend / fieldless variant)
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time

PRINT_LOCK = threading.Lock()

RESULT_CONTENT = "This function `foo` returns the integer `42`."


def emit(obj):
    """Print one protocol frame; always line-buffered and flushed."""
    with PRINT_LOCK:
        print(json.dumps(obj), flush=True)


def ready_frame():
    return {
        "type": "ready",
        "protocol_version": 1,
        "server": {"name": "chibi", "version": "1.0.0"},
        "capabilities": {
            "commands": [
                "/reset",
                "/new_thread_with_current_context",
                "/model",
                "/imagine",
                "/info",
                "/help",
                "/quit",
                "/exit",
            ]
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Fake chibi ide --stdio backend.")
    parser.add_argument("--workspace", default=".", help="accepted for parity, ignored")
    parser.add_argument(
        "--garbage-on-start",
        action="store_true",
        help="emit one non-JSON line right after ready",
    )
    parser.add_argument(
        "--crash-after-running",
        action="store_true",
        help="hard-exit with code 69 right after emitting status running",
    )
    parser.add_argument(
        "--result-without-model",
        action="store_true",
        help="omit model/provider on result frames (fieldless fallback)",
    )
    args = parser.parse_args()

    initialized = False
    # request ids believed to be in flight (result timers may still fire)
    inflight = set()
    finish_timers = []

    def emit_result(request_id: str) -> None:
        if request_id not in inflight:
            return  # cancelled (or crash raced): never answer twice
        inflight.discard(request_id)
        result = {
            "type": "result",
            "request_id": request_id,
            "content": RESULT_CONTENT,
        }
        if not args.result_without_model:
            result["model"] = "gpt-example"
            result["provider"] = "openai"
        emit(result)

    def schedule_result(request_id: str, delay: float = 0.25) -> None:
        timer = threading.Timer(delay, emit_result, args=(request_id,))
        timer.daemon = True
        finish_timers.append(timer)
        timer.start()

    def handle_initialize(obj) -> None:
        nonlocal initialized
        version = obj.get("protocol_version")
        if version != 1:
            emit(
                {
                    "type": "error",
                    "request_id": None,
                    "code": "unsupported_protocol_version",
                    "server_protocol_version": 1,
                    "message": f"Unsupported protocol version: {version}.",
                }
            )
            sys.stdout.flush()
            os._exit(42)

        emit(ready_frame())
        if args.garbage_on_start:
            print("this is not json", flush=True)
        initialized = True

    def handle_request(obj) -> None:
        if not initialized:
            emit(
                {
                    "type": "error",
                    "request_id": obj.get("request_id"),
                    "code": "not_initialized",
                    "message": "Not initialized.",
                }
            )
            return

        required = ("request_id", "thread_id", "prompt", "workspace_root")
        if any(obj.get(field) is None for field in required):
            emit(
                {
                    "type": "error",
                    "request_id": obj.get("request_id"),
                    "code": "malformed_request",
                    "message": "Missing required request fields.",
                }
            )
            return

        request_id = obj["request_id"]
        prompt = obj.get("prompt") or ""

        # feat_thread_clone: deterministic ack for the clone command. The ack
        # text repeats the received frame (destination thread id + raw args)
        # so tests can assert the wire shape end to end. Missing args answer
        # the same invalid_request code the real backend uses.
        if prompt.startswith("/new_thread_with_current_context"):
            parts = prompt.split(maxsplit=1)
            clone_args = parts[1] if len(parts) > 1 else ""
            emit({"type": "status", "request_id": request_id, "state": "queued"})
            emit({"type": "status", "request_id": request_id, "state": "running"})
            if clone_args:
                emit(
                    {
                        "type": "result",
                        "request_id": request_id,
                        "content": (
                            "Thread cloned. "
                            f"dest={obj['thread_id']} args={clone_args}"
                        ),
                    }
                )
            else:
                emit(
                    {
                        "type": "error",
                        "request_id": request_id,
                        "code": "invalid_request",
                        "message": (
                            "Invalid source thread id: ''. Usage: "
                            "/new_thread_with_current_context <source_thread_id> [name]"
                        ),
                    }
                )
            return

        inflight.add(request_id)
        time.sleep(0.05)
        emit({"type": "status", "request_id": request_id, "state": "queued"})
        time.sleep(0.10)
        emit({"type": "status", "request_id": request_id, "state": "running"})
        if args.crash_after_running:
            sys.stdout.flush()
            os._exit(69)
        schedule_result(request_id)

    def handle_cancel(obj) -> None:
        request_id = obj.get("request_id")
        if request_id in inflight:
            inflight.discard(request_id)
            emit({"type": "status", "request_id": request_id, "state": "running"})
            emit(
                {
                    "type": "error",
                    "request_id": request_id,
                    "code": "cancelled",
                    "message": "Request cancelled.",
                }
            )
        else:
            emit(
                {
                    "type": "error",
                    "request_id": request_id,
                    "code": "unknown_request",
                    "message": "Unknown request id.",
                }
            )

    try:
        for raw_line in sys.stdin:
            raw_line = raw_line.strip()
            if not raw_line:
                continue
            try:
                obj = json.loads(raw_line)
            except json.JSONDecodeError:
                emit(
                    {
                        "type": "error",
                        "request_id": None,
                        "code": "malformed_request",
                        "message": "Malformed request.",
                    }
                )
                continue

            message_type = obj.get("type")
            if message_type == "initialize":
                handle_initialize(obj)
            elif message_type == "request":
                handle_request(obj)
            elif message_type == "cancel":
                handle_cancel(obj)
            elif message_type == "shutdown":
                sys.stdout.flush()
                return 0
            else:
                emit(
                    {
                        "type": "error",
                        "request_id": obj.get("request_id"),
                        "code": "unknown_message",
                        "message": f"Unknown message type: {message_type}.",
                    }
                )
    except BrokenPipeError:
        return 0
    finally:
        for timer in finish_timers:
            timer.cancel()

    return 0


if __name__ == "__main__":
    sys.exit(main())
