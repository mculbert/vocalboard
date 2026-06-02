//! Generator for TypeScript IPC bindings.
//!
//! Default: writes `src/lib/ipc/types.ts` from the current proto types.
//! `--check`: compares the committed file to the generated output and exits non-zero on mismatch.

use std::path::PathBuf;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let out_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../src/lib/ipc/types.ts");

    let generated = proto::bindings::render();

    let check_mode = std::env::args().any(|a| a == "--check");
    if check_mode {
        let current = std::fs::read_to_string(&out_path)
            .with_context(|| format!("could not read {}", out_path.display()))?;
        if current != generated {
            eprintln!(
                "TypeScript bindings are out of date.\n\
                 Run `cargo run -p proto --features ts-export --bin gen_bindings` to regenerate."
            );
            std::process::exit(1);
        }
        println!("TypeScript bindings are up to date.");
    } else {
        std::fs::create_dir_all(out_path.parent().context("no parent dir")?)?;
        std::fs::write(&out_path, &generated)
            .with_context(|| format!("could not write {}", out_path.display()))?;
        println!("TypeScript bindings written to {}", out_path.display());
    }

    Ok(())
}
