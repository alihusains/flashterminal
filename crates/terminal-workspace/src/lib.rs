//! # terminal-workspace — Native Multiplexer + Workspace Engine (Phase 1)
//!
//! A UI-agnostic workspace engine:
//!
//! ```text
//! Workspace ── owns ──▶ Tabs ── owns ──▶ Pane split tree (pure data)
//! Pane      ── references ──▶ SessionId
//! Multiplexer ── owns ──▶ Session (PTY + parser + state) per SessionId
//! LayoutEngine ── computes ──▶ pane rectangles per frame
//! ```
//!
//! The domain model (`model`, `pane_tree`, `layout`, `persist`) is pure data
//! and fully serializable; the runtime (`engine`) owns live PTY sessions and
//! applies fairness-aware batched draining (§27–28). `command` provides the
//! shortcut/command registry, `notify` the notification abstraction, and
//! `ipc` a clean Request/Response/Event protocol for the CLI (and later,
//! agents) to drive a running application.
//!
//! Nothing here depends on a GUI framework.

pub mod command;
pub mod engine;
pub mod events;
pub mod ipc;
pub mod layout;
pub mod model;
pub mod notify;
pub mod pane_tree;
pub mod persist;

pub use command::{Command, CommandRegistry, KeyChord, SplitTarget};
pub use engine::{DrainResult, EngineMetrics, Multiplexer};
pub use events::{EventBus, EventFilter, Subscription};
pub use layout::{LayoutEngine, PaneRect, Rect};
pub use model::{
    new_id, Pane, PaneId, PersistedState, SessionId, SplitDirection, Tab, TabId, Workspace,
    WorkspaceId,
};
pub use notify::{Notification, NotificationCenter, NotificationKind};
pub use pane_tree::PaneNode;
pub use terminal_session;
