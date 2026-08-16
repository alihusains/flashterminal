# FlashTerminal Phase 4: Final Pre-Commit Audit and Milestone Commit

Phase 4 has completed all implementation and release gates.

Current known status:

```text
Phase 4: READY FOR PRODUCTION BETA

Full workspace tests: 354 passed, 0 failed
Clippy: 0 errors
Fmt: clean
Release build: clean

Determinism stress:
25/25 parallel runs passed

Phase 4 implementation:
complete

Phase 4 documentation:
complete

Phase 4 screenshots:
captured

Security/adversarial tests:
passing
```

There are currently approximately 183 changed files and the last commit is:

```text
a289039 "3c done"
```

Your task now is ONLY to perform the final repository audit and create the Phase 4 milestone commit.

Do NOT modify product behavior unless the pre-commit audit discovers a genuine blocking issue.

Do NOT begin Phase 5.

---

# 1. Inspect Repository State

Run:

```bash
git status --short
git status
git branch --show-current
git log --oneline -5
```

Confirm the current branch and repository state.

---

# 2. Inspect Changed Files

Run:

```bash
git diff --stat
git diff --name-only
```

Review all changed files.

Group them conceptually:

```text
Phase 4 source
Phase 4 tests
Phase 4 documentation
ADRs
screenshots
benchmarks
configuration
generated artifacts
```

---

# 3. Reject Unintended Files

Before staging, identify and exclude any:

```text
temporary files
debug dumps
logs
/tmp artifacts
local environment files
API keys
credentials
tokens
private keys
build artifacts
binaries
editor files
machine-specific configuration
```

Do NOT stage secrets.

Pay particular attention to:

```text
.env
*.key
*.pem
credential files
provider configs
debug exports
diagnostic dumps
```

---

# 4. Secret Scan

Search the repository for likely secrets.

Check for patterns such as:

```text
sk-
api_key
api-key
token
secret
password
BEGIN PRIVATE KEY
ANTHROPIC_API_KEY
OPENAI_API_KEY
OPENROUTER_API_KEY
```

Distinguish legitimate source-code identifiers/documentation examples from actual credentials.

If an actual credential is found:

STOP.

Do not commit.

Report the file and location.

---

# 5. Validate Persistence Safety

Verify that persisted examples, tests, fixtures and screenshots contain:

```text
credential references
```

rather than:

```text
credential values
```

Confirm the Phase 4 secret-regression tests still pass.

---

# 6. Inspect Diff for Accidental Changes

Review suspicious categories manually:

```text
Cargo.lock
Cargo.toml
configuration files
scripts
benchmark output
screenshots
docs
```

Make sure no unrelated changes slipped into the Phase 4 work.

Do not remove legitimate Phase 4 files simply because they are numerous.

---

# 7. Final Verification

Run one final clean verification from the current working tree:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo build --release --workspace
git diff --check
```

All must pass.

Record exact results.

---

# 8. Verify Phase 4 Documentation

Confirm these exist and are coherent:

```text
docs/phase4.md
docs/security-model.md
docs/policy-engine.md
docs/autonomy.md
docs/audit-trail.md
docs/recovery.md
docs/benchmark-reliability.md
```

Confirm ADRs:

```text
docs/adr/0017-policy-engine.md
docs/adr/0018-autonomy-model.md
docs/adr/0019-audit-trail.md
docs/adr/0020-recovery-model.md
```

Confirm Phase 4 screenshots are present under:

```text
docs/screenshots/phase4/
```

Do not claim a screenshot proves something that could not be captured.

Preserve any honest wgpu-capture limitation note.

---

# 9. Stage Phase 4

Stage only the intended Phase 4 changes.

Use:

```bash
git add ...
```

Do not use:

```bash
git add .
```

blindly unless the preceding audit confirms that every untracked/modified file belongs to Phase 4 and contains no secrets or temporary artifacts.

---

# 10. Review Staged Content

Run:

```bash
git status --short
git diff --cached --stat
git diff --cached --name-only
```

Then inspect the most important staged diffs.

Confirm:

```text
policy engine
autonomy
approval
audit
recovery
security
benchmarks
tests
docs
screenshots
```

are included.

---

# 11. Commit Message

Create exactly one milestone commit:

```text
feat: complete phase 4 production hardening
```

The commit message should make it clear that this is the Phase 4 production-beta milestone.

Do not make unrelated cleanup commits.

---

# 12. After Commit

Immediately run:

```bash
git status
git log --oneline -3
```

The working tree should be clean except for intentionally ignored files.

Confirm:

```text
working tree clean
```

---

# 13. Optional Milestone Tag

After the commit succeeds, DO NOT create a public release tag automatically.

Instead report the exact commit SHA and recommend a beta tag such as:

```text
v0.1.0-beta.1
```

for explicit human approval.

Do not push anything remotely.

Do not publish anything.

Do not create a GitHub release.

---

# 14. Final Response

Return:

## Commit

```text
commit:
message:
SHA:
```

## Verification

```text
tests:
clippy:
fmt:
release build:
diff check:
```

## Files

```text
files changed:
files staged:
```

## Security

State whether the pre-commit secret audit found any credentials.

## Working Tree

State whether the working tree is clean.

## Next Phase

State:

```text
PHASE 4 COMMITTED
READY FOR NEXT PRODUCT DEVELOPMENT CYCLE
```

Do not begin Phase 5 automatically.
