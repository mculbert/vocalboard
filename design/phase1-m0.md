# Phase 1 · M0 — Scaffolding & Contracts (action plan)

Step-by-step plan for the M0 milestone from [phase1.md](phase1.md).

**Definition of done:** a buildable, CI-green skeleton where the Tauri app launches,
spawns the Python sidecar, and a single trivial command round-trips
frontend → Rust → sidecar — proving the whole wiring end-to-end before any real
feature work begins.

## Decisions to lock first (recommended defaults)

- **Shared types:** derive TypeScript from the `proto` Rust types via **`specta`** (or
  `ts-rs`), regenerated into `src/lib/ipc/types.ts` — single source of truth.
  (Alternative: `json-schema-to-typescript` from the command-surface schemas.)
- **Dev sidecar:** spawn `python -m vocalboard_sidecar` in dev behind a dev flag;
  defer the Nuitka one-folder binary + `tauri.conf.json` `externalBin` wiring to M7.
  Keep ML deps **out** of M0 (sidecar uses only stdlib + `structlog`) so CI stays fast.
- **Tooling:** pnpm for the frontend; `uv` (or the existing `.venv`) for Python 3.11.

---

## Step 1 — Toolchain & repo prerequisites

- Add `rust-toolchain.toml` (stable channel; note `nightly` is needed only for M7
  doc-gen), `.nvmrc` (Node LTS), and pin Python 3.11.
- Extend `.gitignore`: `target/`, `node_modules/`, `.svelte-kit/`, `build/`,
  `src-tauri/binaries/`, `__pycache__/`, `*.vbdata/`.
- Document dev setup in `README.md` (toolchain versions, `pnpm install`, `uv sync`,
  `pnpm tauri dev`).
- **Verify:** `rustc --version`, `node -v`, `pnpm -v`, `python --version` all resolve.

## Step 2 — Rust workspace skeleton *(the spine)*

- `src-tauri/Cargo.toml` workspace manifest with members `app`, `core`, `proto`.
- Create the three crates with minimal `lib.rs` / `main.rs` so they compile empty.
- `core/src/` module dirs as empty stubs re-exported from `lib.rs`: `project/`,
  `audio/`, `db/`, `task/`, `ipc/`.
- Add a `[workspace.lints.clippy]` block setting `unwrap_used`, `expect_used`, `panic`,
  `cognitive_complexity`, and `too_many_lines` to `warn` (becomes a hard failure under the
  `clippy -- -D warnings` gate in Step 9); add `#![warn(missing_docs)]` to each crate's
  `lib.rs` / `main.rs`. Enforces [conventions.md](conventions.md) C1, B1, E2.
- Commit a `deny.toml` (license allowlist + advisory policy) for `cargo deny` (Step 9; norm I1).
- **Verify:** `cargo build && cargo test` green (empty); `cargo clippy -- -D warnings` clean.

## Step 3 — `proto` crate: the IPC contract

