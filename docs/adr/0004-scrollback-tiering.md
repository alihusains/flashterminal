# ADR-0004: Tiered Hot/Cold Scrollback

- **Status:** Accepted (Phase 0.5.2)
- **Date:** 2026-08-12
- **Scope:** `crates/terminal-core` — terminal history memory model

## Context

Phase 0.5.1 stress tests showed terminal history RAM growing linearly with
retained rows: a 120-col pane at the 10 000-row scrollback cap holds
~19.2 MB of fully-expanded `Cell` rows, and 20 heavy panes reached
~1.16 GB process-tree RSS. Phase 1 (native multiplexer) will multiply the
pane count, so the per-pane history budget must shrink and stop scaling
with output volume.

Requirement (Phase 0.5.2 §1): *memory usage plateaus as history grows*
rather than *∝ history length*, while preserving scrolling, selection,
copy, resize, and alternate-screen behaviour.

## Decision

Split history into three tiers inside `TerminalState`:

```text
Visible Screen    raw rows in the grid            (rows)
Hot Scrollback    raw rows in the grid, bounded   (HOT_ROWS = 1024)
Cold Scrollback   RLE + flate2-compressed blocks  (128 rows/block, capped)
```

- **Hot write path unchanged** — `write_char`/scroll/render-snapshot keep
  operating on the existing `VecDeque<Row>`.
- **Cold blocks** store `(rows, cols, wrapped-bits, zlib(span stream))`
  where a span is `(len, ch, fg, bg, attrs|flags|width)`; empty runs are a
  2-byte tagged form. `flate2` level 1 with the pure-Rust miniz_oxide
  backend (already in the dependency graph — no new native deps).
- **Streaming encode** on scroll-out: one row per line feed is appended to a
  block buffer, so the row's `Vec<Cell>` is recycled immediately —
  **zero heap allocations in the steady-state scroll path**. Every 128 rows
  the buffer is compressed and pushed to the cold store.
- **Decode on demand, no promotion**: scrolling into history
  `materialize_viewport()` decodes the visible window into a
  `viewport: Option<(base_idx, Vec<Row>)>` buffer; `grid_row()` decodes a
  single block when the viewport does not cover the row. Blocks are never
  spliced back into the grid, so the `[cold][block buffer][grid]` index
  space is immutable and cannot drift.
- **Bounding**: `cold_rows + hot_rows ≤ scrollback_limit`; the oldest cold
  block is dropped at the cap, so memory plateaus.

## Alternatives considered

| Option | Verdict |
|--------|---------|
| **Shrink `Cell` to 8 B** (16-bit color) | Rejected — lossy colour depth, invasive to every renderer/selection path, and only a constant-factor win (2×), not a plateau. |
| **Promote-on-access** (decode a block and splice rows back into the grid, LRU-bounded) | Rejected after a correctness bug during development: promoting blocks from anywhere but the back makes the logical index of every older block shift, breaking deep-window reads (`win at 0` bug). The viewport-buffer design removes the whole class. |
| **Disk-backed cold tier** | Deferred — Phase 0.5.2 keeps everything in RAM; the cold-store boundary is the hook point (a `ColdStore` trait or file-backed block store) for a later disk tier. |
| **Compact rows in RAM** (fixed-width 8-bit cells, RLE rows in memory) | Rejected — RLE-in-memory complicates random access and the renderer; compression-at-rest with decode-on-access gives the same memory win with an unchanged hot path. |
| **lz4/zstd blocks** | Rejected for this phase — flate2 was already in the lockfile and the measured decode cost (0.5–0.7 ms/128-row block, never in the frame loop) is far below the perceived-latency threshold. |

## Benchmarks (measured, 120 cols, release)

- **Bounded state memory: 3.43 MB flat from 10 k to 1 M rows** (old raw:
  ~19.2 MB at 10 k and growing linearly). The cap drops the oldest block.
- **Unbounded 1 M rows: 24.6 MB (`seq`) / 39.8 MB (`yes`)** vs 1.9 GB raw —
  **50–80× smaller**.
- **Insertion: 0.6–1.9 M rows/s**, increasing with volume (steady state is
  buffer-recycling only).
- **Decode: 0.54–0.61 ms per 128-row block** (≈ 2.4 KB compressed); random
  cold access ≈ 0.065 ms/row; deep-scroll content checks pass at every depth.
- **Multi-pane plateau**: 20 panes keep **67.4 MB total state memory flat**
  over 20 s of yes-flooded output (3.37 MB/pane).

## Trade-offs

- **Cost**: cold reads pay a decode; 128-row granularity bounds it to
  ~0.6 ms, well under interactive thresholds, and it never runs inside the
  visible-frame loop.
- **Resize** decodes cold back into the grid once (rare operation).
- **Alternate screen** carries the cold store and viewport into
  `SavedScreen` — correctness preserved, at a slight memory cost while in
  the alternate screen.
- **No search implementation yet** — when search lands, it will scan cold
  blocks decode-by-decode; a future inverted index per block is a natural
  extension of the block format.

## Future scaling

- Disk-backed cold store behind the same block interface (order-of-magnitude
  more history).
- Per-block metadata (first/last line, byte range) for search and
  fast skip-to-relative offsets.
- Row-buffer compaction on decode if allocation counts ever matter (they
  are currently amortised to ~1 alloc per block decode).
