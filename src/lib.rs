//! Chibi TUI library crate — the reusable core of the terminal client.
//!
//! The binary (`main.rs`) only wires the terminal; everything testable lives
//! here. `protocol` defines the serde types of IDE protocol v1, consumed by
//! the real `backend_client` / `request_pipeline` pair.
//!
//! Backend sources: [`backend::MockBackend`] (canned answers for demos) and
//! [`live::LiveBackend`] (real JSONL peer via [`request_pipeline`]). Both
//! surface progress as [`backend::BackendEvent`] streams, so UI code is
//! agnostic to which one is wired in.

pub mod app;
pub mod backend;
pub mod backend_client;
pub mod clipboard;
pub mod diag;
pub mod history;
pub mod live;
pub mod markdown;
pub mod mock;
pub mod model;
pub mod popup;
pub mod protocol;
pub mod request_pipeline;
pub mod splash;
pub mod theme;
pub mod ui;

// Public re-exports: the protocol type surface consumed by `backend_client`
// (task 3a) — keep stable.
pub use crate::protocol::{
    Capabilities, ClientInfo, ClientMessage, CursorPosition, ErrorCode, ProtocolVersion, Selection,
    ServerInfo, ServerMessage, StatusState,
};

// Public re-exports: the process lifecycle surface of the real client (3a).
pub use crate::backend_client::{BackendClient, BackendError};

// Public re-exports: request correlation surface of the real client (3b).
pub use crate::request_pipeline::{RequestArgs, RequestPipeline, StatusUpdate};

// Public re-exports: application-state surface bridging backend → UI (task 4).
pub use crate::app::{App, Chat, Connection, Submitted};
pub use crate::live::LiveBackend;
