//! Terminal Core
//!
//! Owns the authoritative terminal state model: grid, cursor, colors,
//! attributes, Unicode cell model, and dirty tracking.
//!
//! Design constraints (Phase 0.5 architecture audit):
//!
//! * **Single owner.** `TerminalState` is owned by exactly one thread (the UI
//!   thread). The parser feeds it `TerminalEvent`s; the renderer only *reads*
//!   it. No locks in the render path.
//! * **Packed cells.** `Cell` is exactly 16 bytes (ch, fg, bg, attrs, flags,
//!   width), so a 200x60 grid with 10K rows of scrollback stays far below the
//!   per-pane memory budget.
//! * **Unicode.** One Unicode scalar never equals one cell. A cell may be a
//!   wide (2-cell) base, a width-0 combining mark, or a width-0 continuation
//!   of a wide character. ZWJ/VS sequences merge into their base cell.
//! * **Dirty tracking.** A row-bitset tracks exactly which rows changed; the
//!   renderer consumes it. No full-grid repaints.
//! * **Scroll reuse.** Scrolled-out row buffers are recycled, not reallocated.
//!
//! Grid layout: `grid[0]` is the oldest row. The last `rows` entries form the
//! visible window; everything before them is scrollback. All cursor-based
//! operations address *visible* rows via [`TerminalState::visible_row_idx`].

use std::collections::VecDeque;

mod scrollback;

use scrollback::{compress_block, decode_scratch_row, encode_row_into};
pub use scrollback::{
    decode_block, encode_block, ColdBlock, ColdStore, BLOCK_ROWS, HOT_ROWS, MAX_PROMOTED_BLOCKS,
};

/// Terminal color, as used in the public API (parser events, inspectors).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// Packs into a u32: bits [31:30] tag (00=default, 01=indexed,
    /// 10=rgb), low 24 bits value.
    #[inline]
    pub fn to_packed(self) -> u32 {
        match self {
            Color::Default => 0,
            Color::Indexed(i) => 0x4000_0000 | (i as u32 & 0xFF),
            Color::Rgb(r, g, b) => 0x8000_0000 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
        }
    }

    #[inline]
    pub fn from_packed(p: u32) -> Self {
        match p >> 30 {
            0b01 => Color::Indexed(p as u8),
            0b10 => Color::Rgb((p >> 16) as u8, (p >> 8) as u8, p as u8),
            _ => Color::Default,
        }
    }
}

/// Text attributes for a cell, packed into a `u16`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attribute {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    pub reverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
}

pub const ATTR_BOLD: u16 = 1 << 0;
pub const ATTR_DIM: u16 = 1 << 1;
pub const ATTR_ITALIC: u16 = 1 << 2;
pub const ATTR_UNDERLINE: u16 = 1 << 3;
pub const ATTR_BLINK: u16 = 1 << 4;
pub const ATTR_REVERSE: u16 = 1 << 5;
pub const ATTR_HIDDEN: u16 = 1 << 6;
pub const ATTR_STRIKE: u16 = 1 << 7;

impl Attribute {
    #[inline]
    pub fn to_bits(self) -> u16 {
        let mut b = 0;
        if self.bold {
            b |= ATTR_BOLD;
        }
        if self.dim {
            b |= ATTR_DIM;
        }
        if self.italic {
            b |= ATTR_ITALIC;
        }
        if self.underline {
            b |= ATTR_UNDERLINE;
        }
        if self.blink {
            b |= ATTR_BLINK;
        }
        if self.reverse {
            b |= ATTR_REVERSE;
        }
        if self.hidden {
            b |= ATTR_HIDDEN;
        }
        if self.strikethrough {
            b |= ATTR_STRIKE;
        }
        b
    }

    #[inline]
    pub fn from_bits(b: u16) -> Self {
        Self {
            bold: b & ATTR_BOLD != 0,
            dim: b & ATTR_DIM != 0,
            italic: b & ATTR_ITALIC != 0,
            underline: b & ATTR_UNDERLINE != 0,
            blink: b & ATTR_BLINK != 0,
            reverse: b & ATTR_REVERSE != 0,
            hidden: b & ATTR_HIDDEN != 0,
            strikethrough: b & ATTR_STRIKE != 0,
        }
    }
}

/// Per-cell flags.
pub mod flags {
    /// Second cell of a double-width character; draws background only.
    pub const WIDE_CONTINUATION: u8 = 1 << 0;
    /// Zero-width mark (combining char, ZWJ, variation selector).
    pub const COMBINING: u8 = 1 << 1;
    /// This cell (or its base) ends a ZWJ/VS run: the next base character
    /// merges into the same cluster.
    pub const CLUSTER_JOINER: u8 = 1 << 2;
}

/// A single grid cell. Exactly 16 bytes, `Copy`.
///
/// `ch == 0` means "empty".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Cell {
    /// Unicode scalar value; 0 = empty.
    pub ch: u32,
    /// Packed foreground color.
    pub fg: u32,
    /// Packed background color.
    pub bg: u32,
    /// Packed attribute bits.
    pub attrs: u16,
    /// Cell flags (see [`flags`]).
    pub flags: u8,
    /// Display width in cells: 0 (combining), 1, or 2.
    pub width: u8,
}

impl Cell {
    #[inline]
    pub fn empty() -> Self {
        Self {
            ch: 0,
            fg: 0,
            bg: 0,
            attrs: 0,
            flags: 0,
            width: 1,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ch == 0
    }

    #[inline]
    pub fn is_wide_continuation(&self) -> bool {
        self.flags & flags::WIDE_CONTINUATION != 0
    }

    #[inline]
    pub fn is_combining(&self) -> bool {
        self.flags & flags::COMBINING != 0
    }

    #[inline]
    pub fn color_fg(&self) -> Color {
        Color::from_packed(self.fg)
    }

    #[inline]
    pub fn color_bg(&self) -> Color {
        Color::from_packed(self.bg)
    }

    #[inline]
    pub fn attribute(&self) -> Attribute {
        Attribute::from_bits(self.attrs)
    }
}

/// Cursor position and visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub is_hidden: bool,
}

/// A rectangular selection in *visible* terminal space.
///
/// Coordinates are inclusive `(row, col)` bounds with `0 <= row < rows` and
/// `0 <= col < cols`. Stored in visible space so the renderer can paint
/// selection backgrounds directly; this phase intentionally does not track
/// selection across scrollback (documented in ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Inclusive start `(row, col)`.
    pub start: (u16, u16),
    /// Inclusive end `(row, col)`.
    pub end: (u16, u16),
}

/// A single grid row.
#[derive(Debug, Clone)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub is_wrapped: bool,
}

impl Row {
    #[inline]
    pub fn new(cols: u16) -> Self {
        Self {
            cells: vec![Cell::empty(); cols as usize],
            is_wrapped: false,
        }
    }

    /// Clears the row, keeping its buffer (no realloc).
    #[inline]
    pub fn clear(&mut self) {
        self.cells.fill(Cell::empty());
        self.is_wrapped = false;
    }

    #[inline]
    pub fn clear_from(&mut self, start: u16) {
        self.cells[start as usize..].fill(Cell::empty());
    }

    #[inline]
    pub fn clear_to(&mut self, end_inclusive: u16) {
        self.cells[..=end_inclusive as usize].fill(Cell::empty());
    }
}

/// Which rows changed since the last frame.
///
/// Row-bitset over the *visible grid rows* plus flags for cursor, title,
/// scroll delta, and full redraw. Consumed by the renderer via
/// [`TerminalState::consume_dirty`].
#[derive(Debug, Clone, Default)]
pub struct DirtyTracker {
    row_bits: u128,
    row_count: u16,
    pub cursor_changed: bool,
    pub title_changed: bool,
    /// Positive = content scrolled up by N rows.
    pub scroll_delta: i32,
    pub full_redraw: bool,
}

impl DirtyTracker {
    pub fn new(row_count: u16) -> Self {
        Self {
            row_bits: 0,
            row_count,
            cursor_changed: false,
            title_changed: false,
            scroll_delta: 0,
            full_redraw: false,
        }
    }

    #[inline]
    pub fn mark_row(&mut self, row: u16) {
        if self.full_redraw {
            return;
        }
        if row as u32 >= self.row_count as u32 {
            self.full_redraw = true;
            self.row_bits = 0;
            return;
        }
        self.row_bits |= 1u128 << row;
    }

    pub fn mark_all(&mut self) {
        self.full_redraw = true;
        self.row_bits = 0;
        self.scroll_delta = 0;
    }

