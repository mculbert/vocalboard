# Development Conventions

> Phase 1 — development norms for Vocalboard.
> Last updated: 2026-05-26.

These are the norms all code in this repository follows. They complement — and must never
contradict — the **architectural invariants** in [CLAUDE.md](../CLAUDE.md) and the design specs in
this folder ([index.md](index.md)).

## How to read this document

Each rule uses RFC-2119-style keywords and a tag for how it is enforced:

- **MUST / MUST NOT** — a hard rule. CI-gated wherever a machine check exists.
- **SHOULD** — a strong default; a reviewer may ask you to justify deviating.
- **MAY** — explicitly allowed.
- **[CI]** — machine-checked (the bot blocks the merge).
- **[review]** — human judgment (a reviewer checks it).

A rule tagged **[CI + review]** is partly automated and partly checked by a reviewer.

The project is **tri-language** (Rust core, Python ML sidecar, Svelte/TypeScript frontend); rules
that read differently per language say so explicitly.

---

## A. Testing

- **A1 [review].** Rust core logic (timeline ops, persistence, sample/audio math) MUST have inline
  `#[cfg(test)]` unit tests covering boundary conditions — empty input, single sample, overlapping
  ranges, maximum bounds. Cross-cutting flows get per-crate integration tests under `tests/` (see
  [ops.md § Test layout](ops.md#test-layout)). The Python sidecar tests its NDJSON contract and
  pre/post-processing, **not** model-inference quality (non-deterministic and expensive).
- **A2 [review].** Every bug fix MUST include a regression test — at the appropriate layer (unit /
  integration / e2e) — that fails before the fix and passes after. If a bug genuinely cannot be
  captured by an automated test (a race, a platform-specific defect, a purely visual glitch), the
  PR MUST say why.
- **A3 [review].** Test *effectiveness* (not just path coverage) is spot-checked with mutation
  testing (`cargo-mutants`) at milestone boundaries — **not** per-PR, since runs are slow and
  timeout-flaky. Invocation from `src-tauri/`:
  ```bash
  cargo mutants --workspace          # full sweep
  cargo mutants -f path/to/file.rs   # focused pass on changed files
  ```
  Surviving mutants MUST be triaged to zero or annotated with why they are acceptable (e.g.
  genuinely equivalent mutants). This is a periodic [review] gate, not a CI blocker.

## B. Code quality

- **B1 [review].** Functions SHOULD be single-purpose. A reviewer MAY request decomposition when a
  function is hard to follow; the `clippy::cognitive_complexity` / `too_many_lines` lints **[CI]**
  are the objective backstop. Do **not** split solely to hit a line count — this project forbids
  speculative abstractions (see [CLAUDE.md](../CLAUDE.md)).
- **B2 [review].** Names MUST be descriptive. Avoid unclear or non-standard abbreviations.
  Domain-standard terms (`rms`, `fft`, `hz`, `samples`, `aria`, `i18n`, `IPC`, `id`) and short
  idiomatic binders in small scopes (a loop `i`, a closure parameter) are explicitly allowed.
- **B3 [review].** Literals carrying semantic meaning (sample rates, buffer sizes, thresholds, the
  BLAKE3-128 hash width, the blob format-tag byte) MUST be named constants. Trivial literals (`0`,
  `1`, identity values) are exempt.

## C. Error handling (per language)

- **C1 [CI].** Rust: propagate errors with `?` and add context via `anyhow`. No `unwrap` / `expect`
  / `panic` in non-test code without a justifying comment. Gated by `clippy::unwrap_used`,
  `expect_used`, and `panic` set to `warn` in `[workspace.lints]` (CI runs `clippy -- -D warnings`).
- **C2 [review].** Python sidecar: catch failures and convert them to NDJSON `error` messages with
  a stable `code` (from the `ErrorCode` enum), logged via `structlog`. Never let an exception
  escape the dispatch loop untagged.
- **C3 [review].** Frontend: surface backend `error_key`s through Paraglide (`m[error_key]`); never
  swallow them.
- **Unifying rule.** User-relevant errors MUST surface as a typed code/key. Only a truly ignorable
  error MAY be logged-and-dropped, and that decision MUST be explicit in the code.

## D. Frontend

- **D1 [CI + review].** UI MUST meet WCAG 2.1 AA: keyboard-operable and screen-reader-labeled per
  the patterns in [frontend.md § Accessibility](frontend.md) (Bits UI primitives, `role="option"`,
  `aria-label` carrying cut/muted status, `aria-live="polite"` announcements, focus management on
  dialog open/close). Svelte compiler a11y warnings are treated as errors in CI **[CI]**; axe and
  manual keyboard/screen-reader spot-checks happen before release **[review]**.
- **D2 [CI + review].** All user-visible strings MUST go through Paraglide `m.*`; no hardcoded
  literals in markup. This includes backend error message keys (`snake_case`, ICU MessageFormat for
  plurals). A lint flags literal markup text **[CI]**; reviewers catch the rest **[review]**.

## E. Comments & documentation

- **E1 [review].** Inline comments explain **why**, not what (matches [CLAUDE.md](../CLAUDE.md)).
- **E2 [CI + review].** Every public interface MUST carry a doc-comment with a summary and an
  explicit input/output **contract**. (The contract describes *what it does and how to use it* —
  distinct from E1's inline *why*.) Rust `pub` items at crate boundaries are gated by
  `#![warn(missing_docs)]` **[CI]**, and intra-doc links in those contracts are gated against
  silent breakage by `cargo doc` under `RUSTDOCFLAGS=-D rustdoc::broken_intra_doc_links` **[CI]**
  (a `[`Type`]` link that fails to resolve renders as plain text — a doc bug). Only the
  broken-link lint is denied; `private_intra_doc_links` and `redundant_explicit_links` stay at
  warn, since for an internal workspace `pub → pub(crate)` doc links are intentional. Note: these
  lints are in force from M0, even though *rendering* the API site (rustdoc-JSON / pydoc-markdown /
  typedoc → Hugo) is deferred to M7 per
  [ops.md § Internal API documentation](ops.md#internal-api-documentation).
- **E3 [review].** User-facing documentation: every new user-visible feature MUST be documented in
  the Hugo `docs/content/reference/` manual. The development-branch copy describes *planned*
  behavior; its accuracy is audited before merge to `main`. "Feature" here means a user-invocable
  capability or a visible behavior change. Relationship to `design/`: **`design/` is the
  engineering spec (how it is built); `reference/` is the end-user manual (how it is used)** — they
  are not duplicates.

## F. Architectural invariants (pointer, not restatement)

The load-bearing rules for this project live in [CLAUDE.md](../CLAUDE.md) and the design docs: Rust
owns all state, persistence, and audio while Python is inference-only; the command surface is the
only path that mutates state; all time is integer samples at the project rate; hashed structs use
ordered collections only (`Vec` / `BTreeMap`, never `HashMap`) for deterministic serialization.
Conventions MUST NOT contradict these, and violating one of them is more serious than any style nit
in this document.

## G. Data & persistence integrity

- **G1 [CI + review].** Any change to a persisted format — the SQLite schema, the blob format-tag
  byte, the snapshot/postcard encoding, or `settings.json` — MUST ship a migration (a
  `PRAGMA user_version` upgrade function, or a settings-version upgrade function) **and** a
  round-trip test that opens a fixture written by the previous format and verifies it loads.
  Rationale: for a local-first editor, a format change without a migration is silent user **data
  loss**. Unknown `settings.json` keys MUST be preserved (forward-compat — already in the schema).
  Enforced by a fixtures-based round-trip suite **[CI]**; a reviewer confirms a migration
  accompanies any format-touching PR **[review]**. Committed binary fixtures are regenerated after a
  pre-1.0 in-place format revision with the `#[ignore]`d helper (from `src-tauri/`):
  ```bash
  cargo test -p core --lib -- --ignored gen_fixture --nocapture
  ```
- **G2 [review].** `min_app_version` MUST be raised whenever a project file becomes unreadable by
  older app versions, so an old app refuses an incompatible project cleanly instead of corrupting
  it.

## H. Command / IPC contract versioning

- **H1 [review].** A command's param/result JSON schema MUST NOT change in place. Either add fields
  backward-compatibly, or introduce a new `version` and keep the prior version handled. The IPC
  NDJSON envelope and the `ErrorCode` enum are part of this contract. Rationale: Phase 6 scripting
  and plugins call the same command surface, so silent schema drift breaks downstream callers. See
  [command-surface.md](command-surface.md).

  **Tauri command boundary:** versioning is by *command name* — a breaking change ships a new
  `#[tauri::command]` (the old name stays registered). No in-band `version` field on Tauri commands:
  the webview and Rust shell ship together so there is no runtime skew to negotiate. This differs
  from the **sidecar** NDJSON envelope, which carries an in-band `version` because the sidecar is a
  separately-built Nuitka process. Both boundaries are governed by H1's no-in-place-change rule.

## I. Dependency & supply-chain hygiene

- **I1 [CI].** Rust: `cargo audit` (CVE advisories) and `cargo deny` (license + ban policy) run in
  CI. Python: `pip-audit` runs in CI. Frontend: `pnpm audit` (`--audit-level high`) runs in CI.
  Rationale: the app redistributes large native dependencies
  (torch, ffmpeg, whisperx, pyannote); CVEs and incompatible licenses are a real risk for an OSS
  download.
- **I2 [review].** A new dependency is justified in the PR (size, license, maintenance status).
  This app already bundles a lot — default to the libraries chosen in
  [ops.md](ops.md#rust-crate-dependencies-key).

## J. Security & least-privilege

- **J1 [review].** Tauri `capabilities/` files grant the **minimum** permissions needed; the strict
  CSP (per [architecture.md](architecture.md)) is not loosened without justification.
- **J2 [review].** All webview input is untrusted. A command's params are validated against its
  Draft-07 JSON schema **before** any state mutation (already designed — codified here). This
  reinforces the invariant that the command surface is the only way the frontend mutates state, and
  that no raw scripts run from the webview.

  **Tauri command boundary enforcement:** (1) every param struct carries `#[serde(deny_unknown_fields)]`
  so Tauri's deserialization rejects malformed JSON, type mismatches, missing required fields, and
  unknown fields before the handler body runs; (2) value constraints the type system cannot express
  (e.g. `sample_rate >= 8000`) are explicit guards at the top of the handler returning
  `CommandError { code: InvalidParams }` before any state mutation.

## K. Privacy / local-first guarantee

- **K1 [review].** Nothing leaves the user's machine without an explicit user action. The
  error-report flow is opt-in by design — the user reviews a pre-filled GitHub issue in their
  browser before submitting (see [ops.md § One-click anonymous error report](ops.md#one-click-anonymous-error-report)).
  No silent telemetry or background network calls may be added.
- **K2 [review].** Logs and the diagnostics bundle MUST redact file paths and any PII. The bundle
  already redacts sensitive paths; treat redaction as a standing requirement for every new log
  field.

## L. Real-time audio-thread discipline

- **L1 [review].** No heap allocation, locking, or blocking I/O on the `cpal` audio callback or the
  real-time playback path; pre-allocate buffers and use a lock-free hand-off from the control
  thread. Rationale: this protects the "deterministic latency" decision (see [index.md](index.md#key-architectural-decisions)).
- **L2 [review].** Long-running and ML tasks MUST honor the IPC cancel channel and release
  resources promptly on cancel or drop (models idle-unload; queues stay bounded).

## M. Cross-platform correctness & performance budgets

- **M1 [CI + review].** No assumptions about path separators, filesystem case-sensitivity, or line
  endings; use platform-abstraction APIs. CI runs Rust on macOS/Windows/Linux already — the Python
  and frontend jobs are widened beyond Ubuntu-only to catch platform-specific bugs **[CI]**.
- **M2 [review].** Hot paths carry stated performance budgets — playback start latency and
  responsiveness on multi-hour transcripts — validated in the M7 performance pass; a regression
  against the budget blocks release.

---

## Enforcement summary

| Enforced in CI (the bot blocks the merge) | Checked by a reviewer |
|---|---|
| `cargo fmt --check` | Function purpose / decomposition (B1) |
| `cargo clippy -- -D warnings`, incl. `unwrap_used` / `expect_used` / `panic` / `cognitive_complexity` / `too_many_lines` (B1, C1) | Naming quality (B2), named constants (B3) |
| `#![warn(missing_docs)]` on crate boundaries + `cargo doc -D rustdoc::broken_intra_doc_links` (E2) | Comment intent — why-not-what (E1) |
| `cargo test --workspace`, incl. format round-trip fixtures (A1, G1) | Public-interface contract prose (E2) |
| `pytest python/tests/` (A1) | Python / frontend error tagging (C2, C3) |
| `pnpm check` with a11y warnings as errors (D1) | User-facing doc coverage & accuracy (E3) |
| no-hardcoded-string lint (D2) | Migration accompanies any format change (G1, G2) |
| `pnpm build` | Command-schema versioning (H1) |
| `cargo audit`, `cargo deny check`, `pip-audit` (I1) | New-dependency justification (I2) |
| multi-OS matrix for Python + frontend jobs (M1) | Capability least-privilege & CSP (J1, J2) |
| | Privacy / no new network or telemetry, log redaction (K1, K2) |
| | Real-time audio-thread discipline, cancellation/cleanup (L1, L2) |
| | Performance budgets (M2) |
