# Scrollback Architecture

Phase 0.5.2 scope: make terminal history memory **bounded under sustained
high-volume output** without changing the user experience.

---

## 1. Current representation (audit, pre-0.5.2)

All in `crates/terminal-core/src/lib.rs`.

| Item | Value |
|------|-------|
| Cell | `repr(C)`, exactly **16 bytes**: `ch: u32`, `fg: u32`, `bg: u32`, `attrs: u16`, `flags: u8`, `width: u8` |
| Row | `Vec<Cell>` + `is_wrapped: bool` (Vec = 24 B heap pointer + 16 B×cols heap) |
| Grid | `TerminalState.grid: VecDeque<Row>` — index 0 = oldest, last `rows` = visible |
| Retention cap | `grid.len() ≤ rows + scrollback_limit` (`scrollback_limit = 10_000`) |
| Memory per row | 120 cols × 16 B = **1920 B** + Vec/Row overhead ≈ 1.96 KB |
| Memory per pane @ cap | 10 040 rows × ~1.96 KB ≈ **19.2 MB** (120 cols) |
| Allocation pattern | one `Vec<Cell>` alloc per new row (1920 B); steady-state scroll recycles the popped front buffer (`pop_front` → `clear` → `push_back`), so **no per-line allocation at the cap**; 10 k rows = 10 k live allocations |

**Behavioural notes:**
- `scroll_up_one()`: at the cap, pops the oldest row, clears it, pushes it to
  the back (buffer recycling). Below the cap, grows and retains history.
- `resize()`: **truncates scrollback** down to `rows` (pops front), resizes
  every remaining row's cell vector to the new width.
- Alternate screen (`set_alt_screen`): the whole grid (scrollback included)
  is moved into `SavedScreen`; a fresh `rows`-row grid is built.
- `clear_screen(3)` (ED 3): pops all scrollback.
- Rendering: `RenderSnapshot` is a zero-copy view over the **visible rows
  only**; scrollback is only read when the client scroll offset is non-zero.
- Selection is **visible-space only** (ADR 0003); `selection_text()` walks
  `first_visible_row..`, so scrolled-up selection works only because the
  scrolled rows are still `&self.grid` rows.
- Search: not implemented yet.
- Row buffers are recycled, but when the grid is dropped or cleared the
  freed 1920 B arenas are retained by the system allocator until reused —
  this is the dominant part of the allocator-retention RSS seen in the
  Phase 0.5.1 stress runs.

**Measured impact (Phase 0.5.1 harness, 120×40 panes):**
- Idle panes (no scrollback growth): app RSS ≈ 61–70 MB (harness process),
  per-pane delta ≈ +0.2 MB app.
- 10 `yes`-flooded panes ≈ **597 MB** process-tree RSS; 20 ≈ **1.16 GB**.
  The dominant term is 10 k-row scrollback grids (~19 MB/pane when full)
  plus allocator retention and the shells themselves.

---

## 2. Target design (tiered hot/cold scrollback)

```
Visible Screen          raw, render-critical        (rows)
      │
      ▼
Hot Scrollback          raw rows in the grid, bounded (HOT_ROWS = 1024 above the visible window)
      │
      ▼
Cold Scrollback         RLE + flate2-compressed blocks (unbounded up to the scrollback cap)
```

### Principles
- **Visible + hot** keep the exact current `VecDeque<Row>` representation —
  the hot write path (`write_char`, scroll, render snapshot) is unchanged.
- **Cold** rows are stored as compressed blocks of **128 rows** each. A block
  is a byte vector: row count, column count, wrapped-bit flags, then
  run-length-encoded spans `(len, ch, fg, bg, style)`, compressed with
  `flate2` level 1 (miniz_oxide backend, pure Rust, already in the lockfile).
- **Streaming encode**: rows are encoded as they scroll out of the hot tier
  (one row per line feed, appended to a block buffer), so the row's
  `Vec<Cell>` buffer is recycled immediately — steady-state scrolling
  performs **zero heap allocations**. The buffer is compressed and pushed to
  the cold store every 128 rows.
- **Decode on demand, no promotion**: cold blocks stay put. When the user
  scrolls into cold history, the visible window is decoded into a
  `viewport: Option<(base_idx, Vec<Row>)>` buffer (`materialize_viewport`)
  and the render path reads from it. `grid_row` decodes a single block on
  demand when no viewport covers the row. Because blocks are never spliced
  back into the grid, the `[cold blocks][block buffer][grid]` index space
  never drifts.
- **Bounded total**: `cold_rows + hot_rows ≤ scrollback_limit`; beyond that,
  the oldest cold block is dropped (pop front) — memory plateaus.
- **LRU-ish hot allowance**: at most `HOT_ROWS` in-grid history rows; when
  the grid grows past `HOT_ROWS + 4 blocks`, the oldest in-grid rows are
  re-encoded into a block buffer and pushed to cold.

