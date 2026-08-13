//! Command abstraction (§15–16): an action registry decoupled from input
//! handlers so the command palette / customization / platform bindings can
//! be layered on later without touching the engine.

use serde::{Deserialize, Serialize};

use crate::model::{PaneId, SplitDirection, WorkspaceId};

/// Phase 1 commands (terminal/workspace actions only — no agent commands).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    SplitHorizontal,
    SplitVertical,
    ClosePane,
    FocusNext,
    FocusPrevious,
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    ZoomPane,
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
    NewWorkspace,
    SwitchWorkspace(WorkspaceId),
    CloseWorkspace,
}

/// A key chord: optional modifiers + a printable key name (e.g. "d", "w",
/// "F2", "Tab", "ArrowRight"). Kept platform-neutral; the desktop maps its
/// winit key events onto these names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: String,
}

impl KeyChord {
    pub fn plain(key: &str) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            key: key.to_string(),
        }
    }
    pub fn ctrl(key: &str) -> Self {
        Self {
            ctrl: true,
            alt: false,
            shift: false,
            key: key.to_string(),
        }
    }
    pub fn ctrl_shift(key: &str) -> Self {
        Self {
            ctrl: true,
            alt: false,
            shift: true,
            key: key.to_string(),
        }
    }
    pub fn alt(key: &str) -> Self {
        Self {
            ctrl: false,
            alt: true,
            shift: false,
            key: key.to_string(),
        }
    }
}

/// Default Phase 1 bindings (macOS-style: Cmd = ctrl here since winit on
/// macOS reports Cmd via `super_key`; the desktop adapts).
pub fn default_bindings() -> Vec<(KeyChord, Command)> {
    vec![
        (KeyChord::ctrl("d"), Command::SplitHorizontal),
        (KeyChord::ctrl_shift("d"), Command::SplitVertical),
        (KeyChord::ctrl("w"), Command::ClosePane),
        (KeyChord::ctrl("]"), Command::FocusNext),
        (KeyChord::ctrl("["), Command::FocusPrevious),
        (KeyChord::ctrl("t"), Command::NewTab),
        (KeyChord::ctrl_shift("t"), Command::CloseTab),
        (KeyChord::ctrl("Tab"), Command::NextTab),
        (KeyChord::ctrl_shift("Tab"), Command::PreviousTab),
        (KeyChord::ctrl("n"), Command::NewWorkspace),
        (KeyChord::ctrl("z"), Command::ZoomPane),
        (KeyChord::alt("ArrowLeft"), Command::ResizePaneLeft),
        (KeyChord::alt("ArrowRight"), Command::ResizePaneRight),
        (KeyChord::alt("ArrowUp"), Command::ResizePaneUp),
        (KeyChord::alt("ArrowDown"), Command::ResizePaneDown),
    ]
}

/// Maps chords to commands. Later phases add palette entries and
/// per-user rebinding here.
#[derive(Debug, Clone, Default)]
pub struct CommandRegistry {
    bindings: Vec<(KeyChord, Command)>,
}

impl CommandRegistry {
    pub fn with_defaults() -> Self {
        Self {
            bindings: default_bindings(),
        }
    }

    pub fn bind(&mut self, chord: KeyChord, cmd: Command) {
        self.bindings.retain(|(c, _)| c != &chord);
        self.bindings.push((chord, cmd));
    }

    pub fn lookup(&self, chord: &KeyChord) -> Option<&Command> {
        self.bindings
            .iter()
            .find(|(c, _)| c == chord)
            .map(|(_, cmd)| cmd)
    }

    pub fn all_commands(&self) -> impl Iterator<Item = &Command> {
        self.bindings.iter().map(|(_, cmd)| cmd)
    }
}

/// Helper for pane-split targets resolved by the desktop/CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitTarget {
    pub pane_id: PaneId,
    pub direction: SplitDirection,
}