- Define the NDJSON envelope (serde, mirrors
  [architecture.md § IPC](architecture.md#ipc-protocol)):
  `Request { request_id, command, version, payload }`, `Cancel`, and the Python→Rust
  event union `Progress | Log | Result | Error`.
- Define the command param/result types as an enum/structs (start with `new_project`
  + a trivial `ping` / `app_info`) and the `ErrorCode` enum from command-surface.
- Add `specta` / `ts-rs` derives so types export to TS.
- **Verify:** `cargo test -p proto`; the type-export step produces `types.ts`.

## Step 4 — `app` crate: Tauri shell + trivial command

- `main.rs`: Tauri builder; register plugins `tauri-plugin-shell`,
  `tauri-plugin-store`, `tauri-plugin-dialog`; init `tracing` + `tracing-appender`.
- Register one `#[tauri::command] get_app_info()` returning a `proto` type (version +
  sidecar status) — the frontend smoke test.
- `tauri.conf.json`: window config, strict CSP (per architecture.md), bundle
  identifiers; `capabilities/` permission files.
- **Verify:** `pnpm tauri dev` opens a blank window (once Step 7 frontend exists).

## Step 5 — Python sidecar skeleton

- `python/pyproject.toml` (deps: `structlog` only for M0);
  `vocalboard_sidecar/__main__.py` NDJSON dispatch loop: read stdin lines → parse →
  route by `command` → write tagged responses to stdout; emit
  `{"type":"log","msg":"sidecar ready","request_id":null}` on startup.
- `registry.py` stub (lazy dict, no real models); handle a `ping` command returning
  `{type:result, payload:{pong:true}}`.
- A trivial `pytest` in `python/tests/` exercising the dispatch/parse function.
- **Verify:** `python -m vocalboard_sidecar` + a piped `ping` request returns a result;
  `pytest` green.

## Step 6 — SidecarManager: prove Rust ↔ Python

- In `core/task/`: spawn the sidecar (dev: `python -m vocalboard_sidecar`), read
  stdout NDJSON, route by `request_id`, await "sidecar ready" with a 30s timeout, log
  it.
- Wire `get_app_info` / a `ping_sidecar` command to send a request and surface the
  `pong`.
- **Verify:** launching the app logs "sidecar ready"; `ping_sidecar` returns from
  Python.

## Step 7 — SvelteKit frontend scaffold + round-trip

- `create-svelte` with `@sveltejs/adapter-static`; add Tailwind v4, Bits UI, Paraglide
  (`messages/en.json` + compile step).
- `src/lib/ipc/commands.ts` typed `invoke` wrappers; `src/lib/ipc/types.ts` (generated
  in Step 3).
- `routes/+page.svelte` welcome stub that calls `get_app_info` / `ping_sidecar` and
  renders the result — the **end-to-end smoke test**.
- Add `eslint` + `eslint-plugin-svelte` with the `svelte/a11y-*` rules enabled, and configure
  `svelte-check` to fail on a11y warnings ([conventions.md](conventions.md) D1). Add a lightweight
  no-hardcoded-string check for markup text (eslint rule or a small script; scope as SHOULD if no
  clean off-the-shelf rule fits) to enforce D2.
- **Verify:** `pnpm check && pnpm lint && pnpm build` green; in `tauri dev` the page shows the app
  version and sidecar pong.

## Step 8 — App settings

- `settings.json` schema + load/migrate via `tauri-plugin-store`; a `Settings` struct
  with the Phase-1 defaults (from
  [ops.md § Settings schema](ops.md#settings-schema-phase-1)) and a `version` field +
  a no-op v1 migration scaffold.
- Seed the format round-trip fixture pattern ([conventions.md](conventions.md) G1): a
  `tests/fixtures/` set of prior-format files + a test that loads each through the migration path.
  M0 covers `settings.json`; later milestones extend it to project files as those formats land.
- **Verify:** first launch writes defaults; unknown keys preserved on round-trip; the fixture
  round-trip test passes.

## Step 9 — CI skeleton

- `.github/workflows/ci.yml` with three jobs: `rust-tests` (`fmt --check`,
  `clippy -D warnings`, `test --workspace`, `cargo audit`, `cargo deny check` on
  ubuntu/windows/macos), `python-tests` (`pytest`, `pip-audit`), `frontend-tests`
  (`pnpm check` / `lint` / `test` / `build`).
- Widen `python-tests` and `frontend-tests` from ubuntu-only to the
  ubuntu/windows/macos matrix the `rust-tests` job uses ([conventions.md](conventions.md) M1).
- Seed one trivial test per language so runners aren't empty.
- **Verify:** CI green on a throwaway PR.

## M0 exit criteria

- `cargo build && cargo test`, `pytest`, and `pnpm check && pnpm build` all green
  locally and in CI.
- `pnpm tauri dev` launches the window, spawns the sidecar, logs "sidecar ready", and
  the welcome page round-trips `get_app_info` + `ping_sidecar`
  (frontend → Rust → Python → back).
- The `proto` contract + generated TS types are in place; the `core/` module tree
  exists as stubs ready for M1.