    /// Invalidates every visible row (used when the view window shifts).
    pub fn mark_scroll(&mut self, delta: i32) {
        self.scroll_delta = self.scroll_delta.saturating_add(delta);
        if self.row_count as u32 <= 128 {
            self.row_bits |= u128::MAX >> (128 - self.row_count as u32);
        } else {
            self.full_redraw = true;
            self.row_bits = 0;
        }
    }

    #[inline]
    pub fn is_clean(&self) -> bool {
        self.row_bits == 0
            && !self.cursor_changed
            && !self.title_changed
            && self.scroll_delta == 0
            && !self.full_redraw
    }

    /// Dirty row indices in ascending order (visible rows).
    pub fn dirty_rows(&self) -> Vec<u16> {
        let mut out = Vec::new();
        let mut bits = self.row_bits;
        while bits != 0 {
            let idx = bits.trailing_zeros() as u16;
            out.push(idx);
            bits &= bits - 1;
        }
        out
    }

    #[inline]
    pub fn is_row_dirty(&self, row: u16) -> bool {
        self.full_redraw
            || ((row as u32) < self.row_count as u32 && (self.row_bits >> row) & 1 == 1)
    }
}

/// High-level events produced by the parser and applied by the state owner.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// A printable character (UTF-8 decoded by vte).
    WriteChar(char),
    MoveCursor {
        col: u16,
        row: u16,
    },
    CursorUp(u16),
    CursorDown(u16),
    CursorForward(u16),
    CursorBack(u16),
    /// Horizontal tab: move to the next 8-column tab stop.
    Tab,
    CursorToBeginningOfLine,
    /// Erase display: 0=below, 1=above, 2=all, 3=all+scrollback.
    ClearScreen(u8),
    /// Erase line: 0=right, 1=left, 2=all.
    ClearLine(u8),
    InsertLines(u16),
    DeleteLines(u16),
    /// SGR parameter list applied to the current attributes in the state.
    Sgr(Vec<u16>),
    SetTitle(String),
    ScrollUp(u16),
    ScrollDown(u16),
    SaveCursor,
    RestoreCursor,
    SetCursorVisible(bool),
    SetAutoWrap(bool),
    SetApplicationCursorKeys(bool),
    SetBracketedPaste(bool),
    SetAltScreen(bool),
    InsertChars(u16),
    DeleteChars(u16),
    RepeatLastChar(u16),
}

/// Terminal modes relevant to rendering and editing behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modes {
    pub wrap: bool,
    pub cursor_visible: bool,
    pub application_cursor_keys: bool,
    pub bracketed_paste: bool,
    pub alt_screen: bool,
}

impl Modes {
    pub fn new() -> Self {
        Self {
            wrap: true,
            cursor_visible: true,
            application_cursor_keys: false,
            bracketed_paste: false,
            alt_screen: false,
        }
    }
}

impl Default for Modes {
    fn default() -> Self {
        Self::new()
    }
}

/// Saved normal-screen state while the alternate screen is active.
#[derive(Debug, Clone)]
struct SavedScreen {
    grid: VecDeque<Row>,
    cold: ColdStore,
    encode_scratch: Vec<u8>,
    encode_wrapped: Vec<u8>,
    encode_count: usize,
    viewport: Option<(usize, Vec<Row>)>,
    cold_cache: Option<(usize, Vec<Row>)>,
    cursor: Cursor,
    saved_cursor: Cursor,
    fg: u32,
    bg: u32,
    attrs: u16,
    pending_wrap: bool,
}

/// The authoritative terminal state. Owned by one thread; never shared.
#[derive(Debug, Clone)]
pub struct TerminalState {
    pub cols: u16,
    pub rows: u16,
    /// Grid: index 0 is the oldest row. The last `rows` entries are the
    /// visible window; the front is scrollback.
    pub grid: VecDeque<Row>,
    pub cursor: Cursor,
    /// Current SGR state applied to the next printed cell.
    pub current_fg: u32,
    pub current_bg: u32,
    pub current_attrs: u16,
    pub title: String,
    pub modes: Modes,
    /// Client-side scroll offset: how many scrollback rows are shown above
    /// the viewport. First visible row = `grid.len() - rows - scroll_offset`.
    pub scroll_offset: u32,
    pub scrollback_limit: usize,

    /// Cold (compressed) history — the oldest rows, tiered out of the grid
    /// (Phase 0.5.2). Logical index space: `[cold blocks][pending block][grid]`.
    pub cold: ColdStore,
    /// Streaming span buffer for the pending cold block (uncompressed).
    encode_scratch: Vec<u8>,
    /// Wrapped flags for the pending cold block.
    encode_wrapped: Vec<u8>,
    /// Rows currently in `encode_scratch` (part of the logical index space).
    encode_count: usize,
    /// Decoded rows of the visible window when it overlaps cold history
    /// (built by [`TerminalState::snapshot`]). `(logical base, rows)`.
    viewport: Option<(usize, Vec<Row>)>,
    /// One-block decode cache for [`TerminalState::grid_row`] cold reads.
    cold_cache: Option<(usize, Vec<Row>)>,

    saved_cursor: Cursor,
    saved_fg: u32,
    saved_bg: u32,
    saved_attrs: u16,
    saved_wrap: bool,
    saved_pending_wrap: bool,

    alt: Option<SavedScreen>,
    last_char: Option<char>,
    selection: Option<Selection>,

    /// The cursor is parked at the right margin; the wrap happens on the
    /// *next* printable character (xterm deferred-wrap semantics). Any
    /// explicit cursor movement clears this.
    pub pending_wrap: bool,

    dirty: DirtyTracker,
}

impl TerminalState {
    pub fn new(cols: u16, rows: u16) -> Self {
        let mut grid = VecDeque::with_capacity(rows as usize + 64);
        for _ in 0..rows {
            grid.push_back(Row::new(cols));
        }
        Self {
            cols,
            rows,
            grid,
            cursor: Cursor::default(),
            current_fg: 0,
            current_bg: 0,
            current_attrs: 0,
            title: String::from("FlashTerminal"),
            modes: Modes::new(),
            scroll_offset: 0,
            scrollback_limit: 10_000,
            cold: ColdStore::default(),
            encode_scratch: Vec::with_capacity(BLOCK_ROWS * 64),
            encode_wrapped: Vec::new(),
            encode_count: 0,
            viewport: None,
            cold_cache: None,
            saved_cursor: Cursor::default(),
            saved_fg: 0,
            saved_bg: 0,
            saved_attrs: 0,
            saved_wrap: true,
            saved_pending_wrap: false,
            alt: None,
            last_char: None,
            selection: None,
            pending_wrap: false,
            dirty: DirtyTracker::new(rows),
        }
    }

    /// True if `(row, col)` lies inside the current selection.
    #[inline]
    pub fn is_selected(&self, row: u16, col: u16) -> bool {
        match self.selection {
            Some(sel) => {
                let (r0, c0) = sel.start;
                let (r1, c1) = sel.end;
                let (r0, r1) = if r0 <= r1 { (r0, r1) } else { (r1, r0) };
                let (c_start, c_end) = if r0 == r1 {
                    (c0.min(c1), c0.max(c1))
                } else if row == r0 {
                    (c0.min(c1), self.cols - 1)
                } else if row == r1 {
                    (0, c0.max(c1))
                } else {
                    (0, self.cols - 1)
                };
                row >= r0 && row <= r1 && col >= c_start && col <= c_end
            }
            None => false,
        }
    }

    /// Sets the selection (inclusive bounds, clamped to the grid).
    pub fn set_selection(&mut self, start: (u16, u16), end: (u16, u16)) {
        let clamp = |(r, c): (u16, u16)| {
            (
                r.min(self.rows.saturating_sub(1)),
                c.min(self.cols.saturating_sub(1)),
            )
        };
        let start = clamp(start);
        let end = clamp(end);
        if self.selection != Some(Selection { start, end }) {
            self.selection = Some(Selection { start, end });
            self.dirty.mark_all();
        }
    }