### Memory model (120 cols, `scrollback_limit` = 10 000)
- Hot allowance: 1024 rows × 1.96 KB ≈ 2.0 MB (constant).
- Cold: 8976 rows ≈ 90 blocks. RLE+flate on typical output is **10–50×**
  smaller than raw; expect ≈ 50–150 B/row ⇒ **≈ 0.5–1.3 MB**.
- **Total ≈ 2.5–3.3 MB vs 19.2 MB raw** — and it scales sub-linearly:
  unbounded 100 k rows ≈ 2 MB hot + ~2–4 MB cold; 1 M rows ≈ 2 MB hot +
  ~22–36 MB cold (still 50–80× below 1.9 GB raw). True plateau (disk tier)
  is a future option per Phase 0.5.2 §3.

### Trade-offs
- **Cost:** decode ≈ 0.5–0.7 ms per 128-row block (measured); amortised over
  128 lines it is negligible, and it never runs inside the visible-frame
  loop (cold blocks are only touched on history access).
- **Resize:** cold blocks are flushed back into the grid and re-tiered after
  the existing resize logic (one-time decode cost; resize is rare).
- **Alternate screen:** the cold store and viewport move with the grid into
  `SavedScreen` (correctness preserved).
- Full compaction (packing the `Vec<Cell>` row buffers at promotion) is a
  later refinement; recycling already prevents per-line allocation.

---

## 3. Measured results (Phase 0.5.2)

Run: `cargo run --release -p benchmarks --bin scrollback_bench [--unbounded]`
(120 cols; `seq` = repeating-digit output, `yes` = 36-char repeating line,
`code` = log-style lines).

### Bounded (default `scrollback_limit` = 10 000 rows)

| Rows fed | state | hot rows | cold rows (blocks) | cold MB | insert rows/s | deep-scroll check |
|---------:|------:|---------:|-------------------:|--------:|--------------:|-------------------|
| 1 k | 1.99 MB | 961 | 0 (0) | 0.00 | 586 k | OK |
| 5 k | 3.33 MB | 1536 | 3328 (26) | 0.07 | 813 k | OK |
| 10 k | 3.43 MB | 1536 | 8320 (65) | 0.17 | 1024 k | OK |
| 50 k | 3.43 MB | 1536 | 8448 (66) | 0.17 | 1569 k | OK |
| 100 k | 3.43 MB | 1536 | 8448 (66) | 0.17 | 1742 k | OK |
| 1 M | 3.43 MB | 1536 | 8448 (66) | 0.17 | 1873 k | OK |

> **State memory is flat at ~3.4 MB from 10 k to 1 M rows** — the cap drops
> the oldest cold block, so history volume cannot grow RAM. (Old raw design:
> 10 k rows ≈ 19.2 MB and growing linearly.)

### Unbounded (retain all history — shows the true sub-linear curve)

| Rows fed | state (seq) | state (yes) | raw cells would be |
|---------:|------------:|------------:|-------------------:|
| 10 k | 3.43 MB | 3.69 MB | 19.2 MB |
| 50 k | 4.27 MB | 5.16 MB | 96 MB |
| 100 k | 5.32 MB | 6.98 MB | 192 MB |
| 1 M | 24.57 MB | 39.84 MB | 1.9 GB |

> **50–80× smaller than raw**; RLE+flate blocks compress `seq` 1 M rows to
> 21.3 MB and `yes` to 36.5 MB.

### Access costs
- Random cold access (200 rows scattered through 10 k-row history):
  ≈ **13 ms** (0.065 ms/row) — block decode + LRU effect; the scroll path
  itself decodes the window once and then reads the viewport buffer.
- Decode a 128-row block: **0.54–0.61 ms** (≈ 2364 B compressed for `seq`).
- Deep-scroll content check: **OK** at every depth (decode is lossless).

### Multi-pane plateau (§7, `plateau` bin, fresh process, yes-flooded)

| panes | tree RSS @ 20 s | state memory | rows | cold blocks |
|------:|----------------:|-------------:|-----:|------------:|
| 1 | — | 3.37 MB | 10 k | — |
| 10 | — | 33.7 MB | 201 k | — |
| 20 | 812 → 856 MB | 67.41 MB (flat) | 201 k | 1320 |

> State memory is **perfectly flat** at ~3.37 MB/pane (vs ~19 MB/pane raw).
> The process-tree RSS is dominated by the `yes` shell children and the
> allocator's retained arenas, **not** live scrollback. See §5 of
> `docs/performance.md` for the channel-backlog component under saturation.

---

## 4. Design decision log

| Date | Decision |
|------|----------|
| 0.5.2 | Tiered hot/cold scrollback; cold = 128-row RLE+flate2 blocks; **decode-on-demand via a viewport buffer (no grid promotion)**; drop-oldest at cap. Rejected: shrink `Cell` to 8 B globally (lossy color depth, invasive), disk-backed cold tier (out of scope now; hook point preserved), promote-on-access (index drift when blocks are promoted from anywhere but the back — replaced by the viewport buffer). |
