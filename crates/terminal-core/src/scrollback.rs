//! Tiered scrollback (Phase 0.5.2).
//!
//! Terminal history is split into three tiers:
//!
//! 1. **Visible screen** — raw `Row`s in the grid (render-critical).
//! 2. **Hot scrollback** — raw `Row`s in the grid, bounded to a small
//!    allowance above the visible window (fast scrolling).
//! 3. **Cold scrollback** — the oldest rows, stored as compressed blocks of
//!    [`BLOCK_ROWS`] rows each. A block is a byte stream of run-length
//!    encoded cell spans, zlib-compressed with `flate2` (level 1,
//!    miniz_oxide backend — pure Rust).
//!
//! Rows are *stream-encoded* as they scroll out of the hot tier (one row per
//! line feed, appended to the block buffer), so the row's `Vec<Cell>` buffer
//! can be recycled immediately — steady-state scrolling performs **zero heap
//! allocations**. The block buffer is compressed and pushed to [`ColdStore`]
//! every [`BLOCK_ROWS`] rows.
//!
//! Reads into cold history decode whole blocks and *promote* them back into
//! the grid (the grid is the decode cache); the hot allowance bounds how much
//! promoted history stays raw.
//!
//! ## Index space
//!
//! Logical history index `0..` maps to: `[cold blocks][block buffer][grid hot][visible]`.
//! The block buffer holds up to [`BLOCK_ROWS`] stream-encoded rows that have
//! not been compressed yet (they are part of the index space and are read by
//! re-parsing the span stream, which is cheap and allocation-free per row).

use std::collections::VecDeque;
use std::io::{Read, Write};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::{Cell, Row};

/// Rows per cold block.
pub const BLOCK_ROWS: usize = 128;
/// Target raw (hot) scrollback rows kept in the grid above the visible window.
pub const HOT_ROWS: usize = 1024;
/// How many extra blocks a deep scroll may promote before re-tiering.
pub const MAX_PROMOTED_BLOCKS: usize = 4;

/// Maximum run length per span (7 bits; 120-col rows fit comfortably).
const MAX_RUN: u8 = 0x7F;
/// Tag bit: span is an empty-cell run (`len` in the low 7 bits, 2-byte span).
const EMPTY_RUN: u8 = 0x80;

/// One compressed block of terminal history.
#[derive(Debug, Clone)]
pub struct ColdBlock {
    /// Number of rows in this block (`<= BLOCK_ROWS`; the oldest block may be
    /// partial if it was flushed at a checkpoint).
    pub rows: u16,
    /// Column count the rows were encoded with.
    pub cols: u16,
    /// Bit `i` set = row `i` was a wrapped line.
    pub wrapped: Vec<u8>,
    /// zlib-compressed span stream.
    pub data: Vec<u8>,
}

impl ColdBlock {
    /// Memory retained by this block in the cold tier.
    pub fn retained_bytes(&self) -> usize {
        self.data.len() + self.wrapped.len() + std::mem::size_of::<ColdBlock>()
    }
}

/// Ordered cold history: index 0 = oldest block.
#[derive(Debug, Clone, Default)]
pub struct ColdStore {
    pub blocks: VecDeque<ColdBlock>,
    pub total_rows: usize,
}

impl ColdStore {
    #[inline]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.total_rows = 0;
    }

    pub fn push_back(&mut self, blk: ColdBlock) {
        self.total_rows += blk.rows as usize;
        self.blocks.push_back(blk);
    }

    pub fn pop_front(&mut self) -> Option<ColdBlock> {
        let blk = self.blocks.pop_front()?;
        self.total_rows -= blk.rows as usize;
        Some(blk)
    }

    pub fn pop_back(&mut self) -> Option<ColdBlock> {
        let blk = self.blocks.pop_back()?;
        self.total_rows -= blk.rows as usize;
        Some(blk)
    }

    /// Removes and returns the block at index `bi`.
    pub fn remove(&mut self, bi: usize) -> ColdBlock {
        let blk = self.blocks.remove(bi).expect("cold block index in range");
        self.total_rows -= blk.rows as usize;
        blk
    }
}

// ---------------------------------------------------------------------------
// Span encoding
// ---------------------------------------------------------------------------

