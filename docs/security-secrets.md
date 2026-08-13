# Secrets: Storage, Redaction, and Leak Tests

Phase 2B.1 position: **secrets are stored in the OS keychain, referenced
everywhere else, and masked at every boundary before they could leak.**

## Storage model

| Layer | Holds | Never |
|-------|-------|-------|
| OS keychain | the value (single durable copy) | — |
| `AgentLaunchConfig` / pane metadata / `state.json` | `keychain://flashterminal/<provider>` **references** | values |
| `AgentSnapshot` (desktop/IPC `AgentList`) | references, provider/model ids | values |
| `AgentLaunchContext` (ephemeral) | resolved value for the child env | persisted/logged/IPC'd |
| logs, notifications, errors | provider ids, redacted text | values |
| IPC frames | redacted output | values |

## Redaction

`Redactor` (`crates/terminal-session/src/redact.rs`):

- **Registered values**: resolved credentials and test sentinels are
  registered process-wide and masked wherever they appear (longest-first
  so nested secrets mask fully).
- **Known shapes**: `sk-ant-…`, `sk-proj-…`, `sk-…`, `AIza…`, `xai-…`,
  `ghp_…` are masked even when unregistered.
- Applied at: agent output events, permission payloads, error strings,
  spawn diagnostics, IPC event frames, and `AgentLaunchConfig::redact()`
  before any persistence boundary (arguments masked in place, env values
  carrying secrets removed).

## Phase 2B.1 security review (§31) — findings & fixes

Audited: `credential.rs`, `redact.rs`, `provider.rs`, `agent.rs`, IPC,
persistence, logging. Checked `Debug`/`Display` derives, serde
serialization, error messages, panic paths, tracing calls, and IPC frame
shapes.

1. **Fixed — `MemoryBackend` derived `Debug`** printed every stored API
   key (the `Mutex<HashMap>` contents). Replaced with a manual `Debug`
   that lists keys and marks values redacted; unit test
   `debug_never_reveals_stored_secrets`.
2. **Fixed — launch configs were stored raw.** `AgentLaunchConfig::redact()`
   existed but was never invoked and did not cover `arguments`; pane
   metadata and session snapshots carried secret-shaped argument values
   into `state.json`, `snapshot_state`, and `AgentList` IPC responses.
   `redact()` now masks arguments too and is applied at both storage
   points (`AgentRuntime` session store, pane metadata).
3. Clean — `CredentialStore`/`KeychainBackend`/`ProviderRegistry` hold no
   values worth printing; `AgentLaunchContext` is never formatted;
   credential reads log provider ids only; spawn errors redact the
   command line.

## Sentinel leak tests (automated)

```text
SUPER_SECRET_TEST_VALUE_2B1 / sk-ant-SUPER_SECRET_TEST_VALUE_PERSIST_…
```

- `crates/terminal-workspace/tests/ipc_stream.rs`
  `sentinel_secret_never_reaches_events_or_persistence` — the agent
  *emits* the sentinel via `--echo`; asserts it never appears in IPC
  frames, `snapshot_state`, `state.json`, or `AgentSnapshot`.
- `crates/terminal-workspace/tests/persistence.rs` — agent panes persist
  config + references, never contents; crash recovery re-launches a fresh
  process from redacted config.
- `crates/terminal-session/src/redact.rs` — unit coverage of the masker.
- `crates/terminal-session/src/launch.rs` — `redact_masks_registered_secret_in_arguments`.

The IPC slow-client path additionally proves a wedged client can neither
block the engine nor stall subscribers beyond the slow-client policy
(bounded queues, coalescing, drops, disconnect).