    pub fn clear_selection(&mut self) {
        if self.selection.take().is_some() {
            self.dirty.mark_all();
        }
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Extracts the selected text for copying, respecting wrapped lines.
    /// Wide-character continuation cells are skipped so each grapheme
    /// appears once; zero-width combining marks are included in order.
    /// The viewport is materialized first so selection over scrolled
    /// (cold) history works.
    pub fn selection_text(&mut self) -> String {
        // Ensure the viewport buffer reflects the current window.
        self.snapshot();
        let Some(sel) = self.selection else {
            return String::new();
        };
        let (r0, c0) = sel.start;
        let (r1, c1) = sel.end;
        let (r0, r1) = if r0 <= r1 { (r0, r1) } else { (r1, r0) };
        let mut out = String::new();
        for r in r0..=r1 {
            let row = self.visible_row(r);
            let (s, e) = if r == r0 && r == r1 {
                (c0.min(c1), c0.max(c1))
            } else if r == r0 {
                (c0.min(c1), self.cols - 1)
            } else if r == r1 {
                (0, c0.max(c1))
            } else {
                (0, self.cols - 1)
            };
            let e = e.min(row.cells.len().saturating_sub(1) as u16);
            for cell in row.cells[s as usize..=e as usize].iter() {
                if cell.ch != 0 && !cell.is_wide_continuation() {
                    if let Some(ch) = char::from_u32(cell.ch) {
                        out.push(ch);
                    }
                }
            }
            if r != r1 && !row.is_wrapped {
                out.push('\n');
            }
        }
        out
    }

    /// Converts a visible-space row index into a grid index.
    #[inline]
    fn vis_idx(&self, vrow: u16) -> usize {
        self.grid.len() - self.rows as usize + vrow as usize
    }

    // ------------------------------------------------------------------
    // Dirty tracking
    // ------------------------------------------------------------------

    /// Consumes the accumulated dirty state for one frame.
    pub fn consume_dirty(&mut self) -> DirtyTracker {
        std::mem::replace(&mut self.dirty, DirtyTracker::new(self.rows))
    }

    #[inline]
    pub fn dirty(&self) -> &DirtyTracker {
        &self.dirty
    }

    pub fn mark_all_dirty(&mut self) {
        self.dirty.mark_all();
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.dirty = DirtyTracker::new(rows);
        self.dirty.mark_all();

        // Resize drops scrollback (matching pre-0.5.2 behaviour): discard the
        // cold tiers before the existing grid truncation runs.
        self.cold.clear();
        self.encode_count = 0;
        self.encode_scratch.clear();
        self.encode_wrapped.clear();
        self.viewport = None;
        self.cold_cache = None;

        let total = self.grid.len();
        if total > rows as usize {
            for _ in 0..total - rows as usize {
                self.grid.pop_front();
            }
        }
        while self.grid.len() < rows as usize {
            self.grid.push_back(Row::new(cols));
        }
        for row in &mut self.grid {
            if row.cells.len() != cols as usize {
                row.cells.resize(cols as usize, Cell::empty());
            }
        }
        self.cursor.col = self.cursor.col.min(cols.saturating_sub(1));
        self.cursor.row = self.cursor.row.min(rows.saturating_sub(1));
        self.scroll_offset = self.scroll_offset.min(self.scrollback_len());
        self.pending_wrap = false;
        if let Some(sel) = &mut self.selection {
            sel.start.0 = sel.start.0.min(rows.saturating_sub(1));
            sel.start.1 = sel.start.1.min(cols.saturating_sub(1));
            sel.end.0 = sel.end.0.min(rows.saturating_sub(1));
            sel.end.1 = sel.end.1.min(cols.saturating_sub(1));
        }
    }

    // ------------------------------------------------------------------
    // Grid access
    // ------------------------------------------------------------------

    /// Number of scrollback rows currently retained (cold + hot).
    #[inline]
    pub fn scrollback_len(&self) -> u32 {
        (self.grid_len() - self.rows as usize) as u32
    }

    /// State-accounted retained memory: hot rows (16 B cells + Vec/Row
    /// overhead) + compressed cold blocks + pending encode buffer.
    pub fn retained_memory(&self) -> usize {
        let per_row = self.cols as usize * 16 + 64;
        let hot = self.grid.len() * per_row;
        let cold: usize = self.cold.blocks.iter().map(|b| b.retained_bytes()).sum();
        hot + cold + self.encode_scratch.capacity()
    }

    /// Total number of retained rows (cold scrollback + hot scrollback +
    /// visible viewport).
    #[inline]
    pub fn grid_len(&self) -> usize {
        self.cold.total_rows + self.encode_count + self.grid.len()
    }

    /// Row at a raw grid index `0..grid_len` (oldest scrollback first).
    ///
    /// Hot rows come straight from the grid; cold rows are decoded through a
    /// one-block cache (no structural change — cold storage stays put, so
    /// memory stays bounded). Out-of-bounds reads return an empty row.
    pub fn grid_row(&mut self, idx: usize) -> &Row {
        let cold_rows = self.cold.total_rows + self.encode_count;
        if idx < cold_rows {
            if idx < self.cold.total_rows {
                let base = (idx / BLOCK_ROWS) * BLOCK_ROWS;
                let cached = self
                    .cold_cache
                    .as_ref()
                    .map(|(b, _)| *b == base)
                    .unwrap_or(false);
                if !cached {
                    let bi = idx / BLOCK_ROWS;
                    let rows = decode_block(&self.cold.blocks[bi]);
                    self.cold_cache = Some((base, rows));
                }
                let (b, rows) = self.cold_cache.as_ref().expect("cache populated");
                return &rows[idx - b];
            }
            // Pending (uncompressed) block.
            let rel = idx - self.cold.total_rows;
            let row =
                decode_scratch_row(&self.encode_scratch, self.cols, &self.encode_wrapped, rel);
            self.cold_cache = Some((idx, vec![row]));
            let (_, rows) = self.cold_cache.as_ref().expect("cache populated");
            return &rows[0];
        }
        let gi = idx - cold_rows;
        static EMPTY: std::sync::OnceLock<Row> = std::sync::OnceLock::new();
        self.grid
            .get(gi)
            .unwrap_or_else(|| EMPTY.get_or_init(|| Row::new(1)))
    }

    /// Index into `grid` of the first visible row for the current scroll
    /// offset. Only meaningful when the window is hot (see [`TerminalState::visible_row`]).
    #[inline]
    pub fn first_visible_row(&self) -> usize {
        let idx = self.grid.len() as i64 - self.rows as i64 - self.scroll_offset as i64;
        idx.max(0) as usize
    }

    /// Logical index of visible row `vrow` (respects the scroll offset).
    #[inline]
    fn window_logical(&self, vrow: u16) -> usize {
        self.grid_len() - self.rows as usize - self.scroll_offset as usize + vrow as usize
    }

    /// Visible row `0..rows` (respects scroll offset). Rows from the hot
    /// grid when the window is near the bottom; rows from the snapshot-built
    /// viewport buffer when the window is deep in cold history.
    #[inline]
    pub fn visible_row(&self, vrow: u16) -> &Row {
        let logical = self.window_logical(vrow);
        let cold_rows = self.cold.total_rows + self.encode_count;
        if logical >= cold_rows {
            let gi = logical - cold_rows;
            if gi < self.grid.len() {
                return &self.grid[gi];
            }
        } else if let Some((base, rows)) = &self.viewport {
            if logical >= *base && logical - *base < rows.len() {
                return &rows[logical - *base];
            }
        }
        static EMPTY: std::sync::OnceLock<Row> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| Row::new(1))
    }

    pub fn visible_cell(&self, vrow: u16, vcol: u16) -> Cell {
        self.visible_row(vrow)
            .cells
            .get(vcol as usize)
            .copied()
            .unwrap_or(Cell::empty())
    }

    #[inline]
    pub fn set_scroll_offset(&mut self, offset: u32) {
        let new = offset.min(self.scrollback_len());
        if new != self.scroll_offset {
            self.scroll_offset = new;
            self.dirty.mark_all();
        }
    }

    /// Scrolls the view by `delta` rows (positive = towards scrollback).
    pub fn scroll_view(&mut self, delta: i32) {
        let new = (self.scroll_offset as i32 + delta).clamp(0, self.scrollback_len() as i32) as u32;
        if new != self.scroll_offset {
            self.scroll_offset = new;
            self.dirty.mark_all();
        }
    }

    /// Mutable visible row (no scroll correction; used for cursor ops).
    fn vis_row_mut(&mut self, vrow: u16) -> &mut Row {
        let idx = self.grid.len() - self.rows as usize + vrow as usize;
        debug_assert!(idx < self.grid.len());
        &mut self.grid[idx]
    }

    // ------------------------------------------------------------------
    // Scrolling
    // ------------------------------------------------------------------

