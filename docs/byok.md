# Bring Your Own Key (BYOK)

FlashTerminal never stores provider credentials itself. You bring your own
API keys; the app keeps them in the **OS keychain** and references them
from every other layer.

## How it works

```text
keychain://flashterminal/<provider>
        │  (a reference, never the value)
        ▼
AgentLaunchConfig.credential_ref   (persisted with the workspace)
        │
        ▼
AgentRuntime::spawn ──► CredentialStore.get_api_key(provider)
        │                     │
        │  value lives only here ──► OS keychain
        ▼
child process environment (e.g. ANTHROPIC_API_KEY)
        │
        ▼
Redactor.register_secret(value)   ← any output/error/log/IPC/persistence
                                     containing the value is masked
```

## Setting a key

Key provisioning today happens through the `CredentialStore` API —
`store.set_api_key(provider_id, key)` writes the OS keychain
(`CredentialStore::system()`). The desktop UI provisioning surface is
pending (Phase 2C); until then, agents launched with `provider_id` /
`credential_ref` resolve keys from whatever the store holds for the
registered providers listed in `docs/providers.md`.

## Guarantees (Phase 2B.1 §28–§31)

- The keychain entry is the **only** durable copy of the value.
- `AgentLaunchConfig` and pane metadata persist `credential_ref` URIs
  only; launch arguments and environment are defensively redacted before
  every persistence boundary (`AgentLaunchConfig::redact()`).
- The value is registered with the `Redactor` the moment it is resolved,
  so agent output, errors, IPC frames, notifications, and state files can
  never contain it (verified by sentinel tests —
  `crates/terminal-workspace/tests/ipc_stream.rs`,
  `crates/terminal-workspace/tests/persistence.rs`).
- `Debug` implementations on credential holders redact values
  (`MemoryBackend` ships a manual impl; unit-tested).
- Restart/resume re-resolves the key from the keychain via the stored
  reference — the persisted launch config never carries enough to
  authenticate on its own.

## Headless / testing

`CredentialStore::with_backend(MemoryBackend)` swaps the OS keychain for
an in-memory map (tests, `terminal serve`). Same guarantees: the map's
`Debug` output is redacted and it never writes to disk.
