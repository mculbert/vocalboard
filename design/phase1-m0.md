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

- `create-svelte` with `@sveltejs/adapter-static`; add Tailwind v4, Bits UI,
  shadcn-svelte (New York style), Paraglide (`messages/en.json` + compile step).
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

## Step 9 — Docs skeleton

Sets up the Hugo site and auto-generation wiring so the docs build now (with
placeholders) and real content can be added incrementally through M7.

- **Hugo site:** initialise `docs/` as a Hugo module using the
  [Hextra](https://github.com/imfing/hextra) theme (markdown-native, no
  Node build step); add `docs/public/` and `docs/resources/` to `.gitignore`.
  `docs/hugo.toml` sets `title = "Vocalboard"`, `baseURL = "/"`, and
  `theme = "hextra"`.
- **Content directory tree** (mirrors [ops.md § Repository layout](ops.md#repository-layout)):
  - `docs/content/reference/_index.md` — placeholder ("User reference manual — to be
    written in M7")
  - `docs/content/settings/_index.md` — placeholder linking to the settings schema in
    `design/ops.md`; a brief summary table of the Phase 1 keys serves as an interim
    reference
  - `docs/content/internals/_index.md` — one-paragraph orientation pointing to
    `design/index.md` as the authoritative TDD until M7 hand-authored overviews land
  - `docs/content/internals/api/rust/_index.md`, `api/python/_index.md`,
    `api/frontend/_index.md` — each notes "Auto-generated — run `scripts/gen_api_docs.sh`
    to populate"
- **Auto-gen tool dependencies (build-only; zero runtime cost):**
  - Python: add `pydoc-markdown` to `pyproject.toml` under
    `[project.optional-dependencies] docs`; add a `pydoc-markdown.yml` config
    pointing at `python/vocalboard_sidecar/`, output to
    `docs/content/internals/api/python/`
  - Frontend: add `typedoc` + `typedoc-plugin-markdown` to `devDependencies`; add
    `typedoc.json` pointing at `src/lib/**/*.ts` with
    `"out": "docs/content/internals/api/frontend"`; add `"docs:api:frontend": "typedoc"`
    to `package.json` scripts
  - Rust: `rustdoc --output-format json` requires nightly (the app itself stays on
    stable); add a `scripts/rustdoc_to_md.py` stub that reads rustdoc JSON and emits
    Markdown stubs (full implementation deferred to M7; stub emits one placeholder
    `.md` per crate so Hugo doesn't error on empty directories); the script documents
    the `rustup run nightly` invocation required to produce its input
- **`scripts/gen_api_docs.sh`** (bash) and **`scripts/gen_api_docs.ps1`** (PowerShell):
  each calls all three generators in sequence; accept `--rust-only`, `--python-only`,
  `--frontend-only` flags; the Rust step no-ops if `nightly` toolchain is absent (with
  a warning) so CI doesn't block on it until M7
- **npm scripts in `package.json`:**
  - `"docs:api"` → `scripts/gen_api_docs.sh` (or `.ps1` on Windows)
  - `"docs:build"` → `hugo --source docs`
- **Verify:** `pnpm run docs:build` completes with no errors on the placeholder
  content; `pnpm run docs:api` runs without fatal error (stubs emit placeholder
  output); the Hugo output contains pages for all four `internals/api/` subsections.

> **Deferred to M7:** hand-authored architecture/data-structure overviews under
> `internals/`; filling `reference/` and `settings/` manuals; wiring `docs:build`
> into CI; the Rust `rustdoc_to_md.py` full implementation; Nuitka binary build;
> real app icons replacing the placeholder `src-tauri/icons/icon.ico`.

## Step 10 — CI skeleton

- `.github/workflows/ci.yml` with three jobs: `rust-tests` (`fmt --check`,
  `clippy -D warnings`, `test --workspace`, `cargo audit`, `cargo deny check` on
  ubuntu/windows/macos), `python-tests` (`pytest`, `pip-audit`), `frontend-tests`
  (`pnpm check` / `lint` / `test` / `build`).
- Widen `python-tests` and `frontend-tests` from ubuntu-only to the
  ubuntu/windows/macos matrix the `rust-tests` job uses ([conventions.md](conventions.md) M1).
- Seed one trivial test per language so runners aren't empty.
- **Sidecar integration test in `rust-tests`:** `cargo test --workspace` includes
  `core::task::tests::sidecar_start_and_ping`, which requires a working Python
  interpreter with `vocalboard-sidecar` installed. The `rust-tests` job must either
  install the sidecar (`uv pip install -e python/`) and set `VOCALBOARD_PYTHON` to
  that interpreter, or set `SKIP_SIDECAR_TESTS=1` to skip it (acceptable only if
  the `python-tests` job already covers the sidecar logic via `pytest`).
- **Verify:** CI green on a throwaway PR.

## M0 exit criteria

- `cargo build && cargo test`, `pytest`, and `pnpm check && pnpm build` all green
  locally and in CI.
- `pnpm tauri dev` launches the window, spawns the sidecar, logs "sidecar ready", and
  the welcome page round-trips `get_app_info` + `ping_sidecar`
  (frontend → Rust → Python → back).
- The `proto` contract + generated TS types are in place; the `core/` module tree
  exists as stubs ready for M1.
- `pnpm run docs:build` succeeds on the placeholder Hugo site; auto-gen wiring is in
  place for all three languages.