    /// Scroll the whole grid up by one: the oldest row is tiered into cold
    /// storage (or dropped when the total history cap is reached), and a
    /// blank row enters at the bottom. Used when the cursor wraps at the
    /// last line.
    fn scroll_up_one(&mut self) {
        let grid_cap = self.rows as usize + HOT_ROWS + MAX_PROMOTED_BLOCKS * BLOCK_ROWS;
        if self.grid.len() >= grid_cap {
            // Steady state: move the oldest row to cold (or drop it at the
            // total-history cap), recycling its buffer for the new line.
            let oldest = self.grid.pop_front().expect("grid holds >= rows");
            let room = self.history_rows() < self.scrollback_limit;
            if room {
                self.encode_row(&oldest);
            }
            let mut r = oldest;
            r.clear();
            self.grid.push_back(r);
            if self.encode_count >= BLOCK_ROWS {
                self.flush_cold_block();
            }
        } else {
            // Grow: retain the top line as scrollback.
            self.grid.push_back(Row::new(self.cols));
        }
        self.dirty.mark_scroll(1);
    }

    // ------------------------------------------------------------------
    // Tiered scrollback helpers (Phase 0.5.2)
    // ------------------------------------------------------------------

    /// Total retained history rows: cold blocks + pending block + hot grid.
    #[inline]
    fn history_rows(&self) -> usize {
        self.cold.total_rows + self.encode_count + (self.grid.len() - self.rows as usize)
    }

    /// Streams one row into the pending cold block (span encoding only, so
    /// the row's `Vec<Cell>` buffer can be recycled immediately).
    fn encode_row(&mut self, row: &Row) {
        encode_row_into(
            &row.cells,
            row.is_wrapped,
            &mut self.encode_scratch,
            &mut self.encode_wrapped,
            self.encode_count,
        );
        self.encode_count += 1;
    }

    /// Compresses and pushes the pending block to cold storage, reusing the
    /// span buffer.
    fn flush_cold_block(&mut self) {
        if self.encode_count == 0 {
            self.encode_scratch.clear();
            self.encode_wrapped.clear();
            return;
        }
        let body = std::mem::take(&mut self.encode_scratch);
        let wrapped = std::mem::take(&mut self.encode_wrapped);
        let rows = self.encode_count as u16;
        let blk = compress_block(&body, &wrapped, rows, self.cols);
        self.encode_scratch = body; // reuse the (large) span allocation
        self.encode_scratch.clear();
        self.cold.push_back(blk);
        self.encode_count = 0;
    }

    /// Decodes the visible window's rows from cold storage into the viewport
    /// buffer. Blocks are uniform (`k * BLOCK_ROWS`), so lookups are direct.
    fn decode_window(&mut self, win_start: usize) -> (usize, Vec<Row>) {
        let win_end = (win_start + self.rows as usize).min(self.grid_len());
        let cold_rows = self.cold.total_rows;
        let mut out: Vec<Row> = Vec::with_capacity(win_end - win_start);
        let mut pos = win_start;
        while pos < win_end {
            if pos < cold_rows {
                let bi = pos / BLOCK_ROWS;
                let block = decode_block(&self.cold.blocks[bi]);
                let off = pos - bi * BLOCK_ROWS;
                for r in block.into_iter().skip(off).take(win_end - pos) {
                    out.push(r);
                }
                pos = (bi + 1) * BLOCK_ROWS;
            } else {
                let rel = pos - cold_rows;
                out.push(decode_scratch_row(
                    &self.encode_scratch,
                    self.cols,
                    &self.encode_wrapped,
                    rel,
                ));
                pos += 1;
            }
        }
        (win_start, out)
    }

    pub fn scroll_up(&mut self, n: u16) {
        // CSI S: content moves up; top rows enter scrollback and then drop.
        for _ in 0..n {
            self.scroll_up_one();
        }
        self.dirty.mark_all();
    }

    pub fn scroll_down(&mut self, n: u16) {
        for _ in 0..n {
            self.grid.pop_back();
            self.grid.push_front(Row::new(self.cols));
        }
        self.dirty.mark_all();
    }

    // ------------------------------------------------------------------
    // Cursor movement
    // ------------------------------------------------------------------

    pub fn cursor_down(&mut self, n: u16) {
        self.pending_wrap = false;
        for _ in 0..n {
            if self.cursor.row < self.rows - 1 {
                self.cursor.row += 1;
                self.dirty.cursor_changed = true;
            } else {
                self.scroll_up_one();
            }
        }
    }

    pub fn cursor_up(&mut self, n: u16) {
        self.pending_wrap = false;
        let old = self.cursor.row;
        self.cursor.row = self.cursor.row.saturating_sub(n);
        if old != self.cursor.row {
            self.dirty.cursor_changed = true;
        }
    }

    pub fn cursor_forward(&mut self, n: u16) {
        self.pending_wrap = false;
        for _ in 0..n {
            if self.cursor.col < self.cols - 1 {
                self.cursor.col += 1;
                self.dirty.cursor_changed = true;
            } else if self.modes.wrap {
                self.line_wrap();
            } else if self.cursor.col != self.cols - 1 {
                self.cursor.col = self.cols - 1;
                self.dirty.cursor_changed = true;
            }
        }
    }

    /// Moves the cursor forward by `width` cells. At the right margin the
    /// wrap is *deferred*: the cursor parks at the last column with
    /// `pending_wrap` set, and wraps on the next printable character.
    fn cursor_advance(&mut self, width: u8) {
        let w = width as u16;
        let dest = (self.cursor.col + w).min(self.cols);
        if dest < self.cols {
            self.cursor.col = dest;
            self.pending_wrap = false;
            self.dirty.cursor_changed = true;
        } else if dest == self.cols {
            if self.modes.wrap {
                // Park at the margin; wrap happens on the next print.
                self.cursor.col = self.cols.saturating_sub(1);
                self.pending_wrap = true;
            } else {
                self.cursor.col = self.cols.saturating_sub(1);
                self.pending_wrap = false;
            }
            self.dirty.cursor_changed = true;
        } else {
            self.cursor.col = self.cols.saturating_sub(w);
            self.pending_wrap = false;
            self.dirty.cursor_changed = true;
        }
    }

    /// Wrap to the next line (row++ or scroll).
    fn line_wrap(&mut self) {
        self.pending_wrap = false;
        self.vis_row_mut(self.cursor.row).is_wrapped = true;
        if self.cursor.row < self.rows - 1 {
            self.cursor.row += 1;
        } else {
            self.scroll_up_one();
        }
        self.cursor.col = 0;
        self.dirty.cursor_changed = true;
    }

    pub fn cursor_back(&mut self, n: u16) {
        self.pending_wrap = false;
        for _ in 0..n {
            if self.cursor.col > 0 {
                self.cursor.col -= 1;
                self.dirty.cursor_changed = true;
            }
        }
    }

    pub fn cursor_to_beginning_of_line(&mut self) {
        if self.cursor.col != 0 {
            self.cursor.col = 0;
            self.pending_wrap = false;
            self.dirty.cursor_changed = true;
        }
    }

    pub fn cursor_position(&mut self, col: u16, row: u16) {
        self.pending_wrap = false;
        let c = if col == u16::MAX {
            self.cursor.col
        } else {
            col.min(self.cols.saturating_sub(1))
        };
        let r = if row == u16::MAX {
            self.cursor.row
        } else {
            row.min(self.rows.saturating_sub(1))
        };
        if c != self.cursor.col || r != self.cursor.row {
            self.cursor.col = c;
            self.cursor.row = r;
            self.dirty.cursor_changed = true;
        }
    }

    pub fn tab(&mut self) {
        self.pending_wrap = false;
        let next = (self.cursor.col / 8 + 1) * 8;
        self.cursor.col = next.min(self.cols - 1);
        self.dirty.cursor_changed = true;
    }

    // ------------------------------------------------------------------
    // Character writing (the hot path)
    // ------------------------------------------------------------------

    pub fn write_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        let width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u8;

        if width == 0 {
            self.write_zero_width(c);
            return;
        }

        // Deferred wrap: the cursor was parked at the right margin.
        if self.pending_wrap {
            self.pending_wrap = false;
            self.line_wrap();
        }

        let row_idx = self.vis_idx(self.cursor.row);
        let col = self.cursor.col as usize;

