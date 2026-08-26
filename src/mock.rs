//! Static mock conversations. All data lives in code — no I/O, no network.

use crate::model::Message;

pub const MOCK_REPLIES: &[&str] = &[
    "## Quick answer\n\nHere is the short version:\n\n- `chibi ide --stdio` speaks **JSONL v1**\n- every line is one JSON object\n- handshake comes first: `initialize` → `ready`\n\n```rust\nlet msg: Message = serde_json::from_str(&line)?;\n```\n\nWant me to go deeper on cancel semantics?",
    "### Plan\n\n1. Extract the transport layer (`mpsc` based)\n2. Keep the UI as a pure renderer\n3. Swap mocks for the real JSONL client later\n\n> The protocol only exposes coarse `status` events, so the spinner is driven by `queued`/`running`.\n\n```python\ndef parse(line: str) -> dict:\n    return json.loads(line)\n```\n\nThat keeps the swap-in painless.",
    "Found it — the issue was a **missing handshake**. Until `initialize` completes with `ready`, every request fails with `not_initialized`.\n\n- Fix: await `ready` before sending anything\n- Also correlate replies by `request_id`\n\n```rust\nmatch event {\n    Event::Status(s) => ui.set_status(s),\n    Event::Result(r) => ui.push_answer(r),\n    _ => {}\n}\n```\n\nShould be stable now.",
    "## Notes on threading\n\nTokio tasks are cheap, but the render loop should stay on the main task:\n\n- `tokio::select!` between key events and backend messages\n- render **on change**, not on a fixed tick\n- keep `crossterm` polling non-blocking via `event::poll`\n\n```python\nimport asyncio\n\nasync def main():\n    await tui.run()\n```\n\nHappy to sketch the full select loop if useful.",
];

fn chat(name: &str, messages: Vec<Message>) -> crate::app::Chat {
    crate::app::Chat {
        name: name.to_string(),
        id: crate::history::new_thread_id(),
        messages,
        lifecycle: crate::model::ChatLifecycle::Idle,
        queue: std::collections::VecDeque::new(),
    }
}

/// Pre-filled chats shown at startup.
pub fn initial_chats() -> Vec<crate::app::Chat> {
    vec![
        chat(
            "JSONL protocol",
            vec![
                Message::user("How does the IDE integration actually talk to Chibi?"),
                Message::assistant(
                    "The editor spawns Chibi as a **child process** and speaks newline-delimited JSON over stdio.\n\n## Handshake\n\n1. client sends `initialize` with `protocol_version: 1`\n2. backend answers `ready`\n3. normal traffic starts\n\nUntil `ready` arrives every request is rejected with `not_initialized`.\n\n| Message | Direction | Meaning |\n|---|---|---|\n| `initialize` | client → chibi | protocol handshake |\n| `ready` | chibi → client | session accepted |\n| `request` | client → chibi | one prompt |\n| `result` | chibi → client | final markdown answer |",
                ),
                Message::user("And what does a request look like?"),
                Message::assistant(
                    "Every request carries correlation metadata:\n\n- `request_id` — unique per request, used for cancel\n- `thread_id` — stable per chat\n- `prompt` — the actual user text\n- `workspace_root` — absolute path, required\n\nOptional fields like `active_file` stay `null` in the TUI. On the wire it is a frozen dataclass:\n\n```python\n@dataclass(frozen=True)\nclass Request:\n    request_id: str\n    thread_id: str\n    prompt: str\n    workspace_root: str\n    active_file: str | None = None\n\n    def to_jsonl(self) -> str:\n        payload = asdict(self)\n        return json.dumps(payload, ensure_ascii=False)\n```\n\nReplies arrive either as `result`, `error` or coarse `status` updates (`queued` / `running`).",
                ),
            ],
        ),
        chat(
            "Model choice",
            vec![
                Message::user("Which model should I pick for long refactors?"),
                Message::assistant(
                    "For large mechanical edits prefer models with big context windows:\n\n| Model | Provider | Context |\n|---|---|---:|\n| glm-5.2 | ZhipuAI | 200k |\n| kimi-k2.7 | MoonshotAI | 256k |\n| deepseek-v4 | DeepSeek | 128k |\n| qwen3.5-max | Alibaba | 1M |\n\nFor quick Q&A a fast cheap model is usually enough — latency dominates. The same rule as a function:\n\n```rust\nasync fn pick_model<'a>(\n    models: &'a [ModelInfo],\n    budget: &Budget,\n) -> Result<&'a ModelInfo, PickError> {\n    let fits: Vec<_> = models.iter()\n        .filter(|m| m.context_tokens >= budget.min_context)\n        .collect();\n    match fits.iter().min_by_key(|m| m.cost_per_1k) {\n        Some(best) => Ok(best),\n        None => Err(PickError::NoModelFits),\n    }\n}\n```",
                ),
            ],
        ),
        chat(
            "Refactor utils.py",
            vec![
                Message::user("Refactor utils.py — split it into focused modules."),
                Message::assistant(
                    "Suggested split:\n\n1. `strings.py` — slugify, truncation\n2. `datetime.py` — parsing helpers\n3. `fs.py` — safe read/write wrappers\n\nKeep `utils.py` as a thin re-export layer so existing imports don't break:\n\n```python\nfrom .strings import slugify\nfrom .datetime import parse_dt  # noqa: F401\n```\n\nI can prepare the diff once you confirm the layout.",
                ),
                Message::user("Confirmed, go ahead."),
                Message::assistant(
                    "Done — three new modules plus the re-export shim.\n\n**Verification:**\n\n```bash\npytest tests/utils -q   # 42 passed\nruff check .            # clean\n```\n\nNothing else referenced the moved symbols directly, so no call-site changes were needed.",
                ),
            ],
        ),
        chat(
            "Ideas",
            vec![
                Message::user("Any ideas for the TUI roadmap?"),
                Message::assistant(
                    "A few candidates, roughly in order of impact:\n\n- **Syntax highlighting** in code blocks (syntect)\n- Streaming tokens — needs protocol v1.1 on the backend side\n- `/model` picker with fuzzy search\n- Local history search (`Ctrl+R` style)\n\nThe streaming one unlocks real typing feel but requires coordinated backend work.",
                ),
            ],
        ),
    ]
}
