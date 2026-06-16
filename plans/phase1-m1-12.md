# Phase 1 · M1 — Step 12: Tauri wiring + contract — detailed action plan

Detailed breakdown of [Step 12 in phase1-m1.md](phase1-m1.md#step-12--tauri-wiring--contract).
Authoritative specs: [command-surface.md](../design/command-surface.md) (the three project commands + the
error-code table), [conventions.md](../design/conventions.md) (§ C error handling, § D2 i18n,
§ H1 contract versioning, § J2 param validation),
[architecture.md § Rust process](../design/architecture.md#rust-process-tauri) (the per-window
`ProjectState`), and the engine API from [Step 11](phase1-m1-11.md) (`core/src/project/engine.rs`).

Step 11 produced the headless engine (`ProjectState`, no Tauri dependency). Step 12 is the
**wiring layer**: it exposes `new_project` / `open_project` / `save_snapshot_now` as
`#[tauri::command]` handlers backed by a managed, app-global `ProjectState` slot, with matching
`proto` types, regenerated TypeScript bindings, and typed frontend wrappers.

**This revision also back-fixes five M0-era contract conventions** that a review surfaced before
they spread across the ~25 commands M4/M5 will add. M0 predates several stabilised norms, so the
contract plumbing it shipped is corrected here — *before* Step 12 adds a second command family on
top of it. The fixes, and which sub-step owns each:

| # | Finding (M0-era) | Owner |
|---|---|---|
| 1 | Param validation deferred, but **J2** mandates "validate before mutation" | 12c |
| 2 | No version channel on the Tauri command boundary (**H1** unenforceable there) | 12c |
| 3 | Binding generator is a `#[test]` that **writes a tracked file** (non-hermetic `cargo test`) | 12a |
| 4 | Generated types are **ambient globals** (no `export`) — fragile, non-idiomatic | 12a |
| 5 | Handlers return bare `String` (violates **C** unifying rule); error envelope un-unified | 12b |

## Sub-step structure (one commit each, risk-ascending — green before the next)

Mirrors the [Step 11](phase1-m1-11.md) a/b/c/d model: mechanical back-fixes to existing M0 surface
first, then the new wiring on the corrected base.

- **12a** — Binding-generation hardening + ES-module exports (back-fix #3, #4). Touches only the
  existing M0 contract plumbing; no new commands. **Commit `1M1-12a`.**
- **12b** — Unified typed error contract (back-fix #5). `CommandError`, `ErrorMsg` unification, all
  new `ErrorCode` variants, M0-handler migration. Builds on 12a. **Commit `1M1-12b`.**
- **12c** — `ProjectState` command wiring + versioning & validation conventions (the original
  Step 12, resolving #1 and #2). The three commands on the corrected foundation. **Commit
  `1M1-12c`.**

Each sub-step runs the full gate (`cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
`cargo test --workspace`, and — where the frontend changes — `pnpm check && pnpm test && pnpm
build`) and is independently green. All commits are **unsigned** on `claude/1M1` (GPG-by-branch
policy, [CLAUDE.md](../CLAUDE.md)).

## Existing patterns & APIs this step builds on

- **M0 handler shape** — `app/src/main.rs` `get_app_info` / `ping_sidecar`: `#[tauri::command]`,
  managed state via `tauri::State<'_, T>`, registered in `tauri::generate_handler![…]`.
  **App-defined commands need _no_ capability/ACL entry** (only *plugin* commands do — confirmed:
  M0's commands are absent from `app/capabilities/default.json` and work). **Do not touch
  `capabilities/default.json`.**
- **M0 proto shape** — `proto/src/commands.rs` types derive
  `#[derive(Debug, Clone, Serialize, Deserialize)]`; 12a changes the TS-derive gating (below).
  `NewProjectParams` (path + sample_rate) already exists — reuse unchanged.
- **Engine API (Step 11 — confirm signatures in `engine.rs`)**, all under
  `vb_core::project::engine`:
  - `ProjectState::new_project(path: &Path, sample_rate: u32, settings: &Settings) -> Result<Self, EngineError>`
  - `ProjectState::open_project(path: &Path, settings: &Settings) -> Result<(Self, OpenOutcome), EngineError>`
  - `ProjectState::save_snapshot_now(&mut self) -> Result<(), EngineError>`; `sample_rate() -> u32`
  - `OpenOutcome { missing_tracks: Vec<u32>, recovery: Option<RecoveryInfo> }`;
    `RecoveryInfo { failed_row: i64, snapshot_id: i64 }`
  - `EngineError` variants: `Sqlite, Store, Journal, History, Replay, Encode, Decode, OpenDb,
    RecoveryFailed, Tree, TrackTypeMismatch{track_id}, ProjectFileExists{path},
    ProjectFileNotFound{path}`. (`undo`/`redo`/`apply_batch` exist but are **not wired** in M1 — no
    undo/redo or edit command surface yet.)
  - `vb_core::settings::Settings` — `Clone`; already managed in `app` (`app.manage(settings)`).

---

## Step 12a — Binding-generation hardening + ES-module exports

Back-fixes #3 and #4. Pure refactor of the M0 contract plumbing — no behavioural change to any
type's wire format, no new commands. The smallest, lowest-risk piece, so it lands first and
everything after builds on the corrected generator.

### 12a · Problem

1. **#3 — codegen-as-test.** `proto/src/lib.rs`'s `#[test] fn export_ts_bindings` *writes*
   `src/lib/ipc/types.ts` as a side effect. So `cargo test` is non-hermetic (it mutates a tracked
   file), and CI relies on that write + `git diff --exit-code` (ci.yml). The type registry is a
   hand-ordered `sections: &[&str]` list — forgetting a new type is a silent omission that surfaces
   only later as a `pnpm check` failure.
2. **#4 — ambient globals.** The generator emits `type X = …` with **no `export`**, relying on the
   file being a non-module script so `AppInfoResult` etc. are global. Adding any `import`/`export`
   to that file would silently convert it to a module and break **every** reference at once; it
   also pollutes the global type namespace (collision risk as the surface grows).

### 12a · Fix — a real generator binary + exported module types

**Make the generator a binary, gated behind an optional `ts-export` feature, and have `cargo test`
do a read-only check instead of a write.**

1. `proto/Cargo.toml`:
   - Move `ts-rs` from `[dev-dependencies]` to an **optional** normal dependency, and add a feature:
     ```toml
     [dependencies]
     serde = { version = "1", features = ["derive"] }
     serde_json = "1"
     ts-rs = { version = "10", features = ["serde-compat"], optional = true }

     [features]
     ts-export = ["dep:ts-rs"]
     ```
   - This keeps production lean: the default `app` build of `proto` pulls **no** `ts-rs`; only the
     binding-generation command enables `ts-export`.
2. On every proto type, change the TS-derive gating from the test profile to the feature:
   `#[cfg_attr(test, derive(ts_rs::TS))]` → `#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]`
   (and likewise the `#[cfg_attr(test, ts(type = "unknown"))]` on `RequestEnvelope::payload` →
   `#[cfg_attr(feature = "ts-export", ts(type = "unknown"))]`).
3. New module `proto/src/bindings.rs`, **`#![cfg(feature = "ts-export")]`**, exposing
   `pub fn render() -> String` that assembles the single-file output — moving the `sections`
   assembly out of the test. **Emit `export` declarations:** prepend `"export "` to each
   `Type::decl()` so the file becomes a proper ES module
   (`format!("export {}", commands::PingResult::decl())` → `export type PingResult = {…}`). Keep the
   two header comment lines. (ts-rs's `decl()` returns the bare `type … = …`; prefixing `export `
   yields idiomatic module exports.)
4. New binary `proto/src/bin/gen_bindings.rs` with `required-features = ["ts-export"]` (declare it
   under `[[bin]]` in `Cargo.toml` so it only builds with the feature). Behaviour:
   - resolve the target path `CARGO_MANIFEST_DIR/../../src/lib/ipc/types.ts`;
   - default: write `proto::bindings::render()` to that path;
   - `--check`: compare the file's current contents to `render()`; on mismatch print a hint
     (`run \`cargo run -p proto --features ts-export --bin gen_bindings\` to regenerate`) and exit
     non-zero. No `git` dependency.
   - No `unwrap`/`expect` in non-test code without justification — propagate via `anyhow` (add
     `anyhow` to proto deps if needed, gated under the feature) or return `Result` from `main`.
5. **Delete** the `export_ts_bindings` test. `cargo test --workspace` is now hermetic (the
   remaining proto tests are serde round-trips that don't touch `ts-rs`).
6. `proto/src/lib.rs`: add `#[cfg(feature = "ts-export")] pub mod bindings;`. Remove the now-unused
   `use ts_rs::TS` from the test module.
7. **CI** (`.github/workflows/ci.yml`): replace the "Check generated TypeScript bindings" step
   (`git diff --exit-code -- src/lib/ipc/types.ts`) with:
   ```yaml
   - name: Check generated TypeScript bindings are up to date (Linux only)
     if: runner.os == 'Linux'
     run: cargo run -p proto --features ts-export --bin gen_bindings -- --check
   ```

**Convert consumers to `import type` (#4 completion).** With `export`, the types are module
exports, not globals — every reference needs an import:
- `src/lib/ipc/commands.ts` — replace the "globally available, no import needed" header note with
  `import type { AppInfoResult, PingResult } from './types';`.
- `src/lib/ipc/commands.test.ts` — `import type { AppInfoResult, PingResult } from './types';`.
- `src/routes/+page.svelte` — `import type { AppInfoResult } from '$lib/ipc/types';`.
- Grep for any other unimported references (`AppInfoResult|PingResult|SidecarStatus|…`); `pnpm
  check` will flag any missed one.

> **Single-file + exports is fine.** Same-file type references (`type FromSidecar = … & ErrorMsg`)
> resolve within one module without imports; only *cross-file* consumers import. So the one-file
> layout is kept — the change is `export` + consumer imports, not file fan-out.

### 12a · Decision — why a feature-gated bin, not an `#[ignore]`d writer test

A lighter fix (split the test into a read-only "is committed?" assertion plus an `#[ignore]`d
writer) was considered and rejected: it keeps generation inside the test harness (still surprising;
`-- --ignored` is obscure) and keeps the `cfg(test)` derive gating that couples bindings to the
test profile. The bin makes "regenerate" an obvious, documented command, makes `cargo test`
genuinely hermetic, and cleanly separates the production build (no `ts-rs`) from the tooling build
(`--features ts-export`). The cost — one feature + one bin — is one-time and pays off across every
future type.

### 12a · Test plan & verification

- The existing proto serde round-trip tests (`round_trip_*`, `error_code_snake_case_serialisation`)
  still pass (untouched).
- `cargo run -p proto --features ts-export --bin gen_bindings` regenerates `types.ts`; the file now
  starts each declaration with `export type …`. Commit the regenerated file.
- `cargo run -p proto --features ts-export --bin gen_bindings -- --check` exits 0 on the committed
  file, non-zero if a proto type changes without regeneration (verify by tweaking + reverting).
- `cargo test --workspace` no longer modifies the working tree (hermetic) and is green.
- `pnpm check && pnpm test && pnpm build` green with the new `import type` lines.
- `cargo clippy --workspace -- -D warnings` (incl. the feature build:
  `cargo clippy -p proto --features ts-export -- -D warnings`).
- **Commit `1M1-12a: binding generator (feature-gated bin) + ES-module exports`** — stage
  `proto/Cargo.toml`, `proto/src/{lib.rs,bindings.rs,bin/gen_bindings.rs}`, the per-type derive
  changes, `.github/workflows/ci.yml`, `src/lib/ipc/{commands.ts,commands.test.ts}`,
  `src/routes/+page.svelte`, and the regenerated `src/lib/ipc/types.ts`.

---

## Step 12b — Unified typed error contract

Back-fixes #5 and lands **all** new `ErrorCode` variants in one place (the commit that owns the
error contract), so 12c only *uses* them.

### 12b · Problem

M0 handlers return `Result<T, String>`, which violates the **C unifying rule** ("user-relevant
errors MUST surface as a typed code/key") and **C3/D2** (the frontend renders an error by its
`snake_case` code through Paraglide `m[code]`, never a free-form string). And the `{code, message}`
shape is about to be duplicated: the sidecar already has `ErrorMsg { request_id, code, message }`,
and Step 12 needs the same core for Tauri commands.

### 12b · Fix

**1. Canonical `CommandError` (in `proto/src/error.rs`).**

```rust
/// Error returned by a Tauri command to the frontend: a machine-readable `code`
/// the UI branches on (and renders via Paraglide `m[code]`), plus a human-readable
/// `message` for logs/diagnostics. **`message` is never shown verbatim in the UI.**
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct CommandError {
    /// Machine-readable error code (the Paraglide message key).
    pub code: ErrorCode,
    /// Human-readable description for logs/diagnostics; not UI-facing.
    pub message: String,
}
```

**2. Unify the sidecar envelope onto it.** Refactor `sidecar::ErrorMsg` to embed `CommandError` so
the `{code, message}` core is defined once:

```rust
pub struct ErrorMsg {
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub error: CommandError,
}
```

`#[serde(flatten)]` keeps the wire format byte-identical (`{request_id, code, message}`), so the
existing `round_trip_from_sidecar_messages` test must still pass. ts-rs honours serde flatten
(the `serde-compat` feature is on) and renders `ErrorMsg = { request_id: … } & CommandError`.
**Fallback (if flatten causes ts-rs or wire friction):** leave `ErrorMsg`'s `code`/`message` fields
inline and do *not* embed — the binding-level shared contract is `ErrorCode` (already shared) and
`CommandError` stands alone as the Tauri envelope. Verify the sidecar round-trip test byte-for-byte
either way.

**3. New `ErrorCode` variants** — append **before** `#[serde(other)] Unknown` (append-only policy;
`Unknown` stays last), each with a doc-comment:

| Variant | snake_case wire | Used by (12c) |
|---|---|---|
| `ProjectFileExists` | `project_file_exists` | `new_project` on an existing path |
| `ProjectFileNotFound` | `project_file_not_found` | `open_project` on a missing path |
| `ProjectOpenFailed` | `project_open_failed` | fatal open (`RecoveryFailed`, `OpenDb`, other) |
| `NoProjectOpen` | `no_project_open` | `save_snapshot_now` with no project loaded |
| `InvalidParams` | `invalid_params` | param value-constraint failure (12c validation) |

**4. Migrate the M0 handlers** (`app/src/main.rs`) from `Result<_, String>` to
`Result<_, proto::CommandError>`:
- `ping_sidecar`: the "sidecar not available" path → `CommandError { code: SidecarNotReady, … }`;
  `mgr.ping()` error → map to a `CommandError` (`SidecarNotReady` or `InternalError` as fits the
  underlying error — confirm what `ping()` returns).
- `get_app_info`: it never errors today; change its signature to `Result<_, CommandError>` for
  uniformity (still returns `Ok`).

**5. `command-surface.md`** — add the five new rows to § Error codes (contract stays authoritative).

**6. `proto/src/lib.rs`** — `pub use error::{CommandError, ErrorCode};`; add `CommandError` to the
`bindings::render()` section list (after `ErrorCode`, before `ErrorMsg`, since `ErrorMsg` now
references it). Regenerate `types.ts`.

### 12b · Test plan & verification

- `round_trip_from_sidecar_messages` passes unchanged (wire identical after flatten).
- New serde tests: `CommandError` round-trips and serialises `code` as snake_case; the five new
  `ErrorCode` strings (extend `error_code_snake_case_serialisation`).
- `app` builds; the migrated M0 handlers compile and `commands.test.ts` (M0 cases) still pass
  (the JS sees a `CommandError` object on rejection — update any error assertion if present; the
  happy-path mocks are unaffected).
- Regenerate + commit `types.ts`; `gen_bindings -- --check` green.
- Full gate (fmt, clippy incl. `missing_docs` on `CommandError`, `cargo test --workspace`,
  `pnpm check && pnpm test && pnpm build`).
- **Commit `1M1-12b: unified CommandError envelope + project-lifecycle error codes`** — stage
  `proto/src/{error.rs,sidecar.rs,lib.rs}`, `app/src/main.rs`, `design/command-surface.md`, the
  regenerated `types.ts`, and any frontend error-assertion tweak.

---

## Step 12c — `ProjectState` command wiring + versioning & validation conventions

The original Step 12, now built on 12a/12b and resolving findings #1 and #2. Adds the managed
project slot, the three handlers, the new proto param/result types, the TS wrappers, and the
**versioning** and **validation** convention reconciliations that the new handlers embody.

### 12c · Decision — versioning the Tauri command boundary (resolves #2)

**Tauri commands are versioned by command name, not an in-band version field.** A breaking
param/result change ships a **new command** (e.g. a future `new_project` v2 is a distinct
`#[tauri::command]` registered alongside the retained v1), and `command-surface.md` records the
version per name. M1's three commands are v1 and carry **no** `version` field.

**Why this satisfies H1 (no-ADR model).** H1 requires "introduce a new `version` and keep the prior
version handled." Tauri dispatches **statically** by command name within a **single binary** (the
webview assets are bundled with the Rust shell — frontend and backend always ship together), so
there is no in-build version skew to negotiate; "keep the prior version handled" means *keep the old
`#[tauri::command]` registered*. The only cross-version consumer is a **Phase-6 plugin** written
against an older surface — and a plugin pins a *command name*, which a breaking change never mutates
(it adds a new name), so the plugin keeps working. This is the correct asymmetry with the **sidecar**
envelope's in-band `version` (envelope.rs): the sidecar is a *separately built* Nuitka process that
can mismatch the shell, so it needs runtime version negotiation; the Tauri boundary does not.
- **`command-surface.md` / `conventions.md` H1 update (same commit):** add a clause stating the
  Tauri-boundary versioning mechanism is *version-by-command-name* (new command on breaking change,
  old retained), distinct from the sidecar's in-band envelope `version`, and that both are governed
  by H1's no-in-place-change rule.

### 12c · Decision — param validation (resolves #1, reconciles J2)

J2 requires params validated "before any state mutation." The mechanism for the Tauri boundary:

1. **Structural validation = typed serde deserialization + `#[serde(deny_unknown_fields)]`** on
   every command param struct. Tauri deserializes the payload into the typed struct *before* the
   handler body runs, rejecting malformed JSON, type mismatches, missing required fields, and (with
   `deny_unknown_fields`) unknown fields — i.e. before any `ProjectState` mutation. This *is*
   "validate before mutation."
2. **Value constraints** that the Draft-07 schema documents but types can't express (e.g.
   `sample_rate` minimum 8000) → an explicit guard at the top of the handler returning
   `CommandError { code: InvalidParams, … }`, covered by a test.
3. **`conventions.md` J2 + `command-surface.md` update (same commit):** record that for the Tauri
   boundary, serde-typed deserialization + `deny_unknown_fields` is the enforced structural
   validator, with documented value constraints enforced by explicit handler guards.

**Deferred (recorded, not silently dropped):** generating the Draft-07 schemas *from* the Rust
param types (e.g. `schemars`) so the prose schemas in command-surface.md can't drift from the code,
and exposing those schemas for Phase-6 plugin introspection. This is a cross-command build-out, not
M1 scope — record it as an M4/M5+ item in `command-surface.md` and `phase1.md`. M1 keeps the prose
schemas authoritative and the typed structs as the runtime contract.

### 12c · Decision — threading (managed slot, async + spawn_blocking)

The handlers touch the filesystem + SQLite (create/migrate, replay on open). Make each `async` and
run the engine call inside `spawn_blocking`, keeping the event loop responsive and establishing the
pattern the [Step 11d note](phase1-m1-11.md#11d--apply_batch-journals-metadata-in-the-same-transaction)
flags for M4/M5 heavy edits:
- Managed state: a newtype `struct ProjectSlot(Arc<Mutex<Option<ProjectState>>>)` (std `Mutex`).
  `ProjectState` is `Send` (its `rusqlite::Connection`s are `Send`), so
  `Arc<Mutex<Option<ProjectState>>>` is `Send + Sync + 'static` — a valid Tauri managed type.
- Each handler clones the inner `Arc` and clones `Settings` out of their `State` extractors, then
  moves both into the `spawn_blocking` closure. The `std::sync::MutexGuard` is acquired and dropped
  **entirely inside** the closure — never held across an `.await` (the only `.await` is on the join
  handle, outside the closure) — so std `Mutex` is correct (no async lock needed) and there is no
  `Send`-across-await problem. Use `tauri::async_runtime::spawn_blocking` (tokio-backed; `app`
  already depends on `tokio` with `rt`).
- `new_project`/`open_project` **replace** any project in the slot (single window in Phase 1; the
  prior `ProjectState` drops). The newtype keeps Phase 6's second-window-second-`ProjectState`
  shape open ([architecture.md § Rust process](../design/architecture.md#rust-process-tauri)).
- **No `unwrap`/`expect`/`panic`** in non-test code: lock-poison → `InternalError`; join error →
  `InternalError`.

### 12c · Decision — command arguments arrive as one `params` struct

Each handler takes its proto param struct by value as a parameter literally named `params`
(`async fn new_project(params: NewProjectParams, …)`); the JS wrapper invokes with
`invoke('new_project', { params: { path, sample_rate } })`. Tauri maps the top-level `params` key
to the argument and serdes it into the struct. This keeps the param types meaningful as the wire
schema and sidesteps Tauri's per-arg camelCase↔snake_case ambiguity (struct fields serialise by
their serde names, matching the generated TS fields `path` / `sample_rate`).

### 12c · `proto` additions (`commands.rs`)

All derive `#[derive(Debug, Clone, Serialize, Deserialize)]` +
`#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]` (per 12a's gating) and carry doc-comments:

```rust
/// Parameters for the `open_project` command. Version 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct OpenProjectParams { /// Absolute path to the `.vocalboard` file.
    pub path: String, }

/// Parameters for `save_snapshot_now` (no fields). Version 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[serde(deny_unknown_fields)]
pub struct SaveSnapshotNowParams {}

/// Result of `new_project`: echoes the locked sample rate as confirmation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct NewProjectResult { /// Locked sample rate (Hz). pub sample_rate: u32, }

/// Non-fatal facts about `open_project`. `recovery` is `Some` iff a corrupt-journal
/// rollback occurred (post-snapshot edits lost) — the frontend must warn (M6 dialog).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct OpenProjectResult {
    /// Track ids whose source file could not be located (Missing-Files dialog is M6).
    pub missing_tracks: Vec<u32>,
    /// `Some` iff a corrupt-journal recovery rolled the project back to a snapshot.
    pub recovery: Option<RecoveryReport>,
}

/// Wire mirror of the engine's `RecoveryInfo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
pub struct RecoveryReport { pub failed_row: i64, pub snapshot_id: i64, }
```

Add `#[serde(deny_unknown_fields)]` to `NewProjectParams` too (consistency). `save_snapshot_now`
returns `()` (Tauri → `null`); no result struct. Re-export the new types from `lib.rs` and add each
to `bindings::render()` (referenced-before-referrer: `RecoveryReport` before `OpenProjectResult`).

### 12c · `app/src/main.rs`

```rust
use std::path::Path;
use std::sync::{Arc, Mutex};
use proto::{CommandError, ErrorCode, NewProjectParams, NewProjectResult,
            OpenProjectParams, OpenProjectResult, RecoveryReport, SaveSnapshotNowParams};
use vb_core::project::engine::{EngineError, ProjectState};
use vb_core::settings::Settings;

/// App-global slot holding the single open project (Phase 1: one window ⇒ one project).
struct ProjectSlot(Arc<Mutex<Option<ProjectState>>>);

fn to_command_error(e: EngineError) -> CommandError {
    let code = match &e {
        EngineError::ProjectFileExists { .. }   => ErrorCode::ProjectFileExists,
        EngineError::ProjectFileNotFound { .. } => ErrorCode::ProjectFileNotFound,
        EngineError::RecoveryFailed(_) | EngineError::OpenDb(_) => ErrorCode::ProjectOpenFailed,
        _ => ErrorCode::InternalError, // Sqlite, Store, Journal, History, Replay, Encode,
    };                                  // Decode, Tree, TrackTypeMismatch
    CommandError { code, message: e.to_string() }
}
fn err(code: ErrorCode, message: impl Into<String>) -> CommandError {
    CommandError { code, message: message.into() }
}
```

- In `setup` (after `app.manage(settings)`): `app.manage(ProjectSlot(Arc::new(Mutex::new(None))));`
- `new_project` (shape; `params`-struct arg, value-check, spawn_blocking):
  ```rust
  #[tauri::command]
  async fn new_project(
      params: NewProjectParams,
      slot: tauri::State<'_, ProjectSlot>,
      settings: tauri::State<'_, Settings>,
  ) -> Result<NewProjectResult, CommandError> {
      if params.sample_rate < 8000 {
          return Err(err(ErrorCode::InvalidParams, "sample_rate must be >= 8000"));
      }
      let slot = slot.0.clone();
      let settings = settings.inner().clone();
      tauri::async_runtime::spawn_blocking(move || {
          let ps = ProjectState::new_project(Path::new(&params.path), params.sample_rate, &settings)
              .map_err(to_command_error)?;
          let sample_rate = ps.sample_rate();
          *slot.lock().map_err(|_| err(ErrorCode::InternalError, "project slot poisoned"))?
              = Some(ps);
          Ok(NewProjectResult { sample_rate })
      })
      .await
      .map_err(|e| err(ErrorCode::InternalError, format!("worker join error: {e}")))?
  }
  ```
- `open_project(params: OpenProjectParams, …) -> Result<OpenProjectResult, CommandError>`: in the
  closure, `let (ps, outcome) = ProjectState::open_project(Path::new(&params.path), &settings)
  .map_err(to_command_error)?;` then build the result before storing `ps`:
  ```rust
  let result = OpenProjectResult {
      missing_tracks: outcome.missing_tracks,
      recovery: outcome.recovery.map(|r| RecoveryReport {
          failed_row: r.failed_row, snapshot_id: r.snapshot_id }),
  };
  *slot.lock()... = Some(ps);
  Ok(result)
  ```
- `save_snapshot_now(_params: SaveSnapshotNowParams, slot: …) -> Result<(), CommandError>`: in the
  closure, `let mut guard = slot.lock()...; let ps = guard.as_mut().ok_or_else(|| err(
  ErrorCode::NoProjectOpen, "no project open"))?; ps.save_snapshot_now().map_err(to_command_error)?;
  Ok(())`.
- Register: `tauri::generate_handler![get_app_info, ping_sidecar, new_project, open_project,
  save_snapshot_now]`.

### 12c · `src/lib/ipc/commands.ts`

```ts
import type { NewProjectResult, OpenProjectResult } from './types';

/** Creates a new empty project file at `path`, locked to `sampleRate`. */
export async function newProject(path: string, sampleRate: number): Promise<NewProjectResult> {
	return invoke<NewProjectResult>('new_project', { params: { path, sample_rate: sampleRate } });
}
/** Opens an existing project; resolves with missing-track and recovery info. */
export async function openProject(path: string): Promise<OpenProjectResult> {
	return invoke<OpenProjectResult>('open_project', { params: { path } });
}
/** Triggers an immediate snapshot of the open project. */
export async function saveSnapshotNow(): Promise<void> {
	return invoke<void>('save_snapshot_now', { params: {} });
}
```

### 12c · Tests

- **Rust serde** (`proto`): round-trip `OpenProjectResult` with `recovery: Some(..)` and `None`;
  `deny_unknown_fields` rejects an extra field on `OpenProjectParams`/`NewProjectParams`.
- **Frontend** (`commands.test.ts`, `mockIPC` — mirror the M0 cases): `newProject` returns
  `{ sample_rate }` and the mock receives `{ params: { path, sample_rate } }`; `openProject`
  with `recovery: null` and with `recovery: { failed_row, snapshot_id }`; `saveSnapshotNow`
  resolves on `null`. Add `import type { NewProjectResult, OpenProjectResult } from './types';`.
- Handler-level Rust tests are **not** added (consistent with M0 — engine logic is covered by
  `core/tests/engine_*`; the contract is covered by proto serde + frontend mock tests). A
  `sample_rate < 8000` rejection can be asserted at the JS layer via a mock that records the call
  *not* happening — or simply documented; the guard is trivial. (Optional: a small Rust unit test
  on the value-check if extracted into a pure fn.)

### 12c · Verification & commit

- Full gate including `cargo clippy -p proto --features ts-export -- -D warnings`,
  `cargo clippy --workspace -- -D warnings` (incl. `missing_docs`, `unwrap_used`), `cargo test
  --workspace`, `gen_bindings -- --check` green, `pnpm check && pnpm test && pnpm build`.
- **Commit `1M1-12c: ProjectState command wiring + version-by-name & param-validation conventions`**
  — stage `proto/src/{commands.rs,lib.rs}`, `app/src/main.rs`, `src/lib/ipc/{commands.ts,
  commands.test.ts}`, the regenerated `types.ts`, `design/{command-surface.md,conventions.md}`
  (H1 + J2 reconciliations, deferred-schemars note), and `plans/phase1.md` (the schemars deferral
  under M4/M5).

---

## Cross-cutting verification (after 12c)

- `cargo fmt --check`; `cargo clippy --workspace -- -D warnings` **and**
  `cargo clippy -p proto --features ts-export -- -D warnings`.
- `cargo test --workspace` green **and hermetic** (no working-tree changes — the 12a fix).
- `cargo run -p proto --features ts-export --bin gen_bindings -- --check` exits 0.
- `pnpm check && pnpm test && pnpm build` green.
- The three commands round-trip through Tauri with in-sync, exported TS bindings (M1 exit criterion).

## Documentation touches (summary)

- **command-surface.md** — 12b: five new error-code rows. 12c: the version-by-command-name clause,
  the validation-mechanism note, the deferred-schemas note.
- **conventions.md** — 12c: H1 gains the Tauri-boundary versioning clause; J2 gains the
  serde+`deny_unknown_fields` enforcement note.
- **phase1.md** — 12c: record "generate command schemas from types (schemars)" as an M4/M5+ item.
- **phase1-m1.md** — update the Step 12 bullet to point at this doc and the 12a/12b/12c split
  (done alongside this revision).
- No `data-model.md` change (no persisted format added). If implementation forces a contract
  adjustment, update the relevant design doc in the same commit (docs stay authoritative).

## Out of scope (deferred)

- **Missing-Files dialog + recovery-warning UI → M6** (12c plumbs `missing_tracks`/`recovery`
  through `OpenProjectResult`; the engine's open-recovery *data* obligation is met, the *UI*
  obligation is M6).
- **Off-event-loop dispatch of heavy edit commands → M4/M5** (12c adopts the `spawn_blocking`
  pattern so those inherit it; no edit command exists in M1).
- **schemars-generated command schemas (single source of truth for validation + plugin
  introspection) → M4/M5+** (recorded in command-surface.md / phase1.md).
- **Migration-consent / read-only open → M6** (`open_project` migrates unconditionally;
  future-version refusal surfaces as `project_open_failed`).
- **Undo/redo command surface → later** (`ProjectState::undo`/`redo` exist but no command wires
  them in M1).
</content>
