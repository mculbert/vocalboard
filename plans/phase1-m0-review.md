# Phase 1 · M0 — Post-milestone review remediation (action plan)

## Context

M0 (scaffolding & contracts) is complete. Two reviews were performed against the
delivered tree: a focused review of Step 10 (the CI skeleton) and a comprehensive
review of the whole M0 milestone. Both surfaced issues. This document collects every
finding and gives a self-contained remediation plan an implementer can execute without
the original review session's context.

Severity legend: **[M]** medium (fix before building M1 on top), **[L]** low
(cleanup / robustness), **[N]** nit (optional).

The IPC contract (`proto`) is **not** a persisted format, so none of these changes
require a SQLite/blob/snapshot/settings migration — but any change to `proto` types
**must** regenerate `src/lib/ipc/types.ts` (see W6). Work branch is `claude/*`, so
commit with `--no-gpg-sign` per CLAUDE.md.

## Issue inventory

| ID | Sev | Area | Summary |
|----|-----|------|---------|
| W1 | M | proto + python | `ErrorCode` vocabulary mismatch: Python emits `unknown_command`, absent from Rust enum + `command-surface.md` |
| W2 | M | python | A handler exception kills the whole sidecar loop |
| W3 | M | rust + python | "sidecar ready" handshake is a magic-string match |
| W4 | L | rust | Sidecar child process can be orphaned on exit (no `kill_on_drop`) |
| W5 | L | rust | Pending-request map leaks an entry if stdin write fails |
| C1 | M | CI | Rust↔Python integration test skipped in CI (`SKIP_SIDECAR_TESTS=1`) — the one seam where W1 hides |
| C2 | M | CI | Generated `types.ts` drift not detected (`export_ts_bindings` always rewrites + passes) |
| C3 | L | CI | `cargo audit` and `cargo deny` have divergent advisory-ignore configs |
| C4 | L | CI | Non-deterministic frontend toolchain (`pnpm` `version: latest`); Node-version policy |
| C5 | L | CI | Duplicate CI runs (`push: ['**']` + `pull_request`) |
| F1 | L | frontend | a11y rules may not fail the build (conventions D1 intent) |
| F2 | L | assets | Icon config references stray `src-tauri/icons/icon.ico`, ignores PNG set in `src-tauri/app/icons/` |
| N1 | N | tests | Test `.expect("msg")` → `?` lost descriptive labels — no action |

---

## Group A — Contract & sidecar correctness

### W1 [M] — Fix the `ErrorCode` contract mismatch

**Root cause.** `python/vocalboard_sidecar/dispatch.py` returns `code: "unknown_command"`
for an unknown command and for non-`request` message types (and `test_dispatch.py`
asserts it). But `src-tauri/proto/src/error.rs::ErrorCode` has no such variant and no
catch-all, and `design/command-surface.md` § Error codes (the source of truth) does not
list it. Rust's `FromSidecar` deserialization is strict, so such a line fails to parse;
`core::task::route_line` logs *"unrecognised stdout line"* and drops it — the waiting
request never resolves and `send` times out after 30 s.

**Changes.**
1. `design/command-surface.md` § Error codes table — add two rows:
   - `unknown_command` — "Command name not recognized by the sidecar / unsupported message type"
   - `internal_error` — "Unhandled error inside a sidecar handler" (needed by W2)
2. `src-tauri/proto/src/error.rs` — add `UnknownCommand` and `InternalError` variants
   (snake_case via existing `#[serde(rename_all = "snake_case")]`), each with a doc
   comment (missing_docs is gated). Add a catch-all so future code drift degrades
   gracefully instead of dropping the line:
   ```rust
   /// An error code emitted by a newer component than this build understands.
   #[serde(other)]
   Unknown,
   ```
   Verify `ts-rs` emits a usable TS form for the `#[serde(other)]` variant; if it
   produces something undesirable, add `#[cfg_attr(test, ts(...))]` or fold `Unknown`
   into the generated union manually-consistent output. Confirm `route_line` now maps
   an unknown code to `ErrorCode::Unknown` rather than failing the whole parse.
3. Regenerate `types.ts` (see W6).
4. Python: keep `unknown_command` (now valid); ensure W2 uses `internal_error`.

**Test.** Add a `core::task` unit test (or extend the integration test) asserting that an
error line carrying `unknown_command` resolves the pending request as an `Err` rather than
timing out. Python side already covered by `test_dispatch.py`.

### W2 [M] — Don't let a handler exception crash the sidecar

**Root cause.** In `python/vocalboard_sidecar/__main__.py` the `dispatch(...)` call (the
line after the `try/except` that guards only `parse_message`) is unguarded; a raising
handler propagates out of the stdin `for` loop and terminates the process — no error
emitted, all in-flight/future requests dead.

**Change.** Wrap the dispatch call so a handler exception is caught, logged to stderr via
structlog, and converted into an emitted error message (`make_error_msg(request_id,
"internal_error", str(e))`) while the loop continues. Preserve `request_id` when known so
the Rust waiter is resolved instead of timing out. Keep `cancel` returning `None` (no
response) unchanged.

