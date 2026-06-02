//! Integration test that spawns the real Python sidecar.
//!
//! Gated behind the `sidecar-integration` feature so a bare
//! `cargo test --workspace` never requires Python on PATH. Run with (from
//! `src-tauri/`):
//!
//! ```bash
//! cargo test -p core --features sidecar-integration --test sidecar_integration
//! ```
//!
//! `VOCALBOARD_PYTHON` selects the interpreter that has the sidecar installed
//! (CI points it at the synced uv virtualenv); it defaults to `python` on PATH.
#![cfg(feature = "sidecar-integration")]

use vb_core::task::SidecarManager;

fn python_bin() -> String {
    std::env::var("VOCALBOARD_PYTHON").unwrap_or_else(|_| "python".to_owned())
}

/// Spawn the sidecar, await the ready handshake, then ping and check pong.
#[tokio::test]
async fn sidecar_start_and_ping() -> anyhow::Result<()> {
    let bin = python_bin();
    let mgr = SidecarManager::start(&bin, &["-m", "vocalboard_sidecar"]).await?;
    let result = mgr.ping().await?;
    assert!(result.pong, "expected pong == true");
    Ok(())
}
