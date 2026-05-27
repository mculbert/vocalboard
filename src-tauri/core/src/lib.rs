//! Vocalboard core engine: project state, persistence, audio, and task management.
#![warn(missing_docs)]

pub mod audio;
pub mod db;
pub mod ipc;
pub mod project;
pub mod settings;
pub mod task;

pub use task::SidecarManager;
