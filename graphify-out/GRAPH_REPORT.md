# Graph Report - .  (2026-08-12)

## Corpus Check
- 71 files · ~129,843 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 1146 nodes · 2505 edges · 46 communities (42 shown, 4 thin omitted)
- Extraction: 96% EXTRACTED · 4% INFERRED · 0% AMBIGUOUS · INFERRED: 89 edges (avg confidence: 0.83)
- Token cost: 0 input · 0 output

## Community Hubs (Navigation)
- Benchmark & Multiplex Tests
- GPU Render State & Dirty Tracking
- Desktop App & Layout Chrome
- Font Library & Caching
- Soak & Resource Tests
- Layout Engine
- Allocation & Backpressure Reports
- CLI Pane Commands
- Child Process I/O Buffer
- Terminal State & Scrollback
- Workspace Persistence Model
- Cell & Unicode Basics
- VT Event Parser
- Scrollback Rows & Cold Blocks
- Glyph Raster Benchmarks
- Performance Budgets & Gates
- Core Architecture & Multiplexer
- Agent & AI Integration
- Unicode/CJK/Combining
- Tech Stack & Architecture
- Phase 0.5.1 Integration Tests
- Pi CLI Workspace Skill
- Extension Authoring Skill
- Architecture Hardening & Font
- Paste/Scrollback Benchmarks
- Project Structure & CI
- Scrollback Strategy & Benchmarks
- Render Pipeline & Scrollback Tiers
- Core Architecture & Agents
- Pi RPC SDK & CI
- Notification Center
- Screen Clear & Insert/Delete
- Terminal Modes & Attributes
- Cursor & Selection
- PTY/Parsing/IO Pipeline
- Terminal Core Benchmarks
- Allocator Profiler
- Color & SGR Parsing
- Pipeline Benchmarks
- RSS Plateau Test
- Raw Throughput Test
- Pi JSONL/RPC Modes
- ADR-0003 Session Snapshot
- Command Palette Prep
- Event Bus & Engine
- Benchmark Runner Script

## God Nodes (most connected - your core abstractions)
1. `TerminalState` - 105 edges
2. `Multiplexer` - 76 edges
3. `Renderer` - 38 edges
4. `Session` - 34 edges
5. `App` - 33 edges
6. `PaneNode` - 28 edges
7. `GlyphCache` - 26 edges
8. `PtyManager` - 25 edges
9. `Row` - 19 edges
10. `Workspace` - 19 edges

## Surprising Connections (you probably didn't know these)
- `Pi RPC SDK Skill` --semantically_similar_to--> `Pi Agent Integration`  [INFERRED] [semantically similar]
  .pi/skills/pi-rpc-sdk/SKILL.md → README.md
- `state_bytes()` --references--> `TerminalState`  [EXTRACTED]
  benchmarks/src/bin/scrollback_bench.rs → crates/terminal-core/src/lib.rs
- `render_prep_10k_rows_ms()` --calls--> `resolve_color()`  [INFERRED]
  benchmarks/src/main.rs → crates/terminal-renderer/src/lib.rs
- `App` --references--> `CursorStyle`  [EXTRACTED]
  apps/desktop/src/main.rs → crates/terminal-renderer/src/lib.rs
- `App` --references--> `Renderer`  [EXTRACTED]
  apps/desktop/src/main.rs → crates/terminal-renderer/src/lib.rs

## Import Cycles
- 2-file cycle: `crates/terminal-core/src/lib.rs -> crates/terminal-core/src/scrollback.rs -> crates/terminal-core/src/lib.rs`
- 2-file cycle: `crates/terminal-workspace/src/model.rs -> crates/terminal-workspace/src/pane_tree.rs -> crates/terminal-workspace/src/model.rs`

## Communities (46 total, 4 thin omitted)

### Community 0 - "Benchmark & Multiplex Tests"
Cohesion: 0.07
Nodes (37): state_bytes(), close_last_pane_closes_tab(), close_last_workspace_rejected(), close_tab_terminates_sessions(), create_workspace_spawns_session(), drain_empty_is_false(), DrainResult, EngineMetrics (+29 more)

### Community 1 - "GPU Render State & Dirty Tracking"
Cohesion: 0.06
Nodes (44): BindGroup, BindGroupLayout, Buffer, DirtyTracker, RenderSnapshot, atlas_entry_uv_scales(), AtlasEntry, attribute_bits() (+36 more)

