# Phase 4 Desktop Validation — Screenshots & Evidence

`01-demo-workspace.png` is a **real capture** of the running FlashTerminal
desktop window (`desktop --demo`, release build) taken with
`screencapture -l <window-id>` at the exact window bounds reported by
`CGWindowList` (1200×792 pt / 2400×1584 px @2x).

## What was validated (2026-08-15, live app)

Launched `target/release/desktop --demo` and drove the running instance
over its IPC control socket with the CLI:

```text
window on-screen:  CGWindowList reports "FlashTerminal — FlashTerminal Demo",
                   layer 0, bounds (296, 106, 1200, 792)  ✓
renderer clean:    app log 0 bytes (no wgpu/naga errors)   ✓
attention:         4 item(s) need a human decision (agent NeedsApproval,
                   2 review tasks, 1 pending replan)       ✓
PAUSE ALL:         "workflow paused — no new work starts"  ✓
scheduler gated:   0 queued · running preserved honestly   ✓
RESUME ALL:        "workflow resumed"                       ✓
STOP ALL:          "7 agent(s) stopped · 1 task(s) stopped · 3 human
                    decision(s) preserved"                  ✓
workflow summary:  workflows/running/needs-you/cost         ✓
workflow timeline: v1 plan, approved=false, diff           ✓
workflow replans:  pending replan surfaced                  ✓
```

## Capture limitation (honest record)

The wgpu/Metal surface is **not composited into captures** without macOS
Screen Recording permission for this session (identical to the Phase 3A.1
record). The window-id capture therefore shows the native window chrome
(title bar, gray content surface) rather than the rendered terminal
glyphs. Full-screen captures show the window on-screen with real content
regions but are mixed with the desktop.

The launch + render + stability + full IPC surface evidence stands on its
own. For a pixel-perfect visual pass, grant Screen Recording permission
and re-run:

```bash
target/release/desktop --demo
```
