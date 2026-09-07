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

Wave-2 behaviour flags (frames added alongside the base behaviour; thoughts
and subagent frames additionally require the matching client capability
declared in the handshake):

``--with-usage``         result frames gain ``usage`` {input_tokens,
                         output_tokens, context_window: 131072}
``--usage-windowless``   like ``--with-usage`` but ``context_window: null``
                         (implies ``--with-usage``)
``--with-thoughts``      result frames gain a ``thoughts`` string when the
                         handshake declared ``capabilities.thoughts``
``--thoughts-huge``      the thoughts payload is a >64KB trace capped to 64KB
                         with the backend truncation marker (implies
                         ``--with-thoughts``)
``--with-subagents``     each request emits a mid-turn ``agent_event``
                         sequence (started → started → finished → finished,
                         active back to 0) when the handshake declared
                         ``capabilities.subagents``
``--late-finish``        background-subagent variant (implies
                         ``--with-subagents``): the two ``finished`` frames
                         move AFTER the result frame (subagents outliving
                         their turn), guarded on the result having actually
                         been emitted
``--with-unknown-frame`` one unknown-type frame is emitted mid-turn (drives
                         the client's diag-warning path while the request
                         still completes)
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

# Wave-2 thoughts contract: the backend caps reasoning at 64KB (UTF-8-safe)
# and appends a truncation marker when the cap bites.
MAX_THOUGHTS_BYTES = 64 * 1024
THOUGHTS_TRUNCATION_MARKER = "\n[... LLM reasoning truncated: 64 KB limit reached ...]"

THOUGHTS_TRACE = "\n".join(
    [
        "Weighing the request against protocol v1 constraints.",
        "Checking protocol constraints: usage and thoughts ride on the result.",
        "Drafting the answer: one sentence, deterministic.",
    ]
)

# Usage payload riding on --with-usage result frames (deterministic values
# chosen so the TUI's ctx segment maths is checkable in render assertions).
USAGE_INPUT_TOKENS = 18432
USAGE_OUTPUT_TOKENS = 512
USAGE_CONTEXT_WINDOW = 131072


def cap_thoughts(thoughts: str) -> str:
    """Cap reasoning text at MAX_THOUGHTS_BYTES with a truncation marker.

    UTF-8-safe: the byte cut never splits a code point, so the capped
    payload stays a valid string on the wire.

    Args:
        thoughts: Full reasoning text produced for a request.

    Returns:
        The reasoning text, capped at the byte budget with the marker.
    """
    encoded = thoughts.encode("utf-8")
    if len(encoded) <= MAX_THOUGHTS_BYTES:
        return thoughts
    budget = MAX_THOUGHTS_BYTES - len(THOUGHTS_TRUNCATION_MARKER.encode("utf-8"))
    return encoded[:budget].decode("utf-8", errors="ignore") + THOUGHTS_TRUNCATION_MARKER