### Community 2 - "Desktop App & Layout Chrome"
Cohesion: 0.07
Nodes (33): App, AppEvent, key_sequence(), main(), Arc, Instant, Mutex, Option (+25 more)

### Community 3 - "Font Library & Caching"
Cohesion: 0.09
Nodes (27): cache_evicts_lru_first(), cache_memoizes_and_evicts(), coverage_and_missing(), detect_monospace(), FontLibrary, GlyphCache, GlyphCacheStats, GlyphMetrics (+19 more)

### Community 4 - "Soak & Resource Tests"
Cohesion: 0.06
Nodes (35): AtomicBool, main(), Pane, Arc, tree_rss_kb(), Arc, AtomicU64, Box (+27 more)

### Community 5 - "Layout Engine"
Cohesion: 0.11
Nodes (28): horizontal_split_side_by_side(), LayoutEngine, pane(), PaneRect, Rect, resize_ratio_changes(), Option, PaneId (+20 more)

### Community 6 - "Allocation & Backpressure Reports"
Cohesion: 0.14
Nodes (42): alloc_profile(), AllocReport, BackpressureReport, CoalescingReport, collect_latency(), cpu_seconds(), cpu_usage(), drain_all() (+34 more)

### Community 7 - "CLI Pane Commands"
Cohesion: 0.12
Nodes (36): main(), pane_cmd(), print_help(), print_response(), required(), Result, String, tab_cmd() (+28 more)

### Community 8 - "Child Process I/O Buffer"
Cohesion: 0.10
Nodes (20): Child, PendingWrite, PtyManager, PtySession, ReadResult, Arc, Box, HashMap (+12 more)

### Community 9 - "Terminal State & Scrollback"
Cohesion: 0.11
Nodes (4): render_snapshot_is_immutable_view(), String, scroll_reuses_buffers(), TerminalState

### Community 10 - "Workspace Persistence Model"
Cohesion: 0.12
Nodes (27): new_id(), Pane, PaneType, PersistedState, Into, Option, PaneId, Self (+19 more)

### Community 11 - "Cell & Unicode Basics"
Cohesion: 0.13
Nodes (17): ascii_dirty_rows(), basics_ascii(), Cell, combining_marks(), cursor_wrap_and_margin(), dirty_tracking_incremental(), emoji_zwj_cluster_merges(), line_wrap_and_scrollback() (+9 more)

### Community 12 - "VT Event Parser"
Cohesion: 0.12
Nodes (16): TerminalEvent, csi_cursor_moves(), end_to_end_state_update(), osc_title(), parse_events(), Parser, Performer, plain_text_becomes_write_chars() (+8 more)

### Community 13 - "Scrollback Rows & Cold Blocks"
Cohesion: 0.16
Nodes (17): Row, Vec, block_roundtrip(), ColdBlock, ColdStore, compress_block(), decode_block(), decode_scratch_row() (+9 more)

### Community 14 - "Glyph Raster Benchmarks"
Cohesion: 0.15
Nodes (24): baseline_path(), budget_table(), generate_output(), glyph_raster_us_per_glyph(), load_baseline(), main(), measure_input_p95(), MetricGetter (+16 more)

### Community 15 - "Performance Budgets & Gates"
Cohesion: 0.12
Nodes (25): benchmarks Crate, CI Performance Gates, Cold Start Budget, Input Latency Budget, Performance Budgets, Performance Plan, Render Latency Budget, Tiered Scrollback (ADR-0004) (+17 more)

### Community 16 - "Core Architecture & Multiplexer"
Cohesion: 0.11
Nodes (24): CLI Control Interface, Crash Recovery, Pane Fairness / Focus Priority, Shared Glyph Resources (Atlas), Input Routing, IPC Protocol, Layout Engine, Native Multiplexer + Workspace Engine (Phase 1) (+16 more)

### Community 17 - "Agent & AI Integration"
Cohesion: 0.11
Nodes (24): Agent Abstraction, Agent-to-Agent Communication, Agent Integration Levels (CLI/Native/ACP), Agent Registry, Universal Approval Layer, BYOK / Provider Abstraction, CLI / Socket API, AI Context Engine (+16 more)

