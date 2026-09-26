//! Keyboard-dispatch behavior tests (moved verbatim from `src/tests/`):
//! every topic module that drives the key handler via `press()` /
//! `handle_key` / `handle_key_with_copy`, including the Cyrillic chord
//! tests and the DISPATCH_CHORDS drift guard. Shared helpers live in
//! `crate::tests::support` (used by both keymap and the remaining
//! `src/tests/` topics — deliberately not duplicated).

mod cyrillic;
mod delete_confirm;
mod error_popup;
mod esc_ctrl_n;
mod global_search;
mod help;
mod input_clipboard;
pub(crate) mod log_viewer;
mod model_picker;
mod paste;
mod quit_confirm;
mod rename;
mod search;
mod sidebar;
mod stop_reset;
mod submit_gate;
mod thread_arrows;
mod toggles;