def huge_thoughts_trace() -> str:
    """Build a deterministic multi-line reasoning trace larger than 64KB.

    Returns:
        An ASCII trace of roughly 71KB, one numbered line per step.
    """
    return "\n".join(f"step {i:04d}: " + "d" * 60 for i in range(1000))


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
    parser = argparse.ArgumentParser(description="Fake chibi stdio --tui backend.")
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
    parser.add_argument(
        "--with-usage",
        action="store_true",
        help="result frames carry usage {input_tokens, output_tokens, context_window}",
    )
    parser.add_argument(
        "--usage-windowless",
        action="store_true",
        help="report context_window: null on result usage (implies --with-usage)",
    )
    parser.add_argument(
        "--with-thoughts",
        action="store_true",
        help="result frames carry thoughts when the client opted in at handshake",
    )
    parser.add_argument(
        "--thoughts-huge",
        action="store_true",
        help="thoughts payload exceeds 64KB and is capped with the truncation marker",
    )
    parser.add_argument(
        "--with-subagents",
        action="store_true",
        help="mid-turn agent_event sequence when the client opted in at handshake",
    )
    parser.add_argument(
        "--late-finish",
        action="store_true",
        help="move the finished agent_event frames after the result frame "
        "(implies --with-subagents)",
    )
    parser.add_argument(
        "--with-unknown-frame",
        action="store_true",
        help="emit one unknown-type frame mid-turn",
    )
    args = parser.parse_args()
    if args.usage_windowless:
        args.with_usage = True
    if args.thoughts_huge:
        args.with_thoughts = True
    if args.late_finish:
        args.with_subagents = True

    thoughts_payload = ""
    if args.with_thoughts:
        trace = huge_thoughts_trace() if args.thoughts_huge else THOUGHTS_TRACE
        thoughts_payload = cap_thoughts(trace)

    initialized = False
    caps_thoughts = False
    caps_subagents = False
    # request ids believed to be in flight (result timers may still fire)
    inflight = set()
    # request ids whose result frame actually went out on the wire; the
    # --late-finish timers fire only for these (cancelled requests never
    # get a result, so their late finishes must stay silent too)
    results_emitted = set()
    finish_timers = []

    def emit_result(request_id: str) -> None:
        if request_id not in inflight:
            return  # cancelled (or crash raced): never answer twice
        inflight.discard(request_id)
        results_emitted.add(request_id)
        result = {
            "type": "result",
            "request_id": request_id,
            "content": RESULT_CONTENT,
        }
        if not args.result_without_model:
            result["model"] = "gpt-example"
            result["provider"] = "openai"
        if args.with_usage:
            result["usage"] = {
                "input_tokens": USAGE_INPUT_TOKENS,
                "output_tokens": USAGE_OUTPUT_TOKENS,
                "context_window": None if args.usage_windowless else USAGE_CONTEXT_WINDOW,
            }
        if args.with_thoughts and caps_thoughts:
            result["thoughts"] = thoughts_payload
        emit(result)

    def schedule_result(request_id: str, delay: float = 0.25) -> None:
        timer = threading.Timer(delay, emit_result, args=(request_id,))
        timer.daemon = True
        finish_timers.append(timer)
        timer.start()

    def schedule_late_finishes(request_id: str) -> None:
        """Schedule the two finished frames strictly AFTER the result timer.

        Delays guarantee wire order result → finished(1,2) → finished(0,2);
        each timer is a no-op unless the result was actually emitted (a
        cancelled request produces neither result nor late finishes).

        Args:
            request_id: The request whose background subagents outlive it.
        """

        def emit_late(active: int) -> None:
            if request_id not in results_emitted:
                return
            emit(
                {
                    "type": "agent_event",
                    "request_id": request_id,
                    "event": "finished",
                    "active": active,
                    "total": 2,
                }
            )

        for delay, active in ((0.40, 1), (0.55, 0)):
            timer = threading.Timer(delay, emit_late, args=(active,))
            timer.daemon = True
            finish_timers.append(timer)
            timer.start()

    def handle_initialize(obj) -> None:
        nonlocal initialized, caps_thoughts, caps_subagents
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

        # Wave-2 gating: each capability turns on only via an explicit true,
        # mirroring the real backend (absent/false fields mean "off").
        caps = obj.get("capabilities") or {}
        caps_thoughts = caps.get("thoughts") is True
        caps_subagents = caps.get("subagents") is True

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
        if args.with_unknown_frame:
            emit(
                {
                    "type": "holo_deck",
                    "request_id": request_id,
                    "payload": "beams",
                }
            )
        if args.with_subagents and caps_subagents:
            # --late-finish: the finishes ride post-result timers instead of
            # the inline mid-turn sequence (background subagents outlive the
            # turn), so the inline run emits only the two started frames.
            events = (
                (("started", 1, 1, "scout"), ("started", 2, 2, "indexer"))
                if args.late_finish
                else (
                    ("started", 1, 1, "scout"),
                    ("started", 2, 2, "indexer"),
                    ("finished", 1, 2, None),
                    ("finished", 0, 2, None),
                )
            )
            for event, active, total, name in events:
                frame = {
                    "type": "agent_event",
                    "request_id": request_id,
                    "event": event,
                    "active": active,
                    "total": total,
                }
                if name is not None:
                    frame["name"] = name
                emit(frame)
                time.sleep(0.05)
        schedule_result(request_id)
        if args.late_finish and caps_subagents:
            schedule_late_finishes(request_id)

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