**Test.** Add a `test_dispatch.py` case: register a handler that raises and assert
`dispatch` (or a small loop-level helper) returns an `internal_error` message rather than
propagating. If the try/except lives in `main()` rather than `dispatch()`, factor a small
`handle_message(...)` function so it is unit-testable.

### W3 [M] — Replace the "sidecar ready" magic-string handshake

**Root cause.** `core::task::route_line` gates startup on `log.msg == "sidecar ready"`
matching the literal string `__main__.py` emits. Editing either string silently breaks
launch (30 s timeout, no clear error).

**Change (decided: typed `Ready` variant).** Introduce an explicit readiness signal in the
contract instead of overloading a human-readable log line:
   - Add a dedicated `FromSidecar::Ready` variant (no payload beyond `type: "ready"`) in
     `src-tauri/proto/src/sidecar.rs`, with a doc comment (missing_docs gated).
   - Python (`__main__.py`) emits `{"type":"ready"}` on startup instead of (or in addition
     to, if a human log line is still wanted) the `"sidecar ready"` log message.
   - `core::task::route_line` fires the ready oneshot channel on the `Ready` variant; drop
     the `log.msg == "sidecar ready"` string comparison.
   - Regenerate `types.ts` (W6) and update the startup docs in `core::task` and
     `__main__.py`, plus any test asserting the ready text (`test_dispatch.py`
     `test_make_log_msg_defaults` uses `"sidecar ready"` as sample text only — keep or
     adjust as appropriate).

**Note on docs.** `plans/phase1.md:75-76` tracks the M3 deferral that startup currently
blocks on `rx.recv()`; that deferral is unaffected — only the readiness *signal* changes.

### W4 [L] — Prevent orphaned sidecar processes

**Root cause.** `core::task::SidecarManager` holds `Child` in a `Mutex` but the `Command`
is not built with `.kill_on_drop(true)`, and there is no explicit shutdown; tokio does not
kill on drop, so the Python process can linger after app exit/crash.

**Change.** Add `.kill_on_drop(true)` to the `Command` builder in `SidecarManager::start`.
Optionally add an explicit `shutdown`/`Drop` that sends EOF/kills the child. Keep minimal.

### W5 [L] — Fix the pending-map leak on stdin write failure

**Root cause.** `SidecarManager::send` inserts `request_id` into `pending` *before* the
`write_all`/`flush`; if either returns via `?`, the entry is never removed (only the
timeout arm cleans up).

**Change.** Insert into `pending` only after a successful write+flush, OR wrap the write so
the entry is removed on the error path before returning. Add a brief unit test if feasible
without a live sidecar (e.g. drive `send` against a closed stdin and assert the map is
empty afterward) — otherwise note it as covered by review.

---

## Group B — CI hardening (`.github/workflows/ci.yml`, `deny.toml`, `package.json`)

### C1 [M] — Actually exercise the Rust↔Python boundary in CI

**Decided scope: Linux leg only** (keeps cross-OS venv pathing out of the matrix while
still testing the real NDJSON round-trip; W1/W3 regressions would be caught).

**Change.** In the `rust-tests` job, on `runner.os == 'Linux'`:
   1. Add an `astral-sh/setup-uv@v5` step (Python 3.11) and run `uv sync` in `python/`
      (or `uv pip install -e python/`).
   2. Export `VOCALBOARD_PYTHON` pointing at the resulting interpreter
      (e.g. `python/.venv/bin/python`).
   3. Do **not** set `SKIP_SIDECAR_TESTS=1` for the Linux leg; keep the skip on
      windows/macos legs (set the env per-step/conditionally rather than job-wide).
This runs `core::task::tests::sidecar_start_and_ping` against the real sidecar on Linux.
Update the `phase1-m0.md` Step 10 note to reflect that the integration test now runs in CI
on Linux (resolving the "acceptable only if pytest covers it" caveat).

### C2 [M] — Detect generated-binding drift

**Root cause.** `proto::tests::export_ts_bindings` always rewrites `src/lib/ipc/types.ts`
and returns `Ok`, so CI never fails when the committed file is stale.

**Change.** After `cargo test --workspace` in the `rust-tests` job, add a step:
`git diff --exit-code -- src/lib/ipc/types.ts` (run from repo root). Fails the job if the
regenerated bindings differ from what's committed. (Linux leg is sufficient; the generator
output is platform-independent — gate the step on `runner.os == 'Linux'` to avoid CRLF
noise on Windows.)

### C3 [L] — Reconcile `cargo audit` vs `cargo deny`

**Root cause.** The ~17 advisories ignored in `deny.toml` do not apply to `cargo audit`
(no `.cargo/audit.toml`); CI passes today only because they are all `unmaintained`-class
warnings. Divergent sources of truth.

**Decided: drop the redundant `cargo audit` step** — `cargo deny check` already covers the
RUSTSEC advisory DB with the documented ignore list. Remove the "Install cargo-audit" and
"cargo audit" steps from `rust-tests`; `deny.toml`'s `[advisories] ignore` becomes the
single source of truth.

