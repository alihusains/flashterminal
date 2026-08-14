# Phase 3A Manual validation script (desktop)

Manual desktop pass for the Phase 3A task orchestration UI. Run against a
release desktop build.

> Screenshots are best-effort: this session's host lacks Screen Recording
> permission, so the pass below was performed with programmatic window
> checks (CGWindowList) + application log validation. Re-run the visual
> checks with a screen-recording-enabled terminal for full confirmation.

## Setup

1. Build and launch:

   ```text
   cargo build --release -p desktop
   ./target/release/desktop
   ```

2. Verify launch: window appears (≈1200×792), no stderr output, process
   stays alive >30 s.

## Task dashboard

3. `Cmd+Option+T` (or palette → Show Tasks) — dashboard opens with
   live counts: queued · running · completed · failed · cost.
4. Create a task: palette → **Create Task** → type a title → Enter.
   The task appears in the list as Pending.
5. Select ↑/↓ and press `Enter` — **task detail panel** opens: status,
   agent, attempts, duration, cost, dependencies, result summary, files
   changed, commands, artifacts, error (when present). `Esc` returns.
6. Press `u` — all tasks run (dashboard stays open, rows move
   Pending→Ready→Running→Completed; with the fake-agent fixture and the
   real agent binaries present, real agents spawn into their own panes).
7. `c` cancels a running task (Running→Cancelled); `r` retries a Failed
   one; `a`/`d` approve/reject at the review boundary (tasks created
   with `--review` semantics stop at NeedsReview and the workflow halts
   until resolved).

## Filters

8. Palette: **Show Blocked Tasks** — only Blocked rows render (blocked
   rows appear when a dependency failed with the block policy).
9. Palette: **Show Tasks Needing Review** — only NeedsReview rows.
10. Palette: **Open Task** — selects the first visible task and opens
    its detail directly.

## Agent pane

11. From the detail panel press `p` (or `v` for the work view) — the
    task's live agent pane attaches to the workspace and its output is
    visible in the terminal area.

## Esc behavior

12. Esc unwinds: create form → detail panel → dashboard → everything.

## Expected evidence to record

- Launch + stability (no crash, clean log) — **PASS** (2026-08-14,
  programmatic check: window layer 0, bounds 1200×792).
- First frame render — **PASS** (six renderer bugs found + fixed during
  this pass — see `docs/phase3a-verification.md` §8).
- Keystroke interaction — partially performed earlier with assistive
  access; re-run visually by a human with screen permission.