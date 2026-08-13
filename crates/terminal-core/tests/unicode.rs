//! Phase 0.5.1 §8 — Unicode validation.
//!
//! Verifies cursor position, cell width, selection/copy, scrolling, line
//! wrapping and resize behaviour for the spec's reference strings:
//! `hello`, `é`, `e + combining acute`, `你好`, `こんにちは`, `مرحبا`,
//! `🙂`, `🚀`, `🏳️🌈`, plus mixed combinations.

use terminal_core::{flags, TerminalState};

/// Renders a visible row to text; wide-continuation cells are skipped so
/// each base character appears once, combining marks are appended.
fn row_text(state: &TerminalState, row: u16) -> String {
    let mut out = String::new();
    for c in 0..state.cols {
        let cell = state.visible_cell(row, c);
        if cell.ch == 0 {
            continue;
        }
        if cell.is_wide_continuation() {
            continue;
        }
        if let Some(ch) = char::from_u32(cell.ch) {
            out.push(ch);
        }
    }
    out
}

/// Writes a string at the home position of a fresh grid, returns the state.
fn write_string(s: &str) -> TerminalState {
    let mut state = TerminalState::new(80, 24);
    for c in s.chars() {
        state.write_char(c);
    }
    state
}

fn width_of(state: &TerminalState, row: u16, col: u16) -> u8 {
    state.visible_cell(row, col).width
}

#[test]
fn ascii_hello() {
    let s = write_string("hello");
    assert_eq!(row_text(&s, 0), "hello");
    assert_eq!(s.cursor.col, 5);
    assert_eq!(s.cursor.row, 0);
}

#[test]
fn precomposed_accent() {
    let s = write_string("é");
    assert_eq!(s.cursor.col, 1);
    assert_eq!(width_of(&s, 0, 0), 1);
    assert_eq!(s.visible_cell(0, 0).ch, 'é' as u32);
}

#[test]
fn combining_acute() {
    let s = write_string("e\u{301}");
    // Base + zero-width combining mark occupy one logical position.
    assert_eq!(s.visible_cell(0, 0).ch, 'e' as u32);
    assert!(s.visible_cell(0, 1).is_combining());
    assert_eq!(s.visible_cell(0, 1).ch, '\u{301}' as u32);
    assert_eq!(s.cursor.col, 1);
}

#[test]
fn cjk_widths_and_cursor() {
    let s = write_string("你好");
    assert_eq!(row_text(&s, 0), "你好");
    assert_eq!(width_of(&s, 0, 0), 2);
    assert!(s.visible_cell(0, 1).is_wide_continuation());
    assert_eq!(width_of(&s, 0, 2), 2);
    assert_eq!(s.cursor.col, 4);
}

#[test]
fn kana_widths_and_cursor() {
    let s = write_string("こんにちは");
    assert_eq!(row_text(&s, 0), "こんにちは");
    // Kana are double-width (Unicode East Asian W): 5 chars × 2 cells.
    assert_eq!(s.cursor.col, 10);
    for c in 0..10 {
        assert_eq!(width_of(&s, 0, c), if c % 2 == 0 { 2 } else { 0 });
    }
}

#[test]
fn arabic_renders_but_not_shaped() {
    let s = write_string("مرحبا");
    assert_eq!(row_text(&s, 0), "مرحبا");
    assert_eq!(s.cursor.col, 5);
    // No complex-script shaping in this phase (documented); each letter is
    // a width-1 cell with presentation handled by the font fallback.
    for c in 0..5 {
        assert_eq!(width_of(&s, 0, c), 1);
    }
}

#[test]
fn emoji_are_wide() {
    let s = write_string("🙂🚀");
    assert_eq!(width_of(&s, 0, 0), 2);
    assert!(s.visible_cell(0, 1).is_wide_continuation());
    assert_eq!(width_of(&s, 0, 2), 2);
    assert_eq!(s.cursor.col, 4);
}

#[test]
fn rainbow_flag_zwj_cluster() {
    let mut s = write_string("🏳\u{fe0f}\u{200d}🌈");
    // ZWJ/VS merge into the base cell; the cluster is one wide cell.
    assert_eq!(width_of(&s, 0, 0), 2);
    assert!(s.visible_cell(0, 1).is_wide_continuation());
    assert_eq!(s.cursor.col, 2);
    // Selection/copy currently emits the cluster's base scalar only (the
    // full sequence is not reconstructed — documented limitation until
    // text shaping lands).
    s.set_selection((0, 0), (0, 1));
    assert_eq!(s.selection_text(), "🏳");
}

#[test]
fn selection_copy_mixed_ascii_cjk() {
    let mut s = TerminalState::new(80, 24);
    for c in "ab你好cd".chars() {
        s.write_char(c);
    }
    // a b 你 好 c d → cols 0,1,2-3,4-5,6,7; cursor at 8.
    assert_eq!(s.cursor.col, 8);
    s.set_selection((0, 0), (0, 7));
    assert_eq!(s.selection_text(), "ab你好cd");
}

#[test]
fn selection_copy_combining_alone() {
    // e + combining with nothing after: the mark is preserved in copy.
    let mut s = TerminalState::new(80, 24);
    for c in "e\u{301}".chars() {
        s.write_char(c);
    }
    s.set_selection((0, 0), (0, 1));
    assert_eq!(s.selection_text(), "e\u{301}");
}