### C4 [L] — Deterministic frontend toolchain

**Changes.**
   - Add a `"packageManager": "pnpm@<pinned-version>"` field to `package.json` (use the
     version matching `pnpm-lock.yaml`'s lockfileVersion), and change
     `pnpm/action-setup@v4` in the `frontend-tests` job to **omit** `version` (it reads
     `packageManager`). Removes the `version: latest` non-determinism.
   - Node version: **move both `.nvmrc` and CI `node-version` from `26` to `24`** (active
     LTS), per the Step-1 "Node LTS" intent. Update `.nvmrc` (`24`) and the
     `actions/setup-node` `node-version: '24'` in the `frontend-tests` job together so they
     stay consistent.

### C5 [L] — Avoid duplicate CI runs

**Root cause.** `on: push: branches: ['**']` + `pull_request` both fire for in-repo branch
PRs (different `github.ref`, so `concurrency` doesn't dedupe).

**Change.** Scope `push` to `main` (and tags if desired) and rely on `pull_request` for
feature branches:
```yaml
on:
  push:
    branches: [main]
  pull_request:
```

---

## Group C — Frontend & assets

### F1 [L] — Make a11y issues fail the build (conventions D1)

**Investigate first.** Determine current severity of the `svelte/a11y-*` rules under
`eslint-plugin-svelte` `flat/recommended` (v3.x):
`npx eslint --print-config src/routes/+page.svelte | grep -i a11y`. Also confirm whether
`pnpm check` (svelte-check) surfaces a11y as error vs warning.

**Change (if not already error-level).** Add an ESLint override block in
`eslint.config.js` setting the `svelte/a11y-*` rules to `"error"` (or, if the team prefers,
add `--max-warnings 0` to the `lint` script — note this makes *all* warnings fatal). Pick
the narrower a11y-only elevation unless told otherwise. Optionally add a deliberately
inaccessible element to a scratch file to confirm CI now fails, then remove it.

### F2 [L] — Clean up the icon configuration

**Root cause.** `src-tauri/app/tauri.conf.json` sets `"icon": ["../icons/icon.ico"]`,
pointing at a lone placeholder `src-tauri/icons/icon.ico`, while the standard Tauri PNG set
(`32x32.png`, `128x128.png`, `128x128@2x.png`, `icon.png`) sits unreferenced in
`src-tauri/app/icons/`. Two icon directories; mac/linux bundles would lack proper icons.

**Change.** Consolidate to the standard `src-tauri/app/icons/` set and reference the full
list (PNGs + an `.ico` placed in the same dir) in `tauri.conf.json`'s `bundle.icon`; remove
the stray `src-tauri/icons/` directory. Real (non-placeholder) icons remain deferred to M7
(already tracked in `phase1-m0.md`) — this change only makes the config consistent and
bundle-capable. Confirm `tauri.conf.json` paths resolve relative to the `app/` crate dir.

---

## N1 [N] — No action

The Step-10 conversion of test `.expect("descriptive")` → `?` loses the human label in
failure output, but `anyhow` carries the underlying error. Acceptable; no change.

---

## Cross-cutting: regeneration & doc sync (W6)

Whenever `proto` types change (W1, W3):
   - Regenerate bindings: `cd src-tauri && cargo test -p proto -- export_ts_bindings`
     (writes `src/lib/ipc/types.ts`); commit the regenerated file.
   - Keep `design/command-surface.md` § Error codes in sync (W1).
   - Re-check `src/lib/ipc/commands.ts` and `src/routes/+page.svelte` compile against the
     new ambient types.

## Suggested commit grouping

1. Contract + sidecar (W1, W2, W3, W5, W4) + regenerated `types.ts` + `command-surface.md`.
2. CI hardening (C1, C2, C3, C4, C5).
3. Frontend/assets (F1, F2).
Stage the relevant `plans/phase*.md` updates with whichever commit touches them
(per CLAUDE.md pre-commit checklist).

## Verification (end-to-end)

- **Rust:** from `src-tauri/`: `cargo fmt --all -- --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace` (with `VOCALBOARD_PYTHON` set and
  `SKIP_SIDECAR_TESTS` unset, so `sidecar_start_and_ping` runs), `cargo deny check`.
- **Python:** from `python/`: `uv run pytest` — includes new W2 + W1 cases.
- **Frontend:** `pnpm check && pnpm lint && pnpm test && pnpm build`; confirm `pnpm lint`
  now fails on an a11y violation (F1).
- **Bindings drift:** after `cargo test`, `git diff --exit-code -- src/lib/ipc/types.ts`
  reports no diff (C2).
- **App smoke (manual):** `pnpm tauri dev` — window opens, log shows the new typed ready
  signal (W3), welcome page renders version + `sidecar_status: ready` + pong; on app quit,
  no orphaned `python -m vocalboard_sidecar` process remains (W4).
- **CI:** push a throwaway branch / open a PR; confirm a single set of runs (C5), the
  Linux rust leg runs the integration test, and the bindings-drift step is green.