        // ZWJ merge: if the base cell of the grapheme cluster ending at
        // `col - 1` carries a cluster-joiner flag, absorb this base char
        // into that cluster. The first base character is *kept*; only the
        // widest width is adopted (shaping the full cluster is deferred to
        // the text-shaping work item).
        if let Some((base_idx, prev)) = self.cluster_base(row_idx, col) {
            if prev.flags & flags::CLUSTER_JOINER != 0 && !prev.is_empty() {
                let old_width = prev.width;
                let new_width = prev.width.max(width).min(2);
                if let Some(base) = self.grid[row_idx].cells.get_mut(base_idx) {
                    base.width = new_width;
                    base.attrs = self.current_attrs;
                    base.fg = self.current_fg;
                    base.bg = self.current_bg;
                    base.flags &= !flags::CLUSTER_JOINER;
                }
                // If the cluster grew to width 2, fill the cell to the
                // right of the base as its wide continuation.
                if new_width == 2 && old_width < 2 && base_idx + 1 < self.cols as usize {
                    if let Some(cont) = self.grid[row_idx].cells.get_mut(base_idx + 1) {
                        cont.ch = prev.ch;
                        cont.fg = prev.fg;
                        cont.bg = prev.bg;
                        cont.attrs = prev.attrs;
                        cont.flags = flags::WIDE_CONTINUATION;
                        cont.width = 0;
                    }
                }
                self.clamp_cursor_after_cluster(base_idx, new_width as u16);
                self.dirty.mark_row(self.cursor.row);
                return;
            }
        }

        // Wide char that would cross the right margin: wrap first.
        if (col as u16) + width as u16 > self.cols {
            if self.modes.wrap {
                self.line_wrap();
            } else {
                self.cursor.col = self.cols.saturating_sub(width as u16);
            }
        }

        let row_idx = self.vis_idx(self.cursor.row);
        let col = self.cursor.col as usize;
        let row = &mut self.grid[row_idx];