/// Documented model limitation (see ADR 0004 / report): a plain combining
/// mark is stored as a zero-width cell at the cursor. Writing the next base
/// character overwrites that cell, so the accent is dropped (xterm keeps it
/// attached to the base). Pinned here so the behavior is explicit; the fix
/// belongs to the Phase 2 text-shaping work item (Cell has no combining
/// storage slot).
#[test]
fn combining_mark_overwritten_by_next_base() {
    let mut s = TerminalState::new(80, 24);
    for c in "e\u{301}x".chars() {
        s.write_char(c);
    }
    s.set_selection((0, 0), (0, 1));
    assert_eq!(s.selection_text(), "ex");
}

#[test]
fn wide_char_at_margin_wraps() {
    let mut s = TerminalState::new(4, 3);
    s.write_char('a');
    s.write_char('你'); // width 2: cols 1-2, cursor parked at 3 (not wrapped)
    assert_eq!(s.cursor.col, 3);
    assert!(!s.pending_wrap);
    s.write_char('b'); // fills col 3 -> deferred wrap armed
    assert!(s.pending_wrap);
    s.write_char('c'); // wrap fires, lands on row 1
    assert_eq!(s.cursor.row, 1);
    assert_eq!(row_text(&s, 1), "c");
}

#[test]
fn mixed_ascii_emoji_wrap() {
    let mut s = TerminalState::new(5, 3);
    for c in "ab🚀c".chars() {
        s.write_char(c);
    }
    // a(0) b(1) 🚀(2-3), cursor at 4. 'c' fills the last column and parks
    // with a deferred wrap (xterm semantics); the wrap fires on 'd'.
    assert_eq!(s.cursor.row, 0);
    assert!(s.pending_wrap);
    assert_eq!(row_text(&s, 0), "ab🚀c");
    s.write_char('d');
    assert_eq!(s.cursor.row, 1);
    assert_eq!(row_text(&s, 1), "d");
}

#[test]
fn unicode_scrolls_to_scrollback() {
    let mut s = TerminalState::new(6, 2);
    for c in "你好世界你好世界".chars() {
        s.write_char(c);
    }
    // 6 cols = 3 wide chars per row; the 12 wide chars overflow the 2-row
    // window into scrollback (exact count depends on deferred-wrap
    // bookkeeping, so only assert that scrolling exposed the old content).
    assert!(s.scrollback_len() >= 1, "expected scrollback");
    s.set_scroll_offset(s.scrollback_len());
    // The oldest retained row is exposed above the viewport and is CJK.
    let top = row_text(&s, 0);
    assert!(!top.is_empty(), "scrolled view must show content");
    assert_eq!(
        top.chars().count() * 2,
        s.visible_row(0).cells.iter().map(|c| c.width).sum::<u8>() as usize
    );
    // Wide-cell structure is intact in the scrolled view.
    assert_eq!(width_of(&s, 0, 0), 2);
}

#[test]
fn wide_chars_survive_resize() {
    // Content near the bottom of the screen is preserved on shrink (the
    // resize keeps the bottom `rows` rows — standard cursor-region keep).
    let mut s = TerminalState::new(80, 24);
    s.cursor_position(0, 23);
    for c in "你好".chars() {
        s.write_char(c);
    }
    s.resize(80, 3);
    // 你好 was on visible row 23 → now visible row 2, structure intact.
    assert_eq!(s.visible_cell(2, 0).ch, '你' as u32);
    assert_eq!(width_of(&s, 2, 0), 2);
    assert!(s.visible_cell(2, 1).is_wide_continuation());
    assert_eq!(s.visible_cell(2, 2).ch, '好' as u32);
    // Growing preserves everything.
    s.resize(80, 5);
    assert_eq!(s.visible_cell(2, 0).ch, '你' as u32);
}

#[test]
fn combining_emoji_combination() {
    let mut s = TerminalState::new(80, 24);
    for c in "e\u{301}🙂é".chars() {
        s.write_char(c);
    }
    // e(0), combining overwritten by 🙂(1-2), é(3) → cursor 4. The accent
    // is dropped by the zero-width-cell overwrite (see
    // combining_mark_overwritten_by_next_base); emoji stays wide.
    assert_eq!(s.cursor.col, 4);
    assert_eq!(width_of(&s, 0, 1), 2);
    assert!(s.visible_cell(0, 2).is_wide_continuation());
    s.set_selection((0, 0), (0, 3));
    assert_eq!(s.selection_text(), "e🙂é");
}

#[test]
fn cell_width_sum_equals_cursor() {
    // Property check: after writing any mix, the cursor column equals the
    // sum of non-zero widths of the visible row (single-line case).
    for s in [
        "hello",
        "é",
        "e\u{301}",
        "你好",
        "こんにちは",
        "مرحبا",
        "🙂🚀",
        "ab你好cd",
    ] {
        let st = write_string(s);
        let row = st.visible_row(0);
        let width_sum: u16 = row
            .cells
            .iter()
            .map(|c| if c.is_empty() { 0 } else { u16::from(c.width) })
            .sum();
        assert_eq!(
            st.cursor.col, width_sum,
            "cursor must equal consumed cell width for {s:?}"
        );
    }
}

#[test]
fn flags_constants_stable() {
    // Guards the flag encoding used by the unicode model.
    assert_eq!(flags::WIDE_CONTINUATION, 1 << 0);
    assert_eq!(flags::COMBINING, 1 << 1);
    assert_eq!(flags::CLUSTER_JOINER, 1 << 2);
}
