# ADR 0003: Session Hub, Render Snapshot Boundary, and Text Stack

## Status
Accepted (implemented in Phase 0.5)

## Context
ADR-0002 defined the message-passing, single-owner threading model at the design level. Phase 0.5 implementation raised three concrete decisions that deserve a record: where the ownership hub lives, how the renderer is kept honest about never mutating state, and which text/glyph stack satisfies the < 8 ms input-latency and < 40 MB idle-RAM budgets.

## Decision

### 1. `terminal-session` is the ownership hub
A new crate, `terminal-session`, owns the per-session machinery:
- The **reader/parser thread** (blocking `read_available` → `Parser::advance_bytes` → `take_events`).
- A **bounded `crossbeam_channel` (capacity 1024)** carrying `SessionEvent::Terminal(Vec<TerminalEvent>)` and `SessionEvent::Exited`.
- The `Session` handle the UI thread holds: `drain` (non-blocking apply), `write`/`resize` (direct PTY fast path), `terminate`.

The UI thread exclusively owns the authoritative `TerminalState`. Nothing else holds a reference to it. `Session` is designed to be the future unit of multiplexing (panes/tabs) without changing the model.

### 2. `RenderSnapshot` is the renderer boundary
`terminal-core` exposes `TerminalState::snapshot() -> RenderSnapshot` — an immutable borrow-based cell-reader (`visible_cell(row, col)`) plus the dirty-row bitset and scroll delta. Rules:
- The renderer only ever reads a `RenderSnapshot`; it can never mutate `TerminalState`.
- Rendering never takes a lock; the snapshot is a view of state owned by the UI thread.
- Dirty regions are row-granular: the renderer re-encodes only rows that changed since the previous frame.

`Selection` (anchor + active endpoint) lives in `terminal-core` so the model — and its tests — do not depend on the GPU or the windowing layer.

### 3. Text stack: `fontdue` (no `swash`)
`terminal-text` uses **`fontdue` 0.9** for discovery, TTC collection parsing, glyph metrics, and rasterization, with a fixed-size LRU glyph cache. Rationale:
- `fontdue` is a pure-Rust, zero-dependency (no freetype/ICU) rasterizer with subpixel-perfect metrics at the sizes a terminal needs (14–18 px).
- The earlier plan listed `swash`; it was dropped because `fontdue`'s simpler `(font, glyph_index, size) → bitmap` model maps directly onto the atlas/instancing pipeline and has no font-loading runtime (system fonts are loaded lazily, keeping idle RAM low).
- Glyph cache misses rasterize on demand; measured steady-state miss cost is ~0.13 µs/glyph.

### 4. Renderer: glyph atlas + instanced quads
`terminal-renderer` (wgpu 0.19) maintains a growing 1024×1024 glyph atlas, one instanced quad per cell (position/uv/color packed via bytemuck), a pure-color background pass, cursor and selection highlight draws, and a two-buffer instance scheme that updates only dirty rows.

## Consequences

### Positive
- **No shared mutable state**: the reader thread never touches the grid; the renderer never mutates it. Contention under massive output is bounded by channel capacity (verified by the backpressure integration test).
- **Bounded memory**: 10 real sessions measured at ~11 MB RSS, comfortably inside the < 80 MB budget.
- **Testable core**: `terminal-core` has no GPU/window dependencies, so selection, snapshot, wrap, and Unicode semantics are covered by pure unit tests.

### Negative
- The snapshot view is borrow-based, so the renderer must finish encoding a frame before state can be mutated again — the desktop event loop applies events in `RedrawRequested`, so this holds by construction, but the constraint is implicit in the borrow checker rather than a documented lock.
- `fontdue` rasterization is CPU-side; very large font sizes or exotic scripts (complex shaping) would need a different path later. Phase 0.5 accepts this: terminals primarily render pre-shaped monospace text.

## References
- ADR-0001 (architecture/stack), ADR-0002 (threading model)
- `docs/architecture-current.md`, `docs/diagrams/flashterminal-architecture.html`