        // Clearing the position first nukes trailing combining/joiner cells
        // when overwriting.
        if let Some(cell) = row.cells.get_mut(col) {
            cell.ch = c as u32;
            cell.fg = self.current_fg;
            cell.bg = self.current_bg;
            cell.attrs = self.current_attrs;
            cell.flags = 0;
            cell.width = width;
            if width == 2 && col + 1 < row.cells.len() {
                let cont = &mut row.cells[col + 1];
                cont.ch = c as u32;
                cont.fg = self.current_fg;
                cont.bg = self.current_bg;
                cont.attrs = self.current_attrs;
                cont.flags = flags::WIDE_CONTINUATION;
                cont.width = 0;
            }
        }
        self.last_char = Some(c);
        self.dirty.mark_row(self.cursor.row);
        self.cursor_advance(width);
    }

    /// Handles a zero-width character.
    ///
    /// * **Combining marks** (accent, etc.) are stored in the cell at the
    ///   cursor so the renderer can overlay them on the base glyph.
    /// * **Variation selectors** (FE0E/FE0F) adjust the base cell's display
    ///   width (FE0F = emoji presentation = wide) and mark it as a cluster
    ///   joiner; no cell is consumed.
    /// * **Joiners** (ZWJ/ZWNJ) mark the base cell so the next printable
    ///   base character merges into the cluster.
    fn write_zero_width(&mut self, c: char) {
        let col = self.cursor.col as usize;
        let row_idx = self.vis_idx(self.cursor.row);
        if col == 0 {
            return;
        }
        let is_vs = matches!(c, '\u{fe0e}' | '\u{fe0f}');
        let is_joiner = matches!(c, '\u{200d}' | '\u{200c}');

        if !is_vs && !is_joiner {
            // Plain combining mark: store it at the cursor position.
            let row = &mut self.grid[row_idx];
            if let Some(cell) = row.cells.get_mut(col) {
                cell.ch = c as u32;
                cell.fg = self.current_fg;
                cell.bg = self.current_bg;
                cell.attrs = self.current_attrs;
                cell.flags = flags::COMBINING;
                cell.width = 0;
            }
            self.dirty.mark_row(self.cursor.row);
            return;
        }

        // VS / joiner: operate on the base cell of the cluster ending at
        // `col - 1` (skipping any wide-continuation cells).
        let Some((base_idx, base)) = self.cluster_base(row_idx, col) else {
            return;
        };
        if base.is_empty() {
            return;
        }
        if is_vs {
            // A variation selector adjusts the *display width* only. FE0F
            // selects emoji presentation (wide); FE0E selects text (narrow).
            // It is NOT a joiner: a following base character must not merge.
            let want = if c == '\u{fe0f}' { 2u8 } else { 1u8 };
            let target = want.max(base.width).min(2);
            let grew = target == 2 && base.width < 2;
            if let Some(b) = self.grid[row_idx].cells.get_mut(base_idx) {
                b.width = target;
            }
            if grew && base_idx + 1 < self.cols as usize {
                if let Some(cont) = self.grid[row_idx].cells.get_mut(base_idx + 1) {
                    cont.ch = base.ch;
                    cont.fg = base.fg;
                    cont.bg = base.bg;
                    cont.attrs = base.attrs;
                    cont.flags = flags::WIDE_CONTINUATION;
                    cont.width = 0;
                }
                // The cluster now occupies two columns; park the cursor
                // after it (clamped, with deferred wrap at the margin).
                self.clamp_cursor_after_cluster(base_idx, 2);
            }
            self.dirty.mark_row(self.cursor.row);
            return;
        }
        // ZWJ / ZWNJ: the next printable base character merges into this
        // cluster.
        if let Some(b) = self.grid[row_idx].cells.get_mut(base_idx) {
            b.flags |= flags::CLUSTER_JOINER;
        }
        self.dirty.mark_row(self.cursor.row);
    }

    /// Returns the grid index and cell of the grapheme-cluster base ending
    /// at `col - 1`, skipping any wide-continuation cells in between.
    fn cluster_base(&self, row_idx: usize, col: usize) -> Option<(usize, Cell)> {
        let mut i = col;
        while i > 0 {
            i -= 1;
            let c = self.grid[row_idx].cells[i];
            if c.is_wide_continuation() {
                continue;
            }
            return Some((i, c));
        }
        None
    }

    /// Positions the cursor after a cluster of `new_width` starting at
    /// `base_idx`, clamping at the margin with deferred wrap.
    fn clamp_cursor_after_cluster(&mut self, base_idx: usize, new_width: u16) {
        let next = base_idx as u16 + new_width;
        if next >= self.cols {
            self.cursor.col = self.cols.saturating_sub(1);
            self.pending_wrap = self.modes.wrap;
        } else {
            self.cursor.col = next;
            self.pending_wrap = false;
        }
        self.dirty.cursor_changed = true;
    }

    /// REP: repeat the last written character `n` times.
    pub fn repeat_last_char(&mut self, n: u16) {
        if let Some(c) = self.last_char {
            for _ in 0..n {
                self.write_char(c);
            }
        }
    }

    // ------------------------------------------------------------------
    // Erasing
    // ------------------------------------------------------------------

    pub fn clear_screen(&mut self, mode: u8) {
        let (crow, ccol, cols) = (self.cursor.row, self.cursor.col, self.cols);
        match mode {
            0 => {
                for r in crow..self.rows {
                    let row = self.vis_row_mut(r);
                    row.clear_from(if r == crow { ccol } else { 0 });
                }
                self.dirty.mark_all();
            }
            1 => {
                for r in 0..=crow {
                    let row = self.vis_row_mut(r);
                    row.clear_to(if r == crow { ccol } else { cols - 1 });
                }
                self.dirty.mark_all();
            }
            2 | 3 => {
                for r in 0..self.rows {
                    self.vis_row_mut(r).clear();
                }
                if mode == 3 {
                    while self.grid.len() > self.rows as usize {
                        self.grid.pop_front();
                    }
                    self.cold.clear();
                    self.encode_count = 0;
                    self.encode_scratch.clear();
                    self.encode_wrapped.clear();
                    self.viewport = None;
                    self.cold_cache = None;
                }
                self.cursor.col = 0;
                self.cursor.row = 0;
                self.pending_wrap = false;
                self.dirty.mark_all();
            }
            _ => {}
        }
        self.dirty.cursor_changed = true;
    }

    pub fn clear_line(&mut self, mode: u8) {
        let (crow, ccol) = (self.cursor.row, self.cursor.col);
        let row = self.vis_row_mut(crow);
        match mode {
            0 => row.clear_from(ccol),
            1 => row.clear_to(ccol),
            2 => row.clear(),
            _ => {}
        }
        self.dirty.mark_row(crow);
    }

    // ------------------------------------------------------------------
    // Insert / delete
    // ------------------------------------------------------------------

    /// IL: insert `n` blank lines at the cursor; content moves down; lines
    /// pushed past the bottom of the screen are dropped.
    pub fn insert_lines(&mut self, n: u16) {
        let n = n as usize;
        let screen_at = self.vis_idx(self.cursor.row);
        for _ in 0..n {
            let mut bottom = self.grid.pop_back().unwrap_or_else(|| Row::new(self.cols));
            bottom.clear();
            self.grid.insert(screen_at, bottom);
        }
        self.dirty.mark_all();
    }

    /// DL: delete `n` lines at the cursor; content moves up; blank lines
    /// appear at the bottom.
    pub fn delete_lines(&mut self, n: u16) {
        let n = n as usize;
        let screen_at = self.vis_idx(self.cursor.row);
        let del = n.min(self.rows as usize - self.cursor.row as usize);
        let mut dropped: VecDeque<Row> = VecDeque::new();
        for _ in 0..del {
            if let Some(r) = self.grid.remove(screen_at) {
                dropped.push_back(r);
            }
        }
        for _ in 0..del {
            let r = dropped.pop_front().unwrap_or_else(|| Row::new(self.cols));
            let mut r = r;
            r.clear();
            self.grid.push_back(r);
        }
        self.dirty.mark_all();
    }

    /// ICH: insert `n` blank cells at the cursor in the current line.
    pub fn insert_chars(&mut self, n: u16) {
        let (crow, ccol) = (self.cursor.row, self.cursor.col);
        let row = self.vis_row_mut(crow);
        let col = ccol as usize;
        let n = n as usize;
        let len = row.cells.len();
        if col < len && n > 0 {
            let keep = len - col - n.min(len - col);
            row.cells.copy_within(col..col + keep, col + n);
            row.cells[col..col + n.min(len - col)].fill(Cell::empty());
        }
        self.dirty.mark_row(crow);
    }

    /// DCH: delete `n` cells at the cursor; the line shifts left.
    pub fn delete_chars(&mut self, n: u16) {
        let (crow, ccol) = (self.cursor.row, self.cursor.col);
        let row = self.vis_row_mut(crow);
        let col = ccol as usize;
        let n = n as usize;
        let len = row.cells.len();
        if col < len && n > 0 {
            let del = n.min(len - col);
            row.cells.copy_within(col + del..len, col);
            row.cells[len - del..].fill(Cell::empty());
        }
        self.dirty.mark_row(crow);
    }

    // ------------------------------------------------------------------
    // Alternate screen
    // ------------------------------------------------------------------

    pub fn set_alt_screen(&mut self, alt: bool) {
        if alt == self.modes.alt_screen {
            return;
        }
        if alt {
            let saved = SavedScreen {
                grid: std::mem::take(&mut self.grid),
                cold: std::mem::take(&mut self.cold),
                encode_scratch: std::mem::take(&mut self.encode_scratch),
                encode_wrapped: std::mem::take(&mut self.encode_wrapped),
                encode_count: std::mem::take(&mut self.encode_count),
                viewport: std::mem::take(&mut self.viewport),
                cold_cache: std::mem::take(&mut self.cold_cache),
                cursor: self.cursor,
                saved_cursor: self.saved_cursor,
                fg: self.current_fg,
                bg: self.current_bg,
                attrs: self.current_attrs,
                pending_wrap: self.pending_wrap,
            };
            self.grid = VecDeque::new();
            self.grid.reserve(self.rows as usize + 64);
            for _ in 0..self.rows {
                self.grid.push_back(Row::new(self.cols));
            }
            self.cursor = Cursor::default();
            self.pending_wrap = false;
            self.alt = Some(saved);
        } else if let Some(saved) = self.alt.take() {
            self.grid = saved.grid;
            self.cold = saved.cold;
            self.encode_scratch = saved.encode_scratch;
            self.encode_wrapped = saved.encode_wrapped;
            self.encode_count = saved.encode_count;
            self.viewport = saved.viewport;
            self.cold_cache = saved.cold_cache;
            self.cursor = saved.cursor;
            self.saved_cursor = saved.saved_cursor;
            self.current_fg = saved.fg;
            self.current_bg = saved.bg;
            self.current_attrs = saved.attrs;
            self.pending_wrap = saved.pending_wrap;
        }
        self.modes.alt_screen = alt;
        self.dirty.mark_all();
    }

    // ------------------------------------------------------------------
    // SGR
    // ------------------------------------------------------------------

    pub fn sgr(&mut self, params: &[u16]) {
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p {
                0 => {
                    self.current_fg = 0;
                    self.current_bg = 0;
                    self.current_attrs = 0;
                }
                1 => self.current_attrs |= ATTR_BOLD,
                2 => self.current_attrs |= ATTR_DIM,
                3 => self.current_attrs |= ATTR_ITALIC,
                4 => self.current_attrs |= ATTR_UNDERLINE,
                5 => self.current_attrs |= ATTR_BLINK,
                7 => self.current_attrs |= ATTR_REVERSE,
                8 => self.current_attrs |= ATTR_HIDDEN,
                9 => self.current_attrs |= ATTR_STRIKE,
                21 => self.current_attrs |= ATTR_BOLD,
                22 => self.current_attrs &= !(ATTR_BOLD | ATTR_DIM),
                23 => self.current_attrs &= !ATTR_ITALIC,
                24 => self.current_attrs &= !ATTR_UNDERLINE,
                25 => self.current_attrs &= !ATTR_BLINK,
                27 => self.current_attrs &= !ATTR_REVERSE,
                28 => self.current_attrs &= !ATTR_HIDDEN,
                29 => self.current_attrs &= !ATTR_STRIKE,
                30..=37 => self.current_fg = Color::Indexed((p - 30) as u8).to_packed(),
                38 => {
                    let (c, used) = parse_compound_color(&params[i + 1..]);
                    if let Some(c) = c {
                        self.current_fg = c;
                    }
                    i += used;
                }
                39 => self.current_fg = 0,
                40..=47 => self.current_bg = Color::Indexed((p - 40) as u8).to_packed(),
                48 => {
                    let (c, used) = parse_compound_color(&params[i + 1..]);
                    if let Some(c) = c {
                        self.current_bg = c;
                    }
                    i += used;
                }
                49 => self.current_bg = 0,
                90..=97 => self.current_fg = Color::Indexed((p - 90 + 8) as u8).to_packed(),
                100..=107 => self.current_bg = Color::Indexed((p - 100 + 8) as u8).to_packed(),
                _ => {}
            }
            i += 1;
        }
    }

    // ------------------------------------------------------------------
    // Misc
    // ------------------------------------------------------------------

    pub fn save_cursor(&mut self) {
        self.saved_cursor = self.cursor;
        self.saved_fg = self.current_fg;
        self.saved_bg = self.current_bg;
        self.saved_attrs = self.current_attrs;
        self.saved_wrap = self.modes.wrap;
        self.saved_pending_wrap = self.pending_wrap;
    }

    pub fn restore_cursor(&mut self) {
        self.cursor = self.saved_cursor;
        self.current_fg = self.saved_fg;
        self.current_bg = self.saved_bg;
        self.current_attrs = self.saved_attrs;
        self.modes.wrap = self.saved_wrap;
        self.pending_wrap = self.saved_pending_wrap;
        self.dirty.cursor_changed = true;
    }

    pub fn set_title(&mut self, title: String) {
        if self.title != title {
            self.title = title;
            self.dirty.title_changed = true;
        }
    }

    pub fn set_cursor_visible(&mut self, visible: bool) {
        if self.modes.cursor_visible != visible {
            self.modes.cursor_visible = visible;
            self.cursor.is_hidden = !visible;
            self.dirty.cursor_changed = true;
        }
    }

    pub fn set_auto_wrap(&mut self, wrap: bool) {
        if self.modes.wrap != wrap {
            self.modes.wrap = wrap;
        }
    }

    pub fn apply_event(&mut self, event: TerminalEvent) {
        match event {
            TerminalEvent::WriteChar(c) => self.write_char(c),
            TerminalEvent::MoveCursor { col, row } => self.cursor_position(col, row),
            TerminalEvent::CursorUp(n) => self.cursor_up(n.max(1)),
            TerminalEvent::CursorDown(n) => self.cursor_down(n.max(1)),
            TerminalEvent::CursorForward(n) => self.cursor_forward(n.max(1)),
            TerminalEvent::CursorBack(n) => self.cursor_back(n.max(1)),
            TerminalEvent::Tab => self.tab(),
            TerminalEvent::CursorToBeginningOfLine => self.cursor_to_beginning_of_line(),
            TerminalEvent::ClearScreen(m) => self.clear_screen(m),
            TerminalEvent::ClearLine(m) => self.clear_line(m),
            TerminalEvent::InsertLines(n) => self.insert_lines(n),
            TerminalEvent::DeleteLines(n) => self.delete_lines(n),
            TerminalEvent::Sgr(params) => self.sgr(&params),
            TerminalEvent::SetTitle(title) => self.set_title(title),
            TerminalEvent::ScrollUp(n) => self.scroll_up(n),
            TerminalEvent::ScrollDown(n) => self.scroll_down(n),
            TerminalEvent::SaveCursor => self.save_cursor(),
            TerminalEvent::RestoreCursor => self.restore_cursor(),
            TerminalEvent::SetCursorVisible(v) => self.set_cursor_visible(v),
            TerminalEvent::SetAutoWrap(w) => self.set_auto_wrap(w),
            TerminalEvent::SetApplicationCursorKeys(_) => {}
            TerminalEvent::SetBracketedPaste(_) => {}
            TerminalEvent::SetAltScreen(alt) => self.set_alt_screen(alt),
            TerminalEvent::InsertChars(n) => self.insert_chars(n),
            TerminalEvent::DeleteChars(n) => self.delete_chars(n),
            TerminalEvent::RepeatLastChar(n) => self.repeat_last_char(n),
        }
    }
}

