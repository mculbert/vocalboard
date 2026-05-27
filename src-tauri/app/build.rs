//! Build script for the Vocalboard `app` crate.
//!
//! Runs `tauri-build` to generate the Tauri context and register
//! platform-specific resources (window icons, capabilities, etc.).
fn main() {
    tauri_build::build()
}
