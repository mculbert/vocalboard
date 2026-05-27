#!/usr/bin/env python3
"""Convert rustdoc JSON output to Markdown stubs for the Hugo docs site.

Full implementation deferred to M7. This stub emits one placeholder .md per
crate so Hugo doesn't error on empty api/rust/ directories.

Usage:
    rustup run nightly cargo doc --no-deps -Z unstable-options \
        --output-format json --manifest-path src-tauri/Cargo.toml \
        --workspace
    python3 scripts/rustdoc_to_md.py [--json-dir <dir>] [--out-dir <dir>]

    --json-dir  Directory containing rustdoc-generated .json files
                (default: src-tauri/target/doc)
    --out-dir   Output directory for generated Markdown
                (default: docs/content/internals/api/rust)
"""
import argparse
import glob
import os
import sys


CRATES = ["app", "core", "proto"]
PLACEHOLDER = """\
---
title: {crate}
---

Auto-generated — run `scripts/gen_api_docs.sh --rust-only` to populate.

> Full Rust API documentation requires the nightly toolchain:
> ```
> rustup run nightly cargo doc --no-deps -Z unstable-options \\
>     --output-format json --manifest-path src-tauri/Cargo.toml --workspace
> python3 scripts/rustdoc_to_md.py
> ```
"""


def emit_placeholders(out_dir: str) -> None:
    os.makedirs(out_dir, exist_ok=True)
    for crate in CRATES:
        path = os.path.join(out_dir, f"{crate}.md")
        if not os.path.exists(path):
            with open(path, "w") as f:
                f.write(PLACEHOLDER.format(crate=crate))
            print(f"  wrote {path}")


def convert_json(_json_dir: str, _out_dir: str) -> None:
    # M7: walk rustdoc JSON and emit real Markdown
    raise NotImplementedError("Full rustdoc→Markdown conversion deferred to M7")


def main() -> None:
    description = (__doc__ or "").splitlines()[0]
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("--json-dir", default="src-tauri/target/doc")
    parser.add_argument("--out-dir", default="docs/content/internals/api/rust")
    parser.add_argument(
        "--stubs-only",
        action="store_true",
        help="Emit placeholder stubs even if JSON is present (default when JSON absent)",
    )
    args = parser.parse_args()

    json_files = glob.glob(os.path.join(args.json_dir, "*.json"))
    if args.stubs_only or not json_files:
        if not args.stubs_only:
            print("No rustdoc JSON found; emitting placeholder stubs.", file=sys.stderr)
        emit_placeholders(args.out_dir)
    else:
        convert_json(args.json_dir, args.out_dir)


if __name__ == "__main__":
    main()