/// The immutable render boundary (ADR 0003).
///
/// The GPU renderer never touches a [`TerminalState`] directly; the UI
/// thread builds this zero-copy view once per frame (or once per dirty
/// batch) and hands it to the renderer together with the consumed
/// [`DirtyTracker`]. The snapshot exposes only what the renderer needs:
/// visible cells, cursor, selection, title, and grid geometry.
#[derive(Debug)]
pub struct RenderSnapshot<'a> {
    pub cols: u16,
    pub rows: u16,
    /// Rows of scrollback currently shown above the viewport.
    pub scroll_offset: u32,
    /// Cursor position + visibility (renderer styles it from `Modes`-derived
    /// info; blinking is a renderer concern).
    pub cursor: Cursor,
    /// Current window title (set via OSC 0/2).
    pub title: &'a str,
    state: &'a TerminalState,
}

impl<'a> RenderSnapshot<'a> {
    /// Visible cell at viewport coordinates `(row, col)`; out-of-bounds
    /// reads return an empty cell.
    #[inline]
    pub fn visible_cell(&self, row: u16, col: u16) -> Cell {
        self.state.visible_cell(row, col)
    }

    /// Visible row at viewport coordinates (hot grid or cold viewport buffer).
    #[inline]
    pub fn visible_row(&self, row: u16) -> &'a Row {
        self.state.visible_row(row)
    }

    /// Active selection in visible space, if any.
    #[inline]
    pub fn selection(&self) -> Option<Selection> {
        self.state.selection
    }

    /// True if the cell is inside the selection.
    #[inline]
    pub fn is_selected(&self, row: u16, col: u16) -> bool {
        self.state.is_selected(row, col)
    }
}

impl TerminalState {
    /// Builds the zero-copy render view for the current state.
    ///
    /// When the visible window overlaps cold history, the window's rows are
    /// decoded into the viewport buffer so `visible_row`/`visible_cell` can
    /// read them without disturbing the cold tier. When the window is hot,
    /// reads go straight to the grid and the viewport is cleared.
    pub fn snapshot(&mut self) -> RenderSnapshot<'_> {
        let win_start = self
            .grid_len()
            .saturating_sub(self.rows as usize + self.scroll_offset as usize);
        let cold_rows = self.cold.total_rows + self.encode_count;
        self.viewport = if win_start < cold_rows {
            Some(self.decode_window(win_start))
        } else {
            None
        };
        RenderSnapshot {
            cols: self.cols,
            rows: self.rows,
            scroll_offset: self.scroll_offset,
            cursor: self.cursor,
            title: &self.title,
            state: self,
        }
    }
}

