# Generate API docs for all three languages (or a subset via flags).
# Must be run from the repository root.
param(
    [switch]$RustOnly,
    [switch]$PythonOnly,
    [switch]$FrontendOnly
)

$ErrorActionPreference = "Stop"

# If no flags given, run all three.
if (-not $RustOnly -and -not $PythonOnly -and -not $FrontendOnly) {
    $RustOnly = $true; $PythonOnly = $true; $FrontendOnly = $true
}

function Run-Rust {
    Write-Host "==> Rust API docs"
    if (-not (Get-Command rustup -ErrorAction SilentlyContinue)) {
        Write-Warning "rustup not found; emitting placeholder stubs."
        python scripts/rustdoc_to_md.py --stubs-only
        return
    }
    $toolchains = & rustup toolchain list 2>&1
    if ($toolchains -notmatch "nightly") {
        Write-Warning "nightly toolchain not installed; emitting placeholder stubs."
        python scripts/rustdoc_to_md.py --stubs-only
        return
    }
    Write-Host "  Generating rustdoc JSON (nightly)..."
    & rustup run nightly cargo doc --no-deps `
        -Z unstable-options --output-format json `
        --manifest-path src-tauri/Cargo.toml --workspace
    python scripts/rustdoc_to_md.py
}

function Run-Python {
    Write-Host "==> Python API docs"
    if (-not (Get-Command pydoc-markdown -ErrorAction SilentlyContinue)) {
        Write-Warning "pydoc-markdown not found; skipping Python docs."
        Write-Warning "Install with: uv pip install -e 'python/[docs]'"
        return
    }
    & pydoc-markdown
}

function Run-Frontend {
    Write-Host "==> Frontend API docs"
    & pnpm run docs:api:frontend
}

if ($RustOnly)     { Run-Rust }
if ($PythonOnly)   { Run-Python }
if ($FrontendOnly) { Run-Frontend }

Write-Host "Done."