### Community 18 - "Unicode/CJK/Combining"
Cohesion: 0.13
Nodes (14): arabic_renders_but_not_shaped(), ascii_hello(), cell_width_sum_equals_cursor(), cjk_widths_and_cursor(), combining_acute(), emoji_are_wide(), kana_widths_and_cursor(), precomposed_accent() (+6 more)

### Community 19 - "Tech Stack & Architecture"
Cohesion: 0.13
Nodes (23): Strict Layered Architecture, macOS Apple Silicon First, Performance Budgets, portable-pty (PTY Management), Rust (Core Language), vte (VT Parser), wgpu (GPU Rendering), winit (Windowing) (+15 more)

### Community 20 - "Phase 0.5.1 Integration Tests"
Cohesion: 0.18
Nodes (20): burst_paste_without_drain_does_not_deadlock(), child_exits_normally(), child_killed_by_signal(), close_while_streaming(), drain_until(), fzf_roundtrip(), git_diff_pager_roundtrip(), grid_contains() (+12 more)

### Community 21 - "Pi CLI Workspace Skill"
Cohesion: 0.11
Nodes (21): Corpus Pin (pi-mono commit 3b7448d), Pi CLI Workspace Skill, available_skills Read Tool Gate, Skill Name Collisions, Auto-Compaction Cut-Point, Credential Resolution Order, Message Queue (Steering / Follow-up), Model Shorthand (provider/id, name:thinking) (+13 more)

### Community 22 - "Extension Authoring Skill"
Cohesion: 0.10
Nodes (19): Extension Authoring Corpus Pin, extendResources(), ExtensionAPI (TypeScript extension host), Extension Custom Tools & Commands, Pi Extension Authoring Skill, Extension TUI Component Integration, Package Authoring Corpus Pin, loadSkills (first-name-wins) (+11 more)

### Community 23 - "Architecture Hardening & Font"
Cohesion: 0.12
Nodes (21): AI-Independent Foundation Constraint, Architecture Hardening, Benchmark Suite, Dirty Region Tracking, Event Queue / Batching, Font Architecture / Text Stack, GPU Glyph Atlas, Glyph Cache (+13 more)

### Community 24 - "Paste/Scrollback Benchmarks"
Cohesion: 0.14
Nodes (12): grid_has(), main(), shell(), CountingAlloc, fast_rng(), fill_rows(), main(), measure() (+4 more)

### Community 25 - "Project Structure & CI"
Cohesion: 0.14
Nodes (20): CI/CD Pipeline, Crate Structure, FlashTerminal, Performance Budgets, crates/terminal-core, Manual Desktop Validation Checklist, Multiplexer (Phase 1 Engine), Fairness & State Batching (+12 more)

### Community 26 - "Scrollback Strategy & Benchmarks"
Cohesion: 0.15
Nodes (19): Allocator Retention Investigation, Scrollback Benchmark Suite, PTY/Event Channel Capacity, Cold Scrollback Tier, Scrollback Compression Strategies, Optional Disk Storage Tier, Event Coalescing Regression, Explicitly Excluded Phase 1 Features (+11 more)

### Community 27 - "Render Pipeline & Scrollback Tiers"
Cohesion: 0.12
Nodes (19): Bounded crossbeam Channel (cap 1024), fontdue Text Stack, Glyph Atlas + Instanced Quads Renderer, RenderSnapshot Boundary, terminal-session (Ownership Hub), ADR-0004, ColdStore (RLE+flate2 Blocks), Hot Scrollback Tier (+11 more)

### Community 28 - "Core Architecture & Agents"
Cohesion: 0.16
Nodes (18): Agent Abstraction, Core Architecture Layering, Benchmark Harness, Event Bus, Local-Only / No Cloud Backend, Multiplexing (Session/Pane/Tab/Workspace), Pane, Performance Budgets (+10 more)

### Community 29 - "Pi RPC SDK & CI"
Cohesion: 0.15
Nodes (17): CI Workflow, Performance Check Job, AgentSessionRuntime, createAgentSession, Pi RPC SDK Skill, pi-mono Repository, rpc.md Framing Doc, Agent-Native Support (+9 more)

### Community 30 - "Notification Center"
Cohesion: 0.20
Nodes (10): center_broadcasts(), Notification, NotificationCenter, NotificationKind, Option, PaneId, Self, String (+2 more)

