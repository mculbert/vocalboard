#!/usr/bin/env bash
# Generate API docs for all three languages (or a subset via flags).
# Must be run from the repository root.
set -euo pipefail

RUST_ONLY=0
PYTHON_ONLY=0
FRONTEND_ONLY=0

for arg in "$@"; do
  case "$arg" in
    --rust-only)     RUST_ONLY=1 ;;
    --python-only)   PYTHON_ONLY=1 ;;
    --frontend-only) FRONTEND_ONLY=1 ;;
    *) echo "Unknown flag: $arg" >&2; exit 1 ;;
  esac
done

# If no flags given, run all three.
if [[ $RUST_ONLY -eq 0 && $PYTHON_ONLY -eq 0 && $FRONTEND_ONLY -eq 0 ]]; then
  RUST_ONLY=1; PYTHON_ONLY=1; FRONTEND_ONLY=1
fi

run_rust() {
  echo "==> Rust API docs"
  if ! command -v rustup &>/dev/null; then
    echo "  WARNING: rustup not found; skipping Rust docs." >&2
    python3 scripts/rustdoc_to_md.py --stubs-only
    return
  fi
  if ! rustup toolchain list | grep -q nightly; then
    echo "  WARNING: nightly toolchain not installed; emitting placeholder stubs." >&2
    python3 scripts/rustdoc_to_md.py --stubs-only
    return
  fi
  echo "  Generating rustdoc JSON (nightly)…"
  rustup run nightly cargo doc --no-deps \
    -Z unstable-options --output-format json \
    --manifest-path src-tauri/Cargo.toml --workspace
  python3 scripts/rustdoc_to_md.py
}

run_python() {
  echo "==> Python API docs"
  if ! command -v pydoc-markdown &>/dev/null; then
    echo "  WARNING: pydoc-markdown not found; skipping Python docs." >&2
    echo "  Install with: uv pip install -e 'python/[docs]'" >&2
    return
  fi
  pydoc-markdown
}

run_frontend() {
  echo "==> Frontend API docs"
  pnpm run docs:api:frontend
}

[[ $RUST_ONLY     -eq 1 ]] && run_rust
[[ $PYTHON_ONLY   -eq 1 ]] && run_python
[[ $FRONTEND_ONLY -eq 1 ]] && run_frontend

echo "Done."
