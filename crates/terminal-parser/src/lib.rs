//! Terminal VT/ANSI parser.
//!
//! Consumes raw PTY bytes and emits compact `TerminalEvent`s. The parser
//! holds **no terminal state**: every escape sequence is translated into one
//! or more events (SGR sends the raw parameter list; the state owner applies
//! it). This keeps a single source of truth for attributes, modes, and
//! cursor, and lets the parser run on a dedicated thread purely as a
//! byte→event transducer.
//!
//! The `vte::Parser` state machine and the [`Perform`] implementation are
//! split into two structs so that `advance` does not need two simultaneous
//! mutable borrows of the same object.

use terminal_core::TerminalEvent;
use vte::{Params, Perform};

/// The parser that consumes bytes and produces TerminalEvents.
pub struct Parser {
    parser: vte::Parser,
    performer: Performer,
}

/// The `Perform` target: accumulates semantic events from the byte stream.
struct Performer {
    events: Vec<TerminalEvent>,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            parser: vte::Parser::new(),
            performer: Performer {
                events: Vec::with_capacity(256),
            },
        }
    }

    pub fn advance(&mut self, byte: u8) {
        self.parser.advance(&mut self.performer, byte);
    }

    pub fn advance_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.parser.advance(&mut self.performer, byte);
        }
    }

    /// Consumes and returns the accumulated events, clearing the internal buffer.
    pub fn take_events(&mut self) -> Vec<TerminalEvent> {
        std::mem::take(&mut self.performer.events)
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Perform for Performer {
    fn print(&mut self, c: char) {
        self.events.push(TerminalEvent::WriteChar(c));
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => self.events.push(TerminalEvent::CursorBack(1)), // BS
            0x09 => self.events.push(TerminalEvent::Tab),           // HT
            0x0A..=0x0C => self.events.push(TerminalEvent::CursorDown(1)), // LF, VT, FF
            0x0D => self.events.push(TerminalEvent::CursorToBeginningOfLine), // CR
            _ => { /* other C0 controls are ignored (bell etc.) */ }
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if params.len() >= 2 {
            let param = params[0];
            if param == b"0" || param == b"2" {
                if let Ok(title) = std::str::from_utf8(params[1]) {
                    self.events.push(TerminalEvent::SetTitle(title.to_string()));
                }
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        // In vte 0.11 the private-mode marker (`?`) arrives as an
        // intermediate byte rather than a field on `Params`.
        let private = intermediates.contains(&b'?');

        let first = params.iter().next().map(|p| p[0]).unwrap_or(0);
        let at = |i: usize| -> u16 { params.iter().nth(i).map(|p| p[0]).unwrap_or(0) };

        match action {
            // ---- Cursor ---------------------------------------------------
            'A' => self.events.push(TerminalEvent::CursorUp(first.max(1))),
            'B' | 'e' => self.events.push(TerminalEvent::CursorDown(first.max(1))),
            'C' | 'a' => self.events.push(TerminalEvent::CursorForward(first.max(1))),
            'D' => self.events.push(TerminalEvent::CursorBack(first.max(1))),
            'E' => {
                self.events.push(TerminalEvent::CursorToBeginningOfLine);
                self.events.push(TerminalEvent::CursorDown(first.max(1)));
            }
            'F' => {
                self.events.push(TerminalEvent::CursorToBeginningOfLine);
                self.events.push(TerminalEvent::CursorUp(first.max(1)));
            }
            'G' | '`' => self.events.push(TerminalEvent::MoveCursor {
                col: first.max(1) - 1,
                row: u16::MAX, // column-only move; row unchanged
            }),
            'H' | 'f' => {
                let row = first.max(1) - 1;
                let col = at(1).max(1) - 1;
                self.events.push(TerminalEvent::MoveCursor { col, row });
            }
            'd' => self.events.push(TerminalEvent::MoveCursor {
                col: u16::MAX,
                row: first.max(1) - 1,
            }),
            // ---- Erase / edit ---------------------------------------------
            'J' => {
                let mode = if first <= 3 { first as u8 } else { 0 };
                self.events.push(TerminalEvent::ClearScreen(mode));
            }
            'K' => {
                let mode = if first <= 2 { first as u8 } else { 0 };
                self.events.push(TerminalEvent::ClearLine(mode));
            }
            'L' => self.events.push(TerminalEvent::InsertLines(first.max(1))),
            'M' => self.events.push(TerminalEvent::DeleteLines(first.max(1))),
            '@' => self.events.push(TerminalEvent::InsertChars(first.max(1))),
            'P' => self.events.push(TerminalEvent::DeleteChars(first.max(1))),
            'b' => self
                .events
                .push(TerminalEvent::RepeatLastChar(first.max(1))),
            'S' => self.events.push(TerminalEvent::ScrollUp(first.max(1))),
            'T' => self.events.push(TerminalEvent::ScrollDown(first.max(1))),
            // ---- SGR -------------------------------------------------------
            'm' => {
                let mut sgrs = Vec::with_capacity(4);
                for p in params.iter() {
                    sgrs.push(p[0]);
                }
                self.events.push(TerminalEvent::Sgr(sgrs));
            }
            // ---- Modes (SM / RM, private with '?') -------------------------
            'h' | 'l' => {
                let set = action == 'h';
                let mode = first;
                if private {
                    let ev = match mode {
                        1 => Some(TerminalEvent::SetApplicationCursorKeys(set)),
                        7 => Some(TerminalEvent::SetAutoWrap(set)),
                        25 => Some(TerminalEvent::SetCursorVisible(set)),
                        47 | 1047 | 1049 => Some(TerminalEvent::SetAltScreen(set)),
                        2004 => Some(TerminalEvent::SetBracketedPaste(set)),
                        _ => None,
                    };
                    if let Some(ev) = ev {
                        self.events.push(ev);
                    }
                }
            }
            _ => { /* unused sequences are ignored */ }
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, byte: u8) {
        match byte {
            b'7' => self.events.push(TerminalEvent::SaveCursor),
            b'8' => self.events.push(TerminalEvent::RestoreCursor),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::TerminalState;

    fn parse_events(bytes: &[u8]) -> Vec<TerminalEvent> {
        let mut p = Parser::new();
        p.advance_bytes(bytes);
        p.take_events()
    }

    #[test]
    fn plain_text_becomes_write_chars() {
        let evs = parse_events(b"hi");
        assert_eq!(evs.len(), 2);
        assert!(matches!(evs[0], TerminalEvent::WriteChar('h')));
        assert!(matches!(evs[1], TerminalEvent::WriteChar('i')));
    }

    #[test]
    fn csi_cursor_moves() {
        let evs = parse_events(b"\x1b[5A\x1b[2C");
        assert!(matches!(evs[0], TerminalEvent::CursorUp(5)));
        assert!(matches!(evs[1], TerminalEvent::CursorForward(2)));
    }

    #[test]
    fn private_modes_detected() {
        let evs = parse_events(b"\x1b[?1049h\x1b[?25l");
        assert!(matches!(evs[0], TerminalEvent::SetAltScreen(true)));
        assert!(matches!(evs[1], TerminalEvent::SetCursorVisible(false)));
    }

    #[test]
    fn sgr_roundtrip() {
        let evs = parse_events(b"\x1b[38;2;1;2;3m");
        assert!(matches!(&evs[0], TerminalEvent::Sgr(p) if p == &vec![38, 2, 1, 2, 3]));
    }

    #[test]
    fn osc_title() {
        let evs = parse_events(b"\x1b]0;My Title\x07");
        assert!(matches!(&evs[0], TerminalEvent::SetTitle(t) if t == "My Title"));
    }

    #[test]
    fn utf8_multibyte_chars() {
        let evs = parse_events("你好".as_bytes());
        assert!(matches!(evs[0], TerminalEvent::WriteChar('你')));
        assert!(matches!(evs[1], TerminalEvent::WriteChar('好')));
    }

    #[test]
    fn end_to_end_state_update() {
        let mut parser = Parser::new();
        let mut state = TerminalState::new(20, 4);
        parser.advance_bytes(b"hi\x1b[31m there");
        for e in parser.take_events() {
            state.apply_event(e);
        }
        assert_eq!(state.visible_cell(0, 0).ch, 'h' as u32);
        assert_eq!(state.visible_cell(0, 1).ch, 'i' as u32);
        // Red SGR applies to subsequent writes.
        assert_eq!(
            state.visible_cell(0, 3).color_fg(),
            terminal_core::Color::Indexed(1)
        );
    }
}
