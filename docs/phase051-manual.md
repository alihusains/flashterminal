# Phase 0.5.1/0.5.2 — Manual Desktop Validation

This is the **human-in-the-loop** checklist for the FlashTerminal desktop
app (`cargo run --release -p apps/desktop`) on a real macOS session. The
headless harness covers everything automatable; the items below involve a
live window, the GPU surface, and OS sleep/wake and therefore cannot run in
CI.

> Status legend: ⬜ not run · ✅ pass · ❌ fail · ⚠️ notes

---

## 1. Startup

| Check | Expect | Status |
|-------|--------|--------|
| Cold start to first frame | < 250 ms (Instrument or `time`) | ⬜ |
| No black frame on launch | first frame = shell prompt | ⬜ |
| Focus/keystrokes immediately accepted | `echo hi` renders | ⬜ |

## 2. TUI applications

| App | Startup | Resize | Scroll | Cursor | Alt-screen | Colors | Selection | Copy |
|-----|:-------:|:------:|:------:|:------:|:----------:|:------:|:---------:|:----:|
| vim / nvim | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |
| less | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |
| fzf | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |
| htop / top | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |
| git diff | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ | ⬜ |

Record any incompatibility below (scrollback tiering §8 of 0.5.2: exit
alt-screen with history intact; scroll through cold history in `less`).

## 3. Window lifecycle

| Check | Status |
|-------|--------|
| Open / close / reopen (no PTY leak, no orphan shell) | ⬜ |
| Minimize → restore (grid intact) | ⬜ |
| Maximize → restore | ⬜ |
| Resize (grid reflows, no crash) | ⬜ |
| Rapid resize (drag corner fast, ≥ 10 changes/s) | ⬜ |
| Close while a process is running (child reaped) | ⬜ |
| Close while output is streaming (reader thread exits cleanly) | ⬜ |

## 4. Sleep / wake (macOS)

| Check | Status |
|-------|--------|
| Multiple terminals + active output streams → sleep → wake | ⬜ |
| After wake: typing works, no input loss | ⬜ |
| After wake: resize works | ⬜ |
| After wake: scroll works | ⬜ |
| Start new processes after wake | ⬜ |
| No crash / deadlock / permanent black frame / broken PTY / invalid GPU surface | ⬜ |

## 5. Failure handling

| Check | Status |
|-------|--------|
| Child crashes (`kill -9 $$`) → clean message, session restorable | ⬜ |
| Shell exits normally (EOF) → exit status shown, no hang | ⬜ |
| Window closed mid-output → no GPU resource leak | ⬜ |

## 6. Scrollback UX (0.5.2 §8, with tiered cold storage)

| Check | Status |
|-------|--------|
| Scroll into hot history: instant | ⬜ |
| Scroll deep into cold history: no perceptible freeze (< ~16 ms/frame) | ⬜ |
| Selection + copy across a cold/hot boundary | ⬜ |
| Resize while scrolled deep: returns to a consistent position | ⬜ |
| Alternate screen exit restores history (incl. cold) | ⬜ |

---

## How to run

```bash
cargo run --release -p apps/desktop
```

Record results here as the checklist is completed; a full pass is required
for the Phase 0.5/0.5.2 release gate (`docs/performance.md` §20).
