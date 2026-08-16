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

## Not yet resolved — flagged for follow-up

A specific letter-substitution pattern appears in the terminal content in both the before and after screenshots: every lowercase `p` renders as uppercase `P` (`export` → `exPort`, `Applications` → `APPlications`, `.app` → `.aPP`), while every other letter is unaffected. This is deterministic and reproducible (same shell startup output both times), but its origin is unconfirmed — it could be a narrow glyph-selection bug (a case-folding or index-offset error isolated to one character) or genuine pre-existing shell-startup content on this machine (a `.bashrc`/`.bash_profile` line). `crates/terminal-text/src/lib.rs`'s glyph cache lookup (keyed by `(glyph_index, px, font_hash)` from `fontdue`, a mature upstream crate) showed no obvious bug on inspection. Needs a dedicated investigation — not resolved in this pass.

## Verification

Full workspace `fmt`/`clippy`/`check`/`test` (2×) green; release build green; performance gate (`cargo run --release -p benchmarks -- --ci`) unaffected — a surface format choice and a compile-time font preference list touch neither the hot render/parse path nor any measured metric.

## Screenshots

- `docs/screenshots/phase5/01-first-launch.png` — before (as shipped)
- `docs/screenshots/phase5/01-first-launch-redesign.png` — after palette/spacing changes, before the gamma fix (still washed out — proves the gamma bug was the dominant factor, not the palette choice)
- `docs/screenshots/phase5/01-first-launch-fixed.png` — after the gamma fix — the real current state