/// Parses `;5;n` (indexed) or `;2;r;g;b` (RGB) color extensions.
/// Returns the packed color and how many params were consumed.
fn parse_compound_color(rest: &[u16]) -> (Option<u32>, usize) {
    match rest.first() {
        Some(5) if rest.len() >= 2 => (Some(Color::Indexed(rest[1] as u8).to_packed()), 2),
        Some(2) if rest.len() >= 4 => (
            Some(Color::Rgb(rest[1] as u8, rest[2] as u8, rest[3] as u8).to_packed()),
            4,
        ),
        _ => (None, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_rows(state: &TerminalState) -> Vec<String> {
        (0..state.rows)
            .map(|r| {
                let row = &state.grid[state.vis_idx(r)];
                row.cells
                    .iter()
                    .map(|c| {
                        if c.is_empty() {
                            ' '
                        } else {
                            char::from_u32(c.ch).unwrap_or('?')
                        }
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn basics_ascii() {
        let mut s = TerminalState::new(10, 3);
        for c in "hello".chars() {
            s.write_char(c);
        }
        assert_eq!(text_rows(&s)[0], "hello     ");
        assert_eq!(s.cursor.col, 5);
    }

    #[test]
    fn line_wrap_and_scrollback() {
        // 20 chars on a 5x2 grid fill 4 rows; the 20th lands at the right
        // margin with a *deferred* wrap (xterm semantics), so no 5th row.
        let mut s = TerminalState::new(5, 2);
        for c in "abcdefghijklmnopqrst".chars() {
            s.write_char(c);
        }
        assert_eq!(s.grid.len(), 4);
        assert_eq!(text_rows(&s)[1], "pqrst");
        assert_eq!(s.scrollback_len(), 2);
        assert!(s.pending_wrap);
        // The next printable character performs the deferred wrap.
        s.write_char('u');
        assert_eq!(text_rows(&s)[1], "u    ");
        assert_eq!(s.scrollback_len(), 3);
    }

    #[test]
    fn wide_chars_cjk() {
        let mut s = TerminalState::new(10, 3);
        for c in "你好".chars() {
            s.write_char(c);
        }
        let row = &s.grid[0];
        assert_eq!(row.cells[0].ch, '你' as u32);
        assert_eq!(row.cells[0].width, 2);
        assert!(row.cells[1].is_wide_continuation());
        assert_eq!(row.cells[2].ch, '好' as u32);
        assert_eq!(s.cursor.col, 4);
    }

    #[test]
    fn combining_marks() {
        let mut s = TerminalState::new(10, 3);
        s.write_char('e');
        s.write_char('\u{301}');
        let row = &s.grid[0];
        assert_eq!(row.cells[0].ch, 'e' as u32);
        assert!(row.cells[1].is_combining());
        assert_eq!(row.cells[1].ch, '\u{301}' as u32);
        assert_eq!(s.cursor.col, 1);
    }

    #[test]
    fn emoji_zwj_cluster_merges() {
        let mut s = TerminalState::new(20, 3);
        s.write_char('\u{1f3f3}');
        s.write_char('\u{fe0f}');
        s.write_char('\u{200d}');
        s.write_char('\u{1f308}');
        let row = &s.grid[0];
        assert_eq!(row.cells[0].ch, '\u{1f3f3}' as u32);
        assert_eq!(row.cells[0].width, 2);
        assert!(row.cells[1].is_wide_continuation());
        assert_eq!(s.cursor.col, 2);
        assert!(row.cells[2].is_empty());
        assert!(row.cells[3].is_empty());
    }

    #[test]
    fn ascii_dirty_rows() {
        let mut s = TerminalState::new(10, 3);
        for c in "abcdefghijklmnopqrst".chars() {
            s.write_char(c);
        }
        let d = s.consume_dirty();
        assert!(!d.is_clean());
        assert!(!d.dirty_rows().is_empty());
    }

    #[test]
    fn clear_screen_and_line() {
        let mut s = TerminalState::new(10, 3);
        for c in "abc".chars() {
            s.write_char(c);
        }
        s.cursor.col = 0;
        s.clear_line(2);
        assert!(s.grid[0].cells.iter().all(|c| c.is_empty()));
    }

    #[test]
    fn insert_delete_lines() {
        let mut s = TerminalState::new(5, 3);
        for c in "aaa".chars() {
            s.write_char(c);
        }
        s.cursor.row = 0;
        s.cursor.col = 0;
        s.insert_lines(1);
        assert!(s.grid[0].cells.iter().all(|c| c.is_empty()));
        assert_eq!(s.grid[0].cells[0].ch, 0);
        assert_eq!(s.grid[1].cells[0].ch, 'a' as u32);
        s.delete_lines(1);
        // Deleting the blank line at row 0 restores the shifted content.
        assert_eq!(s.grid[0].cells[0].ch, 'a' as u32);
        assert!(s.grid[1].cells.iter().all(|c| c.is_empty()));
        assert_eq!(s.grid[2].cells[0].ch, 0);
    }

    #[test]
    fn insert_delete_chars() {
        let mut s = TerminalState::new(10, 3);
        for c in "abcd".chars() {
            s.write_char(c);
        }
        s.cursor.col = 1;
        s.insert_chars(2);
        assert_eq!(s.grid[0].cells[0].ch, 'a' as u32);
        assert_eq!(s.grid[0].cells[1].ch, 0);
        assert_eq!(s.grid[0].cells[3].ch, 'b' as u32);
        s.cursor.col = 1;
        s.delete_chars(2);
        assert_eq!(s.grid[0].cells[1].ch, 'b' as u32);
    }

    #[test]
    fn resize_shrinks_and_grows() {
        let mut s = TerminalState::new(10, 3);
        for c in "abcdefghijklmnopqrst".chars() {
            s.write_char(c);
        }
        s.resize(5, 2);
        assert_eq!(s.cols, 5);
        assert_eq!(s.rows, 2);
        assert_eq!(s.grid.len(), 2);
        for row in &s.grid {
            assert_eq!(row.cells.len(), 5);
        }
        s.resize(12, 4);
        assert_eq!(s.grid.len(), 4);
        assert_eq!(s.grid[3].cells.len(), 12);
    }

    #[test]
    fn alt_screen_preserves() {
        let mut s = TerminalState::new(10, 3);
        s.write_char('x');
        s.set_alt_screen(true);
        assert!(s.grid[0].cells.iter().all(|c| c.is_empty()));
        s.write_char('y');
        s.set_alt_screen(false);
        assert_eq!(s.grid[0].cells[0].ch, 'x' as u32);
        assert!(!s.modes.alt_screen);
    }

    #[test]
    fn color_roundtrip() {
        for c in [Color::Default, Color::Indexed(200), Color::Rgb(1, 2, 3)] {
            assert_eq!(Color::from_packed(c.to_packed()), c);
        }
    }

    #[test]
    fn cell_size_is_16_bytes() {
        assert_eq!(std::mem::size_of::<Cell>(), 16);
    }

    #[test]
    fn tab_moves_to_stop() {
        let mut s = TerminalState::new(20, 3);
        s.write_char('a');
        s.tab();
        assert_eq!(s.cursor.col, 8);
    }

    #[test]
    fn scroll_reuses_buffers() {
        let mut s = TerminalState::new(80, 24);
        for _ in 0..10_000 {
            s.write_char('a');
            s.cursor_to_beginning_of_line();
            s.cursor_down(1);
        }
        assert!(s.scrollback_len() <= 10_000);
        // Logical history is rows + retained scrollback.
        assert_eq!(s.grid_len(), s.rows as usize + s.scrollback_len() as usize);
        // Phase 0.5.2: the bulk of history tiered into cold (compressed)
        // storage; the raw grid holds only the hot allowance + visible.
        assert!(
            s.cold.total_rows > 5_000,
            "cold holds most history: {}",
            s.cold.total_rows
        );
        assert_eq!(
            s.grid.len(),
            s.rows as usize + HOT_ROWS + MAX_PROMOTED_BLOCKS * BLOCK_ROWS
        );
        // Scrolling deep into cold history stays correct: the snapshot
        // builds a viewport buffer for the window, and reads come back
        // identical to the raw grid rows.
        s.set_scroll_offset(s.scrollback_len());
        let (off, slen) = (s.scroll_offset, s.scrollback_len());
        assert_eq!(off, slen);
        let snap = s.snapshot();
        assert_eq!(snap.visible_cell(0, 0).ch, 'a' as u32);
        assert!(snap.visible_cell(0, 1).is_empty());
    }

    #[test]
    fn sgr_compound_colors() {
        let mut s = TerminalState::new(10, 3);
        s.sgr(&[38, 2, 10, 20, 30]);
        assert_eq!(s.current_fg, Color::Rgb(10, 20, 30).to_packed());
        s.sgr(&[38, 5, 123]);
        assert_eq!(s.current_fg, Color::Indexed(123).to_packed());
        s.sgr(&[39]);
        assert_eq!(s.current_fg, 0);
    }

    #[test]
    fn dirty_tracking_incremental() {
        let mut s = TerminalState::new(10, 3);
        for c in "ab".chars() {
            s.write_char(c);
        }
        let d = s.consume_dirty();
        assert_eq!(d.dirty_rows(), vec![0]);
        // Typing moves the cursor, so the cursor slot must be flagged.
        assert!(d.cursor_changed);
        assert!(!d.full_redraw);
        assert!(s.dirty().is_clean());
    }

    #[test]
    fn cursor_wrap_and_margin() {
        // Deferred wrap: the 4th char parks at the margin.
        let mut s = TerminalState::new(4, 2);
        for c in "abcd".chars() {
            s.write_char(c);
        }
        assert_eq!(s.cursor.col, 3);
        assert_eq!(s.cursor.row, 0);
        assert!(s.pending_wrap);
        // The 5th char performs the wrap, then writes on the next row.
        s.write_char('e');
        assert_eq!(s.cursor.col, 1);
        assert_eq!(s.cursor.row, 1);
        assert_eq!(text_rows(&s)[0], "abcd");
        assert_eq!(text_rows(&s)[1], "e   ");
        // Explicit movement clears the pending wrap.
        s.write_char('f');
        s.cursor_back(1);
        assert!(!s.pending_wrap);
    }

    #[test]
    fn wide_char_at_margin_wraps() {
        let mut s = TerminalState::new(4, 2);
        s.write_char('a'); // col 1
        s.write_char('b'); // col 2
        s.write_char('你'); // width 2 exactly fills cols 2-3 -> parked
        assert_eq!(s.grid[0].cells[2].ch, '你' as u32);
        assert!(s.grid[0].cells[3].is_wide_continuation());
        assert_eq!(s.cursor.col, 3);
        assert_eq!(s.cursor.row, 0);
        assert!(s.pending_wrap);
        // Next char wraps to the next line.
        s.write_char('x');
        assert_eq!(s.cursor.row, 1);
        assert_eq!(s.grid[1].cells[0].ch, 'x' as u32);
    }

    #[test]
    fn selection_single_row() {
        let mut s = TerminalState::new(10, 3);
        for c in "hello world".chars() {
            s.write_char(c);
        }
        s.set_selection((0, 0), (0, 4));
        assert_eq!(s.selection_text(), "hello");
        assert!(s.is_selected(0, 0));
        assert!(s.is_selected(0, 4));
        assert!(!s.is_selected(0, 5));
        s.clear_selection();
        assert!(s.selection().is_none());
    }

    #[test]
    fn selection_multi_row_wrap() {
        let mut s = TerminalState::new(5, 3);
        for c in "abcdefghij".chars() {
            s.write_char(c);
        }
        // Row 0 is wrapped (is_wrapped = true), row 1 is not.
        s.set_selection((0, 1), (1, 1));
        let text = s.selection_text();
        assert!(text.starts_with("bcde"));
        assert_eq!(text, "bcdefg");
    }

    #[test]
    fn selection_reversed_bounds() {
        let mut s = TerminalState::new(10, 3);
        for c in "abcdef".chars() {
            s.write_char(c);
        }
        s.set_selection((0, 4), (0, 1));
        assert_eq!(s.selection_text(), "bcde");
    }

    #[test]
    fn selection_skips_wide_continuation() {
        let mut s = TerminalState::new(10, 3);
        for c in "你好x".chars() {
            s.write_char(c);
        }
        // 你 occupies cols 0-1, 好 cols 2-3, x at col 4.
        s.set_selection((0, 0), (0, 4));
        assert_eq!(s.selection_text(), "你好x");
    }

    #[test]
    fn selection_clamped_on_resize() {
        let mut s = TerminalState::new(10, 3);
        s.set_selection((2, 9), (2, 9));
        s.resize(4, 2);
        let sel = s.selection().unwrap();
        assert_eq!(sel.start, (1, 3));
        assert_eq!(sel.end, (1, 3));
    }

    #[test]
    fn render_snapshot_is_immutable_view() {
        let mut s = TerminalState::new(10, 3);
        for c in "hi".chars() {
            s.write_char(c);
        }
        let snap = s.snapshot();
        assert_eq!(snap.cols, 10);
        assert_eq!(snap.rows, 3);
        assert_eq!(snap.cursor.col, 2);
        assert_eq!(snap.visible_cell(0, 0).ch, 'h' as u32);
        assert_eq!(snap.title, "FlashTerminal");
        assert!(snap.selection().is_none());
    }
}
