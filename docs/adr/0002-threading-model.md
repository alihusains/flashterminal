# ADR 0002: Threading Model and State Ownership

## Status
Accepted

## Context
The initial implementation used an `Arc<Mutex<TerminalState>>` shared between a background PTY reader thread and the main UI/render thread. This caused severe contention under heavy output, full-state cloning on every update, and potential UI freezes, violating our <8ms input latency and <40MB RAM budgets.

## Decision
We will adopt a **Message-Passing, Single-Owner** architecture for the terminal state.

1. **Single Owner**: The Main Thread exclusively owns the authoritative `TerminalState` and the `Renderer`.
2. **Message Passing**: The PTY Reader Thread will read bytes and parse them into a compact batch of `TerminalEvent`s (e.g., `WriteChar`, `MoveCursor`, `ClearScreen`).
3. **Bounded Channel**: These event batches are sent to the Main Thread via a bounded `crossbeam-channel` (or `tokio::sync::mpsc`). This provides natural backpressure: if the UI is busy, the PTY thread will block on the channel send, preventing unbounded memory growth from queued events.
4. **Dirty Tracking**: The `TerminalState` will maintain a `DirtyTracker` that records exactly which rows, the cursor, or the title have changed during a batch of events. The renderer will only process dirty regions.

## Consequences

### Positive
- **No Lock Contention**: The main thread never blocks waiting for the PTY thread, and vice versa (except for bounded channel backpressure, which is desired).
- **Zero-Copy Rendering Prep**: Dirty tracking allows the renderer to only update changed rows, drastically reducing GPU vertex buffer updates.
- **Predictable Memory**: Bounded channels prevent OOM during massive output bursts.

### Negative
- **Parsing Overhead**: Parsing on the background thread means we must serialize state changes into `TerminalEvent`s. However, this is far cheaper than cloning the entire grid.
- **Complexity**: Requires careful design of the `TerminalEvent` enum to cover all VT sequences without becoming a bottleneck itself.

## Alternatives Considered
1. **Main Thread Parsing**: Read PTY asynchronously on the main thread. *Rejected*: Large bursts of PTY output (e.g., `cat` a 10MB file) would block the event loop, causing dropped frames and input latency spikes.
2. **Shared State with RwLock**: *Rejected*: Still causes contention. Writers (PTY) are high-frequency; Readers (Renderer) are also high-frequency (60+ FPS). This leads to writer starvation or reader stalls.
3. **Lock-Free Ring Buffer**: *Rejected*: Overly complex for the current stage. Bounded channels provide sufficient backpressure with standard library/`crossbeam` reliability.
