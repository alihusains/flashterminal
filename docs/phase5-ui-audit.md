# Phase 5 UI Audit — Visual Design Pass

Scoped response to a direct visual-quality complaint against the real running app ("the look sucks"), backed by `ui-ux-pro-max` research. This is **not** the full Phase 5 audit (`phases/5.md`'s 45 sections — screenshots of 20 product states, three-persona evaluation, agent/approval/workflow UX, command palette, etc.) — that remains outstanding. This document covers what was actually found and fixed in this pass.

## Root cause: colors were rendering 2–3× brighter than authored (fixed)

`crates/terminal-renderer/src/lib.rs` selected an **sRGB** wgpu surface format (`.find(|f| f.is_srgb())`). Every color value in the codebase — `DEFAULT_BG`/`DEFAULT_FG`, ANSI 256-color and 24-bit true-color resolution (`resolve_color`, naive `channel as f32 / 255.0`), and the desktop app's chrome/state colors — is authored as a direct, non-gamma-encoded float. With an sRGB surface, the GPU treats every value the shader writes as **linear** and auto-converts it to sRGB on present. Decoding what the surface actually displays: the intended near-black `DEFAULT_BG` (`#13151C`) was rendering as `#595961` — a medium slate gray, confirmed against the pre-fix screenshot. This affected every terminal color the app has ever rendered (`ls` colors, `git diff`, prompt themes, syntax highlighting), not just the sidebar — the systemic reason the whole app looked flat and washed out rather than the high-contrast dark theme the color constants were clearly written to produce.

**Fix**: prefer a non-sRGB surface format instead (`crates/terminal-renderer/src/lib.rs`) — one line, no shader changes, no per-color-site changes needed anywhere in the codebase. Verified via real screenshots (`docs/screenshots/phase5/01-first-launch.png` before → `01-first-launch-fixed.png` after).

## Design system applied (research: `ui-ux-pro-max`, evidence-backed)

- **Font preference reordered** (`crates/terminal-text/src/lib.rs`): JetBrains Mono / Cascadia now preferred over Menlo/Monaco when installed — purpose-built for coding legibility (dotted zero, distinct 1/l/I, taller x-height). Menlo/SF Mono remain reliable fallbacks (always present on macOS); zero packaging cost, no bundled font added.
- **Color palette redesigned** (`apps/desktop/src/main.rs`): near-black slate family (`#0B1220`/`#111827`) instead of neutral flat gray — avoids pure-black/pure-white extreme contrast (OLED halation, eye strain on long sessions) while giving chrome and terminal content areas visibly distinct, intentional depth instead of one undifferentiated panel.
- **State colors widened**: `StateStarting` (amber) and `NeedsApproval` (orange) were close enough in hue to be hard to tell apart at a glance; separated them. Every state is still shown with its name in text next to the color dot — never color alone.
- **Visual hierarchy**: added a 1px sidebar/terminal divider and section dividers (WORKSPACES/TABS/AGENTS), widened inter-section spacing (`cell_h × 1.6` vs. the previous single-line gap) for clearer grouping without adding visual noise.

## Resolved: the "p renders as P" anomaly was a glyph baseline-positioning bug

Root-caused and fixed. It was never a text-content or case bug — a controlled diagnostic (`Session::spawn` a real PTY, write a known string, dump `TerminalState`'s grid cells directly) proved the parsed/stored characters were always correct (`export`, `Applications`, etc.), ruling out parsing entirely. A pixel-level zoom into the actual rendered screenshot then showed every lowercase `p` (and every other descender: g/j/q/y) drawn small and floating above the baseline, at superscript height — which reads as a capital letter at a glance because it lacks a visible descender, not because the wrong character is drawn.

**Root cause**: `crates/terminal-renderer/src/lib.rs`'s glyph-positioning formula computed `ascent + bearing_y - height` where it needed `ascent - bearing_y - height`. `bearing_y` is fontdue's `ymin` — confirmed against fontdue's own source doc comment — negative when a glyph's bitmap extends *below* the baseline (descenders), positive/zero otherwise. This codebase's own doc comment on the field had the sign backwards ("negative = above"), which is what produced the formula bug: for glyphs with `bearing_y ≈ 0` (the vast majority of letters) the sign error is invisible, but for descenders it shifts the glyph up by roughly `2×|bearing_y|` pixels — exactly the "floating above baseline" artifact observed, and present at **both** glyph-drawing call sites (terminal grid and sidebar/chrome text — e.g. "Please" in the sidebar was affected too).

**Fix**: extracted the position math into one pure function, `glyph_top_y(ascent, bearing_y, height)`, used at both call sites, with the correct sign. Added two permanent unit tests (`terminal-renderer::tests`) asserting a descender's bitmap must extend below the baseline and that a deeper descender renders lower, not higher — regression coverage that would have caught this on day one.

**Verified**: real screenshot zoom before/after (`docs/screenshots/phase5/02-zoomed.png` → `03-zoomed-fixed.png`) — every `p`/descender now sits correctly on the baseline. Full workspace test suite, clippy, release build, and the performance gate all green; this is a pure position-arithmetic fix, no hot-path or measured-metric impact.

## Verification

Full workspace `fmt`/`clippy`/`check`/`test` (2×) green; release build green; performance gate (`cargo run --release -p benchmarks -- --ci`) unaffected — a surface format choice and a compile-time font preference list touch neither the hot render/parse path nor any measured metric.

## Screenshots

- `docs/screenshots/phase5/01-first-launch.png` — before (as shipped)
- `docs/screenshots/phase5/01-first-launch-redesign.png` — after palette/spacing changes, before the gamma fix (still washed out — proves the gamma bug was the dominant factor, not the palette choice)
- `docs/screenshots/phase5/01-first-launch-fixed.png` — after the gamma fix — the real current state