/// Appends the span encoding of one row to the block buffer.
/// (Streaming: the row buffer can be recycled immediately afterwards.)
#[inline]
pub fn encode_row_into(
    cells: &[Cell],
    is_wrapped: bool,
    buf: &mut Vec<u8>,
    wrapped: &mut Vec<u8>,
    row_idx: usize,
) {
    if row_idx.is_multiple_of(8) {
        wrapped.push(0);
    }
    if is_wrapped {
        let byte = wrapped.last_mut().expect("wrapped byte pushed");
        *byte |= 1 << (row_idx % 8);
    }
    let mut i = 0usize;
    let n = cells.len();
    while i < n {
        let c = cells[i];
        let start = i;
        i += 1;
        while i < n && (i - start) < MAX_RUN as usize && cells[i] == c {
            i += 1;
        }
        let len = (i - start) as u8;
        if c.is_empty() {
            buf.push(EMPTY_RUN | len);
        } else {
            buf.push(len);
            buf.extend_from_slice(&c.ch.to_le_bytes());
            buf.extend_from_slice(&c.fg.to_le_bytes());
            buf.extend_from_slice(&c.bg.to_le_bytes());
            let style = (c.attrs as u32) | ((c.flags as u32) << 16) | ((c.width as u32) << 24);
            buf.extend_from_slice(&style.to_le_bytes());
        }
    }
    buf.push(0); // row terminator (empty run of length 0)
}

/// Compresses a completed span stream into a [`ColdBlock`].
pub fn compress_block(body: &[u8], wrapped: &[u8], rows: u16, cols: u16) -> ColdBlock {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(1));
    enc.write_all(body).expect("zlib write");
    let data = enc.finish().expect("zlib finish");
    ColdBlock {
        rows,
        cols,
        wrapped: wrapped.to_vec(),
        data,
    }
}

/// Encodes a slice of rows into a single cold block (used for bulk tier-down
/// and for the strategy benchmark).
pub fn encode_block(rows: &[Row], cols: u16) -> ColdBlock {
    let mut buf = Vec::with_capacity(rows.len() * 64);
    let mut wrapped = Vec::with_capacity(rows.len().div_ceil(8));
    for (i, r) in rows.iter().enumerate() {
        encode_row_into(&r.cells, r.is_wrapped, &mut buf, &mut wrapped, i);
    }
    compress_block(&buf, &wrapped, rows.len() as u16, cols)
}

/// Decodes a block back into raw rows (promotion path).
pub fn decode_block(blk: &ColdBlock) -> Vec<Row> {
    let mut dec = ZlibDecoder::new(&blk.data[..]);
    let mut body = Vec::with_capacity(blk.rows as usize * blk.cols as usize * 8);
    dec.read_to_end(&mut body).expect("zlib read");
    let cols = blk.cols as usize;
    let mut out = Vec::with_capacity(blk.rows as usize);
    let mut pos = 0usize;
    for ri in 0..blk.rows as usize {
        let mut row = Row::new(blk.cols);
        let mut ci = 0usize;
        loop {
            let tag = body[pos];
            pos += 1;
            if tag == 0 {
                break;
            }
            if tag & EMPTY_RUN != 0 {
                ci += (tag & 0x7F) as usize;
            } else {
                let len = tag as usize;
                let ch = u32::from_le_bytes(body[pos..pos + 4].try_into().expect("span ch"));
                pos += 4;
                let fg = u32::from_le_bytes(body[pos..pos + 4].try_into().expect("span fg"));
                pos += 4;
                let bg = u32::from_le_bytes(body[pos..pos + 4].try_into().expect("span bg"));
                pos += 4;
                let style = u32::from_le_bytes(body[pos..pos + 4].try_into().expect("span style"));
                pos += 4;
                let cell = Cell {
                    ch,
                    fg,
                    bg,
                    attrs: style as u16,
                    flags: (style >> 16) as u8,
                    width: (style >> 24) as u8,
                };
                let end = (ci + len).min(cols);
                for k in ci..end {
                    row.cells[k] = cell;
                }
                ci += len;
            }
        }
        if !blk.wrapped.is_empty() && blk.wrapped[ri / 8] & (1 << (ri % 8)) != 0 {
            row.is_wrapped = true;
        }
        out.push(row);
    }
    out
}