### Community 31 - "Screen Clear & Insert/Delete"
Cohesion: 0.24
Nodes (3): clear_screen_and_line(), insert_delete_chars(), insert_delete_lines()

### Community 32 - "Terminal Modes & Attributes"
Cohesion: 0.18
Nodes (5): alt_screen_preserves(), Attribute, Modes, Default, Self

### Community 33 - "Cursor & Selection"
Cohesion: 0.17
Nodes (7): Cursor, RenderSnapshot<'a>, Option, VecDeque, SavedScreen, Selection, selection_clamped_on_resize()

### Community 34 - "PTY/Parsing/IO Pipeline"
Cohesion: 0.24
Nodes (13): Bounded Channel, FlashTerminal App, Font Stack, Window / GPU, Input & Selection, IO Thread · Terminal-Session, VT Parser, PTY Manager (+5 more)

### Community 35 - "Terminal Core Benchmarks"
Cohesion: 0.39
Nodes (11): bench_cell_write(), bench_clear(), bench_cursor_move(), bench_dirty_tracking(), bench_insert_delete_lines(), bench_random_access(), bench_resize(), bench_row_scroll() (+3 more)

### Community 36 - "Allocator Profiler"
Cohesion: 0.27
Nodes (6): CountingAlloc, main(), read_counters(), reset_counters(), GlobalAlloc, Layout

### Community 37 - "Color & SGR Parsing"
Cohesion: 0.36
Nodes (3): Color, parse_compound_color(), sgr_compound_colors()

### Community 38 - "Pipeline Benchmarks"
Cohesion: 0.57
Nodes (6): bench_input_latency_under_load(), bench_pipeline(), bench_throughput_mbps(), generate_output(), Criterion, Vec

### Community 40 - "Raw Throughput Test"
Cohesion: 0.83
Nodes (3): grid_has(), main(), render_visible()

### Community 41 - "Pi JSONL/RPC Modes"
Cohesion: 1.00
Nodes (3): JSONL LF Framing, Pi --mode json, Pi --mode rpc

### Community 42 - "ADR-0003 Session Snapshot"
Cohesion: 0.67
Nodes (3): ADR-0003, Architecture Proposal, Current Architecture (Phase 1)

## Knowledge Gaps
- **67 isolated node(s):** `Uniforms`, `run_benchmarks.sh script`, `Shell / TUI`, `Window / GPU`, `Scrollback Separation` (+62 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **4 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `TerminalState` connect `Terminal State & Scrollback` to `Terminal Modes & Attributes`, `GPU Render State & Dirty Tracking`, `Cursor & Selection`, `Terminal Core Benchmarks`, `Soak & Resource Tests`, `Color & SGR Parsing`, `Allocation & Backpressure Reports`, `Benchmark & Multiplex Tests`, `Raw Throughput Test`, `Cell & Unicode Basics`, `Scrollback Rows & Cold Blocks`, `Unicode/CJK/Combining`, `Phase 0.5.1 Integration Tests`, `Paste/Scrollback Benchmarks`, `Screen Clear & Insert/Delete`?**
  _High betweenness centrality (0.265) - this node is a cross-community bridge._
- **Why does `Multiplexer` connect `Benchmark & Multiplex Tests` to `Desktop App & Layout Chrome`, `Soak & Resource Tests`, `Layout Engine`, `CLI Pane Commands`, `Child Process I/O Buffer`, `Terminal State & Scrollback`, `Workspace Persistence Model`, `Notification Center`?**
  _High betweenness centrality (0.244) - this node is a cross-community bridge._
- **Why does `Renderer` connect `GPU Render State & Dirty Tracking` to `Desktop App & Layout Chrome`, `Font Library & Caching`?**
  _High betweenness centrality (0.098) - this node is a cross-community bridge._
- **What connects `Uniforms`, `run_benchmarks.sh script`, `Shell / TUI` to the rest of the system?**
  _67 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Benchmark & Multiplex Tests` be split into smaller, more focused modules?**
  _Cohesion score 0.07211646136618141 - nodes in this community are weakly interconnected._
- **Should `GPU Render State & Dirty Tracking` be split into smaller, more focused modules?**
  _Cohesion score 0.05621621621621622 - nodes in this community are weakly interconnected._
- **Should `Desktop App & Layout Chrome` be split into smaller, more focused modules?**
  _Cohesion score 0.06836055656382335 - nodes in this community are weakly interconnected._