/// Re-parses row `row_idx` (in `0..` within the block buffer) from an
/// uncompressed span stream. Allocation-free apart from the returned row.
pub fn decode_scratch_row(buf: &[u8], cols: u16, wrapped: &[u8], row_idx: usize) -> Row {
    let mut row = Row::new(cols);
    let mut pos = 0usize;
    let mut cur = 0usize;
    while cur <= row_idx {
        let mut ci = 0usize;
        loop {
            let tag = buf[pos];
            pos += 1;
            if tag == 0 {
                break;
            }
            if tag & EMPTY_RUN != 0 {
                ci += (tag & 0x7F) as usize;
            } else {
                let len = tag as usize;
                let ch = u32::from_le_bytes(buf[pos..pos + 4].try_into().expect("ch"));
                pos += 4;
                let fg = u32::from_le_bytes(buf[pos..pos + 4].try_into().expect("fg"));
                pos += 4;
                let bg = u32::from_le_bytes(buf[pos..pos + 4].try_into().expect("bg"));
                pos += 4;
                let style = u32::from_le_bytes(buf[pos..pos + 4].try_into().expect("style"));
                pos += 4;
                if cur == row_idx {
                    let cell = Cell {
                        ch,
                        fg,
                        bg,
                        attrs: style as u16,
                        flags: (style >> 16) as u8,
                        width: (style >> 24) as u8,
                    };
                    let end = (ci + len).min(cols as usize);
                    for k in ci..end {
                        row.cells[k] = cell;
                    }
                }
                ci += len;
            }
        }
        cur += 1;
    }
    if !wrapped.is_empty() && wrapped[row_idx / 8] & (1 << (row_idx % 8)) != 0 {
        row.is_wrapped = true;
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Color;

    fn sample_rows(cols: u16, n: usize) -> Vec<Row> {
        let mut rows = Vec::with_capacity(n);
        for i in 0..n {
            let mut r = Row::new(cols);
            // Pattern: a few styled chars, then a long run of spaces, then a
            // repeated digit — representative of `seq`/log output.
            if i % 2 == 0 {
                r.cells[0] = Cell {
                    ch: b'#' as u32,
                    fg: Color::Indexed(196).to_packed(),
                    bg: 0,
                    attrs: 1,
                    flags: 0,
                    width: 1,
                };
                r.cells[3] = Cell {
                    ch: b'0' as u32 + (i % 10) as u32,
                    fg: 0,
                    bg: 0,
                    attrs: 0,
                    flags: 0,
                    width: 1,
                };
                r.is_wrapped = i % 3 == 0;
            } else {
                r.cells[5] = Cell {
                    ch: b'x' as u32,
                    fg: 0,
                    bg: 0,
                    attrs: 0,
                    flags: 0,
                    width: 1,
                };
            }
            rows.push(r);
        }
        rows
    }

    #[test]
    fn block_roundtrip() {
        let cols = 120u16;
        let rows = sample_rows(cols, BLOCK_ROWS);
        let blk = encode_block(&rows, cols);
        let decoded = decode_block(&blk);
        assert_eq!(decoded.len(), rows.len());
        for (a, b) in rows.iter().zip(decoded.iter()) {
            assert_eq!(a.cells, b.cells);
            assert_eq!(a.is_wrapped, b.is_wrapped);
        }
        assert!(
            blk.data.len() < rows.len() * 64,
            "compression should shrink"
        );
    }

    #[test]
    fn partial_block_roundtrip() {
        let cols = 80u16;
        let rows = sample_rows(cols, 37);
        let blk = encode_block(&rows, cols);
        let decoded = decode_block(&blk);
        assert_eq!(decoded.len(), 37);
        for (a, b) in rows.iter().zip(decoded.iter()) {
            assert_eq!(a.cells, b.cells);
            assert_eq!(a.is_wrapped, b.is_wrapped);
        }
    }

    #[test]
    fn scratch_row_roundtrip() {
        let cols = 120u16;
        let rows = sample_rows(cols, 10);
        let mut buf = Vec::new();
        let mut wrapped = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            encode_row_into(&r.cells, r.is_wrapped, &mut buf, &mut wrapped, i);
        }
        for (i, r) in rows.iter().enumerate() {
            let got = decode_scratch_row(&buf, cols, &wrapped, i);
            assert_eq!(got.cells, r.cells);
            assert_eq!(got.is_wrapped, r.is_wrapped);
        }
    }

    #[test]
    fn empty_rows_compress_tiny() {
        let cols = 120u16;
        let rows: Vec<Row> = (0..BLOCK_ROWS).map(|_| Row::new(cols)).collect();
        let blk = encode_block(&rows, cols);
        // 128 empty rows: 128 terminators, zlib-wrapped -> a few hundred bytes.
        assert!(
            blk.data.len() < 512,
            "empty block should be tiny: {}",
            blk.data.len()
        );
        let decoded = decode_block(&blk);
        assert!(decoded.iter().all(|r| r.cells.iter().all(|c| c.is_empty())));
    }
}
